use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::paths;
use crate::voices;

/// Default voice when nothing else is configured.
pub const DEFAULT_VOICE: &str = "default";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VoicePreset {
    pub id: String,
    pub display_name: Option<String>,
    pub reference_wav: Option<String>,
    pub embedding_path: Option<String>,
    pub temperature: Option<f32>,
    pub speed: Option<f32>,
    pub language: Option<String>,
}

/// Embedded copies of every preset bundled in `configs/presets/`.
/// `include_str!` is greppable and keeps the build script free; switch
/// to `build.rs` enumeration only if/when the count grows past ~15.
pub(crate) const EMBEDDED_PRESETS: &[(&str, &str)] = &[
    (
        "default",
        include_str!("../../../configs/presets/default.json"),
    ),
    (
        "angry_duck",
        include_str!("../../../configs/presets/angry_duck.json"),
    ),
    (
        "hype_narrator",
        include_str!("../../../configs/presets/hype_narrator.json"),
    ),
    (
        "sarcastic_goblin",
        include_str!("../../../configs/presets/sarcastic_goblin.json"),
    ),
    (
        "tiny_robot",
        include_str!("../../../configs/presets/tiny_robot.json"),
    ),
];

/// Resolve which presets directory to load from, in order:
///   1. `$VOICEFORGE_HOME/presets/`
///   2. `<repo>/configs/presets/` (workspace checkout fallback)
///
/// Returns `None` when neither exists on disk — caller falls back to
/// `embedded_presets()`.
pub fn resolve_presets_dir() -> Option<PathBuf> {
    if let Some(home) = paths::user_home() {
        let candidate = home.join("presets");
        if candidate.is_dir() {
            return Some(candidate);
        }
    }

    if let Some(repo) = paths::repo_config_dir() {
        let candidate = repo.join("presets");
        if candidate.is_dir() {
            return Some(candidate);
        }
    }

    None
}

/// Embedded presets, parsed from the compiled-in JSON. Panics if the
/// embedded JSON ever fails to parse — that's a build-time bug.
pub fn embedded_presets() -> Vec<VoicePreset> {
    EMBEDDED_PRESETS
        .iter()
        .map(|(id, raw)| {
            serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("BUG: embedded preset {id:?} must parse: {e}"))
        })
        .collect()
}

/// Load all presets. Tries the resolved on-disk dir first; if that
/// dir is missing, empty, or every file fails to parse, falls back to
/// the embedded defaults so a freshly-installed binary always has
/// voices.
pub fn load_presets() -> Result<Vec<VoicePreset>> {
    let Some(dir) = resolve_presets_dir() else {
        return Ok(embedded_presets());
    };

    let from_disk = load_presets_from(&dir)?;
    if from_disk.is_empty() {
        // On-disk dir exists but contains zero usable presets — treat
        // the same as missing dir and fall back. (Matches the rules
        // resolver, which also falls back on parse failure.)
        return Ok(embedded_presets());
    }
    Ok(from_disk)
}

fn load_presets_from(dir: &Path) -> Result<Vec<VoicePreset>> {
    let mut presets = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    for entry in fs::read_dir(dir)
        .with_context(|| format!("could not read presets directory {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("reading preset {}", path.display()))?;

        let preset: VoicePreset = serde_json::from_str(&raw)
            .with_context(|| format!("parsing preset {}", path.display()))?;

        if preset.id.trim().is_empty() {
            bail!(
                "preset in {} has empty id; refusing to load",
                path.display()
            );
        }

        if !seen_ids.insert(preset.id.clone()) {
            bail!(
                "duplicate preset id {:?} found in {}; refusing to load (silent last-write-wins is a footgun)",
                preset.id,
                dir.display()
            );
        }

        presets.push(preset);
    }

    Ok(presets)
}

// ---- active-voice in ~/.voiceforge/config.toml ----------------------

/// Resolve the user's `~/.voiceforge/config.toml` path.
pub fn config_toml_path() -> Option<PathBuf> {
    paths::user_home().map(|h| h.join("config.toml"))
}

/// Read `active_voice` from `~/.voiceforge/config.toml`. Returns
/// `DEFAULT_VOICE` on missing file, missing key, parse failure, or a
/// value that fails `voices::validate_name` (defends against a
/// hand-edited config with `active_voice = "../etc"`).
pub fn read_active_voice() -> String {
    let Some(path) = config_toml_path() else {
        return DEFAULT_VOICE.to_string();
    };
    let Ok(raw) = fs::read_to_string(&path) else {
        return DEFAULT_VOICE.to_string();
    };
    let Ok(doc) = raw.parse::<toml_edit::DocumentMut>() else {
        return DEFAULT_VOICE.to_string();
    };
    let Some(val) = doc.get("active_voice").and_then(|i| i.as_str()) else {
        return DEFAULT_VOICE.to_string();
    };
    if voices::validate_name(val).is_err() {
        return DEFAULT_VOICE.to_string();
    }
    val.to_string()
}

/// Write `active_voice = "<name>"` to `~/.voiceforge/config.toml`,
/// preserving any other keys + comments via `toml_edit`. Atomic via
/// tmp+rename. Caller is responsible for validating that the voice
/// exists; this fn only validates the name shape.
pub fn write_active_voice(name: &str) -> Result<()> {
    voices::validate_name(name)?;

    let path =
        config_toml_path().ok_or_else(|| anyhow::anyhow!("could not resolve config.toml path"))?;

    // Parse-edit-write rather than overwriting; preserves comments +
    // any other top-level keys (e.g. future [tts] table).
    let raw = fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing {} as TOML", path.display()))?;
    doc["active_voice"] = toml_edit::value(name);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    {
        let mut f =
            fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(doc.to_string().as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
    }
    fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn with_voiceforge_home<F: FnOnce()>(home: &Path, f: F) {
        let prev = std::env::var("VOICEFORGE_HOME").ok();
        std::env::set_var("VOICEFORGE_HOME", home);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match prev {
            Some(v) => std::env::set_var("VOICEFORGE_HOME", v),
            None => std::env::remove_var("VOICEFORGE_HOME"),
        }
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn embedded_presets_parse_and_include_default() {
        let presets = embedded_presets();
        assert!(presets.iter().any(|p| p.id == "default"));
        assert!(presets.iter().any(|p| p.id == "angry_duck"));
        assert_eq!(presets.len(), 5);
    }

    #[test]
    #[serial]
    fn loads_from_voiceforge_home_when_dir_populated() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("presets");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("custom.json"),
            r#"{"id": "custom_voice", "display_name": "Custom"}"#,
        )
        .unwrap();

        with_voiceforge_home(tmp.path(), || {
            let presets = load_presets().expect("load");
            assert_eq!(presets.len(), 1, "on-disk should win entirely (no union)");
            assert_eq!(presets[0].id, "custom_voice");
            // No-union assertion: embedded IDs must NOT appear.
            assert!(!presets.iter().any(|p| p.id == "default"));
            assert!(!presets.iter().any(|p| p.id == "angry_duck"));
        });
    }

    #[test]
    #[serial]
    fn falls_back_to_embedded_when_dir_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Note: do NOT create presets/ subdir.
        with_voiceforge_home(tmp.path(), || {
            let presets = load_presets().expect("load");
            assert_eq!(presets.len(), 5);
        });
    }

    #[test]
    #[serial]
    fn falls_back_to_embedded_when_dir_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(tmp.path().join("presets")).unwrap();

        with_voiceforge_home(tmp.path(), || {
            let presets = load_presets().expect("load");
            assert_eq!(
                presets.len(),
                5,
                "empty dir should fall back to embedded, matching the rules resolver"
            );
        });
    }

    #[test]
    #[serial]
    fn skips_non_json_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("presets");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.txt"), "some random notes").unwrap();
        fs::write(dir.join("real.json"), r#"{"id": "real_one"}"#).unwrap();

        with_voiceforge_home(tmp.path(), || {
            let presets = load_presets().expect("load");
            assert_eq!(presets.len(), 1);
            assert_eq!(presets[0].id, "real_one");
        });
    }

    #[test]
    #[serial]
    fn rejects_malformed_preset_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("presets");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("bad.json"), "not json {{{").unwrap();

        with_voiceforge_home(tmp.path(), || {
            let err = load_presets().unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("bad.json"), "missing path in error: {msg}");
        });
    }

    #[test]
    #[serial]
    fn rejects_duplicate_ids() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("presets");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.json"), r#"{"id": "twin"}"#).unwrap();
        fs::write(dir.join("b.json"), r#"{"id": "twin"}"#).unwrap();

        with_voiceforge_home(tmp.path(), || {
            let err = load_presets().unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("duplicate"),
                "expected duplicate error, got: {msg}"
            );
            assert!(msg.contains("twin"), "missing id in error: {msg}");
        });
    }

    #[test]
    #[serial]
    fn rejects_empty_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("presets");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("empty.json"), r#"{"id": "  "}"#).unwrap();

        with_voiceforge_home(tmp.path(), || {
            let err = load_presets().unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("empty id"),
                "expected empty-id error, got: {msg}"
            );
        });
    }

    // -- active-voice tests -------------------------------------------

    #[test]
    #[serial]
    fn read_active_voice_returns_default_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        with_voiceforge_home(tmp.path(), || {
            assert_eq!(read_active_voice(), DEFAULT_VOICE);
        });
    }

    #[test]
    #[serial]
    fn read_active_voice_returns_default_on_corrupted_name() {
        let tmp = tempfile::tempdir().unwrap();
        with_voiceforge_home(tmp.path(), || {
            std::fs::write(
                tmp.path().join("config.toml"),
                br#"active_voice = "../etc/passwd""#,
            )
            .unwrap();
            // corrupted config → fall back to DEFAULT, never propagate
            assert_eq!(read_active_voice(), DEFAULT_VOICE);
        });
    }

    #[test]
    #[serial]
    fn write_and_read_active_voice_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        with_voiceforge_home(tmp.path(), || {
            write_active_voice("peter").unwrap();
            assert_eq!(read_active_voice(), "peter");
        });
    }

    #[test]
    #[serial]
    fn write_active_voice_preserves_other_keys_and_comments() {
        let tmp = tempfile::tempdir().unwrap();
        with_voiceforge_home(tmp.path(), || {
            std::fs::write(
                tmp.path().join("config.toml"),
                b"# Hand-edited config\nactive_voice = \"default\"\nother_key = \"keep_me\"\n",
            )
            .unwrap();
            write_active_voice("peter").unwrap();
            let raw = std::fs::read_to_string(tmp.path().join("config.toml")).unwrap();
            assert!(
                raw.contains("# Hand-edited config"),
                "comment dropped: {raw}"
            );
            assert!(raw.contains("other_key"), "other_key dropped: {raw}");
            assert!(raw.contains("\"peter\""), "active_voice not updated: {raw}");
        });
    }

    #[test]
    #[serial]
    fn write_active_voice_rejects_invalid_name() {
        let tmp = tempfile::tempdir().unwrap();
        with_voiceforge_home(tmp.path(), || {
            let err = write_active_voice("../etc").unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("invalid char"), "got: {msg}");
        });
    }
}
