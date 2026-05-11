//! `voiceforge install-cloning` — wraps `scripts/install_cloning.sh`.
//!
//! The bash script is the source of truth for the install recipe; this
//! module just sets the right env vars, finds the script, streams its
//! output, and parses the resulting `~/.voiceforge/cloning/INSTALLED.toml`
//! marker for `voiceforge doctor`.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::install_ui::{self, InstallEngine};
use crate::{branding, paths};

const MARKER_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct InstallState {
    pub schema_version: u32,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub installed_at: String,
    pub gpt_sovits_sha: String,
    pub python_path: String,
    pub ffmpeg6_prefix: String,
    #[serde(default)]
    pub venv_path: String,
    #[serde(default)]
    pub repo_path: String,
    #[serde(default, rename = "model_sha256")]
    pub model_sha256: HashMap<String, String>,
}

/// Resolve `<voiceforge_home>/cloning/INSTALLED.toml`.
pub fn marker_path() -> Option<PathBuf> {
    paths::user_home().map(|h| h.join("cloning/INSTALLED.toml"))
}

/// `<voiceforge_home>/cloning/venv/bin/python` — the cloning runtime
/// interpreter. `None` only when home itself is unresolvable.
#[allow(dead_code)] // wired in commit 4 of this PR (tts.rs Engine::Cloning)
pub fn cloning_venv_python() -> Option<PathBuf> {
    paths::user_home().map(|h| h.join("cloning/venv/bin/python"))
}

/// Path to the GPT-SoVITS clone managed by install-cloning.sh.
#[allow(dead_code)]
pub fn cloning_repo_dir() -> Option<PathBuf> {
    paths::user_home().map(|h| h.join("cloning/repo"))
}

/// Path to the voice-forge-shipped synth worker. The repo's
/// `scripts/cloning_synth.py` is loaded by the cloning venv's python.
#[allow(dead_code)]
pub fn cloning_synth_script() -> Option<PathBuf> {
    if let Some(repo_configs) = paths::repo_config_dir() {
        if let Some(repo) = repo_configs.parent() {
            let candidate = repo.join("scripts/cloning_synth.py");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// `true` when the marker exists and parses with the expected schema.
/// Public API for callers (e.g. future `clone` subcommand) who only
/// want a yes/no.
#[allow(dead_code)]
pub fn is_installed() -> bool {
    read_install_state().is_ok()
}

/// Parse the marker, validating `schema_version`.
pub fn read_install_state() -> Result<InstallState> {
    let path = marker_path().ok_or_else(|| anyhow!("could not resolve $VOICEFORGE_HOME"))?;
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let state: InstallState =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if state.schema_version != MARKER_SCHEMA_VERSION {
        bail!(
            "INSTALLED.toml schema_version={} but this voiceforge expects {} — re-run install-cloning --force",
            state.schema_version,
            MARKER_SCHEMA_VERSION
        );
    }
    Ok(state)
}

/// Resolve a `scripts/<name>` file. Walks up from `CARGO_MANIFEST_DIR`
/// for source builds, and from `current_exe()` for installed binaries.
/// Used to find both `install_cloning.sh` (v1) and `install_cloning_fish.sh` (v2).
fn resolve_script_named(name: &str) -> Result<PathBuf> {
    if let Some(repo) = paths::repo_config_dir() {
        // repo_config_dir returns <repo>/configs; we want <repo>.
        if let Some(parent) = repo.parent() {
            let candidate = parent.join("scripts").join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    bail!(
        "could not locate scripts/{name}.
The cloning installer is shipped with the source checkout. If you installed
via the binary release, the embedded copy at apps/voiceforge-cli/src/embedded_install.rs
must be extracted first (PR-AB step 6c will wire that up automatically)."
    )
}

/// Invoke the install script with the right env vars + stream output.
///
/// Engine selection: defaults to fish-speech S2 Pro (v2). Set
/// `VOICEFORGE_INSTALL_CLONING_ENGINE=gpt-sovits-v2` to use the legacy
/// v1 path (existing schema-1 voices keep working).
///
/// When the engine is v2 AND we're attached to a TTY AND color is on
/// (per `branding::use_color()`), the install runs through the
/// indicatif wizard. Otherwise we fall back to plain stdio inheritance
/// — same behavior as the shipped v1 path. The wizard is cosmetic; the
/// install recipe must work identically without it.
///
/// Branding header (full or compact banner) prints at the very start
/// of a normal install. Suppressed for `--check` and `--uninstall`
/// because those are noisy + fast and the banner would be in the way.
pub fn run(force: bool, check: bool, uninstall: bool) -> Result<()> {
    if [force, check, uninstall].iter().filter(|b| **b).count() > 1 {
        bail!("--force, --check, --uninstall are mutually exclusive");
    }

    let engine = install_ui::engine_from_env()?;
    let script = resolve_script_named(engine.script_filename())?;
    let mode = if check {
        "check"
    } else if uninstall {
        "uninstall"
    } else {
        "normal"
    };

    if mode == "normal" {
        branding::print_brand_header();
    }

    let use_wizard =
        engine == InstallEngine::FishSpeechS2Pro && mode == "normal" && branding::use_color();

    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .env("VOICEFORGE_INSTALL_CLONING_MODE", mode)
        .env(
            "VOICEFORGE_INSTALL_CLONING_FORCE",
            if force { "1" } else { "0" },
        );

    let status = if use_wizard {
        install_ui::run_with_wizard(&mut cmd, engine.approx_phase_count(), engine.human_title())
            .with_context(|| format!("running install wizard for {}", script.display()))?
    } else {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        cmd.status()
            .with_context(|| format!("spawning {}", script.display()))?
    };

    if !status.success() {
        bail!("{} exited non-zero: {status}", engine.script_filename());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn write_marker(home: &std::path::Path, body: &str) {
        let dir = home.join("cloning");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("INSTALLED.toml"), body).unwrap();
    }

    fn with_home<F: FnOnce()>(home: &std::path::Path, f: F) {
        let prev = std::env::var("VOICEFORGE_HOME").ok();
        std::env::set_var("VOICEFORGE_HOME", home);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match prev {
            Some(v) => std::env::set_var("VOICEFORGE_HOME", v),
            None => std::env::remove_var("VOICEFORGE_HOME"),
        }
        if let Err(p) = result {
            std::panic::resume_unwind(p);
        }
    }

    #[test]
    #[serial]
    fn is_installed_false_when_marker_absent() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            assert!(!is_installed(), "no marker should mean not installed");
        });
    }

    #[test]
    #[serial]
    fn is_installed_true_when_marker_present_and_schema_matches() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            write_marker(
                tmp.path(),
                r#"
schema_version = 1
version = "0.1.0"
installed_at = "2026-05-04T00:00:00Z"
gpt_sovits_sha = "08d627c3"
python_path = "/opt/homebrew/bin/python3.11"
ffmpeg6_prefix = "/opt/homebrew/opt/ffmpeg@6"
venv_path = "/whatever"
repo_path = "/whatever"

[model_sha256]
s2G2333k = "abc"
                "#,
            );
            assert!(is_installed());
        });
    }

    #[test]
    #[serial]
    fn is_installed_false_when_schema_version_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            write_marker(
                tmp.path(),
                r#"
schema_version = 99
gpt_sovits_sha = "x"
python_path = "y"
ffmpeg6_prefix = "z"
                "#,
            );
            assert!(
                !is_installed(),
                "future schema should not register as installed"
            );
        });
    }

    #[test]
    #[serial]
    fn read_install_state_parses_real_toml() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            write_marker(
                tmp.path(),
                r#"
schema_version = 1
version = "0.1.0"
installed_at = "2026-05-04T00:00:00Z"
gpt_sovits_sha = "08d627c3"
python_path = "/opt/homebrew/bin/python3.11"
ffmpeg6_prefix = "/opt/homebrew/opt/ffmpeg@6"
venv_path = "/v"
repo_path = "/r"

[model_sha256]
s2G2333k = "924fdcca"
s1bert25hz = "732f94e6"
chinese_hubert_base = "24164f12"
                "#,
            );
            let st = read_install_state().expect("parse");
            assert_eq!(st.schema_version, 1);
            assert_eq!(st.gpt_sovits_sha, "08d627c3");
            assert_eq!(st.model_sha256.get("s2G2333k").unwrap(), "924fdcca");
            assert_eq!(st.model_sha256.len(), 3);
        });
    }

    #[test]
    fn run_rejects_conflicting_flags() {
        let err = run(true, true, false).unwrap_err();
        assert!(format!("{err:#}").contains("mutually exclusive"));
    }
}
