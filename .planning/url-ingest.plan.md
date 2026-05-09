# Plan v2: URL ingest via yt-dlp (ROADMAP 2.3)

> Plan v1 -> rust-expert REVISE with 7 items + 4 nits. v2 folds them in.

## Goal

`voiceforge clone peter "https://youtube.com/watch?v=..."` and
`voiceforge ingest "https://..." out.wav` Just Work. Audit's #1 P1
("paste a YouTube URL is the universal voice-cloning UX").

```
$ voiceforge clone peter https://www.youtube.com/watch?v=T2w5SQ0L65I
==> downloading via yt-dlp (max 250 MiB) ...
==> 64 s of audio -> /tmp/voiceforge-yt-XXXXX/source.wav
==> proceeding with cloning pipeline
[Whisper-verified output ...]
$ voiceforge say --voice peter --text "the build is on fire"
[Peter speaks]
```

## CLI shape

No new flags. URL detection is implicit:

- `voiceforge clone <name> <SOURCE>` -- if SOURCE matches a URL,
  route through yt-dlp first.
- `voiceforge ingest <INPUT> <output>` -- same.
- `voiceforge clone --help` copy: "Local file path OR URL (yt-dlp
  resolves it). Schemeless `youtube.com/...` not auto-recognized --
  prefix with `https://`."

## URL detection (item 1)

```rust
pub fn is_url(s: &str) -> bool {
    s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("ytsearch")     // covers ytsearch:, ytsearch1:, ytsearch5:
        || s.starts_with("file://")
}
```

Schemeless hostnames like `youtube.com/watch?v=...` are NOT
recognized -- they fall through to the `Local` path which then
errors crisply ("path does not exist"). The error becomes the
teaching moment ("hint: prefix with https://"). Rejected: regex
hostname-detection added more code than it saved.

`file://` strips the scheme and goes to the `Local` path -- never
hits yt-dlp.

## Download path (item 2: hardened)

`url_ingest::download(url: &str) -> Result<DownloadResult>`:

1. Resolve `yt-dlp` via testable PATH lookup (item nit-3).
2. `tempfile::tempdir()` -- system temp, RAII cleanup.
3. Spawn yt-dlp with hardened flags:
   ```
   yt-dlp
     --quiet --no-warnings
     --no-playlist                  # don't download a 100-video playlist
     --max-filesize 250M            # cap download
     --socket-timeout 30            # default is unlimited
     --retries 3                    # yt-dlp's own retry
     -x --audio-format wav --audio-quality 0
     -o "<dir>/source.%(ext)s"
     "<url>"
   ```
4. **stderr capture** (item 6): pipe always (NEVER inherit -- we need
   it for the error path). Cap at 4 KiB tail. On TTY, also echo to
   stderr live via a `BufReader::lines()` reader thread that mirrors.
   Simpler v1: pipe always, print captured stderr on failure only.
   Pick the simpler v1 -- live mirror can land in 2.3.1 if users ask.
5. Glob `<dir>/source.*`; require exactly one file.
6. Return `DownloadResult { local_path, _tempdir: TempDir }` (item 5).

## Ingest source resolution (item 3: CLI-dispatcher routing)

`ingest::ingest(input: &Path, output: &Path, cfg)` STAYS Path-typed
and pure. No `Source` enum. URL knowledge stays out of the ingest
module entirely.

A new helper in `url_ingest`:

```rust
/// Caller MUST keep this alive while reading local_path() — the
/// tempdir is dropped (and the file deleted) when this value is
/// dropped. If a future caller wraps this in async, the value MUST
/// outlive the .await of any consumer.
#[must_use]
pub struct ResolvedSource {
    local_path: PathBuf,
    _tempdir: Option<TempDir>,  // None for Local, Some for Downloaded
}

impl ResolvedSource {
    pub fn local_path(&self) -> &Path { &self.local_path }
}

pub fn resolve_source(source: &str) -> Result<ResolvedSource>;
```

CLI dispatcher (`main.rs`):

```rust
Commands::Ingest { input, output } => {
    let resolved = url_ingest::resolve_source(&input.display().to_string())?;
    let report = ingest::ingest(resolved.local_path(), &output, &IngestConfig::default())?;
    // ... print report
}
```

`resolve_source`:
- `file://...` -> strip scheme, return Local.
- `is_url(s)` -> `download` then return Downloaded.
- Otherwise -> path.canonicalize() then Local.

## Files

### New

- `apps/voiceforge-cli/src/url_ingest.rs` (~150 lines + tests):
  - `is_url`, `download`, `ResolvedSource`, `resolve_source`,
    `which_with_path` (testable PATH lookup).
  - 7 tests (unit + 1 hermetic missing-yt-dlp test via PATH override).

### Modified

- `apps/voiceforge-cli/src/clone.rs` -- `run` calls
  `url_ingest::resolve_source(&source)?` and passes
  `resolved.local_path()` into `clone_voice.sh`. The `ResolvedSource`
  value stays in scope for the entire `Command::status()` call so
  the tempdir survives the script run.
- `apps/voiceforge-cli/src/main.rs` -- `Commands::Ingest` dispatcher
  routes via `resolve_source`. `Commands::Clone` already calls
  `clone::run` -- no main.rs change needed there.
- `apps/voiceforge-cli/src/doctor.rs` -- `check_yt_dlp` using the
  existing `which_async` helper (item nit-2). Append to checks list.
  Update `report_has_expected_check_names` test.
- `README.md` -- update the cloning example to use a URL. Update
  "Sourcing audio" section. Add **privacy note** (item 7) to the
  privacy section: "URL ingest fetches via yt-dlp -- every URL is
  an outbound request to that platform. To stay fully local,
  download the audio yourself and pass a file path."
- `docs/index.html` -- update cloning snippet + same privacy note.

## Tests

`apps/voiceforge-cli/src/url_ingest.rs` `#[cfg(test)] mod tests`:

1. `is_url_recognizes_http_https`
2. `is_url_recognizes_ytsearch_variants` -- including `ytsearch5:`
3. `is_url_recognizes_file_scheme`
4. `is_url_rejects_local_paths` -- `/tmp/x.wav`, `./relative.mp3`,
   `~/audio`, `peter:foo`, **`C:/foo.wav`** (Windows-style absolute path)
5. `is_url_rejects_schemeless_hostname` -- `youtube.com/watch?v=x` is
   NOT a URL by our rules; documents the design.
6. `download_errors_clearly_when_yt_dlp_missing` -- HERMETIC: spawns
   with `env_clear()` + `PATH=/nonexistent`. Asserts error message
   contains "install yt-dlp".
7. `resolve_source_strips_file_scheme` -- write a tempfile, pass
   `file://<abs-path>`, assert `local_path()` matches the stripped
   path.
8. `resolve_source_returns_local_for_path` -- existing tempfile,
   assert Local.
9. `resolve_source_errors_on_missing_local_path` -- nonexistent
   path, assert error mentions the path.
10. `download_caps_stderr_to_4kib` -- run yt-dlp against a synthetic
    failing input that produces lots of stderr (gated on yt-dlp
    presence); assert error message <=4KiB. (Skip if yt-dlp absent.)

## Manual smoke (post-merge)

1. `voiceforge clone peter https://www.youtube.com/watch?v=T2w5SQ0L65I`
2. `voiceforge ingest https://www.youtube.com/watch?v=T2w5SQ0L65I /tmp/peter.wav`
3. `voiceforge ingest file:///tmp/some.wav /tmp/out.wav`
4. `voiceforge clone peter https://nope.invalid/x.mp4` -> clear error.
5. PATH without yt-dlp: `voiceforge clone peter <url>` -> install hint.
6. Playlist URL: `voiceforge ingest "https://www.youtube.com/playlist?list=..." /tmp/out.wav`
   -> downloads the FIRST video only (because of `--no-playlist`).

## Doctor integration

Add `check_yt_dlp` after `check_ffmpeg` in `doctor.rs` using
`which_async` for consistency. Reports:
- `ok` when on PATH (with discovered location).
- `warn` when missing (with `brew install yt-dlp` / `pipx install yt-dlp`
  install hint -- not an error because URL ingest is optional).
- Update `report_has_expected_check_names` to include `"yt-dlp"`.

## Risks (acknowledged)

- yt-dlp version drift -- error message hint: "if yt-dlp fails,
  upgrade with `pip install -U yt-dlp` -- YouTube format changes
  land there first."
- TempDir cleanup on SIGINT/SIGKILL -- OS handles `/tmp` GC.
- Network egress -- README + docs privacy note cover it (item 7).

## Out of scope

- 2.3.1 Live stderr mirror to TTY during yt-dlp run.
- 2.3.2 Schemeless-URL auto-detection (`youtube.com/...` -> URL).
- 2.3.3 Cache yt-dlp downloads under `~/.voiceforge/cache/url-dl/`
  (needs content-addressed scheme + eviction).

## Atomic commits

1. `feat(url-ingest): add url_ingest module + is_url + hardened yt-dlp download`
2. `feat(clone): accept URLs as source via url_ingest::resolve_source`
3. `feat(ingest): voiceforge ingest accepts URL inputs (CLI dispatcher route)`
4. `feat(doctor): add yt-dlp check`
5. `docs: update README + index.html cloning examples; add URL-ingest privacy note`

## What plan v1 got wrong (audit log)

1. `is_url` accepted schemeless hostnames implicitly via the audit's
   suggestion. v2: explicit "scheme required" + crisp local-path
   error becomes the teach.
2. yt-dlp was unhardened. v2: `--no-playlist --max-filesize 250M
   --socket-timeout 30 --retries 3`.
3. Considered a `Source` enum on `ingest::ingest`. v2: kept it
   Path-typed; URL routing lives at the CLI dispatcher.
4. Tempdir under `~/.voiceforge/cache/url-dl/` floated. v2: rejected
   (cache-shared needs content-address+eviction); use system temp.
5. `ResolvedSource` exposed `_guard: TempDir` field directly. v2:
   `#[must_use]` struct with private fields + `local_path() -> &Path`
   accessor; rename to `_tempdir` (not `_guard`).
6. stderr was "inherited", losing the error path. v2: pipe always,
   cap at 4 KiB tail, print on failure. Live TTY mirror deferred.
7. Privacy note was risk-list-only. v2: required in docs commit.

## Coding-time implementation notes (rust-expert v2 review)

(To be appended after v2 review.)
