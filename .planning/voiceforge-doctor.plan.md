# Plan: voiceforge doctor (ROADMAP 1.7)

## Goal

```bash
$ voiceforge doctor
voiceforge 0.1.0 — system check

[OK]   binary           /Users/you/.local/bin/voiceforge
[OK]   home             /Users/you/.voiceforge
[OK]   audio backend    rodio (CoreAudio)
[OK]   embedded TTS     macOS say (PATH)
[WARN] python server    not running at http://127.0.0.1:5555
[OK]   cache            ~/.voiceforge/cache (3 files, 188 KB)
[OK]   presets          5 installed
[OK]   ffmpeg           /usr/local/bin/ffmpeg

all-clear: 7   warning: 1   error: 0
```

`voiceforge doctor --json` outputs a structured report for tooling.

## Out of scope

- Healing checks (no `--fix` flag in this PR; doctor reports, doesn't act).
- Network diagnostics beyond a single TCP connect to the server URL.
- Cloning install detection (`voiceforge install-cloning` is 2.1).

## Files

### New

- `apps/voiceforge-cli/src/doctor.rs`:
  - `pub struct Check { name: &'static str, status: CheckStatus, detail: String }`
  - `pub enum CheckStatus { Ok, Warn, Error }`
  - `pub struct DoctorReport { version: &'static str, checks: Vec<Check> }`
  - `pub fn run_doctor() -> DoctorReport` — collects every check.
  - Per-check helpers (`check_binary_path`, `check_home`, `check_audio`,
    `check_embedded_tts`, `check_python_server`, `check_cache`,
    `check_presets`, `check_ffmpeg`). Each returns a single `Check`.
  - `pub fn print_human(report: &DoctorReport, w: &mut impl Write)
-> io::Result<()>` — the table above + summary.
  - `pub fn print_json(report: &DoctorReport, w: &mut impl Write)
-> io::Result<()>` — `serde_json::to_writer_pretty` on a small
    serializable view of the report. CheckStatus serializes as
    lowercase strings: `"ok"`, `"warn"`, `"error"`.

### Modified

- `apps/voiceforge-cli/src/main.rs` — add
  `Commands::Doctor { #[arg(long)] json: bool }`. Calls `run_doctor()`,
  prints in the chosen format. Exit code:
  - `0` if no errors (warnings allowed).
  - `1` if any check returned `CheckStatus::Error`.

## Per-check semantics

- **binary** — `OK` always (we're running). Detail: `current_exe()`.
- **home** — `OK` if `~/.voiceforge` exists and is a dir; `WARN` if
  it doesn't exist (bootstrap will create it on next call); `ERROR`
  if it exists but isn't a dir.
- **audio backend** — `OK` if `rodio::OutputStream::try_default()`
  succeeds; `WARN` otherwise (CI containers, headless boxes — not
  fatal because `say` subcommand still cache-writes).
- **embedded TTS** — `OK` if the platform binary (`say` on macOS,
  `espeak-ng` on Linux) is on `PATH`; `WARN` otherwise (server is the
  fallback then).
- **python server** — Try a `reqwest::Client` GET to
  `${VOICEFORGE_TTS_URL:-http://127.0.0.1:5555}/health` with a 1 s
  timeout. `OK` on 200, `WARN` otherwise (server is opt-in).
- **cache** — Walk `~/.voiceforge/cache/`, count files + sum sizes.
  `OK` always; detail shows count and human-readable size.
- **presets** — Count `*.json` files in `~/.voiceforge/presets/`.
  `OK` if ≥ 1, `WARN` if 0.
- **ffmpeg** — `which ffmpeg`. `OK` with the path, `WARN` if missing
  (only needed for `voiceforge ingest`).

## Tests

`apps/voiceforge-cli/src/doctor.rs` `#[cfg(test)] mod tests`:

- `report_has_expected_check_names` — call `run_doctor()` and assert
  every expected name is present (catches a future PR removing one
  by accident).
- `print_json_round_trips` — render the report to JSON, parse it
  back via `serde_json::Value`, assert structure (version field,
  checks array, status enum strings).
- `print_human_includes_summary_line` — render to a `Vec<u8>`, assert
  the output ends with the count summary.
- Per-check tests use the existing `paths::user_home` mock pattern
  (`#[serial]` + tempdir env override) to check the `home`, `cache`,
  `presets` checks against staged filesystems.
- `python_server_check_warn_when_unreachable` — point
  `VOICEFORGE_TTS_URL` at `http://127.0.0.1:1` (closed port, fast
  fail) and assert `Warn`.

## Verify

1. `cargo test doctor::` — passes.
2. `cargo test` (full) — no regressions.
3. Manual: `voiceforge doctor` on this machine prints the table; with
   the Python server stopped, `python_server` shows WARN; with it
   running, OK.
4. `voiceforge doctor --json | jq` parses, has the expected shape.
5. Exit code: simulate an error by passing an invalid `VOICEFORGE_HOME`
   pointing at a file (not a dir); confirm `home` check is `Error`
   and the binary exits 1.

## Risks

- **`reqwest::blocking` vs async** — main is `#[tokio::main]`, so use
  the async client with a 1 s timeout. Don't pull in
  `reqwest::blocking` just for doctor.
- **`which ffmpeg` portability** — use `which` crate? No, just use
  `Command::new("ffmpeg").arg("-version").output()` and check the
  exit. Consistent with how ingest discovers ffmpeg.

## Atomic commits

1. `feat(doctor): module with check enum + per-check probes`
2. `feat(cli): wire 'voiceforge doctor [--json]' subcommand + exit code`
