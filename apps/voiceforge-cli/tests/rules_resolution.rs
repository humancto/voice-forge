//! Integration tests for path-resolution precedence.
//!
//! These tests mutate `$VOICEFORGE_HOME`, which is process-global —
//! every test in this file is `#[serial]` so they can't race each
//! other or the unit tests in `paths.rs`.
//!
//! We can't reach `mod rules` directly from a binary-crate integration
//! test, so we shell out to the built `voiceforge` binary and observe
//! its behavior via stderr (the runner prints the running command and
//! events.json miss/hit info).

use serial_test::serial;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn voiceforge_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_voiceforge"))
}

fn write_stub_rules(home: &std::path::Path) {
    let dir = home.join("rules");
    fs::create_dir_all(&dir).expect("rules dir");
    fs::write(
        dir.join("events.json"),
        r#"{
          "build_failed": {
            "voice": "stub_voice",
            "lines": ["STUB FAIL LINE 1", "STUB FAIL LINE 2"]
          },
          "build_success": {
            "voice": "stub_voice",
            "lines": ["STUB OK LINE"]
          }
        }"#,
    )
    .expect("write stub rules");
}

/// Run `voiceforge voices` (a side-effect-free subcommand that still
/// touches the binary's startup path) with VOICEFORGE_HOME pointing at
/// a tempdir that has a stub rules file. The point isn't the output —
/// it's that the binary doesn't crash when the user-home rules path
/// has a custom file. Real precedence-of-pick verification happens via
/// the unit tests in rules.rs which are deterministic and fast.
#[test]
#[serial]
fn voiceforge_home_with_custom_rules_doesnt_crash_startup() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_stub_rules(tmp.path());

    let result = Command::new(voiceforge_bin())
        .env("VOICEFORGE_HOME", tmp.path())
        .arg("voices")
        .output()
        .expect("spawn voiceforge");

    assert!(
        result.status.success(),
        "voiceforge voices failed under custom VOICEFORGE_HOME: stderr={}",
        String::from_utf8_lossy(&result.stderr)
    );
}

/// Negative test: VOICEFORGE_HOME pointing at a dir with no
/// rules/events.json must NOT crash — the binary should fall back to
/// the embedded defaults silently.
#[test]
#[serial]
fn voiceforge_home_without_rules_falls_back_silently() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Deliberately do NOT write a rules file.

    let result = Command::new(voiceforge_bin())
        .env("VOICEFORGE_HOME", tmp.path())
        .arg("voices")
        .output()
        .expect("spawn voiceforge");

    assert!(
        result.status.success(),
        "voiceforge voices should succeed with empty VOICEFORGE_HOME: stderr={}",
        String::from_utf8_lossy(&result.stderr)
    );
}

/// Negative test: malformed rules JSON in VOICEFORGE_HOME must NOT
/// crash startup — `Rules::load` returns Err, we silently fall back to
/// embedded defaults. (The embedded path is the safety net; users
/// shouldn't be able to brick their CLI by editing a config file
/// wrong.)
#[test]
#[serial]
fn malformed_user_rules_does_not_brick_cli() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("rules");
    fs::create_dir_all(&dir).expect("rules dir");
    fs::write(dir.join("events.json"), "not valid json {{{").expect("write garbage");

    let result = Command::new(voiceforge_bin())
        .env("VOICEFORGE_HOME", tmp.path())
        .arg("voices")
        .output()
        .expect("spawn voiceforge");

    assert!(
        result.status.success(),
        "voiceforge voices should survive malformed user rules: stderr={}",
        String::from_utf8_lossy(&result.stderr)
    );
}
