//! Voice profile storage at `~/.voiceforge/voices/<name>/`.
//!
//! Each cloned voice is a directory containing:
//!   profile.toml         schema, source, recipe, created_at
//!   ref_main.wav + .txt  the main reference clip (10 s mono 32 kHz)
//!   aux_1..5.wav + .txt  five auxiliary references for tone fusion
//!
//! Loading a voice asserts every referenced file exists, the recipe is
//! known, and the schema matches.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::paths;

pub const VOICE_SCHEMA_VERSION: u32 = 1;
pub const KNOWN_RECIPES: &[&str] = &["gpt-sovits-v2-multi-aux-ref"];
pub const RESERVED_NAMES: &[&str] = &[
    "presets",
    "cache",
    "cloning",
    "voices",
    "embeddings",
    "logs",
];

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct VoiceProfile {
    pub schema_version: u32,
    pub name: String,
    pub source: String,
    /// Required: feeds the cloning cache key. Defaulting it would make
    /// every clone of a missing-created_at voice collide on the same
    /// cache entry, breaking `--force` invalidation.
    pub created_at: String,
    pub duration_seconds: f64,
    pub recipe: String,
    pub aux_count: usize,
    #[serde(skip)]
    pub dir: PathBuf,
    #[serde(skip)]
    pub ref_main_wav: PathBuf,
    #[serde(skip)]
    pub ref_main_txt: PathBuf,
    #[serde(skip)]
    pub aux_wavs: Vec<PathBuf>,
    #[serde(skip)]
    pub aux_txts: Vec<PathBuf>,
}

pub fn voices_dir() -> Option<PathBuf> {
    paths::user_home().map(|h| h.join("voices"))
}

/// Resolve `voices_dir/<name>` and assert the canonicalized path stays
/// under `voices_dir` (defense against symlink escape + path traversal
/// in `name`).
pub fn voice_dir(name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    let root = voices_dir().ok_or_else(|| anyhow!("could not resolve voices dir"))?;
    let candidate = root.join(name);
    Ok(candidate)
}

pub fn voice_exists(name: &str) -> bool {
    if validate_name(name).is_err() {
        return false;
    }
    let Ok(dir) = voice_dir(name) else {
        return false;
    };
    dir.join("profile.toml").is_file()
}

/// Reject names that could path-traverse, collide with reserved sibling
/// dirs under `~/.voiceforge/`, or contain shell-unsafe characters.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("voice name cannot be empty");
    }
    if name.len() > 32 {
        bail!("voice name too long (max 32 chars): {name:?}");
    }
    if name == "." || name == ".." {
        bail!("voice name cannot be '.' or '..'");
    }
    if RESERVED_NAMES.contains(&name) {
        bail!("voice name {name:?} collides with a reserved ~/.voiceforge/ subdir");
    }
    for c in name.chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
        if !ok {
            bail!("voice name {name:?} has invalid char {c:?}; allowed: [a-z0-9_-], 1..=32");
        }
    }
    Ok(())
}

pub fn load_voice(name: &str) -> Result<VoiceProfile> {
    let dir = voice_dir(name)?;
    if !dir.is_dir() {
        bail!("voice {name:?} not found at {}", dir.display());
    }

    // Defense-in-depth: canonicalize and assert under voices_dir.
    let canonical = dir
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", dir.display()))?;
    let voices_root = voices_dir()
        .ok_or_else(|| anyhow!("voices dir unresolved"))?
        .canonicalize()
        .with_context(|| "canonicalizing voices dir")?;
    if !canonical.starts_with(&voices_root) {
        bail!(
            "voice dir {} escapes {} (symlink?); refusing to load",
            canonical.display(),
            voices_root.display()
        );
    }

    let profile_path = canonical.join("profile.toml");
    let raw = std::fs::read_to_string(&profile_path)
        .with_context(|| format!("reading {}", profile_path.display()))?;

    let mut profile: VoiceProfile =
        toml::from_str(&raw).with_context(|| format!("parsing {}", profile_path.display()))?;

    if profile.schema_version != VOICE_SCHEMA_VERSION {
        bail!(
            "voice {name:?} profile.toml schema_version={} but this voiceforge expects {} — re-clone",
            profile.schema_version,
            VOICE_SCHEMA_VERSION
        );
    }
    if !KNOWN_RECIPES.contains(&profile.recipe.as_str()) {
        bail!(
            "voice {name:?} uses unknown recipe {:?}; known: {:?}",
            profile.recipe,
            KNOWN_RECIPES
        );
    }

    profile.dir = canonical.clone();
    profile.ref_main_wav = canonical.join("ref_main.wav");
    profile.ref_main_txt = canonical.join("ref_main.txt");
    profile.aux_wavs = (1..=profile.aux_count)
        .map(|i| canonical.join(format!("aux_{i}.wav")))
        .collect();
    profile.aux_txts = (1..=profile.aux_count)
        .map(|i| canonical.join(format!("aux_{i}.txt")))
        .collect();

    assert_path_exists(&profile.ref_main_wav, "ref_main.wav")?;
    assert_path_exists(&profile.ref_main_txt, "ref_main.txt")?;
    for (i, p) in profile.aux_wavs.iter().enumerate() {
        assert_path_exists(p, &format!("aux_{}.wav", i + 1))?;
    }
    for (i, p) in profile.aux_txts.iter().enumerate() {
        assert_path_exists(p, &format!("aux_{}.txt", i + 1))?;
    }

    Ok(profile)
}

fn assert_path_exists(p: &Path, label: &str) -> Result<()> {
    if !p.is_file() {
        bail!("voice profile is missing {} at {}", label, p.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn with_home<F: FnOnce(&Path)>(home: &Path, f: F) {
        let prev = std::env::var("VOICEFORGE_HOME").ok();
        std::env::set_var("VOICEFORGE_HOME", home);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(home)));
        match prev {
            Some(v) => std::env::set_var("VOICEFORGE_HOME", v),
            None => std::env::remove_var("VOICEFORGE_HOME"),
        }
        if let Err(p) = result {
            std::panic::resume_unwind(p);
        }
    }

    fn write_full_profile(home: &Path, name: &str, recipe: &str) {
        let dir = home.join("voices").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let toml = format!(
            r#"
schema_version = 1
name = "{name}"
source = "/tmp/x.wav"
created_at = "2026-05-04T00:00:00Z"
duration_seconds = 60.0
recipe = "{recipe}"
aux_count = 5
"#
        );
        std::fs::write(dir.join("profile.toml"), toml).unwrap();
        std::fs::write(dir.join("ref_main.wav"), b"RIFF\0\0\0\0WAVE").unwrap();
        std::fs::write(dir.join("ref_main.txt"), b"main").unwrap();
        for i in 1..=5 {
            std::fs::write(dir.join(format!("aux_{i}.wav")), b"RIFF\0\0\0\0WAVE").unwrap();
            std::fs::write(dir.join(format!("aux_{i}.txt")), format!("aux_{i}")).unwrap();
        }
    }

    #[test]
    fn validate_name_accepts_simple() {
        validate_name("peter").unwrap();
        validate_name("trump_2024").unwrap();
        validate_name("a-b-c").unwrap();
        validate_name("x").unwrap();
    }

    #[test]
    fn validate_name_rejects_empty_or_too_long() {
        assert!(validate_name("").is_err());
        assert!(validate_name(&"a".repeat(33)).is_err());
    }

    #[test]
    fn validate_name_rejects_dot_and_traversal() {
        assert!(validate_name(".").is_err());
        assert!(validate_name("..").is_err());
        // contains slash → invalid char
        assert!(validate_name("../etc").is_err());
        assert!(validate_name("peter/etc").is_err());
        assert!(validate_name("peter\\etc").is_err());
    }

    #[test]
    fn validate_name_rejects_reserved() {
        for r in RESERVED_NAMES {
            assert!(validate_name(r).is_err(), "should reject {r}");
        }
    }

    #[test]
    fn validate_name_rejects_uppercase_and_special() {
        assert!(validate_name("Peter").is_err());
        assert!(validate_name("peter griffin").is_err());
        assert!(validate_name("peter@home").is_err());
        assert!(validate_name("../foo").is_err());
    }

    #[test]
    #[serial]
    fn voice_exists_false_when_dir_absent() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |_| {
            assert!(!voice_exists("peter"));
        });
    }

    #[test]
    #[serial]
    fn voice_exists_true_when_profile_present() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            assert!(voice_exists("peter"));
        });
    }

    #[test]
    #[serial]
    fn load_voice_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            let v = load_voice("peter").unwrap();
            assert_eq!(v.name, "peter");
            assert_eq!(v.recipe, "gpt-sovits-v2-multi-aux-ref");
            assert_eq!(v.aux_count, 5);
            assert_eq!(v.aux_wavs.len(), 5);
            assert!(v.ref_main_wav.is_file());
        });
    }

    #[test]
    #[serial]
    fn load_voice_errors_on_missing_aux() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "gpt-sovits-v2-multi-aux-ref");
            std::fs::remove_file(home.join("voices/peter/aux_3.wav")).unwrap();
            let err = load_voice("peter").unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("aux_3.wav"), "got: {msg}");
        });
    }

    #[test]
    #[serial]
    fn load_voice_errors_on_unknown_recipe() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            write_full_profile(home, "peter", "future-bigger-better-recipe");
            let err = load_voice("peter").unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("unknown recipe"), "got: {msg}");
        });
    }

    #[test]
    #[serial]
    fn load_voice_errors_on_schema_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            let dir = home.join("voices/peter");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("profile.toml"),
                br#"
schema_version = 99
name = "peter"
source = "x"
created_at = "2026-05-04T00:00:00Z"
duration_seconds = 60.0
recipe = "gpt-sovits-v2-multi-aux-ref"
aux_count = 5
"#,
            )
            .unwrap();
            let err = load_voice("peter").unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("schema_version"), "got: {msg}");
        });
    }

    #[test]
    #[serial]
    fn load_voice_errors_on_missing_created_at() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            let dir = home.join("voices/peter");
            std::fs::create_dir_all(&dir).unwrap();
            // No created_at — required field; cache key would otherwise
            // collide with every other no-created_at clone.
            std::fs::write(
                dir.join("profile.toml"),
                br#"
schema_version = 1
name = "peter"
source = "x"
duration_seconds = 60.0
recipe = "gpt-sovits-v2-multi-aux-ref"
aux_count = 5
"#,
            )
            .unwrap();
            let err = load_voice("peter").unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("created_at") || msg.contains("missing field"),
                "expected missing-field error mentioning created_at, got: {msg}"
            );
        });
    }

    #[test]
    #[serial]
    fn load_voice_blocks_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |home| {
            // Stage a real voice profile *outside* voices_dir.
            let outside = home.join("evil");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("marker"), b"x").unwrap();

            // Create voices_dir + a symlink in it pointing outside.
            let voices = home.join("voices");
            std::fs::create_dir_all(&voices).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&outside, voices.join("escaper")).unwrap();

            // load_voice should refuse — symlink target is outside voices_dir.
            let err = load_voice("escaper").unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("escapes") || msg.contains("not found") || msg.contains("missing"),
                "expected escape/not-found error, got: {msg}"
            );
        });
    }
}
