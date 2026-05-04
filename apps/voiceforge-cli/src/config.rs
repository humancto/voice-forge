use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::paths;

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
const EMBEDDED_PRESETS: &[(&str, &str)] = &[
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
}
