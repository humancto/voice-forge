//! Integration tests for `scripts/clone_voice_fish.sh` (PR-C C-2).
//!
//! Exercises the script's contract with the Rust orchestrator
//! (`apps/voiceforge-cli/src/clone.rs::run_v2`, lands in C-3) without
//! requiring a real fish-speech install. The contract:
//!
//!   1. `==> ` lines mark phase boundaries (mirrors the
//!      install_cloning_fish.sh contract).
//!   2. Dry-run mode (`VOICEFORGE_CLONE_DRY_RUN=1`) walks the full
//!      pipeline without invoking ffmpeg / whisper.
//!   3. Script bails loud when:
//!      - the schema-2 INSTALLED.toml is absent (no v2 install)
//!      - the marker is schema-1 (legacy v1 install)
//!      - source is < 8s
//!      - source file missing
//!      - URL source passed (must be a local path)
//!      - voice name invalid
//!   4. Writes a schema-2 profile.toml with `recipe = "fish-speech-s2-pro"`.
//!
//! All tests use a unique tempdir for VOICEFORGE_HOME so they can
//! run in parallel.

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn script_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("scripts").join("clone_voice_fish.sh"))
        .expect("locate clone_voice_fish.sh")
}

fn write_v2_marker(home: &Path) {
    let cloning = home.join("cloning");
    std::fs::create_dir_all(&cloning).unwrap();
    std::fs::write(
        cloning.join("INSTALLED.toml"),
        r#"
schema_version = 2
version = "0.4.0"
fish_speech_sha = "3dd1f85c402ee6f0a17c2971d3b0dd8d881ca139"
python_path = "/opt/homebrew/bin/python3.11"
ffmpeg6_prefix = "/opt/homebrew/opt/ffmpeg@6"
"#,
    )
    .unwrap();
}

fn write_v1_marker(home: &Path) {
    let cloning = home.join("cloning");
    std::fs::create_dir_all(&cloning).unwrap();
    std::fs::write(
        cloning.join("INSTALLED.toml"),
        r#"
schema_version = 1
gpt_sovits_sha = "08d627c3"
python_path = "/opt/homebrew/bin/python3.11"
ffmpeg6_prefix = "/opt/homebrew/opt/ffmpeg@6"
"#,
    )
    .unwrap();
}

/// Stage a real silent WAV via `ffmpeg` so duration probing works.
/// 12 seconds of silence at 32 kHz mono PCM_16 = ~770 KB.
fn write_silent_wav(path: &Path, seconds: u32) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=channel_layout=mono:sample_rate=32000",
            "-t",
            &seconds.to_string(),
            "-acodec",
            "pcm_s16le",
        ])
        .arg(path)
        .status()
        .expect("ffmpeg must be available to stage test WAVs");
    assert!(status.success(), "ffmpeg failed for silent WAV stub");
}

fn run_script(
    home: &Path,
    name: &str,
    source: &Path,
    force: &str,
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let mut cmd = Command::new("bash");
    cmd.arg(script_path());
    cmd.arg(name);
    cmd.arg(source);
    cmd.arg(force);
    cmd.env("VOICEFORGE_HOME", home);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run clone_voice_fish.sh");
    if std::env::var("VOICEFORGE_TEST_DEBUG_SCRIPT").is_ok() {
        eprintln!("--- status: {} ---", out.status);
        eprintln!("--- stderr ---\n{}", String::from_utf8_lossy(&out.stderr));
        eprintln!("--- stdout ---\n{}", String::from_utf8_lossy(&out.stdout));
    }
    out
}

#[test]
fn script_is_executable_and_present() {
    let path = script_path();
    assert!(path.is_file(), "script missing at {}", path.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o111;
        assert!(mode != 0, "script is not executable: {}", path.display());
    }
}

#[test]
fn dry_run_emits_expected_phase_lines() {
    let tmp = TempDir::new().unwrap();
    write_v2_marker(tmp.path());
    let src = tmp.path().join("source.wav");
    std::fs::write(&src, b"stub - dry-run never reads this").unwrap();
    let out = run_script(
        tmp.path(),
        "tyson",
        &src,
        "0",
        &[("VOICEFORGE_CLONE_DRY_RUN", "1")],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let required_phases = [
        "voiceforge clone tyson (fish-speech S2 Pro recipe)",
        "reading schema-2 marker",
        "transcoding to 32 kHz mono PCM_16",
        "duration check + middle-trim",
        "whisper-base transcribe",
        "writing schema-2 profile.toml",
        "promoting",
        "done",
    ];
    for needle in required_phases {
        assert!(
            stdout.contains(needle),
            "missing phase line {needle:?} in dry-run output:\n{stdout}"
        );
    }
}

#[test]
fn dry_run_phase_lines_use_arrow_prefix() {
    let tmp = TempDir::new().unwrap();
    write_v2_marker(tmp.path());
    let src = tmp.path().join("source.wav");
    std::fs::write(&src, b"stub").unwrap();
    let out = run_script(
        tmp.path(),
        "tyson",
        &src,
        "0",
        &[("VOICEFORGE_CLONE_DRY_RUN", "1")],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let phase_count: usize = stdout.lines().filter(|l| l.starts_with("==> ")).count();
    assert!(
        phase_count >= 7,
        "expected >=7 phase markers, got {phase_count}\noutput:\n{stdout}"
    );
}

#[test]
fn script_bails_when_v2_marker_absent() {
    let tmp = TempDir::new().unwrap();
    // No INSTALLED.toml at all
    let src = tmp.path().join("source.wav");
    write_silent_wav(&src, 12);
    let out = run_script(tmp.path(), "tyson", &src, "0", &[]);
    assert!(
        !out.status.success(),
        "must fail without v2 marker, got success: {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cloning not installed") || stderr.contains("install-cloning"),
        "stderr must point at install-cloning, got: {stderr}"
    );
}

#[test]
fn script_bails_when_marker_is_schema_v1() {
    let tmp = TempDir::new().unwrap();
    write_v1_marker(tmp.path()); // schema-1 install present
    let src = tmp.path().join("source.wav");
    write_silent_wav(&src, 12);
    let out = run_script(tmp.path(), "tyson", &src, "0", &[]);
    assert!(
        !out.status.success(),
        "must fail when marker is v1, got success: {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("schema_version=2") || stderr.contains("gpt-sovits-v2"),
        "stderr must distinguish v1 vs v2 + hint at engine override, got: {stderr}"
    );
}

#[test]
fn script_rejects_invalid_voice_name() {
    let tmp = TempDir::new().unwrap();
    write_v2_marker(tmp.path());
    let src = tmp.path().join("source.wav");
    write_silent_wav(&src, 12);
    let out = run_script(tmp.path(), "../etc", &src, "0", &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid voice name"),
        "expected name-validation error, got: {stderr}"
    );
}

#[test]
fn script_rejects_url_source() {
    let tmp = TempDir::new().unwrap();
    write_v2_marker(tmp.path());
    let out = run_script(
        tmp.path(),
        "tyson",
        Path::new("https://example.invalid/audio.mp4"),
        "0",
        &[],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("URL sources are not supported"),
        "URL rejection must be explicit, got: {stderr}"
    );
}

#[test]
fn script_rejects_missing_source_file() {
    let tmp = TempDir::new().unwrap();
    write_v2_marker(tmp.path());
    let out = run_script(
        tmp.path(),
        "tyson",
        Path::new("/nonexistent/audio.wav"),
        "0",
        &[],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("local source not found"),
        "missing-source error must be explicit, got: {stderr}"
    );
}

#[test]
fn script_rejects_under_8_second_source() {
    let tmp = TempDir::new().unwrap();
    write_v2_marker(tmp.path());
    let src = tmp.path().join("short.wav");
    write_silent_wav(&src, 5); // 5s < 8s floor
    let out = run_script(tmp.path(), "tyson", &src, "0", &[]);
    assert!(
        !out.status.success(),
        "5s source must fail the 8s floor, got success: {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("≥8s") || stderr.contains(">=8s") || stderr.contains("8s"),
        "duration error must name the 8s floor, got: {stderr}"
    );
}
