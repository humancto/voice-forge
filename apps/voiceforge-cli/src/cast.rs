//! Multi-voice cast configuration (ROADMAP 4.3).
//!
//! A cast maps an event to a list of voices that should react together
//! as a short exchange. Today the LLM provider (4.1) returns a single
//! `(voice, line)` pair; with a cast configured, it returns 2-3 turns
//! played in sequence by the playback queue.
//!
//! ## File format
//!
//! Single TOML file at `~/.voiceforge/casts.toml` (or
//! `$VOICEFORGE_HOME/casts.toml`), with the repo's `configs/casts.toml`
//! as the embedded fallback. One file (a registry, not per-event
//! profiles) so the surface stays grep-able and `voiceforge doctor` can
//! preview it cleanly.
//!
//! ```toml
//! [casts.build_failed]
//! voices = ["peter", "brian"]
//! max_turns = 3
//!
//! [casts.deploy_failed]
//! voices = ["trump", "musk"]
//! ```
//!
//! ## Why `NonZeroU8` for `max_turns`
//!
//! Pushes the "0 turns" footgun to deserialize time. We can't reach
//! the cast loop with a zero cap; the validator runs before any code
//! sees the value.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::num::NonZeroU8;
use std::path::{Path, PathBuf};

/// Hard cap on cast size. Six voices is more than any human can keep
/// straight; LLMs degrade past 4. Reject larger configs at load time.
pub const MAX_CAST_VOICES: usize = 6;

/// Default `max_turns` when not specified. 3 lands in the LLM sweet
/// spot — enough for setup/punchline, not so many that the user is
/// waiting for the bit to end.
fn default_max_turns() -> NonZeroU8 {
    NonZeroU8::new(3).expect("3 != 0")
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CastConfig {
    pub voices: Vec<String>,
    #[serde(default = "default_max_turns")]
    pub max_turns: NonZeroU8,
}

#[derive(Debug, Clone, Deserialize)]
struct CastsFile {
    casts: HashMap<String, CastConfig>,
}

#[derive(Debug, Clone)]
pub struct Casts {
    by_event: HashMap<String, CastConfig>,
    #[allow(dead_code)] // used by doctor row for diagnostics
    source: Option<PathBuf>,
}

impl Casts {
    /// An empty registry — used as the default when no `casts.toml`
    /// exists on disk and no embedded fallback is wanted.
    pub fn empty() -> Self {
        Self {
            by_event: HashMap::new(),
            source: None,
        }
    }

    /// Parse a TOML string. Validates per-cast invariants:
    ///   - 1 <= voices.len() <= MAX_CAST_VOICES
    ///   - duplicate voice names are deduped (with a warning logged)
    ///   - max_turns: NonZeroU8 (enforced by serde at deserialize)
    pub fn from_str(raw: &str, source: Option<PathBuf>) -> Result<Self> {
        let parsed: CastsFile = toml::from_str(raw).context("parsing casts.toml")?;
        let mut by_event = HashMap::with_capacity(parsed.casts.len());
        for (event, mut cfg) in parsed.casts {
            if cfg.voices.is_empty() {
                bail!("cast for event {event:?} has no voices");
            }
            if cfg.voices.len() > MAX_CAST_VOICES {
                bail!(
                    "cast for event {event:?} has {} voices (cap is {MAX_CAST_VOICES})",
                    cfg.voices.len()
                );
            }
            // Dedupe + warn if duplicates collapsed.
            let original_len = cfg.voices.len();
            let mut seen = std::collections::HashSet::with_capacity(original_len);
            cfg.voices.retain(|v| seen.insert(v.clone()));
            if cfg.voices.len() != original_len {
                eprintln!(
                    "voiceforge: cast for event {event:?} had duplicate voices; deduped to {:?}",
                    cfg.voices
                );
            }
            by_event.insert(event, cfg);
        }
        Ok(Self { by_event, source })
    }

    /// Load from the standard location. Order:
    ///   1. `$VOICEFORGE_HOME/casts.toml`
    ///   2. `~/.voiceforge/casts.toml` (covered by `user_home`)
    ///   3. embedded `configs/casts.toml` (compile-time fallback)
    ///
    /// Returns `Casts::empty()` when nothing exists on disk and the
    /// embedded copy is also empty/missing.
    pub fn load() -> Result<Self> {
        if let Some(home) = crate::paths::user_home() {
            let candidate = home.join("casts.toml");
            if candidate.is_file() {
                let raw = std::fs::read_to_string(&candidate)
                    .with_context(|| format!("reading {}", candidate.display()))?;
                return Self::from_str(&raw, Some(candidate));
            }
        }
        // Repo fallback (workspace checkout).
        if let Some(repo) = crate::paths::repo_config_dir() {
            let candidate = repo.join("casts.toml");
            if candidate.is_file() {
                let raw = std::fs::read_to_string(&candidate)
                    .with_context(|| format!("reading {}", candidate.display()))?;
                return Self::from_str(&raw, Some(candidate));
            }
        }
        Ok(Self::empty())
    }

    pub fn for_event(&self, event: &str) -> Option<&CastConfig> {
        self.by_event.get(event)
    }

    pub fn is_empty(&self) -> bool {
        self.by_event.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_event.len()
    }

    /// Iterate `(event, cast)` for diagnostics — used by `voiceforge
    /// doctor` to preview the configured casts.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &CastConfig)> {
        self.by_event.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_toml() {
        let raw = r#"
[casts.build_failed]
voices = ["peter", "brian"]
"#;
        let casts = Casts::from_str(raw, None).expect("parse");
        assert_eq!(casts.len(), 1);
        let cfg = casts.for_event("build_failed").expect("cast");
        assert_eq!(cfg.voices, vec!["peter", "brian"]);
        assert_eq!(cfg.max_turns.get(), 3); // default
    }

    #[test]
    fn parses_with_max_turns() {
        let raw = r#"
[casts.deploy_failed]
voices = ["trump", "musk"]
max_turns = 4
"#;
        let casts = Casts::from_str(raw, None).expect("parse");
        let cfg = casts.for_event("deploy_failed").expect("cast");
        assert_eq!(cfg.max_turns.get(), 4);
    }

    #[test]
    fn rejects_zero_max_turns() {
        let raw = r#"
[casts.build_failed]
voices = ["peter"]
max_turns = 0
"#;
        // NonZeroU8 deserialize rejects 0.
        let err = Casts::from_str(raw, None).expect_err("must reject");
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("zero") || msg.contains("0"),
            "got: {msg}"
        );
    }

    #[test]
    fn rejects_empty_voices() {
        let raw = r#"
[casts.build_failed]
voices = []
"#;
        let err = Casts::from_str(raw, None).expect_err("must reject");
        assert!(format!("{err:#}").contains("no voices"));
    }

    #[test]
    fn rejects_voices_over_six() {
        let raw = r#"
[casts.build_failed]
voices = ["a", "b", "c", "d", "e", "f", "g"]
"#;
        let err = Casts::from_str(raw, None).expect_err("must reject");
        assert!(format!("{err:#}").contains("cap is 6"));
    }

    #[test]
    fn dedupes_duplicate_voices() {
        let raw = r#"
[casts.build_failed]
voices = ["peter", "brian", "peter"]
"#;
        let casts = Casts::from_str(raw, None).expect("parse");
        let cfg = casts.for_event("build_failed").expect("cast");
        assert_eq!(cfg.voices, vec!["peter", "brian"]);
    }

    #[test]
    fn parses_multiple_events() {
        let raw = r#"
[casts.build_failed]
voices = ["peter", "brian"]

[casts.deploy_failed]
voices = ["trump", "musk"]
"#;
        let casts = Casts::from_str(raw, None).expect("parse");
        assert_eq!(casts.len(), 2);
        assert!(casts.for_event("build_failed").is_some());
        assert!(casts.for_event("deploy_failed").is_some());
    }

    #[test]
    fn for_event_returns_none_for_unconfigured() {
        let raw = r#"[casts.build_failed]
voices = ["peter"]
"#;
        let casts = Casts::from_str(raw, None).expect("parse");
        assert!(casts.for_event("totally_unknown").is_none());
    }

    #[test]
    fn empty_registry() {
        let casts = Casts::empty();
        assert!(casts.is_empty());
        assert_eq!(casts.len(), 0);
        assert!(casts.for_event("anything").is_none());
    }

    #[test]
    fn iter_yields_all_entries() {
        let raw = r#"
[casts.a]
voices = ["x", "y"]

[casts.b]
voices = ["p", "q"]
"#;
        let casts = Casts::from_str(raw, None).expect("parse");
        let mut events: Vec<&str> = casts.iter().map(|(k, _)| k).collect();
        events.sort();
        assert_eq!(events, vec!["a", "b"]);
    }
}
