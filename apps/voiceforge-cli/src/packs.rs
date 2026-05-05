//! Pre-rendered voice pack lookup at `~/.voiceforge/packs/<name>/`.
//!
//! Each installed pack is a directory containing:
//!
//! - `manifest.toml` — schema, name, license, attribution
//! - `reference.wav` — the source clip used to render (informational)
//! - `wav/<event>.wav` — one WAV per event id (build_failed, tests_passed, ...)
//! - `checksums.txt` — sha256 manifest
//!
//! `voiceforge play --pack <name> --event <id>` resolves to
//! `~/.voiceforge/packs/<name>/wav/<event>.wav` and plays it directly —
//! no TTS engine, no model load, sub-100ms warm path.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::paths;

/// Names that would collide with sibling dirs under `~/.voiceforge/`.
/// Mirrors `voices::RESERVED_NAMES`; pack names live in their own
/// `~/.voiceforge/packs/` namespace but the same hygiene applies in
/// case the layout changes.
const RESERVED_NAMES: &[&str] = &[
    "presets",
    "cache",
    "cloning",
    "voices",
    "embeddings",
    "logs",
    "packs",
];

/// Failures during pack lookup / playback.
///
/// Variants map 1:1 to process exit codes documented in `voiceforge play --help`.
/// `#[non_exhaustive]` so future variants don't break callers that match
/// exhaustively.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PlayError {
    /// Pack or event id failed `validate_pack_name` / `validate_event_id`.
    /// Process exits 2.
    #[error("invalid name: {0}")]
    InvalidName(String),

    /// `~/.voiceforge/packs/<pack>/` is not a directory (pack not installed).
    /// Process exits 2.
    #[error("pack not installed: {0:?}")]
    PackMissing(String),

    /// Pack exists but `wav/<event>.wav` is missing.
    /// Process exits 3 — callers (e.g. `voiceforge hook`) can fall back here.
    #[error("event {event:?} not in pack {pack:?}")]
    EventMissing { pack: String, event: String },

    /// WAV file present but rodio could not decode it.
    /// Process exits 4. Don't silently fall through — corruption is loud.
    #[error("wav file present but undecodable: {path}")]
    WavCorrupt {
        path: PathBuf,
        #[source]
        source: rodio::decoder::DecoderError,
    },

    /// rodio's `OutputStream::try_default` failed (no audio device, etc).
    /// Process exits 5.
    #[error("audio backend unavailable")]
    AudioBackend(#[source] rodio::StreamError),

    /// Catch-all for other I/O errors (EACCES on readdir, etc).
    /// Process exits 2 by default; dispatch may remap by context.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl PlayError {
    /// Process exit code per the documented contract.
    pub fn exit_code(&self) -> i32 {
        match self {
            PlayError::InvalidName(_) | PlayError::PackMissing(_) | PlayError::Io(_) => 2,
            PlayError::EventMissing { .. } => 3,
            PlayError::WavCorrupt { .. } => 4,
            PlayError::AudioBackend(_) => 5,
        }
    }
}

pub type Result<T> = std::result::Result<T, PlayError>;

/// Reject pack names that could path-traverse, contain shell-unsafe
/// characters, or collide with reserved sibling dirs under `~/.voiceforge/`.
///
/// Same rules as `voices::validate_name` plus an explicit `.` rejection
/// so `--pack foo.bar` and `--pack ..` both fail at parse, before any
/// FS access.
pub fn validate_pack_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(PlayError::InvalidName("pack name is empty".into()));
    }
    if name.len() > 64 {
        return Err(PlayError::InvalidName(format!(
            "pack name too long (max 64 chars): {name:?}"
        )));
    }
    if name == "." || name == ".." {
        return Err(PlayError::InvalidName(format!(
            "pack name cannot be {name:?}"
        )));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(PlayError::InvalidName(format!(
            "pack name {name:?} collides with a reserved subdir"
        )));
    }
    for c in name.chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
        if !ok {
            return Err(PlayError::InvalidName(format!(
                "pack name {name:?} has invalid char {c:?}; allowed: [a-z0-9_-], 1..=64"
            )));
        }
    }
    Ok(())
}

/// Reject event ids that could path-traverse or break the
/// `wav/<event>.wav` filename layout. Same rules as `validate_pack_name`.
///
/// In particular this rejects any `.` so `--event ../../etc/passwd` and
/// `--event foo.bar` fail at parse, before canonicalize.
pub fn validate_event_id(event: &str) -> Result<()> {
    if event.is_empty() {
        return Err(PlayError::InvalidName("event id is empty".into()));
    }
    if event.len() > 64 {
        return Err(PlayError::InvalidName(format!(
            "event id too long (max 64 chars): {event:?}"
        )));
    }
    for c in event.chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
        if !ok {
            return Err(PlayError::InvalidName(format!(
                "event id {event:?} has invalid char {c:?}; allowed: [a-z0-9_-], 1..=64"
            )));
        }
    }
    Ok(())
}

/// `~/.voiceforge/packs/` (or `$VOICEFORGE_HOME/packs/` if set).
pub fn packs_root() -> Option<PathBuf> {
    paths::user_home().map(|h| h.join("packs"))
}

/// Resolve `<packs_root>/<pack>/wav/<event>.wav` with the full
/// path-traversal posture documented in the plan:
///
/// 1. Validate names.
/// 2. Check `pack_dir.is_dir()` — distinguishes `PackMissing` (exit 2)
///    from `EventMissing` (exit 3).
/// 3. Canonicalize pack dir, assert prefix under canonical packs root.
/// 4. Check `wav/<event>.wav` exists.
/// 5. Canonicalize, assert prefix under **per-pack** `wav/` (not packs root).
///
/// TOCTOU between resolve and open is bounded by the per-pack `wav/`
/// prefix check; same trust boundary as direct write to `<pack>/wav/`.
pub fn resolve_event_wav(pack: &str, event: &str) -> Result<PathBuf> {
    validate_pack_name(pack)?;
    validate_event_id(event)?;

    let packs_root = packs_root()
        .ok_or_else(|| PlayError::InvalidName("could not resolve ~/.voiceforge/packs".into()))?;
    let pack_dir = packs_root.join(pack);
    if !pack_dir.is_dir() {
        return Err(PlayError::PackMissing(pack.to_owned()));
    }

    // Canonicalize pack dir + packs root, assert containment. Catches a
    // symlink-pivoted pack dir that points outside packs_root.
    let canonical_packs_root = packs_root.canonicalize().map_err(PlayError::from)?;
    let canonical_pack_dir = pack_dir.canonicalize().map_err(PlayError::from)?;
    if !canonical_pack_dir.starts_with(&canonical_packs_root) {
        // Don't reveal that the symlink existed; report as PackMissing.
        return Err(PlayError::PackMissing(pack.to_owned()));
    }

    let wav_path = pack_dir.join("wav").join(format!("{event}.wav"));
    if !wav_path.is_file() {
        return Err(PlayError::EventMissing {
            pack: pack.to_owned(),
            event: event.to_owned(),
        });
    }

    // Per-pack wav/ prefix check. Per rust-expert review: prefix must be
    // <canonical_pack_dir>/wav, NOT canonical_packs_root, otherwise an
    // attacker with write access to one pack can symlink into another's wav/.
    let canonical_wav = wav_path.canonicalize().map_err(PlayError::from)?;
    let canonical_pack_wav_dir = canonical_pack_dir.join("wav");
    if !canonical_wav.starts_with(&canonical_pack_wav_dir) {
        // Cross-pack symlink leak attempt; report as EventMissing.
        return Err(PlayError::EventMissing {
            pack: pack.to_owned(),
            event: event.to_owned(),
        });
    }

    Ok(canonical_wav)
}

/// List sorted event ids available in an installed pack.
///
/// Reads `<pack>/wav/`, strips `.wav`, sorts lexicographically. Same
/// canonicalize+prefix posture as `resolve_event_wav` so a symlinked
/// `<pack>/wav/` pointing elsewhere doesn't enumerate someone else's files.
pub fn list_events(pack: &str) -> Result<Vec<String>> {
    validate_pack_name(pack)?;

    let packs_root = packs_root()
        .ok_or_else(|| PlayError::InvalidName("could not resolve ~/.voiceforge/packs".into()))?;
    let pack_dir = packs_root.join(pack);
    if !pack_dir.is_dir() {
        return Err(PlayError::PackMissing(pack.to_owned()));
    }

    let canonical_packs_root = packs_root.canonicalize()?;
    let canonical_pack_dir = pack_dir.canonicalize()?;
    if !canonical_pack_dir.starts_with(&canonical_packs_root) {
        return Err(PlayError::PackMissing(pack.to_owned()));
    }

    let wav_dir = canonical_pack_dir.join("wav");
    if !wav_dir.is_dir() {
        // Pack exists but has no wav/ subdir — return empty list, not error.
        // Lets callers introspect partially-installed packs.
        return Ok(Vec::new());
    }
    let canonical_wav_dir = wav_dir.canonicalize()?;
    if !canonical_wav_dir.starts_with(&canonical_pack_dir) {
        return Err(PlayError::PackMissing(pack.to_owned()));
    }

    let mut events: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&canonical_wav_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if ext != "wav" {
            continue;
        }
        events.push(stem.to_owned());
    }
    events.sort();
    Ok(events)
}

/// True iff the WAV path is under `<canonical_pack_dir>/wav`. Helper
/// for callers that have already done their own resolution and want a
/// final containment check.
#[allow(dead_code)]
pub(crate) fn assert_under_pack_wav(wav: &Path, canonical_pack_dir: &Path) -> bool {
    wav.starts_with(canonical_pack_dir.join("wav"))
}

/// Play the resolved WAV synchronously through rodio with **typed**
/// errors so the `voiceforge play` dispatch can map to the documented
/// exit codes (4 for corrupt WAV, 5 for audio backend).
///
/// Distinct from `audio::play` (which returns opaque `anyhow::Error`)
/// because callers of `voiceforge say` / `run` don't need to distinguish
/// these failure modes — they just propagate to the user. Pack playback
/// has the structured exit-code contract.
pub fn play_wav(path: &Path) -> Result<()> {
    use rodio::{Decoder, OutputStream, Sink};
    use std::fs::File;
    use std::io::BufReader;

    let (_stream, handle) = OutputStream::try_default().map_err(PlayError::AudioBackend)?;
    // Sink::try_new returns rodio::PlayError (different from our PlayError).
    // Failure here means the device was lost between try_default and try_new
    // — same exit-5 bucket as the primary backend failure.
    let sink = Sink::try_new(&handle)
        .map_err(|_| PlayError::AudioBackend(rodio::StreamError::NoDevice))?;
    let file = File::open(path)?;
    let source = Decoder::new(BufReader::new(file)).map_err(|e| PlayError::WavCorrupt {
        path: path.to_owned(),
        source: e,
    })?;
    sink.append(source);
    sink.sleep_until_end();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    fn setup_tmp_home() -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::env::set_var("VOICEFORGE_HOME", tmp.path());
        tmp
    }

    fn make_pack(home: &Path, pack: &str, events: &[&str]) {
        let pack_dir = home.join("packs").join(pack);
        let wav_dir = pack_dir.join("wav");
        fs::create_dir_all(&wav_dir).unwrap();
        for ev in events {
            // Tiny valid WAV: write 1 sample of silence at 32k mono 16-bit.
            let path = wav_dir.join(format!("{ev}.wav"));
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 32_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::create(&path, spec).unwrap();
            writer.write_sample(0i16).unwrap();
            writer.finalize().unwrap();
        }
    }

    // ---- validate_pack_name ----

    #[test]
    fn validate_pack_name_accepts_simple() {
        validate_pack_name("peter").unwrap();
        validate_pack_name("obama_2").unwrap();
        validate_pack_name("bob-ross").unwrap();
        validate_pack_name("a").unwrap();
        validate_pack_name(&"a".repeat(64)).unwrap();
    }

    #[test]
    fn validate_pack_name_rejects_bad() {
        assert!(matches!(
            validate_pack_name(""),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_pack_name(&"a".repeat(65)),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_pack_name("Foo"),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_pack_name("peter/x"),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_pack_name(".."),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_pack_name("."),
            Err(PlayError::InvalidName(_))
        ));
        // Reject `.` so `peter.wav` fails before any FS access.
        assert!(matches!(
            validate_pack_name("peter.wav"),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_pack_name("voices"),
            Err(PlayError::InvalidName(_))
        ));
    }

    // ---- validate_event_id ----

    #[test]
    fn validate_event_id_accepts_simple() {
        validate_event_id("build_success").unwrap();
        validate_event_id("tests_passed").unwrap();
        validate_event_id("a").unwrap();
    }

    #[test]
    fn validate_event_id_rejects_bad() {
        assert!(matches!(
            validate_event_id(""),
            Err(PlayError::InvalidName(_))
        ));
        // Reject `.` so `../../etc/passwd` and `foo.bar` fail at parse.
        assert!(matches!(
            validate_event_id("../../etc/passwd"),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_event_id("foo.bar"),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_event_id("BUILD_SUCCESS"),
            Err(PlayError::InvalidName(_))
        ));
    }

    // ---- resolve_event_wav ----

    #[test]
    #[serial]
    fn resolve_event_wav_happy_path() {
        let tmp = setup_tmp_home();
        make_pack(tmp.path(), "peter", &["tests_passed"]);
        let resolved = resolve_event_wav("peter", "tests_passed").unwrap();
        assert!(resolved.is_file());
        assert!(resolved.ends_with("packs/peter/wav/tests_passed.wav"));
    }

    #[test]
    #[serial]
    fn resolve_event_wav_event_missing() {
        let tmp = setup_tmp_home();
        make_pack(tmp.path(), "peter", &["build_success"]);
        let err = resolve_event_wav("peter", "doesnt_exist").unwrap_err();
        assert!(matches!(err, PlayError::EventMissing { .. }));
        assert_eq!(err.exit_code(), 3);
    }

    #[test]
    #[serial]
    fn resolve_event_wav_pack_missing() {
        let _tmp = setup_tmp_home();
        let err = resolve_event_wav("nonexistent", "x").unwrap_err();
        assert!(matches!(err, PlayError::PackMissing(_)));
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    #[serial]
    fn resolve_event_wav_invalid_name_short_circuits() {
        let _tmp = setup_tmp_home();
        // No FS access happens — name validation fires first.
        let err = resolve_event_wav("peter", "../../etc/passwd").unwrap_err();
        assert!(matches!(err, PlayError::InvalidName(_)));
        assert_eq!(err.exit_code(), 2);
    }

    /// Cross-pack symlink leak: `peter/wav/escape.wav -> other/wav/build_success.wav`.
    /// resolve_event_wav("peter", "escape") MUST report EventMissing
    /// (per-pack wav/ prefix check), not silently leak the other pack's WAV.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn resolve_event_wav_blocks_cross_pack_symlink_leak() {
        let tmp = setup_tmp_home();
        make_pack(tmp.path(), "other", &["build_success"]);
        // Create peter pack with NO real wavs, just a symlink escape.
        let peter_wav_dir = tmp.path().join("packs/peter/wav");
        fs::create_dir_all(&peter_wav_dir).unwrap();
        let target = tmp
            .path()
            .canonicalize()
            .unwrap()
            .join("packs/other/wav/build_success.wav");
        std::os::unix::fs::symlink(&target, peter_wav_dir.join("escape.wav")).unwrap();
        let err = resolve_event_wav("peter", "escape").unwrap_err();
        // Per-pack wav/ prefix check rejects this.
        assert!(matches!(err, PlayError::EventMissing { .. }));
    }

    // ---- list_events ----

    #[test]
    #[serial]
    fn list_events_sorted() {
        let tmp = setup_tmp_home();
        make_pack(
            tmp.path(),
            "peter",
            &["tests_passed", "build_failed", "agent_done"],
        );
        let events = list_events("peter").unwrap();
        assert_eq!(events, vec!["agent_done", "build_failed", "tests_passed"]);
    }

    #[test]
    #[serial]
    fn list_events_empty_pack() {
        let tmp = setup_tmp_home();
        let pack_dir = tmp.path().join("packs/peter");
        fs::create_dir_all(&pack_dir).unwrap();
        // No wav/ subdir at all → empty list, no error.
        let events = list_events("peter").unwrap();
        assert!(events.is_empty());
    }

    #[test]
    #[serial]
    fn list_events_pack_missing() {
        let _tmp = setup_tmp_home();
        let err = list_events("nonexistent").unwrap_err();
        assert!(matches!(err, PlayError::PackMissing(_)));
    }

    // ---- exit_code ----

    #[test]
    fn exit_code_mapping() {
        assert_eq!(PlayError::InvalidName("x".into()).exit_code(), 2);
        assert_eq!(PlayError::PackMissing("x".into()).exit_code(), 2);
        assert_eq!(
            PlayError::EventMissing {
                pack: "x".into(),
                event: "y".into()
            }
            .exit_code(),
            3
        );
    }
}
