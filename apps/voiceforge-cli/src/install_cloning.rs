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

use crate::paths;

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

/// Resolve `scripts/install_cloning.sh`. Walks up from
/// `CARGO_MANIFEST_DIR` for source builds, and from `current_exe()`
/// for installed binaries.
fn resolve_script() -> Result<PathBuf> {
    if let Some(repo) = paths::repo_config_dir() {
        // repo_config_dir returns <repo>/configs; we want <repo>.
        if let Some(parent) = repo.parent() {
            let candidate = parent.join("scripts/install_cloning.sh");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    bail!(
        "could not locate scripts/install_cloning.sh.
The cloning installer is shipped with the source checkout. If you installed
via the binary release, follow the manual steps in docs (ROADMAP 2.1+1.7
will package this script with the binary)."
    )
}

/// Invoke the install script with the right env vars + stream output.
pub fn run(force: bool, check: bool, uninstall: bool) -> Result<()> {
    if [force, check, uninstall].iter().filter(|b| **b).count() > 1 {
        bail!("--force, --check, --uninstall are mutually exclusive");
    }

    let script = resolve_script()?;
    let mode = if check {
        "check"
    } else if uninstall {
        "uninstall"
    } else {
        "normal"
    };

    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .env("VOICEFORGE_INSTALL_CLONING_MODE", mode)
        .env(
            "VOICEFORGE_INSTALL_CLONING_FORCE",
            if force { "1" } else { "0" },
        )
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd
        .status()
        .with_context(|| format!("spawning {}", script.display()))?;

    if !status.success() {
        bail!("install_cloning.sh exited non-zero: {status}");
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
