# Plan: embedded fallback TTS in Rust (ROADMAP 1.1)

## Goal

`voiceforge say --text "hello"` produces audible speech with **no Python
server running**. Defaults to OS-native TTS; setting
`VOICEFORGE_TTS_URL` flips to the existing Flask-server path.

This is the foundation of the one-curl install (1.5) — without it, a
fresh user has to set up Python before hearing anything.

## Engine selection

Default = embedded. Explicit env override:

```text
VOICEFORGE_TTS_URL unset  -> EmbeddedEngine (OS-native)
VOICEFORGE_TTS_URL set    -> ServerEngine (existing reqwest path)
```

No "try server, fall back" magic — that adds a connect-fail timeout to
every call and makes failures hard to diagnose. Explicit is better than
clever.

## Out of scope

- **Voice preset → engine arg mapping** (`say -v "Karen"`,
  `espeak-ng -v en+f3`). Embedded engine ignores `voice` for this PR
  and uses the OS default voice. Voice mapping is a follow-up
  (1.1.1) once the engine plumbing exists.
- **Windows SAPI.** Skip-with-error on Windows; ROADMAP item filed as
  1.1.2.
- **Streaming.** ROADMAP 4.2.
- **Cache.** Embedded engine writes directly to a deterministic path
  under `~/.voiceforge/cache/<sha>.wav` so repeated text + voice
  reuses the file, but no eviction or size cap — defer.

## Files

### New

- `apps/voiceforge-cli/src/tts.rs`:
  - `pub trait TtsEngine: Send + Sync` with
    `async fn speak(&self, text: &str, voice: &str) -> anyhow::Result<PathBuf>`.
  - `pub struct EmbeddedEngine { cache_dir: PathBuf }`.
    - macOS: spawn `say -o <tmp>.aiff <text>`, then
      `afconvert -f WAVE -d LEI16 <tmp>.aiff <out>.wav`. Both via
      `tokio::process::Command`. Cache key:
      `sha256(text + voice + "embedded.macos.say")`.
    - Linux: `espeak-ng -w <out>.wav <text>`. Same key prefix
      `embedded.linux.espeak`.
    - Windows: returns
      `bail!("Embedded TTS on Windows not yet implemented (ROADMAP 1.1.2). Set VOICEFORGE_TTS_URL to use the server path.")`.
  - `pub struct ServerEngine` — wraps existing `tts_client::speak`,
    moved here behind the trait.
  - `pub fn select_engine() -> anyhow::Result<Box<dyn TtsEngine>>` —
    reads `$VOICEFORGE_TTS_URL`; if set, returns `ServerEngine`; else
    constructs `EmbeddedEngine` rooted at `paths::user_home()? .join("cache")`,
    creating the dir on first call.

### Modified

- `apps/voiceforge-cli/src/main.rs` — `mod tts;`. `Commands::Say`
  calls `tts::select_engine()?.speak(...)?` instead of
  `tts_client::speak`.
- `apps/voiceforge-cli/src/runner.rs` — same swap.
- `apps/voiceforge-cli/src/tts_client.rs` — kept for now (still used
  by `ServerEngine`), but its `speak()` becomes an internal helper of
  `tts::ServerEngine`. Rename file? No — defer naming churn to a
  follow-up.

### Tests

- `apps/voiceforge-cli/tests/embedded_engine.rs`:
  - **Shim-on-PATH approach.** Build a tiny `say-shim`/`espeak-shim`
    wrapper at test time that records its invocation and writes a
    valid WAV/AIFF, then prepend a tempdir to PATH. Asserts:
    - `EmbeddedEngine::speak("hi", "default")` invokes the shim with
      the right args and produces an output WAV at the cache path.
    - Second call with the same text+voice is a cache hit (shim is
      NOT invoked the second time).
    - Different text → different cache path.
    - Different voice → different cache path.
  - Skip the macOS-specific afconvert wiring with a cfg(target_os) gate
    in the test for now; the macOS `say` path runs natively in CI on
    macos-14 because shimming `afconvert` is harder than just letting
    the real binary run.
  - Linux test uses an `espeak-ng` shim; runs on ubuntu-24.04 in CI.

## Verify

1. `cargo test --manifest-path apps/voiceforge-cli/Cargo.toml --all-features`
   → all suites pass on darwin-arm64 locally.
2. Manual: kill the Python server, then
   `cargo run -- say --text "Embedded fallback works"` should speak
   via macOS `say` and play through rodio.
3. Manual: `VOICEFORGE_TTS_URL=http://127.0.0.1:5555 cargo run -- say
--text hi --voice default` after starting the Python server should
   route through `ServerEngine` (verify via server log).
4. CI on ubuntu-24.04 + macos-14 must stay green.

## Risks

- **macOS `say` blocks SIGINT briefly.** Wrap in a 30 s timeout.
- **Linux espeak-ng default voice quality is rough.** Acceptable for
  the fallback; voice cloning is the headline feature anyway.
- **Cache dir permission errors** if `$HOME` is unset. `paths::user_home`
  already returns `Option<PathBuf>` — handle the None case with a
  clear error.
- **`select_engine()` is sync but `EmbeddedEngine` does file IO.**
  Defer disk-creation until the first `speak()` call; constructor is
  cheap.

## Atomic commits

1. `feat(tts): add TtsEngine trait + ServerEngine wrapper`
2. `feat(tts): add EmbeddedEngine (macOS say + Linux espeak-ng paths)`
3. `feat(cli): default Say + Run to embedded engine when VOICEFORGE_TTS_URL unset`
4. `test(tts): shim-based tests for EmbeddedEngine cache + invocation`
