//! E2E integration test for `voiceforge voices migrate` (PR-C-b B-3).
//!
//! Stages a complete v1 voice fixture (with a real 32 kHz mono PCM_16 WAV
//! written via hound — the key compatibility precondition for the
//! fish-speech S2 Pro engine to accept the post-migration ref clip),
//! invokes the built `voiceforge voices migrate <name>` binary against a
//! private VOICEFORGE_HOME, and asserts:
//!
//!   - profile.toml is now schema_version = 2, recipe = fish-speech-s2-pro
//!   - ref.wav + ref.txt exist at top level
//!   - ref_main.wav is GONE from top level
//!   - .v1.bak/ exists with profile.toml + ref_main pair + 5 aux pairs
//!   - post-migration ref.wav opens via hound::WavReader with
//!     channels=1, sample_rate=32000, bits_per_sample=16, format=Int.
//!
//! The WAV-format assertion is the load-bearing claim of the entire PR:
//! the migration silently produces a v2 profile whose `ref.wav` MUST be
//! consumable by fish-speech. If a hand-edited v1 voice ever had a
//! non-conforming `ref_main.wav`, the migration would copy it through
//! and the next `voiceforge say --voice <name>` would fail at synth
//! time with no error tracing back to migrate.

use hound::{SampleFormat, WavSpec, WavWriter};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn voiceforge_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_voiceforge"))
}

/// Write a real 32 kHz mono PCM_16 silent WAV. Matches the format the
/// v1 clone pipeline produces in `scripts/clone_voice.sh` (line 129,
/// `-ac 1 -ar 32000 -acodec pcm_s16le`).
fn write_real_v1_ref_wav(path: &Path, duration_seconds: f32) {
    let spec = WavSpec {
        channels: 1,
        sample_rate: 32000,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::create(path, spec).unwrap();
    let total_samples = (32000.0 * duration_seconds) as i64;
    for _ in 0..total_samples {
        writer.write_sample(0i16).unwrap();
    }
    writer.finalize().unwrap();
}

/// Stage a complete v1 voice with REAL WAVs at top level (so we can prove
/// the migrated ref.wav is fish-speech-compatible). Test fixture is
/// duplicated from `voices.rs::tests::write_full_profile` per the plan's
/// test-fixture duplication policy (rust-expert R7).
fn stage_v1_voice(home: &Path, name: &str) {
    let dir = home.join("voices").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let toml = format!(
        r#"
schema_version = 1
name = "{name}"
source = "/legacy/source.wav"
created_at = "2025-12-01T00:00:00Z"
duration_seconds = 60.0
recipe = "gpt-sovits-v2-multi-aux-ref"
aux_count = 5
"#
    );
    std::fs::write(dir.join("profile.toml"), toml).unwrap();
    // ref_main.wav: a real 10-second 32 kHz mono PCM_16 silent WAV.
    write_real_v1_ref_wav(&dir.join("ref_main.wav"), 10.0);
    std::fs::write(dir.join("ref_main.txt"), b"stub legacy transcript").unwrap();
    for i in 1..=5 {
        // Aux files: stubs are fine; migration just copies them into .v1.bak/.
        // We assert their presence post-migration, not their WAV shape.
        std::fs::write(
            dir.join(format!("aux_{i}.wav")),
            b"RIFF\x00\x00\x00\x00WAVEdata",
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("aux_{i}.txt")),
            format!("stub aux transcript {i}"),
        )
        .unwrap();
    }
}

#[test]
fn voiceforge_voices_migrate_e2e_round_trip() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    stage_v1_voice(home, "peter");

    let output = Command::new(voiceforge_bin())
        .arg("voices")
        .arg("migrate")
        .arg("peter")
        .env("VOICEFORGE_HOME", home)
        .output()
        .expect("run voiceforge voices migrate");

    assert!(
        output.status.success(),
        "voiceforge voices migrate failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("migrated voice"),
        "expected success message, got: {stdout}"
    );
    assert!(
        stdout.contains("fish-speech-s2-pro"),
        "expected new-recipe mention, got: {stdout}"
    );

    let dir = home.join("voices/peter");

    // profile.toml is v2.
    let profile = std::fs::read_to_string(dir.join("profile.toml")).unwrap();
    assert!(
        profile.contains("schema_version = 2"),
        "profile.toml not v2 after migrate: {profile}"
    );
    assert!(
        profile.contains(r#"recipe = "fish-speech-s2-pro""#),
        "profile.toml recipe not fish-speech: {profile}"
    );

    // ref.wav / ref.txt at top level; ref_main GONE from top level.
    assert!(
        dir.join("ref.wav").is_file(),
        "ref.wav missing at top level"
    );
    assert!(
        dir.join("ref.txt").is_file(),
        "ref.txt missing at top level"
    );
    assert!(
        !dir.join("ref_main.wav").exists(),
        "ref_main.wav must be gone from top level (moved to .v1.bak/)"
    );
    assert!(
        !dir.join("ref_main.txt").exists(),
        "ref_main.txt must be gone from top level"
    );

    // .v1.bak/ with 13 files (1 profile + 2 ref + 10 aux).
    let bak = dir.join(".v1.bak");
    assert!(bak.is_dir(), ".v1.bak/ should exist");
    assert!(bak.join("profile.toml").is_file());
    assert!(bak.join("ref_main.wav").is_file());
    assert!(bak.join("ref_main.txt").is_file());
    for i in 1..=5 {
        assert!(
            bak.join(format!("aux_{i}.wav")).is_file(),
            ".v1.bak/aux_{i}.wav missing"
        );
        assert!(
            bak.join(format!("aux_{i}.txt")).is_file(),
            ".v1.bak/aux_{i}.txt missing"
        );
    }

    // R5: post-migration ref.wav MUST be 32 kHz mono PCM_16 — the
    // load-bearing fish-speech compatibility claim.
    let reader = hound::WavReader::open(dir.join("ref.wav"))
        .expect("post-migration ref.wav must open via hound");
    let spec = reader.spec();
    assert_eq!(spec.channels, 1, "ref.wav channels");
    assert_eq!(spec.sample_rate, 32000, "ref.wav sample rate");
    assert_eq!(spec.bits_per_sample, 16, "ref.wav bit depth");
    assert_eq!(
        spec.sample_format,
        SampleFormat::Int,
        "ref.wav sample format"
    );

    // Idempotency on second run.
    let output2 = Command::new(voiceforge_bin())
        .arg("voices")
        .arg("migrate")
        .arg("peter")
        .env("VOICEFORGE_HOME", home)
        .output()
        .expect("re-run voiceforge voices migrate");
    assert!(output2.status.success(), "idempotent re-run must succeed");
    let stdout2 = String::from_utf8_lossy(&output2.stdout);
    assert!(
        stdout2.contains("already on schema 2"),
        "expected no-op message, got: {stdout2}"
    );
    assert!(
        stdout2.contains("recovery files from a prior migration"),
        "idempotent re-run should surface .v1.bak/ path: {stdout2}"
    );
}
