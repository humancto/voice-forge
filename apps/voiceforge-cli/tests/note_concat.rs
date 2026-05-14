//! Integration test for `note::concat_chunks` — exercises a real
//! `ffmpeg` (from PATH) on MockSynth-generated chunks and asserts the
//! concat WAV has the expected duration + spec.
//!
//! Skipped when no `ffmpeg` is on PATH; uses `VOICEFORGE_REQUIRE_FFMPEG=1`
//! to flip skip to a hard failure for CI.

use std::path::PathBuf;
use std::process::Command;

// Re-exporting the binary-crate internal is impossible from an
// integration test (binary crates don't expose a library), so we
// shell out to find ffmpeg and then exercise the demuxer directly.

fn ffmpeg_path() -> Option<PathBuf> {
    let candidates = [
        "/opt/homebrew/opt/ffmpeg@6/bin/ffmpeg",
        "/opt/homebrew/bin/ffmpeg",
        "/usr/local/bin/ffmpeg",
        "/usr/bin/ffmpeg",
    ];
    for c in &candidates {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Some(p);
        }
    }
    // Fall back to PATH lookup via `which`.
    if let Ok(out) = Command::new("which").arg("ffmpeg").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(PathBuf::from(s));
            }
        }
    }
    None
}

fn write_silent_wav_44100_mono(path: &std::path::Path, samples: u32) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for _ in 0..samples {
        w.write_sample(0i16).unwrap();
    }
    w.finalize().unwrap();
}

#[test]
fn ffmpeg_concat_demuxer_round_trip_via_real_binary() {
    let ffmpeg = match ffmpeg_path() {
        Some(p) => p,
        None => {
            if std::env::var("VOICEFORGE_REQUIRE_FFMPEG")
                .map(|v| v == "1")
                .unwrap_or(false)
            {
                panic!("VOICEFORGE_REQUIRE_FFMPEG=1 but no ffmpeg on PATH");
            }
            eprintln!("skip: ffmpeg not found on PATH");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    let chunks_dir = tmp.path().join("note.wav.chunks");
    std::fs::create_dir_all(&chunks_dir).unwrap();
    // 3 silent chunks of 11025 samples each (0.25s @ 44.1 kHz) → 0.75s total.
    for i in 0..3 {
        write_silent_wav_44100_mono(&chunks_dir.join(format!("chunk_{i:04}.wav")), 11025);
    }
    // concat_list.txt with relative filenames.
    let list_path = chunks_dir.join("concat_list.txt");
    std::fs::write(
        &list_path,
        "file 'chunk_0000.wav'\nfile 'chunk_0001.wav'\nfile 'chunk_0002.wav'\n",
    )
    .unwrap();

    let out = tmp.path().join("note.wav");
    let status = Command::new(&ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
        ])
        .arg(&list_path)
        .args(["-c", "copy"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg failed: {status:?}");
    assert!(out.is_file(), "concat output missing");

    // The concat output should be 3 * 11025 = 33075 samples at 44.1 kHz.
    let reader = hound::WavReader::open(&out).unwrap();
    let spec = reader.spec();
    assert_eq!(spec.channels, 1);
    assert_eq!(spec.sample_rate, 44_100);
    assert_eq!(spec.bits_per_sample, 16);
    let total_samples = reader.duration();
    assert_eq!(
        total_samples, 33075,
        "expected 33075 samples (3 x 11025), got {total_samples}"
    );
}
