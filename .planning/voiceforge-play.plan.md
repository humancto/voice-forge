# `voiceforge play` — runtime pack playback (ROADMAP 6.5)

> **Plan v2** — incorporates rust-expert REVISE feedback on v1.

## Goal

Ship a Rust subcommand that takes `(pack, event)` and plays the corresponding pre-rendered WAV in **sub-100ms warm-path**. This is the runtime that makes the pack distribution layer useful from agent hooks.

```bash
voiceforge play --pack peter --event tests_passed
voiceforge play --pack peter --event build_failed
```

## Why this is its own subcommand (not `say --pack`)

`say` synthesizes from text — pays a TTS engine cost, has cold-start, can fall back across `Embedded`/`Server`/`Cloning` engines.

`play` is a **WAV-file lookup + playback**. No model load, no TTS engine, no fallback chain. Just resolve `~/.voiceforge/packs/<pack>/wav/<event>.wav` and stream it through `audio::play`. The latency profile is fundamentally different (~50ms vs ~2s) and the failure modes are fundamentally different (file-missing vs synth-failed).

**Per rust-expert feedback v1: dropping `--fallback` from this PR.** Fallback is a _caller_ concern (the future `voiceforge hook` in 3.3 can shell out to `voiceforge say` on exit-3). Subcommand-level fallback would couple `play` to the entire `tts` module's startup cost — defeating the sub-100ms target on the fallback path.

## Surface

```
voiceforge play [OPTIONS] --pack <NAME> --event <ID>

Options:
  --pack <NAME>      Pack to look up. Must be installed under ~/.voiceforge/packs/<NAME>/
  --event <ID>       Event id; resolves to <pack>/wav/<event>.wav
  --list             Print all events available in the pack and exit
  -h, --help
```

Exit codes:

- 0 — played successfully
- 2 — pack not installed (`~/.voiceforge/packs/<pack>/` is not a directory)
- 3 — event not in pack (pack exists, but `wav/<event>.wav` missing)
- 4 — WAV present but corrupt / undecodable by rodio
- 5 — audio backend unavailable (rodio `OutputStream::try_default` failed)

Callers fall back at exit-3. Document this clearly in `--help`.

## Resolution order (this is load-bearing — see Showstopper #2 in v1 review)

`canonicalize()` errors with `NotFound` if any path component is absent, which collapses the "pack missing vs event missing" distinction we need. Explicit branches:

1. **`validate_pack_name(&pack)`** — ascii lowercase, digits, `_`, `-`; 1..=64 chars; reject reserved names (`.`, `..`, `cache`, `voices`, `cloning`, etc.); reject any containing `.`.
2. **`validate_event_id(&event)`** — same rules. Reject any containing `.` so `../../etc/passwd` and `foo.bar` both fail at parse.
3. Compute `packs_root = paths::voiceforge_home()?.join("packs")`. Canonicalize once.
4. `pack_dir = packs_root.join(&pack)`. Check `pack_dir.is_dir()` — if false, return `PlayError::PackMissing` (exit 2).
5. Canonicalize `pack_dir`. Assert `canonical_pack_dir.starts_with(&packs_root_canonical)`. If not, this is a symlink-escape; reject as `PackMissing` (don't reveal that the symlink existed).
6. `wav_path = pack_dir.join("wav").join(format!("{event}.wav"))`. Check `wav_path.is_file()` — if false, `EventMissing` (exit 3).
7. Canonicalize `wav_path`. Assert it starts with `canonical_pack_dir.join("wav")` **(per-pack `wav/`, not packs_root — per rust-expert showstopper #1)**. If not, `EventMissing`.
8. Hand off to `audio::play(canonical_wav.to_str()?)`.

Cross-pack symlinks are blocked by the per-pack-wav prefix check, not the packs-root check.

## Code locations

- `apps/voiceforge-cli/src/main.rs` — new `Commands::Play { pack, event, list }` clap variant.
- `apps/voiceforge-cli/src/packs.rs` — new module:
  - `validate_pack_name`, `validate_event_id` — same negative-character rules as `voices::validate_name`, plus reject `.`.
  - `pub fn pack_dir(pack: &str) -> Result<PathBuf>` — name validation only, no FS check.
  - `pub fn list_events(pack: &str) -> Result<Vec<String>>` — readdir `wav/`, strip `.wav`, sort lexicographically. Same canonicalize+prefix posture as `resolve_event_wav` (per rust-expert risk #6).
  - `pub fn resolve_event_wav(pack: &str, event: &str) -> Result<PathBuf, PlayError>` — implements steps 1–7 above.
  - `pub enum PlayError` (`#[non_exhaustive]`) with variants:
    - `InvalidName(String)` — exit 2
    - `PackMissing(String)` — exit 2
    - `EventMissing { pack: String, event: String }` — exit 3
    - `WavCorrupt { path: PathBuf, #[source] source: rodio::DecoderError }` — exit 4
    - `AudioBackend(#[source] rodio::StreamError)` — exit 5
    - `Io(#[from] io::Error)` — covers EACCES on readdir, etc. Maps to exit 2 or 5 by context (use `#[from]` for the variant; explicit conversion at the dispatch boundary).
- `apps/voiceforge-cli/src/audio.rs` — already has `play(path: &str)`. Reuse. **Wrap call in `tokio::task::spawn_blocking`** from the dispatch — rodio's `sleep_until_end` blocks (per rust-expert missing #2).
- `apps/voiceforge-cli/src/paths.rs` — add `pub fn packs_root() -> Result<PathBuf>` if not present.

## Path-traversal posture

(Spelled out in resolution order above.)

Per-pack `wav/` prefix check, not packs-root prefix check. The latter would let an attacker with write access to one pack symlink into another pack. The former blocks it.

### TOCTOU surface (per rust-expert v2 review)

The `is_dir → canonicalize → is_file → canonicalize` sequence has four windows where an attacker with write access to `~/.voiceforge/packs/` could swap a directory for a symlink between checks. The **`starts_with(...)` prefix check after each canonicalize is what makes the sequence sound** — remove either and there's a real vuln. The residual race (between final canonicalize and `audio::play`'s `File::open`) is benign: an attacker with write access to `<pack>/wav/` can already drop arbitrary WAVs there directly. Same trust boundary, no privilege escalation.

Document this in `resolve_event_wav`'s rustdoc:

> TOCTOU between resolve and open is bounded by the per-pack `wav/`
> prefix check; same trust boundary as direct write to `<pack>/wav/`.

## Tests

`#[serial]` for any test that mutates env (`VOICEFORGE_HOME`).

1. `validate_pack_name` — accepts `peter`, `obama_2`, `bob-ross`; rejects empty, > 64 chars, `Foo` (uppercase), `peter/x`, `..`, `peter.wav`, ` `.
2. `validate_event_id` — same negatives.
3. `resolve_event_wav("peter", "tests_passed")` — happy path with a fixture pack in tempdir; returns canonicalized PathBuf.
4. `resolve_event_wav("peter", "doesnt_exist")` — returns `PlayError::EventMissing`. Verify the error variant, not just is_err().
5. `resolve_event_wav("nonexistent", "x")` — returns `PlayError::PackMissing`. Verify variant.
6. `resolve_event_wav("peter", "../../etc/passwd")` — returns `PlayError::InvalidName` (validation rejects before any FS access).
7. **Symlink-escape test**: in a tempdir, create `packs/peter/wav/escape.wav` as a symlink pointing to `packs/other/wav/build_success.wav`. `resolve_event_wav("peter", "escape")` must return `EventMissing` (not silently leak the other pack's WAV). Per-pack-wav prefix check is what catches this.
8. `list_events("peter")` — returns sorted `Vec<String>` matching the fixture.
9. `list_events("nonexistent")` — returns `PlayError::PackMissing`.
10. `Commands::Play { list: true, pack, event: _ }` — prints sorted event names from a fixture pack, exits 0. Empty pack prints nothing, exits 0. Missing pack exits 2.
11. **Latency bench** — integration test that times `Commands::Play` end-to-end with a tiny test WAV (200ms of silence). Asserts wall-clock < 200ms warm (allowing 100ms for CoreAudio cold-start on first invocation; subsequent invocations should be < 50ms but we don't gate on that). Per rust-expert bug #4 — this gates the perf claim.

Use `tempfile::TempDir` + `VOICEFORGE_HOME=<tempdir>` for fixture pack tests. Test fixtures: synthesize 5 short WAVs at test-fixture-build time using `hound` (already a dev-dep).

## Edge cases

1. **Multiple files match `wav/<event>.*`?** No — only `wav/<event>.wav` is canonical. Manual `event.mp3` is ignored. (S2 Pro outputs WAV; no multi-format packs in v1.)

2. **WAV is corrupt or truncated?** rodio's `Decoder::new` returns `DecoderError`. Surface as `PlayError::WavCorrupt { path, source }` with exit 4. Don't fall through silently.

3. **No audio device?** `OutputStream::try_default` errors → `PlayError::AudioBackend` → exit 5.

4. **`--list` doesn't read `manifest.toml`** — just readdir `wav/`. Lets users introspect packs even with manifest schema drift.

5. **Concurrent invocations?** rodio handles multiple sinks. No locking needed.

6. **No cache** — pack WAVs are already on disk. Direct read.

7. **Logging** — print to stderr on miss, succeed silently on hit. Exit code carries the structured outcome for callers.

## What's NOT in this PR

- Pack installation (`voiceforge pack install` — ROADMAP 6.2).
- Pack listing across all installed packs (`voiceforge pack list` — ROADMAP 6.2; this PR does in-pack `--list` only).
- The `voiceforge hook` JSON-stream entry point (ROADMAP 3.3) — that consumes `play` once it lands.
- Manifest validation / sha256 verification (`voiceforge pack info` — ROADMAP 6.2).
- `--fallback` — dropped per rust-expert review v1; callers handle exit-3 themselves.

## Migration / compatibility

No breaking changes. New subcommand. New module. Existing tests pass.

## Plan order

1. `packs` module skeleton: `PlayError`, `validate_pack_name`, `validate_event_id`.
2. Tests for validation (cases 1, 2 above).
3. `resolve_event_wav` with the explicit step-by-step branch order (steps 3–7 in Resolution order).
4. Tests for resolution (cases 3–7).
5. `list_events`.
6. Tests for `list_events` (cases 8, 9).
7. `Commands::Play` clap variant + dispatch (with `spawn_blocking` for the rodio call).
8. Tests for the dispatch (case 10).
9. Latency bench (case 11).
10. Docs: update README example to use `voiceforge play` (already done at the table level), mark ROADMAP 6.5 in-progress.

Estimated diff size: **~350–400 LOC** including tests and the bench (per rust-expert calibration on v1). Not 250.

## What I disagreed with from v1 review

Nothing. Every point landed.
