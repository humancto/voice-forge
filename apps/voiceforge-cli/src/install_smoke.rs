//! Post-install smoke synth (ROADMAP v0.4 PR-AB step 6d).
//!
//! After `voiceforge install-cloning` (v2 fish-speech path) finishes
//! its bash installer, this module runs a one-shot end-to-end synth
//! against the embedded smoke reference fixture to prove the runtime
//! actually works. A torn install where pip succeeded but text2semantic
//! weights are corrupt would otherwise pass the bash side cleanly,
//! write a "ready" marker, and only fail at the user's first real
//! `voiceforge clone` attempt 30 seconds later.
//!
//! ## Architecture (rust-expert plan v3 N2)
//!
//! `SmokeSynth` is the trait the orchestrator calls. `FishEngine`
//! implements it for prod; mock-python integration tests inject a
//! `MockSynth` so the test suite doesn't need a real
//! `~/.voiceforge/cloning/venv/bin/python` on disk.
//!
//! ## Cleanup contract (B2)
//!
//! `SmokeCleanup` is a real `Drop` impl that removes
//! `~/.voiceforge/cloning/.smoke/` on drop. Runs even on panic in the
//! synth path. Tests pin this with `std::panic::catch_unwind`.
//!
//! ## Smoke voice location (R3)
//!
//! Hidden under `~/.voiceforge/cloning/.smoke/`, NOT under
//! `~/.voiceforge/voices/<name>/`. Reasons:
//!   1. Doesn't appear in `voiceforge voices list`
//!   2. Doesn't tab-complete on `voiceforge clone <name>`
//!   3. Doesn't trip `voices::load_voice`'s v1 schema check (the smoke
//!      voice has neither `recipe = "gpt-sovits-v2-multi-aux-ref"` nor
//!      the required aux files, and never will)

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::{embedded_install, paths};

#[allow(dead_code)] // wired via install_cloning::run() in step 6d-7
pub const SMOKE_VOICE_DIRNAME: &str = ".smoke";

/// Text the smoke synth asks fish-speech to render. Short, deterministic,
/// covers the alphabet (so the encoder + decoder both exercise a wide
/// phoneme range). The output WAV is verified for size + sample count
/// only — we don't transcribe-back-and-compare; that's a v0.4.1 polish.
#[allow(dead_code)]
pub const SMOKE_SYNTH_TEXT: &str = "the quick brown fox jumps over the lazy dog";

/// Min output WAV size in bytes (B3 fix — separate from the embedded
/// fixture's commit-time minimum). 3-second utterance @ 32 kHz mono
/// PCM_16 ≈ 190 KB; 100 KB floor catches "valid header, zero data"
/// failures from a torn text2semantic load.
#[allow(dead_code)]
pub const MIN_OUTPUT_WAV_BYTES: u64 = 100_000;

// ============================================================================
// SmokeSynth trait + adapter (N2)
// ============================================================================

/// Object-safe synth contract for the smoke orchestrator. `FishEngine`
/// is the prod impl; tests inject a `MockSynth` that writes a known
/// WAV without spawning a Python child.
#[async_trait]
#[allow(dead_code)] // wired in step 6d-7
pub trait SmokeSynth: Send + Sync {
    async fn speak_with_explicit_ref(
        &self,
        text: &str,
        ref_wav: &Path,
        ref_txt: &str,
        out: &Path,
    ) -> Result<()>;
}

#[async_trait]
impl SmokeSynth for crate::tts::FishEngine {
    async fn speak_with_explicit_ref(
        &self,
        text: &str,
        ref_wav: &Path,
        ref_txt: &str,
        out: &Path,
    ) -> Result<()> {
        crate::tts::FishEngine::speak_with_explicit_ref(self, text, ref_wav, ref_txt, out).await
    }
}

// ============================================================================
// SmokeResult + SmokeCleanup
// ============================================================================

/// Outcome of one smoke run. Returned regardless of pass/fail so the
/// orchestrator can write a SmokeRecord either way (lands in step 6d-4).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SmokeResult {
    pub passed: bool,
    pub duration_ms: u64,
    pub wav_bytes: u64,
    pub sample_count: usize,
    pub message: String,
}

/// Drop guard that wipes `~/.voiceforge/cloning/.smoke/` on drop.
/// Runs even on panic in the synth path (B2 — pinned by
/// `smoke_cleanup_drop_runs_on_panic`).
struct SmokeCleanup {
    dir: PathBuf,
}

impl Drop for SmokeCleanup {
    fn drop(&mut self) {
        if self.dir.exists() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

// ============================================================================
// WAV verification (M5 + B3)
// ============================================================================

/// Verify an output WAV from the smoke synth. Returns `(file_bytes,
/// sample_count)` on success. Bails loud when:
///
///   - File missing
///   - First 4 bytes ≠ b"RIFF"
///   - Bytes 8..12 ≠ b"WAVE"
///   - Total size < `MIN_OUTPUT_WAV_BYTES`
///   - hound finds zero readable samples (header valid, data chunk
///     empty — the M5 failure mode)
#[allow(dead_code)]
pub fn verify_wav(path: &Path) -> Result<(u64, usize)> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("smoke output WAV missing at {}", path.display()))?;
    let bytes = meta.len();

    let mut header = [0u8; 12];
    use std::io::Read;
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("opening smoke output WAV at {}", path.display()))?;
    let read_n = f
        .read(&mut header)
        .with_context(|| format!("reading WAV header at {}", path.display()))?;
    if read_n < 12 {
        bail!(
            "smoke output WAV truncated to {read_n} bytes (need at least 12 for RIFF/WAVE header)"
        );
    }
    if &header[..4] != b"RIFF" {
        bail!(
            "smoke output WAV at {} lacks RIFF header (got {:?})",
            path.display(),
            &header[..4]
        );
    }
    if &header[8..12] != b"WAVE" {
        bail!(
            "smoke output WAV at {} lacks WAVE marker (got {:?})",
            path.display(),
            &header[8..12]
        );
    }

    if bytes < MIN_OUTPUT_WAV_BYTES {
        bail!(
            "smoke output WAV too small: {bytes} bytes (min {MIN_OUTPUT_WAV_BYTES}). \
             A torn fish-speech install often emits a header-only WAV; this catches that."
        );
    }

    // M5 defense: header-valid + size-passes is not sufficient. fish-speech
    // could write a 100KB file that's all silence padding. hound will fail
    // to read samples if the data chunk is malformed, OR return zero on
    // an empty data chunk. Either way, count must be > 0.
    let reader = hound::WavReader::open(path)
        .with_context(|| format!("hound failed to open WAV at {}", path.display()))?;
    let sample_count = reader.into_samples::<i16>().filter(|r| r.is_ok()).count();
    if sample_count == 0 {
        bail!(
            "smoke output WAV at {} has zero readable i16 samples (header OK, data missing). \
             Likely a torn text2semantic load — re-run `voiceforge install-cloning --force`.",
            path.display()
        );
    }

    Ok((bytes, sample_count))
}

// ============================================================================
// Smoke orchestration
// ============================================================================

/// Resolve `~/.voiceforge/cloning/.smoke/`.
#[allow(dead_code)]
pub fn smoke_dir() -> Result<PathBuf> {
    let home = paths::user_home().ok_or_else(|| anyhow!("could not resolve $VOICEFORGE_HOME"))?;
    Ok(home.join("cloning").join(SMOKE_VOICE_DIRNAME))
}

/// Prod entry point. Constructs FishEngine from the on-disk schema-2
/// marker; bails up if the install is broken upstream. Tests use
/// `run_smoke_test_with` directly with a mock synth.
#[allow(dead_code)]
pub async fn run_smoke_test() -> Result<SmokeResult> {
    let engine = crate::tts::FishEngine::new()
        .context("FishEngine::new for smoke test (is the schema-2 marker present?)")?;
    run_smoke_test_with(&engine).await
}

/// Test-friendly entry point. Caller injects the synth (mock or real).
/// Mock-python tests pre-seed a fake schema-2 marker via the existing
/// `install_cloning::tests::with_home + write_v2_marker` pattern then
/// pass a `MockSynth` here.
#[allow(dead_code)]
pub async fn run_smoke_test_with<S: SmokeSynth + ?Sized>(synth: &S) -> Result<SmokeResult> {
    let dir = smoke_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    // Drop guard: wipes the smoke dir on return (success, error, panic).
    let _cleanup = SmokeCleanup { dir: dir.clone() };

    // Extract the embedded smoke fixture into the smoke dir.
    let ref_wav = dir.join("ref.wav");
    let ref_txt_path = dir.join("ref.txt");
    let out_path = dir.join("smoke_out.wav");
    std::fs::write(&ref_wav, embedded_install::SMOKE_REFERENCE_WAV)
        .with_context(|| format!("writing smoke ref.wav to {}", ref_wav.display()))?;
    std::fs::write(&ref_txt_path, embedded_install::SMOKE_REFERENCE_TXT)
        .with_context(|| format!("writing smoke ref.txt to {}", ref_txt_path.display()))?;

    let started = Instant::now();
    let synth_outcome = synth
        .speak_with_explicit_ref(
            SMOKE_SYNTH_TEXT,
            &ref_wav,
            embedded_install::SMOKE_REFERENCE_TXT,
            &out_path,
        )
        .await;
    let duration_ms = started.elapsed().as_millis() as u64;

    if let Err(e) = synth_outcome {
        return Ok(SmokeResult {
            passed: false,
            duration_ms,
            wav_bytes: 0,
            sample_count: 0,
            message: format!("synth failed: {e:#}"),
        });
    }

    match verify_wav(&out_path) {
        Ok((wav_bytes, sample_count)) => Ok(SmokeResult {
            passed: true,
            duration_ms,
            wav_bytes,
            sample_count,
            message: String::new(),
        }),
        Err(e) => Ok(SmokeResult {
            passed: false,
            duration_ms,
            wav_bytes: 0,
            sample_count: 0,
            message: format!("verify_wav failed: {e:#}"),
        }),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Mock SmokeSynth that writes a pre-built WAV to `out` and returns
    /// success. Tests use this to bypass the real fish-speech python
    /// child entirely.
    struct MockSynth {
        bytes_to_write: Vec<u8>,
    }

    #[async_trait]
    impl SmokeSynth for MockSynth {
        async fn speak_with_explicit_ref(
            &self,
            _text: &str,
            _ref_wav: &Path,
            _ref_txt: &str,
            out: &Path,
        ) -> Result<()> {
            std::fs::write(out, &self.bytes_to_write)
                .with_context(|| format!("MockSynth writing {}", out.display()))?;
            Ok(())
        }
    }

    /// Mock SmokeSynth that always errors — exercises the synth-failure
    /// branch in `run_smoke_test_with`.
    struct FailingMockSynth {
        message: &'static str,
    }

    #[async_trait]
    impl SmokeSynth for FailingMockSynth {
        async fn speak_with_explicit_ref(
            &self,
            _text: &str,
            _ref_wav: &Path,
            _ref_txt: &str,
            _out: &Path,
        ) -> Result<()> {
            bail!("mock synth failure: {}", self.message);
        }
    }

    /// Build a real-shaped WAV in memory: RIFF header + WAVE marker +
    /// fmt chunk + N i16 samples (mono 32 kHz PCM_16). Used by tests
    /// that need a hound-readable WAV.
    fn build_test_wav(n_samples: usize) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 32_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut buf: Vec<u8> = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut writer = hound::WavWriter::new(cursor, spec).unwrap();
            for i in 0..n_samples {
                // Simple sine-ish content so samples aren't all zero
                // (hound's into_samples filters Err but counts Ok(0)).
                writer.write_sample((i as i16).wrapping_mul(7)).unwrap();
            }
            writer.finalize().unwrap();
        }
        buf
    }

    fn with_home<F: FnOnce()>(home: &Path, f: F) {
        let prev = std::env::var("VOICEFORGE_HOME").ok();
        std::env::set_var("VOICEFORGE_HOME", home);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match prev {
            Some(v) => std::env::set_var("VOICEFORGE_HOME", v),
            None => std::env::remove_var("VOICEFORGE_HOME"),
        }
        if let Err(p) = result {
            std::panic::resume_unwind(p);
        }
    }

    #[test]
    fn smoke_dir_under_voiceforge_home() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let d = smoke_dir().expect("smoke_dir");
            assert!(d.starts_with(tmp.path()));
            assert!(d.ends_with(".smoke"));
            assert!(d.parent().unwrap().ends_with("cloning"));
        });
    }

    #[test]
    fn verify_wav_rejects_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("nonexistent.wav");
        let err = verify_wav(&p).unwrap_err();
        assert!(format!("{err:#}").contains("missing"));
    }

    #[test]
    fn verify_wav_rejects_non_riff_header() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("garbage.wav");
        let mut bogus = vec![0u8; MIN_OUTPUT_WAV_BYTES as usize + 100];
        bogus[..4].copy_from_slice(b"NOPE");
        std::fs::write(&p, bogus).unwrap();
        let err = verify_wav(&p).unwrap_err();
        assert!(format!("{err:#}").contains("RIFF"));
    }

    #[test]
    fn verify_wav_rejects_truncated_below_min_size() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("tiny.wav");
        // Real RIFF/WAVE header but tiny payload.
        let mut bytes = build_test_wav(8); // only 8 samples
                                           // Force well below MIN_OUTPUT_WAV_BYTES
        bytes.truncate(60);
        std::fs::write(&p, bytes).unwrap();
        let err = verify_wav(&p).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("RIFF") || msg.contains("too small"),
            "expected RIFF or too-small error, got: {msg}"
        );
    }

    #[test]
    fn verify_wav_rejects_zero_sample_wav() {
        // M5 defense: a valid RIFF/WAVE header with zero samples must
        // be rejected so a torn text2semantic load that emits header
        // bytes only doesn't pass the smoke gate.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("zero_samples.wav");
        let mut bytes = build_test_wav(0); // 0 samples
                                           // Pad up past MIN_OUTPUT_WAV_BYTES so the size check passes
                                           // and we exercise the hound count > 0 branch specifically.
        bytes.resize(MIN_OUTPUT_WAV_BYTES as usize + 1024, 0);
        // But the WAV header still says 0 samples — hound's
        // into_samples will respect the data-chunk length and return
        // zero items.
        std::fs::write(&p, bytes).unwrap();
        let err = verify_wav(&p).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("zero readable") || msg.contains("samples"),
            "expected zero-samples error, got: {msg}"
        );
    }

    #[test]
    fn verify_wav_accepts_valid_wav_above_min_size() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("good.wav");
        // 100k samples × 2 bytes = ~200 KB, well above 100K floor.
        let bytes = build_test_wav(100_000);
        std::fs::write(&p, &bytes).unwrap();
        let (wav_bytes, sample_count) = verify_wav(&p).expect("good wav must pass");
        assert!(wav_bytes >= MIN_OUTPUT_WAV_BYTES);
        assert_eq!(sample_count, 100_000);
    }

    #[tokio::test]
    #[serial]
    async fn run_smoke_test_with_mock_passes_when_synth_writes_valid_wav() {
        let tmp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var("VOICEFORGE_HOME").ok();
        std::env::set_var("VOICEFORGE_HOME", tmp.path());

        let synth = MockSynth {
            bytes_to_write: build_test_wav(100_000),
        };
        let result = run_smoke_test_with(&synth).await.expect("orchestrator");

        match prev_home {
            Some(v) => std::env::set_var("VOICEFORGE_HOME", v),
            None => std::env::remove_var("VOICEFORGE_HOME"),
        }

        assert!(result.passed, "expected pass, got: {result:?}");
        assert!(result.wav_bytes >= MIN_OUTPUT_WAV_BYTES);
        assert_eq!(result.sample_count, 100_000);
        assert!(result.message.is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn run_smoke_test_with_failing_synth_records_failure_with_message() {
        let tmp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var("VOICEFORGE_HOME").ok();
        std::env::set_var("VOICEFORGE_HOME", tmp.path());

        let synth = FailingMockSynth {
            message: "model load failed",
        };
        let result = run_smoke_test_with(&synth).await.expect("orchestrator");

        match prev_home {
            Some(v) => std::env::set_var("VOICEFORGE_HOME", v),
            None => std::env::remove_var("VOICEFORGE_HOME"),
        }

        assert!(!result.passed);
        assert!(
            result.message.contains("model load failed"),
            "synth error must propagate into SmokeResult.message: {result:?}"
        );
        // wav_bytes / sample_count must be zeroed when synth itself failed
        assert_eq!(result.wav_bytes, 0);
        assert_eq!(result.sample_count, 0);
    }

    #[tokio::test]
    #[serial]
    async fn run_smoke_test_with_zero_sample_wav_records_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var("VOICEFORGE_HOME").ok();
        std::env::set_var("VOICEFORGE_HOME", tmp.path());

        // Synth claims success but writes a valid-header zero-sample WAV.
        let mut bytes = build_test_wav(0);
        bytes.resize(MIN_OUTPUT_WAV_BYTES as usize + 1024, 0);
        let synth = MockSynth {
            bytes_to_write: bytes,
        };
        let result = run_smoke_test_with(&synth).await.expect("orchestrator");

        match prev_home {
            Some(v) => std::env::set_var("VOICEFORGE_HOME", v),
            None => std::env::remove_var("VOICEFORGE_HOME"),
        }

        assert!(!result.passed, "zero-sample WAV must NOT pass smoke");
        assert!(
            result.message.contains("zero readable")
                || result.message.contains("samples")
                || result.message.contains("verify_wav"),
            "M5 verify_wav failure must be in the message: {result:?}"
        );
    }

    #[test]
    #[serial]
    fn smoke_cleanup_drop_runs_on_normal_return() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".smoke");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("artifact"), b"data").unwrap();
        assert!(dir.exists());
        {
            let _g = SmokeCleanup { dir: dir.clone() };
            assert!(dir.exists(), "guard should not fire until drop");
        }
        assert!(!dir.exists(), "drop must remove the smoke dir");
    }

    #[test]
    #[serial]
    fn smoke_cleanup_drop_runs_on_panic() {
        // B2 regression net: the guard must fire on unwinding panics
        // OR on normal return. catch_unwind catches the panic but Drop
        // still runs as the stack unwinds.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".smoke");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("artifact"), b"data").unwrap();
        let dir_clone = dir.clone();
        let result = std::panic::catch_unwind(move || {
            let _g = SmokeCleanup {
                dir: dir_clone.clone(),
            };
            panic!("simulated panic mid-smoke");
        });
        assert!(result.is_err(), "panic should propagate");
        assert!(
            !dir.exists(),
            "Drop must run during panic unwind and remove the smoke dir"
        );
    }
}
