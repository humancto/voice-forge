//! Integration tests for `scripts/fish_speech_synth.py` (the v2
//! fish-speech NDJSON worker; ROADMAP v0.4 PR-AB step 7).
//!
//! End-to-end synth requires a working fish-speech install, ~10 GB of
//! weights, and ~30s of model-load time per run. We skip that here
//! and only exercise the *protocol contract* the Rust client (FishEngine,
//! lands in step 8) depends on:
//!
//!   1. `python3 -m py_compile` succeeds (catches syntax regressions).
//!   2. Missing `INSTALLED.toml` -> emits `{"ok": false, ...}` to
//!      stdout AND exits with code 2 (so the Rust client distinguishes
//!      "no install" from "synth failure").
//!   3. Wrong schema_version (v1 marker) -> bails with code 2 + a
//!      `re-install --force` hint.
//!   4. fish_speech_sha mismatch -> bails with code 2.
//!   5. Stdout bytes BEFORE any framework chatter must be valid NDJSON
//!      (the protocol-stdout-stash trick at the top of the script).
//!
//! All tests use a unique tempdir for `VOICEFORGE_HOME` so they can
//! run in parallel.

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const PINNED_FISH_SPEECH_SHA: &str = "3dd1f85c402ee6f0a17c2971d3b0dd8d881ca139";

fn script_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("scripts").join("fish_speech_synth.py"))
        .expect("locate fish_speech_synth.py")
}

fn write_marker(home: &Path, body: &str) {
    let dir = home.join("cloning");
    std::fs::create_dir_all(&dir).expect("mkdir cloning");
    std::fs::write(dir.join("INSTALLED.toml"), body).expect("write marker");
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
fn script_passes_python_syntax_check() {
    // Quick regression net for syntax breakage. `py_compile` is in the
    // stdlib so this only requires python3, which voiceforge already
    // requires for the cloning runtime — same precedent as the v1
    // cloning_synth.py (which has zero in-repo tests but ships).
    let out = Command::new("python3")
        .arg("-m")
        .arg("py_compile")
        .arg(script_path())
        .output()
        .expect("run py_compile");
    assert!(
        out.status.success(),
        "py_compile failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
fn missing_marker_emits_ndjson_error_and_exits_2() {
    let tmp = TempDir::new().unwrap();
    // No INSTALLED.toml in the home — script must bail BEFORE trying
    // to import fish_speech.
    let out = Command::new("python3")
        .arg(script_path())
        .env("VOICEFORGE_HOME", tmp.path())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("invoke synth script");
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit code 2 (install-not-ready), got {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first_line = stdout.lines().next().unwrap_or("");
    let parsed: serde_json::Value =
        serde_json::from_str(first_line).expect("first stdout line must be NDJSON");
    assert_eq!(parsed["ok"], false, "missing-marker must emit ok:false");
    assert!(
        parsed["error"]
            .as_str()
            .unwrap_or("")
            .contains("cloning marker missing"),
        "error must name the missing artifact: {parsed:?}"
    );
}

#[test]
fn schema_v1_marker_bails_with_upgrade_hint() {
    let tmp = TempDir::new().unwrap();
    write_marker(
        tmp.path(),
        r#"
schema_version = 1
gpt_sovits_sha = "08d627c3"
python_path = "/p"
ffmpeg6_prefix = "/f"
"#,
    );
    let out = Command::new("python3")
        .arg(script_path())
        .env("VOICEFORGE_HOME", tmp.path())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("invoke synth script");
    assert_eq!(
        out.status.code(),
        Some(2),
        "schema-1 marker must exit 2; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap_or(""))
        .expect("first stdout line must be NDJSON");
    let err = parsed["error"].as_str().unwrap_or("");
    assert!(
        err.contains("schema_version") && err.contains("install-cloning"),
        "v1-on-v2-script error must reference both the version mismatch AND the
         install-cloning command, got: {err}"
    );
}

#[test]
fn fish_speech_sha_mismatch_bails_with_reinstall_hint() {
    let tmp = TempDir::new().unwrap();
    // Schema is v2, but the SHA pin is wrong — simulates a torn install
    // where the Python script and the marker are out of sync.
    write_marker(
        tmp.path(),
        r#"
schema_version = 2
fish_speech_sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
python_path = "/p"
ffmpeg6_prefix = "/f"
repo_path = "/r"
checkpoint_path = "/c"
whisper_model = "medium"
engine = "fish-speech-s2-pro"
"#,
    );
    let out = Command::new("python3")
        .arg(script_path())
        .env("VOICEFORGE_HOME", tmp.path())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("invoke synth script");
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap_or(""))
        .expect("first stdout line must be NDJSON");
    let err = parsed["error"].as_str().unwrap_or("");
    assert!(
        err.contains("SHA mismatch") && err.contains("install-cloning"),
        "SHA mismatch error must reference the install command, got: {err}"
    );
    // The mismatch error must mention BOTH SHAs so the user sees what
    // the script expected vs what they have on disk.
    assert!(
        err.contains("deadbeef") && err.contains(&PINNED_FISH_SPEECH_SHA[..8]),
        "SHA mismatch error must include both SHAs, got: {err}"
    );
}

#[test]
fn pinned_sha_matches_install_script() {
    // Single-source-of-truth check: the Python pin and the bash pin
    // must match. If they drift, a working install ships with a
    // synth-side SHA failure on every request — silently broken on
    // every clone after the next install.
    let bash_path = script_path().with_file_name("install_cloning_fish.sh");
    let bash = std::fs::read_to_string(&bash_path).expect("read bash installer");
    assert!(
        bash.contains(&format!(r#"FISH_SPEECH_SHA="{PINNED_FISH_SPEECH_SHA}""#)),
        "install_cloning_fish.sh's FISH_SPEECH_SHA does not match the python pin
         {PINNED_FISH_SPEECH_SHA}.\nThe two MUST be bumped in lockstep."
    );

    let py = std::fs::read_to_string(script_path()).expect("read python script");
    assert!(
        py.contains(&format!(
            r#"EXPECTED_FISH_SPEECH_SHA = "{PINNED_FISH_SPEECH_SHA}""#
        )),
        "fish_speech_synth.py's pin diverged from the test constant"
    );
}

#[test]
fn first_stdout_byte_starts_with_brace() {
    // The protocol contract: every stdout line is JSON, no leading
    // banner / log line. The script's `_PROTOCOL_STDOUT = sys.stdout;
    // sys.stdout = sys.stderr` prelude protects this. Verify by
    // capturing bytes 0..1 of stdout for a missing-marker invocation
    // (the simplest path that produces output without needing fish_speech).
    let tmp = TempDir::new().unwrap();
    let out = Command::new("python3")
        .arg(script_path())
        .env("VOICEFORGE_HOME", tmp.path())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("invoke synth script");
    assert!(
        out.stdout.first() == Some(&b'{'),
        "first stdout byte must be '{{' (NDJSON), got: {:?}\nfull stdout: {}",
        out.stdout.first(),
        String::from_utf8_lossy(&out.stdout)
    );
}
