//! Integration tests for the audio-ingest pipeline.
//!
//! `ingest_fixture_yields_canonical_wav` exercises the real Peter Griffin
//! clip fetched by `scripts/fetch_fixtures.sh`. When the fixture is absent,
//! the test SKIPs by default so contributors who haven't run the script
//! still get a green suite. Setting `VOICEFORGE_REQUIRE_FIXTURES=1` flips
//! that skip to a hard failure — CI uses this to keep the skip path honest.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

// In a binary crate, integration tests cannot reach `mod ingest` directly,
// so we shell out to the built `voiceforge ingest` binary (located via
// CARGO_BIN_EXE_voiceforge) and assert its observable behavior + the
// resulting WAV's properties via ffprobe. That's a stronger end-to-end
// test than calling the function in-process anyway, since it exercises
// arg parsing, the `--` separator, and the exit-code surface.

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at apps/voiceforge-cli/; the workspace root
    // is two levels up.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("workspace root should be two levels above CARGO_MANIFEST_DIR")
        .to_path_buf()
}

fn fixture_path() -> PathBuf {
    workspace_root().join("tests/fixtures/peter_griffin.wav")
}

fn voiceforge_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_voiceforge"))
}

fn require_fixtures() -> bool {
    std::env::var("VOICEFORGE_REQUIRE_FIXTURES")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn run_ingest(input: &Path, output: &Path) -> std::process::Output {
    std::process::Command::new(voiceforge_bin())
        .arg("ingest")
        .arg(input)
        .arg(output)
        .output()
        .expect("spawn voiceforge ingest")
}

fn ffprobe_audio(path: &Path) -> serde_json::Value {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-of",
            "json",
            "-show_streams",
            "-show_format",
            "--",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(
        out.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("ffprobe json")
}

#[test]
fn ingest_fixture_yields_canonical_wav() {
    let fixture = fixture_path();
    if !fixture.exists() {
        if require_fixtures() {
            panic!(
                "fixture required but missing: {} (run scripts/fetch_fixtures.sh)",
                fixture.display()
            );
        }
        eprintln!(
            "SKIP: fixture missing, run scripts/fetch_fixtures.sh ({})",
            fixture.display()
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let out = tmp.path().join("out.wav");

    let result = run_ingest(&fixture, &out);
    assert!(
        result.status.success(),
        "ingest failed: stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );

    let probe = ffprobe_audio(&out);
    let stream = &probe["streams"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["codec_type"] == "audio"))
        .expect("audio stream in output");
    let format = &probe["format"];

    assert_eq!(stream["sample_rate"].as_str(), Some("32000"));
    assert_eq!(stream["channels"].as_u64(), Some(1));
    assert_eq!(stream["codec_name"].as_str(), Some("pcm_s16le"));

    let bits = stream["bits_per_sample"]
        .as_u64()
        .or_else(|| {
            stream["bits_per_raw_sample"]
                .as_str()
                .and_then(|s| s.parse().ok())
        })
        .expect("bits_per_sample");
    assert_eq!(bits, 16);

    let duration: f64 = format["duration"]
        .as_str()
        .expect("duration")
        .parse()
        .expect("duration parses");
    assert!(
        (9.5..=60.5).contains(&duration),
        "duration {duration:.3}s outside [9.5, 60.5]"
    );
}

#[test]
fn ingest_rejects_too_short() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let short = tmp.path().join("short.wav");
    write_silence_wav(&short, 22_050, 1, 1.0);

    let out = tmp.path().join("out.wav");
    let result = run_ingest(&short, &out);

    assert!(!result.status.success(), "ingest should reject 1s input");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.to_lowercase().contains("duration"),
        "stderr should explain the duration violation, got: {stderr}"
    );
}

#[test]
fn ingest_rejects_missing_input() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join("does-not-exist.wav");
    let out = tmp.path().join("out.wav");

    let result = run_ingest(&missing, &out);

    assert!(
        !result.status.success(),
        "ingest should reject missing input"
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("not found") || stderr.to_lowercase().contains("does-not-exist"),
        "stderr should mention the missing path, got: {stderr}"
    );
}

#[test]
fn ingest_handles_dash_prefixed_input_path() {
    // Regression test: a filename starting with '-' must not be parsed as
    // an ffmpeg flag. We use the canonical fixture content so the test
    // doesn't depend on whatever ffmpeg makes of a synthetic 1s file.
    let fixture = fixture_path();
    if !fixture.exists() {
        if require_fixtures() {
            panic!(
                "fixture required but missing: {} (run scripts/fetch_fixtures.sh)",
                fixture.display()
            );
        }
        eprintln!("SKIP: fixture missing, run scripts/fetch_fixtures.sh");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let weird = tmp.path().join("-rf evil.wav");
    std::fs::copy(&fixture, &weird).expect("copy fixture under dash-prefixed name");
    let out = tmp.path().join("out.wav");

    let result = run_ingest(&weird, &out);
    assert!(
        result.status.success(),
        "dash-prefixed input should be handled safely: stderr={}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(out.exists(), "output WAV should exist");
}

fn write_silence_wav(path: &Path, sample_rate: u32, channels: u16, seconds: f64) {
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let file = File::create(path).expect("create wav");
    let writer = BufWriter::new(file);
    let mut wav = hound::WavWriter::new(writer, spec).expect("wav writer");
    let total_samples = (sample_rate as f64 * seconds) as u32 * channels as u32;
    for _ in 0..total_samples {
        wav.write_sample(0_i16).expect("write sample");
    }
    wav.finalize().expect("finalize wav");
}

/// Write a sine-wave WAV at `freq_hz` and full-scale amplitude
/// `amplitude` (0.0 to 1.0). Used to feed the silence-rejection test
/// real audio (vs `write_silence_wav`'s pure zeros).
fn write_sine_wav(
    path: &Path,
    sample_rate: u32,
    channels: u16,
    seconds: f64,
    freq_hz: f64,
    amplitude: f32,
) {
    use std::f64::consts::PI;
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let file = File::create(path).expect("create wav");
    let writer = BufWriter::new(file);
    let mut wav = hound::WavWriter::new(writer, spec).expect("wav writer");
    let total_frames = (sample_rate as f64 * seconds) as u32;
    let scale = (amplitude * (i16::MAX as f32)) as f64;
    for i in 0..total_frames {
        let t = (i as f64) / (sample_rate as f64);
        let s = (2.0 * PI * freq_hz * t).sin() * scale;
        let s_i16 = s.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        for _ in 0..channels {
            wav.write_sample(s_i16).expect("write sample");
        }
    }
    wav.finalize().expect("finalize wav");
}

// ROADMAP 2.2.1 — silence rejection.
//
// Pure-silent input passes the duration gate (>=10s) but should be
// rejected by the post-encode amplitude probe. Without this guard,
// the cloning pipeline downstream would hand pure silence to Whisper,
// which would either crash or produce a useless "" transcription
// that GPT-SoVITS then trains a useless reference embedding from.
#[test]
fn ingest_rejects_silent_input_2_2_1() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let silent = tmp.path().join("silent.wav");
    // 12 seconds (passes min_seconds=10) of pure silence.
    write_silence_wav(&silent, 22_050, 1, 12.0);

    let out = tmp.path().join("out.wav");
    let result = run_ingest(&silent, &out);

    assert!(
        !result.status.success(),
        "ingest should reject 12s of silence; stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr),
    );
    let stderr = String::from_utf8_lossy(&result.stderr).to_lowercase();
    assert!(
        stderr.contains("silent") || stderr.contains("quiet") || stderr.contains("amplitude"),
        "stderr should explain the silence rejection, got: {stderr}"
    );
    // The silent output should have been cleaned up — don't leave a
    // zero-amplitude .wav for a later cloning attempt to choke on.
    assert!(
        !out.exists(),
        "rejected silent output should be removed; still exists at {}",
        out.display()
    );
}

// ROADMAP 2.2.1 — loudnorm.
//
// A QUIET (but non-silent) sine-wave input passes the silence gate
// after loudnorm bumps it to broadcast level. The amplitude on the
// way OUT should be substantially higher than on the way IN.
#[test]
fn ingest_loudnorm_brings_quiet_input_to_broadcast_level() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let quiet = tmp.path().join("quiet_sine.wav");
    // 12s of 440 Hz sine at 5% amplitude (~-26 dBFS peak). Real
    // speech recorded too low sounds about like this.
    write_sine_wav(&quiet, 32_000, 1, 12.0, 440.0, 0.05);

    let out = tmp.path().join("out.wav");
    let result = run_ingest(&quiet, &out);

    assert!(
        result.status.success(),
        "ingest should normalize a quiet input through loudnorm; stderr={}",
        String::from_utf8_lossy(&result.stderr),
    );
    assert!(out.exists());

    // Read both, compare mean abs. Loudnorm should raise the quiet
    // input substantially. We assert a multiplier rather than an
    // absolute target because loudnorm's exact gain depends on the
    // input's loudness measurement.
    let in_mean = mean_abs_of_wav(&quiet);
    let out_mean = mean_abs_of_wav(&out);
    assert!(
        out_mean > in_mean * 2.0,
        "loudnorm should raise mean amplitude by >=2x; in={in_mean:.4} out={out_mean:.4}",
    );
    // And confirm we cleared the silence-rejection threshold.
    assert!(
        out_mean > 0.005,
        "loudnorm output should clear the silence-rejection threshold; got {out_mean:.4}"
    );
}

fn mean_abs_of_wav(path: &Path) -> f32 {
    let mut reader = hound::WavReader::open(path).expect("open wav");
    let spec = reader.spec();
    assert_eq!(spec.bits_per_sample, 16);
    let mut sum: u64 = 0;
    let mut n: u64 = 0;
    for s in reader.samples::<i16>() {
        let s = s.expect("sample");
        sum += s.unsigned_abs() as u64;
        n += 1;
    }
    assert!(n > 0);
    (sum as f32) / (n as f32) / (i16::MAX as f32)
}
