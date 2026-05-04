//! Event → voice + reaction-line rules. Loaded from
//! `configs/rules/events.json` (or `$VOICEFORGE_HOME/rules/events.json`)
//! at runtime, with an embedded copy compiled in as a last-resort
//! fallback so a freshly-installed binary still has reactions.

use anyhow::{bail, Context, Result};
use rand::seq::SliceRandom;
use rand::Rng;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::paths;

/// Embedded copy of `configs/rules/events.json`. The compile-time
/// `include_str!` makes the binary self-contained — no `configs/`
/// directory required at runtime.
const EMBEDDED_RULES: &str = include_str!("../../../configs/rules/events.json");

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct EventRule {
    pub voice: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Rules(HashMap<String, EventRule>);

impl Rules {
    /// Load + parse rules from a path. Returns an `Err` whose context
    /// chain carries the path on parse failure or empty `lines`.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading rules from {}", path.display()))?;
        Self::from_str(&raw, path)
    }

    /// Embedded defaults baked in at compile time. Used when no rules
    /// file is found on disk.
    pub fn default_builtin() -> Self {
        // The embedded file is part of our own repo, not user input —
        // a parse failure here is a build-time bug, panic loudly.
        Self::from_str(EMBEDDED_RULES, Path::new("<embedded>"))
            .expect("BUG: embedded events.json must always parse")
    }

    fn from_str(raw: &str, source: &Path) -> Result<Self> {
        let parsed: HashMap<String, EventRule> = serde_json::from_str(raw)
            .with_context(|| format!("parsing rules at {}", source.display()))?;

        for (key, rule) in &parsed {
            if rule.lines.is_empty() {
                bail!(
                    "event {key:?} in {} has no lines; refusing to load",
                    source.display()
                );
            }
            if rule.voice.trim().is_empty() {
                bail!(
                    "event {key:?} in {} has empty voice; refusing to load",
                    source.display()
                );
            }
        }

        Ok(Self(parsed))
    }

    /// Pick a `(voice, line)` for the given event. Returns `None` when
    /// the event isn't configured. Uses the caller-supplied RNG so
    /// tests can seed for reproducibility.
    pub fn pick<R: Rng + ?Sized>(&self, event: &str, rng: &mut R) -> Option<(&str, &str)> {
        let rule = self.0.get(event)?;
        let line = rule.lines.choose(rng)?;
        Some((rule.voice.as_str(), line.as_str()))
    }

    #[allow(dead_code)] // public API for callers / tests; not used internally yet
    pub fn contains(&self, event: &str) -> bool {
        self.0.contains_key(event)
    }
}

/// Resolve which rules file to load, in order:
/// 1. `$VOICEFORGE_HOME/rules/events.json`
/// 2. `~/.voiceforge/rules/events.json` (already covered by `user_home`)
/// 3. `<repo>/configs/rules/events.json` (workspace checkout fallback)
///
/// Returns `None` when nothing exists on disk — caller falls back to
/// `Rules::default_builtin()`.
pub fn resolve_rules_path() -> Option<PathBuf> {
    if let Some(home) = paths::user_home() {
        let candidate = home.join("rules/events.json");
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    if let Some(repo) = paths::repo_config_dir() {
        let candidate = repo.join("rules/events.json");
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    None
}

/// Pure choice helper for `runner.rs` — returns `(voice, line)` for
/// the requested event, falling back to the inline pair if neither the
/// loaded rules nor the embedded defaults configured that event.
/// Owned strings because `runner` doesn't keep `rules` alive across
/// the TTS call boundary.
pub fn choose_reaction<R: Rng + ?Sized>(
    rules: &Rules,
    event: &str,
    fallback: (&str, &str),
    rng: &mut R,
) -> (String, String) {
    if let Some((voice, line)) = rules.pick(event, rng) {
        return (voice.to_string(), line.to_string());
    }
    (fallback.0.to_string(), fallback.1.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    #[test]
    fn embedded_defaults_parse() {
        let rules = Rules::default_builtin();
        assert!(rules.contains("build_failed"));
        assert!(rules.contains("build_success"));
    }

    #[test]
    fn pick_returns_one_of_configured_lines() {
        let rules = Rules::default_builtin();
        let mut rng = StdRng::seed_from_u64(7);
        let (voice, line) = rules.pick("build_failed", &mut rng).expect("pick");
        assert_eq!(voice, "angry_duck");
        let allowed = [
            "The build failed again.",
            "That did not go well.",
            "The compiler has chosen violence.",
        ];
        assert!(allowed.contains(&line), "unexpected line: {line:?}");
    }

    #[test]
    fn pick_is_reproducible_with_seeded_rng() {
        let rules = Rules::default_builtin();
        let mut rng_a = StdRng::seed_from_u64(42);
        let mut rng_b = StdRng::seed_from_u64(42);
        let a = rules.pick("build_failed", &mut rng_a);
        let b = rules.pick("build_failed", &mut rng_b);
        assert_eq!(a, b);
    }

    #[test]
    fn pick_returns_none_for_unknown_event() {
        let rules = Rules::default_builtin();
        let mut rng = StdRng::seed_from_u64(0);
        assert!(rules.pick("never_configured", &mut rng).is_none());
    }

    #[test]
    fn choose_reaction_falls_back_when_event_missing() {
        let rules = Rules::default_builtin();
        let mut rng = StdRng::seed_from_u64(0);
        let (voice, line) =
            choose_reaction(&rules, "never_configured", ("default", "Done."), &mut rng);
        assert_eq!(voice, "default");
        assert_eq!(line, "Done.");
    }

    #[test]
    fn rejects_empty_lines() {
        let raw = r#"{"empty_event": {"voice": "x", "lines": []}}"#;
        let err = Rules::from_str(raw, Path::new("test")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no lines"), "unexpected error: {msg}");
    }

    #[test]
    fn rejects_empty_voice() {
        let raw = r#"{"e": {"voice": "  ", "lines": ["a"]}}"#;
        let err = Rules::from_str(raw, Path::new("test")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("empty voice"), "unexpected error: {msg}");
    }

    #[test]
    fn parse_error_carries_path_in_context() {
        let raw = "not json at all {{{";
        let err = Rules::from_str(raw, Path::new("/tmp/fake/events.json")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("/tmp/fake/events.json"), "missing path: {msg}");
    }
}
