# Plan: voiceforge clone <name> <source> (ROADMAP 2.5) — REVISED

## Goal

```bash
voiceforge clone <name> <source>          # name first, source second (git-style)
voiceforge say --voice <name> --text "..."
voiceforge run -- npm test                # speaks in cloned voice when --voice set
```

After this lands the demo holds:

```bash
voiceforge install-cloning
voiceforge clone peter https://youtube.com/...
voiceforge say --voice peter --text "Holy crap, the build is on fire."
```

## Out of scope (deferred items)

- `voiceforge use <name>` (set active default voice) — 2.6, follow-up PR.
- `voiceforge voices list/remove` — 2.7, same follow-up.
- `mic` source — 2.4 (cpal capture).
- Multiple `--from` sources — defer; v1 stacks 6 chunks from a single ≥60 s source.

## Voice profile layout

```text
~/.voiceforge/voices/<name>/
├── profile.toml            # schema_version, name, source, created_at,
│                           # duration_seconds, recipe, aux_count
├── ref_main.wav            # 1 main 10 s reference (32 kHz mono 16-bit)
├── ref_main.txt            # whisper transcript
├── aux_1.wav .. aux_5.wav  # 5 aux refs
└── aux_1.txt .. aux_5.txt  # whisper transcripts
```

`profile.toml`:

```toml
schema_version = 1
name = "peter"
source = "https://youtube.com/..."
created_at = "2026-05-04T..."
duration_seconds = 60.0
recipe = "gpt-sovits-v2-multi-aux-ref"
aux_count = 5
```

Atomic write: clone stages into `voices/<name>.partial/`, renames on success.
A SIGINT mid-clone leaves `<name>.partial/` orphaned (cleaned up on retry).

## Architecture: long-lived synth child (the big design call)

Per rust-expert review, **per-call subprocess is unusable** — 15 s model
load every `voiceforge say` defeats the build-failure UX (failure happens,
voice speaks 15 s later, you've already moved on).

Instead: lazy-spawn a single `cloning_synth.py` child on first cloned-voice
call, keep it alive for the rest of the process, communicate via NDJSON on
stdin/stdout. ~30 lines of Rust + a `while True: read stdin` loop in Python.
Lives inside the running voiceforge process — no daemon, no socket, no PID
file. Solves cold start within `voiceforge run -- ...` (process spans the
whole build). One-shot `voiceforge say` still pays cold start; documented.

This shape is also what 1.8 (Unix-socket daemon) needs later — same NDJSON
contract, just bound to a socket instead of stdin.

### Wire format (NDJSON over stdin/stdout)

Request line:

```json
{ "text": "Holy crap", "voice": "peter", "out": "/tmp/.../<sha>.wav" }
```

Response line:

```json
{ "ok": true, "sample_rate": 32000 }
```

Or:

```json
{ "ok": false, "error": "voice 'peter' not found" }
```

Python child:

1. On startup: read `INSTALLED.toml`, assert `gpt_sovits_sha` matches a
   constant baked into the script (refuse mismatch with `--force` hint).
2. Lazy-load GPT-SoVITS v2 once on first request (~15 s).
3. Loop: read NDJSON line, run inference with main + 5 aux refs from
   `~/.voiceforge/voices/<voice>/`, atomic-write WAV, emit response line.
4. On EOF or SIGTERM: clean exit.

## Engine refactor

Per review: keep `select_engine()` voice-agnostic at construction; route
per-call.

```rust
pub enum Engine {
    Embedded(EmbeddedEngine),
    Server(ServerEngine),
    Cloning(CloningEngine),     // new
}

impl Engine {
    pub async fn speak(&self, text: &str, voice: &str) -> Result<PathBuf> {
        // Per-call dispatch:
        //   if voice is a cloned voice + cloning installed -> Cloning
        //   else if VOICEFORGE_TTS_URL set -> Server
        //   else -> Embedded
        ...
    }
}
```

`CloningEngine` owns an `Arc<Mutex<Option<ChildHandle>>>`. First call
spawns the python child; subsequent calls reuse it. `Drop` sends EOF +
waits with timeout. Cache-key includes `voice`, `text`, and the voice
profile's `created_at` (so `clone --force` naturally orphans old cache
entries).

## Files

### New

- `scripts/clone_voice.sh` — bash, strict mode. Idempotent on same name
  via `voices/<name>.partial/` staging + rename. Lock file
  `voices/<name>.lock` via `flock` for concurrency safety.
- `scripts/cloning_synth.py` — long-lived NDJSON worker. Pins
  `EXPECTED_GPT_SOVITS_SHA` constant; mismatch = exit 2 with hint. Loads
  GPT-SoVITS v2 lazily; subsequent requests hit the warm model.
- `apps/voiceforge-cli/src/voices.rs`:
  - `pub const VOICE_SCHEMA_VERSION: u32 = 1`
  - `pub struct VoiceProfile { name, source, duration_seconds, recipe,
aux_count, dir, ref_main_wav/txt, aux_wavs/txts: Vec<PathBuf> }`
  - `pub fn voices_dir() -> Option<PathBuf>`
  - `pub fn voice_dir(name) -> Result<PathBuf>` — also `canonicalize()`s
    and asserts result is under `voices_dir()` (path-traversal proof).
  - `pub fn voice_exists(name) -> bool`
  - `pub fn load_voice(name) -> Result<VoiceProfile>` — parses
    profile.toml, asserts schema_version match, asserts recipe is
    known, asserts all referenced files exist.
  - `pub fn validate_name(name) -> Result<()>` — `^[a-z0-9_-]{1,32}$`,
    rejects `.`/`..`/reserved (`presets`, `cache`, `cloning`, `voices`,
    `embeddings`, `logs`).
- `apps/voiceforge-cli/src/clone.rs`:
  - `pub async fn run(name: String, source: String, force: bool) -> Result<()>`
  - validates install, validates name, refuses if exists w/o `--force`,
    spawns `clone_voice.sh` with `Stdio::inherit`, on success prints
    next-step hint.

### Modified

- `apps/voiceforge-cli/src/tts.rs`:
  - New `Engine::Cloning(CloningEngine)` variant.
  - `Engine::speak(text, voice)` per-call routes:
    1. If `voices::voice_exists(voice) && install_cloning::is_installed()` → Cloning
    2. Else if `VOICEFORGE_TTS_URL` set → Server
    3. Else → Embedded
  - `select_engine() -> Result<Engine>` — unchanged signature; constructs
    all three lazily, returns the wrapped enum, dispatches per-call.
  - `CloningEngine` holds `Arc<Mutex<Option<SynthChild>>>`. `for_testing`
    constructor injects a fake `SynthChild` writing canned WAVs (mirrors
    the existing `EmbeddedEngine::for_testing` pattern).
  - Cache key: `sha256(text + voice + voice_profile.created_at + "cloning.gpt-sovits-v2")`.
  - Atomic write: tmp + rename, same pattern as embedded.
- `apps/voiceforge-cli/src/main.rs`:
  - `Commands::Clone { name: String, source: String, #[arg(long)] force: bool }`
  - Positional grammar: `voiceforge clone <name> <source>` (git-remote-add style).
- `apps/voiceforge-cli/src/install_cloning.rs`:
  - `pub fn cloning_python() -> Option<PathBuf>` — venv python from marker.
  - `pub fn cloning_repo_dir() -> Option<PathBuf>` — gpt-sovits clone path.
  - `pub fn cloning_synth_script() -> Option<PathBuf>` — voice-forge's `scripts/cloning_synth.py`.

### Tests

- `voices.rs` `#[cfg(test)] mod tests` (#[serial]):
  - `validate_name_accepts_simple`
  - `validate_name_rejects_dot_dot_dot_slash` (`.`, `..`, `peter/etc`)
  - `validate_name_rejects_reserved` (presets/cache/cloning/voices/etc.)
  - `validate_name_rejects_uppercase_and_special`
  - `voice_dir_canonicalizes_under_voices_dir` — symlink-escape attempt
    (create symlink in voices_dir pointing outside, ensure load fails).
  - `load_voice_round_trips`
  - `load_voice_errors_on_missing_aux`
  - `load_voice_errors_on_unknown_recipe`
- `clone.rs`:
  - `run_errors_when_install_marker_absent`
  - `run_errors_on_invalid_name`
  - `run_errors_on_existing_voice_without_force`
- `tts.rs`:
  - `engine_dispatch_cloning_when_voice_exists_and_installed`
  - `engine_dispatch_falls_back_to_embedded_for_unknown_voice`
  - `cloning_engine_speak_via_fake_synth_child` — uses the SynthBuilder
    injection pattern, writes canned WAV, asserts cache hit on second call.
- `tests/install/clone_voice_smoke.sh` — stub-PATH dry run (yt-dlp, ffmpeg,
  whisper). Asserts `profile.toml` + 12 files emitted.
- CI: `clone_voice.sh smoke` step on macOS only (Linux clone is 2.5.1).

## Verify

1. `voiceforge install-cloning` — already shipped, confirms presence.
2. `voiceforge clone trump_test /path/to/clean-trump.wav` — completes,
   writes voice profile.
3. `voiceforge say --voice trump_test --text "Holy crap"` — speaks,
   ~15 s first call, fast after (within same `voiceforge` process).
4. `voiceforge say --voice trump_test --text "Holy crap"` again, fresh
   process — pays cold start again (documented limitation; daemon is 1.8).
5. `voiceforge run -- false` with `--voice trump_test` — speaks at end of
   command after one cold start (within the long-lived process).
6. `voiceforge clone trump_test /other/source` (no --force) — refuses
   with clear "use --force" hint.
7. `voiceforge clone trump_test /other/source --force` — invalidates old
   cache (via created_at change) and replaces profile.
8. `voiceforge clone path/with/slash bad-name` — rejects on validate_name.
9. `voiceforge say --voice nonexistent` — falls through to embedded
   engine with a clear "voice not found, falling back to default" warn.
10. CI dry-run smoke passes.

## Atomic commits

1. `feat(voices): voice profile module + ~/.voiceforge/voices/<name>/ layout`
2. `feat(scripts): clone_voice.sh + cloning_synth.py NDJSON worker`
3. `feat(install_cloning): cloning_python/repo/synth_script accessors`
4. `feat(tts): Engine::Cloning + per-call dispatch in Engine::speak`
5. `feat(cli): voiceforge clone <name> <source> [--force]`
6. `test(clone): unit + integration + dry-run smoke`
7. `ci: clone_voice.sh smoke job`
