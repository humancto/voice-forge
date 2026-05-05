# `voiceforge pack {list,install,remove,info}` (ROADMAP 6.2)

## Goal

Ship the user-facing pack distribution surface. After this lands:

```bash
voiceforge pack list                       # see what's available
voiceforge pack install peter              # one-command install
voiceforge pack info peter                 # what's in this pack
voiceforge pack remove peter
voiceforge play --pack peter --event tests_passed   # already shipped in 6.5
```

This closes the loop between `voice-forge-packs` (the content repo, already live) and `voiceforge play` (the runtime, shipped in PR #17). Without this PR, packs install via curl-tarball-yourself.

## Surface

```
voiceforge pack list [--json]
  Lists all packs available in the index (humancto/voice-forge-packs)
  with their installed status. JSON for tooling.

voiceforge pack install <NAME> [--force]
  Downloads <NAME>.tar.gz from the pack index, sha256-verifies, extracts
  to ~/.voiceforge/packs/<NAME>/. --force replaces an existing install.

voiceforge pack info <NAME>
  Shows manifest.toml: source, license, attribution, phrase count.
  Works on installed packs (reads ~/.voiceforge/packs/<NAME>/manifest.toml)
  and on uninstalled packs (fetches manifest URL from the index).

voiceforge pack remove <NAME> [--force]
  Removes ~/.voiceforge/packs/<NAME>/. Confirmation prompt unless
  --force is passed (or stdin is not a TTY in which case --force is
  required, mirroring `voiceforge voices remove`).
```

Exit codes per subaction:

- `list`: 0 always (empty index = empty output, exit 0)
- `install`: 0 success / 2 unknown pack / 3 download failed / 4 sha256 mismatch / 5 extract failed / 6 already installed (without --force)
- `info`: 0 success / 2 unknown pack / 3 manifest fetch failed / 4 manifest parse failed
- `remove`: 0 success / 2 not installed / 3 user aborted

## File layout assumed (already in voice-forge-packs)

```
voice-forge-packs/
├── packs.json                      # index, schema_version 1
├── packs/<name>/
│   ├── manifest.toml
│   ├── reference.wav
│   ├── wav/<event>.wav
│   └── checksums.txt
```

Tarballs (`<name>.tar.gz`) are GitHub release assets, served via:

- `https://github.com/humancto/voice-forge-packs/releases/latest/download/<name>.tar.gz`

ROADMAP 6.7 adds the GitHub Action that builds these on tag push. Until 6.7 ships, we'll need a manual one-time tarball release for Peter.

## Where the index lives

The canonical URL: `https://raw.githubusercontent.com/humancto/voice-forge-packs/main/packs.json`

Already declared in `packs.json::index_url`. Hard-code this default; allow override via `VOICEFORGE_PACK_INDEX_URL` env var for testing + future mirroring.

## Code locations

- `apps/voiceforge-cli/src/main.rs` — new `Commands::Pack { action: PackAction }` enum + dispatch.
- `apps/voiceforge-cli/src/packs.rs` — extend with:
  - `PackIndex` / `PackEntry` (deserialize `packs.json` with `#[serde(deny_unknown_fields)]`).
  - `PackManifest` (deserialize `manifest.toml`).
  - **`InstallError`** as a new `#[non_exhaustive]` enum (separate from `PlayError` so the exit-code contracts don't collide). Variants:
    - `IndexFetch(reqwest::Error)` — exit 3
    - `IndexParse(serde_json::Error)` — exit 3
    - `UnknownPack(String)` — exit 2
    - `Sha256Mismatch { expected: String, actual: String }` — exit 4
    - `BadEntry(PathBuf, String)` — exit 5 (tarball entry rejected: traversal, symlink, etc.)
    - `ManifestSchema(u32)` — exit 5 (manifest version drift)
    - `ChecksumMismatch(PathBuf)` — exit 5
    - `AlreadyInstalled(String)` — exit 6
    - `LockHeld(String)` — exit 6 (concurrent install)
    - `DiskSpace { needed: u64, available: u64 }` — exit 5
    - `Io(#[from] std::io::Error)` — exit 5
    - `RequestTimeout` — exit 3
  - `fetch_index() -> Result<PackIndex, InstallError>` — async `reqwest::Client::get` (NOT blocking). User-Agent `voiceforge/0.1.0`. 60s overall, 10s connect timeout. Catch `reqwest::Error::is_timeout()` explicitly → `InstallError::RequestTimeout` (not swallowed by generic `IndexFetch`).
  - `LockGuard` RAII struct wrapping the `.<name>.lock.d` mkdir — `Drop` impl logs-and-swallows on `rm -rf` failure (panicking in Drop during unwind aborts the process). Acquired before any FS work in `install_pack`; released automatically on every exit path including panic.
  - `HashingWriter` impls `tokio::io::AsyncWrite` (not `std::io::Write`) since the reqwest body is async-streamed via `tokio_util::io::StreamReader`.
  - `install_pack(name: &str, force: bool) -> Result<(), InstallError>` — orchestrates the 14-step flow above. Async download; `spawn_blocking` for tar+flate2 extraction.
  - `remove_pack(name: &str) -> Result<()>` — same canonicalize+prefix posture as `voices::remove_cloned_voice`. Use the existing `PlayError::PackMissing` variant where appropriate.
  - `pack_info(name: &str, fetch_remote: bool) -> Result<PackManifest, InstallError>` — reads installed manifest first, falls through to manifest_url fetch if not installed and `fetch_remote=true`.
  - `list_installed() -> Result<Vec<String>>` — readdir packs_root, **skip `*.old`, `*.partial`, `*.partial.tar.gz`, `*.lock.d`**.

- `HashingWriter` adapter in packs.rs — wraps a writer + Sha256, `write` updates both.

- New deps in `Cargo.toml`:
  - `tar = "0.4"` — tarball extraction
  - `flate2 = "1"` — gzip decompression
  - `fs2 = "0.4"` — `available_space` for disk-space precheck
  - `tokio-util = { version = "0.7", features = ["io"] }` — `StreamReader` bridges `bytes_stream()` to `AsyncRead` for `copy_buf`
  - **`reqwest` features changed**: `default-features = false, features = ["json", "rustls-tls", "stream"]` (was `["json"]` which gave us platform-default-tls). Explicit rustls-tls + the `stream` feature for `bytes_stream()`.
  - (`sha2` already present)
  - (`serde_json` already present for the index)
- New dev-deps:
  - `mockito = "1"` for the HTTP test fixture.

## Atomic install (v2 — all rust-expert REVISE feedback applied)

**Per-process lock first**: `mkdir(<packs_root>/.<name>.lock.d)` is atomic on POSIX and Windows. If it fails (EEXIST), error with "another install of <name> is in progress" exit 6. Drop the lock with `rm -rf` on every exit path. Mirrors the cloning install pattern.

Then download → verify → extract → atomic rename:

1. **Pre-cleanup (before doing anything)**: if `<name>.old` exists from a prior crashed install, `rm -rf <name>.old`. Same for `<name>.partial`. Logs to stderr.
2. **Disk-space precheck**: `fs2::available_space(packs_root)` must be ≥ `tarball_size_bytes * 3` (tarball + extracted + slack). Else error early.
3. `tarball_path = <packs_root>/.<name>.partial.tar.gz`
4. **Streaming download with single-pass sha256**: open the file as a `HashingWriter { inner: tokio::fs::File, hasher: sha2::Sha256 }`. Use `reqwest::Client::get(url).send().await?.bytes_stream()` and `tokio::io::copy_buf` into the hashing writer. After EOF, finalize the hasher. **No re-read of the file to compute the hash** — one pass.
5. Compare digest against `packs.json::packs.<name>.tarball_sha256`. If mismatch: delete tarball_path, error 4.
6. `staging = <packs_root>/.<name>.partial` directory.
7. **Extract in `spawn_blocking`** (tar + flate2 are sync). Use `tar::Archive::entries()` (NOT `unpack()`):
   - For each `entry`:
     - `header().entry_type()` must be `Regular` or `Directory`. Reject `Symlink`/`Link`/`Char`/`Block`/`Fifo`/`GNUSparse` — clear error citing the entry path.
     - `entry.path()?` must not be absolute and must contain no `..` / `Prefix` / `RootDir` components.
     - Set `entry.set_preserve_permissions(false)` and `entry.set_preserve_mtime(false)` to avoid surprise +x or backdated files.
     - `entry.unpack_in(&staging)?` (the `_in` form does its own containment check since tar 0.4.40+; we keep our own as defense-in-depth).
8. **Validate `staging/manifest.toml` schema_version BEFORE the swap**. Parse with `toml`. If `schema_version != 1`, `rm -rf staging`, error. The `<name>.old` rename never happens for a schema-mismatch, so we can't end up with a half-installed broken pack.
9. **Parse `staging/checksums.txt` in Rust** (don't shell out to `sha256sum -c` — won't exist on Windows / minimal containers). Format is `<hex>  <relpath>` per line. For each line:
   - Reject relpath containing `..`, absolute, or with `\\` in it.
   - Recompute sha256 with `sha2`. Mismatch → error.
10. If a previous install exists at `<packs_root>/<name>/`:
    - If `--force`: `rename(<name>, <name>.old)`, then continue.
    - Else: error 6 with message "use --force".
11. `rename(staging, <packs_root>/<name>)`.
12. **Best-effort cleanup**: try to `rm -rf <name>.old`. If this fails (Windows file-in-use), log warning to stderr but **return success** — the new pack is in place; cleanup is non-load-bearing. Next `install --force` (step 1) will mop up.
13. `rm tarball_path`.
14. Drop the lock dir.

`pack list` / `list_installed` MUST skip `*.old`, `*.partial`, `*.partial.tar.gz`, `*.lock.d` — same posture as `voices.rs` already does.

## Path-traversal posture

Tarball extraction is the major attack surface. Already covered in step 7 above. Summary:

- Reject any non-Regular / non-Directory entry type (no symlinks, hardlinks, devices, fifos, sparse).
- Reject any path containing `..`, absolute paths, drive prefixes.
- `unpack_in` provides a final containment check; we keep our own pre-check for clearer errors.
- `set_preserve_permissions(false)` + `set_preserve_mtime(false)` to avoid +x / backdate surprises.
- Plus `validate_pack_name` on the user-provided `<NAME>` arg.

## Tests

`#[serial]` for env-mutating tests.

1. `validate_pack_name` already covered (PR #17).
2. `PackIndex` deserialize: round-trip `packs.json` test fixture.
3. `install_pack`: happy path with a mock HTTP server (`mockito` or hand-rolled `tiny_http` test fixture). Asserts file layout and sha256 verification.
4. `install_pack`: sha256 mismatch → error code 4.
5. `install_pack`: already installed without --force → error 6.
6. `install_pack`: already installed with --force → succeeds, replaces.
7. `install_pack`: tarball with `..` entry → rejected before extraction.
8. `install_pack`: tarball with absolute-path entry → rejected.
9. `install_pack`: tarball with symlink → rejected.
10. `remove_pack`: happy path.
11. `remove_pack`: not installed → error 2.
12. `remove_pack`: --force skips confirm; non-TTY without --force errors.
13. `pack_info`: installed pack reads from disk.
14. `pack_info`: uninstalled pack fetches manifest URL.
15. `Commands::Pack { action: List }` happy path with mock index.
16. `Commands::Pack { action: List, json: true }` — output is parseable JSON.

## Test fixtures

Build a tiny tarball at test-fixture-build time with `tar` crate's writer API:

- `tests/fixtures/tiny_pack.tar.gz` — minimal valid pack (manifest + 1 WAV + checksums.txt).
- Test HTTP server serves this fixture at `/peter.tar.gz` with corresponding `/packs.json` index.

## Edge cases

1. **Network down** — clear error message, exit 3, suggest `--offline` mode (later).
2. **Disk full** — caught early by step 2 disk-space precheck; if it slips past, `tokio::io::copy_buf` errors mid-stream and we leave `.partial` for next-attempt cleanup.
3. **HTTPS cert validation** — explicit rustls-tls (not platform-default-tls). Refuse `http://` and `file://` URLs in `VOICEFORGE_PACK_INDEX_URL` unless `VOICEFORGE_DEV=1`.
4. **Index schema drift** — if `packs.json::schema_version != 1`, error with "this voiceforge expects v1, index is vN; upgrade voiceforge".
5. **Pack manifest schema drift** — gated as step 8 of install: `staging/manifest.toml::schema_version != 1` → refuse install, `rm -rf staging`, leave nothing behind.
6. **Concurrent installs** — `mkdir(.<name>.lock.d)` provides a per-pack lock. Two concurrent `install peter` → second one fails fast with "another install of peter is in progress."
7. **`--force` removes a pack that's currently being played** — rodio holds the file open. macOS/Linux allow unlink-while-open; the playing process keeps the inode. Behavior is fine.
8. **HTTP timeouts** — `Client::builder().timeout(60s).connect_timeout(10s)`. Without these a hung TCP connection wedges the CLI forever.
9. **User-Agent** — `voiceforge/0.1.0` so GitHub log forensics is possible later. Set on the `Client::builder()`.
10. **CI live test** — gate the real-network test behind `#[ignore]` + a separate CI step. Rate-limited by GitHub for unauthenticated CI.

## What's NOT in this PR

- `voiceforge pack update` (refresh installed packs to latest). Deferred to 6.2.1.
- `voiceforge pack search <query>`. Deferred.
- Pack signing / checksum-of-checksums. Defer to security follow-up.
- Tarball release automation in voice-forge-packs (ROADMAP 6.7).
- `--offline` mode for `pack list` (cached index). Deferred.

## Migration / compatibility

Brand new subcommand. No breaking changes.

## Plan order

1. `Cargo.toml`: add `tar`, `flate2`, `fs2`; switch `reqwest` to `default-features=false, features=["json","rustls-tls","stream"]`; dev-dep `mockito = "1"`.
2. `packs.rs`: `InstallError` enum, `PackIndex`/`PackEntry`/`PackManifest` deserialize structs.
3. `HashingWriter` adapter + unit test for it (1 byte at a time, large chunks, hash matches `sha2::Sha256` reference).
4. `fetch_index()` with mockito — happy path, schema_version drift, parse error.
5. `install_pack` skeleton: lock dir, pre-cleanup, disk-space precheck.
6. `install_pack` streaming download with HashingWriter — happy path against mockito-served fixture tarball.
7. `install_pack` extraction with `Archive::entries()` per-entry validation.
8. Test fixtures: build `tests/fixtures/build_tarballs.rs` (or a `build.rs` step) that creates `tiny_pack.tar.gz` + 4 attack tarballs (`..-traversal.tar.gz`, `absolute-path.tar.gz`, `symlink.tar.gz`, `hardlink.tar.gz`). Each attack tarball must be rejected by step 7 with the right `BadEntry` payload.
9. `install_pack` atomic rename + `.old` cleanup.
10. Concurrent-install lock test: spawn two threads racing on the same pack name; assert one gets `LockHeld`.
11. `remove_pack` + tests.
12. `pack_info` + tests.
13. `list_installed` + tests (must skip `.old`/`.partial`/`.lock.d`).
14. `Commands::Pack { action }` clap variant + dispatch in main.rs.
15. Integration test: full `voiceforge pack install peter` end-to-end against mockito serving the fixture index + tarball.
16. `#[ignore]`-gated live test: `voiceforge pack install peter` against the real GitHub release at `voice-forge-packs/v0.1.0`.
17. Docs: README replaces curl-tarball example with `voiceforge pack install peter`; ROADMAP 6.2 → done.

Estimated diff size: **~700 LOC** including the InstallError enum, HashingWriter adapter, build_tarballs.rs test fixture builder, and ~16 tests. Larger than v1 estimate because of the fixture build + concurrent-lock test + per-attack-vector rejection tests.

## What I disagreed with from review v1

Nothing. All 15 items landed.
