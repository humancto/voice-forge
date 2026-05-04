# Plan: voiceforge clone <source> as <name> (ROADMAP 2.5)

## Goal

```bash
voiceforge clone <source> as <name>
  source = local file path | URL (anything yt-dlp resolves)

voiceforge say --voice <name> --text "hello"
  # speaks in the cloned voice via GPT-SoVITS v2 (~15s first call, ~3-5s after)
```

End-to-end demo after this lands:

```bash
voiceforge install-cloning              # ROADMAP 2.1 (already shipped)
voiceforge clone https://youtube.com/... as peter
voiceforge say --voice peter --text "Holy crap, the build is on fire."
```

## Out of scope (deferred)

- **`voiceforge use <name>`** (set active voice) — ROADMAP 2.6, follow-up PR.
- **`voiceforge voices list/remove`** — ROADMAP 2.7, same follow-up PR.
- **`mic` source** — ROADMAP 2.4 (cpal mic capture).
- **Multiple `--from` sources** — defer; for v1 the script auto-stacks 6 chunks from a single source ≥60 s.
- **Persistent cloning daemon** — each `voiceforge say` spawns a fresh Python process. ~15 s model-load latency per call. Optimization punted to a follow-up alongside ROADMAP 1.8.
- **Quality fallback for ref < 60 s** — script bails clearly if source is too short. Future: stretch / repeat for short refs.

## Voice profile layout

```text
~/.voiceforge/voices/<name>/
├── profile.toml            # source, created_at, recipe version
├── ref_main.wav            # 1 main 10s reference (32 kHz mono)
├── ref_main.txt            # whisper transcript
├── aux_1.wav  .. aux_5.wav # 5 aux 10s references
└── aux_1.txt  .. aux_5.txt # whisper transcripts each
```

**`profile.toml`** schema:

```toml
schema_version = 1
name = "peter"
source = "https://youtube.com/watch?v=..."
created_at = "2026-05-04T..."
duration_seconds = 60.0
recipe = "gpt-sovits-v2-multi-aux-ref"
```

## Files

### New

- **`scripts/clone_voice.sh`** — bash, strict mode, idempotent on re-run with same name (refuses unless `--force`):
  1. Validate `<name>` matches `^[a-z0-9_-]+$`, length 1-32.
  2. Resolve source: if `http(s)://` or `youtube.com` etc., `yt-dlp -x --audio-format wav` into `$TMPDIR/raw.wav`. Else treat as local file path, `[[ -f ]]` check.
  3. ffprobe duration; require ≥ 60 s, bail clearly otherwise.
  4. ffmpeg trim 6 × 10 s windows (auto-skip first 5 s as intro buffer): main = window 0, aux_1..aux_5 = windows 1..5. Apply `loudnorm=I=-16:TP=-1.5:LRA=11`. Output 32 kHz mono 16-bit pcm.
  5. Whisper-transcribe each (use the **cloning venv's** `whisper` install, accessed via the `python_path` from `INSTALLED.toml`). Save `.txt` per chunk.
  6. Write `profile.toml`.
  7. Print success summary.

- **`scripts/cloning_synth.py`** — runs INSIDE the cloning venv. Args: `--voice <name> --text "<text>" --out <wav-path>`. Loads GPT-SoVITS v2 from the cloning install, runs inference with 1 main + 5 aux refs from the voice profile, concatenates output chunks, writes WAV. ~15 s first-call cold start, ~3-5 s per call after model is loaded — but each subprocess pays cold-start. (Daemon optimization punted.)

- **`apps/voiceforge-cli/src/voices.rs`** — voice profile module:
  - `pub struct VoiceProfile { name, source, duration_seconds, recipe, dir, ref_main_wav, ref_main_txt, aux_wavs: [PathBuf;5], aux_txts: [PathBuf;5] }`
  - `pub fn voices_dir() -> Option<PathBuf>` — `<voiceforge_home>/voices/`
  - `pub fn voice_dir(name) -> Option<PathBuf>` — `voices_dir/name/`
  - `pub fn load_voice(name) -> Result<VoiceProfile>` — parses `profile.toml`, asserts all 12 files (1+5 wav + 1+5 txt) exist, returns assembled struct.
  - `pub fn voice_exists(name) -> bool` — true when `profile.toml` is present and parses.
  - `pub fn validate_name(name) -> Result<()>` — regex check.

- **`apps/voiceforge-cli/src/clone.rs`** — Rust CLI subcommand:
  - `pub async fn run(source: String, name: String, force: bool) -> Result<()>` — validates `install-cloning` ran first (errors clearly if not), validates name, refuses if voice exists w/o `--force`, spawns `scripts/clone_voice.sh` with right env, streams output, on success prints next-step `voiceforge say --voice <name> --text "..."` hint.

### Modified

- **`apps/voiceforge-cli/src/tts.rs`** — new variant `Engine::Cloning(CloningEngine)`:
  - `CloningEngine::new()` — looks up cloning install state via `install_cloning::read_install_state()`. Errors if not installed.
  - `CloningEngine::speak(text, voice)` — looks up `VoiceProfile::load(voice)`, builds the synth-script command:
    ```rust
    Command::new(&install_state.python_path_in_venv())
      .arg(<repo_root>/scripts/cloning_synth.py)
      .args(["--voice", voice, "--text", text, "--out", &cache_path])
      .env("DYLD_FALLBACK_LIBRARY_PATH", format!("{}/lib", install_state.ffmpeg6_prefix))
      .env("PYTHONPATH", format!("{repo}:{repo}/GPT_SoVITS"))
    ```
  - Cache key sha256(text + voice + "cloning.gpt-sovits-v2") at `~/.voiceforge/cache/<key>.wav`. Atomic write via tmp+rename (same pattern as embedded engine).
  - **`select_engine()` updated**: if `--voice <name>` is provided AND `voices::voice_exists(name)` AND cloning is installed → `Engine::Cloning`. Else falls through to existing `Engine::Server` (URL set) / `Engine::Embedded` (default).

  Wait — current `select_engine()` doesn't take a voice argument. Plumb it through: `select_engine(voice: &str) -> Result<Engine>`.

- **`apps/voiceforge-cli/src/main.rs`**:
  - `Commands::Clone { source: String, name: String, #[arg(long)] force: bool }`
  - Note: `voiceforge clone <source> as <name>` requires custom clap parsing. Easiest: positional `source`, then a literal `as` token via `arg(value_parser = ...)` or use a value-parser that asserts. Cleaner: ditch the `as` keyword and use `voiceforge clone <source> <name>`. Let's go with that — `voiceforge clone <source> --name <name>` is even cleaner CLI-wise. Ship `voiceforge clone <source> --as <name>` to match the README pitch but parsed as a normal `--as <NAME>` flag.
  - All `Commands::Say`, `Commands::Run`, `Commands::Daemon` callers of `select_engine()` must pass the voice name.

- **`apps/voiceforge-cli/src/runner.rs`** — `select_engine` call gets the picked voice.
- **`apps/voiceforge-cli/src/daemon.rs`** — same.
- **`apps/voiceforge-cli/src/install_cloning.rs`**:
  - Add `pub fn cloning_repo_dir() -> Option<PathBuf>` and `pub fn cloning_python() -> Option<PathBuf>` so callers don't manually parse.
  - Add `cloning_synth.py` path resolver — points at `repo_path/scripts/cloning_synth.py` (or our shipped one in `scripts/cloning_synth.py` of voice-forge repo? Decision: ship our own at `voice-forge/scripts/cloning_synth.py`, the cloning venv just runs it).

### Tests

- **`apps/voiceforge-cli/src/voices.rs`** `#[cfg(test)] mod tests`:
  - `validate_name_accepts_alpha_and_underscore`
  - `validate_name_rejects_path_traversal` (e.g. `../etc`, `peter/../../`)
  - `validate_name_rejects_empty_or_too_long`
  - `voice_exists_false_when_dir_absent`
  - `voice_exists_true_when_profile_complete`
  - `load_voice_errors_on_missing_aux_chunk` — populate dir but delete `aux_3.wav`, assert clear error
  - `load_voice_round_trips_profile_toml`

- **`apps/voiceforge-cli/src/clone.rs`** `#[cfg(test)] mod tests`:
  - `run_errors_when_cloning_not_installed`
  - `run_errors_when_name_invalid`
  - `run_errors_when_voice_exists_without_force`
  - (Real fetch-and-transcribe path is too heavy for unit tests; CI smoke test wraps it with stubbed `yt-dlp`/`ffmpeg`/`whisper` binaries.)

- **`tests/install/clone_voice_smoke.sh`** — same stub-PATH pattern as install-cloning smoke. Stubs `yt-dlp` to copy a fixture WAV, `ffmpeg` to a no-op (touches output files), `whisper` to write canned transcripts. Asserts profile.toml + 6 wavs + 6 txts emitted at expected paths.

- **`apps/voiceforge-cli/tests/clone_smoke.rs`** — Rust integration test that drives `voiceforge clone` against a stubbed PATH; asserts exit code, voice-dir contents, profile.toml schema.

- **No test for `Engine::Cloning::speak`** in CI — needs the real GPT-SoVITS model + ffmpeg@6 dynlibs which are 3 GB. Real synthesis path covered by manual verification on the developer's machine. CI does cover the engine-selection logic (which Engine variant gets returned for which voice name).

## Verify

1. `voiceforge install-cloning` (already shipped, just confirms presence).
2. `voiceforge clone /path/to/local-trump-60s.wav --as trump_test` — completes in ~30 s, writes voice profile.
3. `voiceforge say --voice trump_test --text "Holy crap"` — speaks in the cloned voice, ~15 s first-call latency.
4. `voiceforge say --voice trump_test --text "Holy crap"` (re-run) — cache hit, plays instantly.
5. `voiceforge say --voice nonexistent_voice` — clear error: "voice 'nonexistent_voice' not found; run `voiceforge clone <source> --as nonexistent_voice` first."
6. `voiceforge clone https://youtube.com/watch?v=... --as quagmire` — yt-dlp path.
7. `voiceforge clone src.wav --as some/path` — rejects path-traversal.
8. CI dry-run smoke passes (stubbed binaries).

## Risks

- **Cold start latency** — 15 s model load per `voiceforge say` call when voice is cloned. UX is OK for "build failed" notifications but bad for rapid-fire usage. Daemon optimization is the cure (item 1.8 + warm pool); document for v1.
- **`Engine::Cloning::speak` errors mid-synthesis** — Python subprocess could exit non-zero or hang. Wrap in 60 s timeout (matches ingest pattern).
- **Path traversal via `<name>`** — already in test list; regex enforcement on entry.
- **Reusing the install_cloning's repo path** — if user `--uninstalls` cloning between `clone` and `say`, the say errors. Detect via `read_install_state()` on each call; clear remediation message.
- **First time using `voiceforge say --voice peter` after clone**: GPT-SoVITS v2 model loads from disk to RAM (~1.5 GB). Document the latency.

## Atomic commits

1. `feat(voices): voice profile module + ~/.voiceforge/voices/<name>/ layout`
2. `feat(scripts): clone_voice.sh + cloning_synth.py`
3. `feat(tts): Engine::Cloning variant + select_engine voice plumbing`
4. `feat(cli): voiceforge clone <source> --as <name> [--force]`
5. `test(clone): unit + integration + dry-run smoke`
6. `ci: clone_voice.sh smoke (stubbed-PATH dry run)`
