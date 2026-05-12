//! Integration tests for `scripts/install_cloning_fish.sh` (the v2
//! fish-speech installer; ROADMAP v0.4 PR-AB step 6a).
//!
//! These tests exercise the script's *contract* with the Rust
//! orchestrator (`apps/voiceforge-cli/src/install_cloning.rs`) without
//! kicking off the real ~12 GB download. The contract:
//!
//!   1. `==>` lines mark phase boundaries the orchestrator parses
//!      for the indicatif install wizard.
//!   2. Dry-run mode (`VOICEFORGE_INSTALL_CLONING_DRY_RUN=1`) walks
//!      the full pipeline without invoking pip / git / huggingface-cli.
//!   3. Uninstall mode runs without weights and preserves HF + whisper
//!      caches.
//!   4. Check mode fails loud when the marker is absent.
//!   5. A schema-1 marker triggers an `INSTALLED.v1.bak` backup
//!      (in normal mode, before the rest of the v2 flow runs).
//!   6. `VOICEFORGE_INSTALL_CLONING_SKIP_WEIGHTS=1` bypasses the
//!      ~10 GB HF download path.
//!
//! All tests use a unique tempdir for `VOICEFORGE_HOME` so they can
//! run in parallel.

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn script_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // CARGO_MANIFEST_DIR for an integration test in apps/voiceforge-cli/
    // points at that crate root; the script is two levels up at
    // <repo>/scripts/install_cloning_fish.sh.
    manifest
        .parent() // apps/
        .and_then(|p| p.parent()) // <repo>/
        .map(|p| p.join("scripts").join("install_cloning_fish.sh"))
        .expect("locate install_cloning_fish.sh")
}

fn run_script(home: &Path, extra_env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new("bash");
    cmd.arg(script_path());
    cmd.env("VOICEFORGE_HOME", home);
    cmd.env("VOICEFORGE_INSTALL_CLONING_DRY_RUN", "1");
    // cargo test launches subprocesses under Rosetta on Apple Silicon
    // (`uname -m == x86_64` even though the hardware is arm64); the
    // production platform check would die there. The bypass is test-
    // only; the orchestrator NEVER sets it (will be asserted in 6c).
    cmd.env("VOICEFORGE_INSTALL_CLONING_SKIP_PLATFORM_CHECK", "1");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run install_cloning_fish.sh");
    if std::env::var("VOICEFORGE_TEST_DEBUG_SCRIPT").is_ok() {
        eprintln!("--- script status: {} ---", out.status);
        eprintln!(
            "--- script stderr ---\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        eprintln!(
            "--- script stdout ---\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
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
    let out = run_script(tmp.path(), &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The orchestrator parses these prefixes to drive the indicatif
    // progress wizard. If a phase line goes missing, the install UX
    // freezes silently — make this explicit.
    let required_phases = [
        "voiceforge install-cloning v2 (fish-speech S2 Pro)",
        "disk-space precheck",
        "platform check",
        "arm64 Homebrew check",
        "creating venv at",
        "cloning fish-speech @ 3dd1f85c",
        "checking out pinned SHA",
        "asserting required repo paths exist",
        "upgrading pip + wheel",
        "installing fish-speech + deps",
        "installing voiceforge-side python helpers",
        "downloading fishaudio/s2-pro",
        "sha256-verifying load-bearing model files",
        "downloading whisper medium model",
        "smoke test:",
        "writing schema-v2 marker",
        "done — fish-speech S2 Pro cloning stack ready",
    ];
    for needle in required_phases {
        assert!(
            stdout.contains(needle),
            "missing phase line \"{needle}\" in dry-run output:\n{stdout}"
        );
    }
}

#[test]
fn dry_run_phase_lines_use_arrow_prefix() {
    // The orchestrator's parser looks for `\n==> ` at start of line.
    // Strip whitespace + count distinct phase markers; we expect at
    // least 14 (one per major step).
    let tmp = TempDir::new().unwrap();
    let out = run_script(tmp.path(), &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let phase_count: usize = stdout.lines().filter(|l| l.starts_with("==> ")).count();
    assert!(
        phase_count >= 14,
        "expected >=14 `==> ` phase lines, got {phase_count}\noutput:\n{stdout}"
    );
}

#[test]
fn dry_run_does_not_invoke_pip_or_git() {
    // In dry-run, all heavy commands route through `run()` which
    // prefixes them with `[dry-run]`. Verify by asserting the venv
    // python was never actually created on disk.
    let tmp = TempDir::new().unwrap();
    let _out = run_script(tmp.path(), &[]);
    let venv_python = tmp.path().join("cloning/venv/bin/python");
    assert!(
        !venv_python.exists(),
        "dry-run created real venv at {} — expected no-op",
        venv_python.display()
    );
    let repo_git = tmp.path().join("cloning/repo/.git");
    assert!(
        !repo_git.exists(),
        "dry-run cloned real repo at {} — expected no-op",
        repo_git.display()
    );
}

#[test]
fn uninstall_mode_skips_heavy_setup_phases() {
    let tmp = TempDir::new().unwrap();
    // Seed a fake install layout so uninstall has something to "remove".
    std::fs::create_dir_all(tmp.path().join("cloning/venv/bin")).unwrap();
    std::fs::create_dir_all(tmp.path().join("cloning/repo")).unwrap();
    std::fs::write(
        tmp.path().join("cloning/INSTALLED.toml"),
        "schema_version = 2\n",
    )
    .unwrap();
    let out = run_script(
        tmp.path(),
        &[("VOICEFORGE_INSTALL_CLONING_MODE", "uninstall")],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "uninstall mode exited non-zero: {}\n{stdout}",
        out.status
    );
    // Must NOT touch the disk-space precheck / venv-creation phases.
    assert!(
        !stdout.contains("disk-space precheck"),
        "uninstall walked past mode dispatch into install flow: {stdout}"
    );
    assert!(
        !stdout.contains("creating venv at"),
        "uninstall walked past mode dispatch into venv creation: {stdout}"
    );
    // Must mention the preserved-cache contract.
    assert!(
        stdout.contains("preserved: ~/.cache/huggingface"),
        "uninstall must document HF cache preservation: {stdout}"
    );
    assert!(
        stdout.contains("preserved: ~/.cache/whisper"),
        "uninstall must document whisper cache preservation: {stdout}"
    );
}

#[test]
fn check_mode_fails_when_marker_absent() {
    let tmp = TempDir::new().unwrap();
    let out = run_script(tmp.path(), &[("VOICEFORGE_INSTALL_CLONING_MODE", "check")]);
    assert!(
        !out.status.success(),
        "check mode must fail when marker is absent"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("marker missing") || stderr.contains("voiceforge install-cloning"),
        "check mode error must point at the install command, got stderr:\n{stderr}"
    );
}

#[test]
fn schema_v1_marker_triggers_v1_backup() {
    let tmp = TempDir::new().unwrap();
    let cloning = tmp.path().join("cloning");
    std::fs::create_dir_all(&cloning).unwrap();
    // Synthesize a schema-1 marker like v1 install_cloning.sh would write.
    std::fs::write(
        cloning.join("INSTALLED.toml"),
        r#"schema_version = 1
version = "0.1.0"
installed_at = "2025-12-01T00:00:00Z"
gpt_sovits_sha = "08d627c3338173c3229286d8787060d6559fe0f8"
"#,
    )
    .unwrap();
    let out = run_script(tmp.path(), &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("detected schema-1 (GPT-SoVITS) install"),
        "schema-1 detection line missing:\n{stdout}"
    );
    // In dry-run the cp doesn't actually execute, but the orchestrator
    // contract is the phase line — that's what the test asserts.
}

#[test]
fn skip_weights_omits_hf_download_phase() {
    let tmp = TempDir::new().unwrap();
    let out = run_script(
        tmp.path(),
        &[("VOICEFORGE_INSTALL_CLONING_SKIP_WEIGHTS", "1")],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The HF-download phase line must be elided when SKIP_WEIGHTS=1.
    assert!(
        !stdout.contains("downloading fishaudio/s2-pro"),
        "SKIP_WEIGHTS=1 must skip HF download phase:\n{stdout}"
    );
    // And the sha256 verify phase too.
    assert!(
        !stdout.contains("sha256-verifying load-bearing"),
        "SKIP_WEIGHTS=1 must skip sha256 verify phase:\n{stdout}"
    );
    // BUT the warning that weights were skipped MUST appear.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("SKIP_WEIGHTS"),
        "SKIP_WEIGHTS=1 must emit a visible warning. combined output:\n{combined}"
    );
}

#[test]
fn unknown_mode_fails_loud() {
    let tmp = TempDir::new().unwrap();
    let out = run_script(
        tmp.path(),
        &[("VOICEFORGE_INSTALL_CLONING_MODE", "frobnicate")],
    );
    assert!(
        !out.status.success(),
        "unknown mode must error, got success: {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown") && stderr.contains("frobnicate"),
        "unknown-mode error must name the offending value, got stderr:\n{stderr}"
    );
}
