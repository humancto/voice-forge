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

const MARKER_SCHEMA_VERSION_V1: u32 = 1;
const MARKER_SCHEMA_VERSION_V2: u32 = 2;

/// Schema-1 marker (GPT-SoVITS / v1 installer). Kept intact for the
/// legacy CloningEngine in tts.rs and the `clone` subcommand which both
/// targeted the v1 runtime. Adding fields here is backward-compat;
/// removing or renaming fields breaks installs in the wild.
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

/// Schema-2 marker (fish-speech S2 Pro / v2 installer). Format mirrors
/// the TOML written by `scripts/install_cloning_fish.sh` step 14.
/// Distinct struct (not a superset) so the borrow-vs-rename trade-off
/// is explicit: future v2 schema bumps don't risk breaking v1 readers.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct InstallStateV2 {
    pub schema_version: u32,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub installed_at: String,
    /// Always `"fish-speech-s2-pro"` for v2 today; the field exists so
    /// the marker can carry alternate engines (s1-mini, etc.) later.
    #[serde(default)]
    pub engine: String,
    pub fish_speech_sha: String,
    pub python_path: String,
    pub ffmpeg6_prefix: String,
    #[serde(default)]
    pub venv_path: String,
    #[serde(default)]
    pub repo_path: String,
    #[serde(default)]
    pub checkpoint_path: String,
    #[serde(default)]
    pub whisper_model: String,
    #[serde(default, rename = "model_sha256")]
    pub model_sha256: HashMap<String, String>,
}

/// Schema-discriminated union. Returned by `read_install_state_any()`
/// so the doctor command and any other read-side caller can dispatch
/// on the schema instead of guessing.
#[derive(Debug, Clone, PartialEq)]
pub enum InstallStateAny {
    V1(InstallState),
    V2(InstallStateV2),
}

impl InstallStateAny {
    /// Schema version the marker self-reports. Useful for logging
    /// without forcing the caller to match.
    #[allow(dead_code)]
    pub fn schema_version(&self) -> u32 {
        match self {
            Self::V1(_) => MARKER_SCHEMA_VERSION_V1,
            Self::V2(_) => MARKER_SCHEMA_VERSION_V2,
        }
    }

    /// Engine label suitable for `voiceforge doctor` output.
    #[allow(dead_code)]
    pub fn engine_label(&self) -> &'static str {
        match self {
            Self::V1(_) => "GPT-SoVITS v2 (legacy)",
            Self::V2(_) => "fish-speech S2 Pro",
        }
    }
}

/// Resolve `<voiceforge_home>/cloning/INSTALLED.toml`.
pub fn marker_path() -> Option<PathBuf> {
    paths::user_home().map(|h| h.join("cloning/INSTALLED.toml"))
}

/// Resolve `<voiceforge_home>/cloning/INSTALLED.v1.bak`. Written by
/// `scripts/install_cloning_fish.sh` when it detects a schema-1 marker
/// before overwriting it with v2. Powers the doctor command's "you
/// upgraded from GPT-SoVITS — old voices need migration" hint.
pub fn v1_backup_path() -> Option<PathBuf> {
    paths::user_home().map(|h| h.join("cloning/INSTALLED.v1.bak"))
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

/// `true` when a schema-1 (GPT-SoVITS) install is present. Kept as the
/// canonical "is the legacy cloning runtime ready?" check. Used by
/// `clone.rs` and the legacy CloningEngine in tts.rs.
#[allow(dead_code)]
pub fn is_installed() -> bool {
    read_install_state().is_ok()
}

/// `true` when a schema-2 (fish-speech S2 Pro) install is present.
/// Used by FishEngine + the fish-speech-routed `voiceforge clone` /
/// `voiceforge note` paths (PR-AB step 8 wires these in).
#[allow(dead_code)]
pub fn is_installed_v2() -> bool {
    read_install_state_v2().is_ok()
}

/// `true` when *any* schema (V1 or V2) is present. The doctor command
/// uses this to decide whether to render the cloning row at all.
#[allow(dead_code)]
pub fn is_installed_any() -> bool {
    read_install_state_any().is_ok()
}

/// Probe just the `schema_version` field so the dispatcher can pick
/// the right struct without serde tripping on missing-field errors.
#[derive(Deserialize)]
struct SchemaProbe {
    schema_version: u32,
}

fn peek_schema_version(raw: &str, path: &std::path::Path) -> Result<u32> {
    let probe: SchemaProbe = toml::from_str(raw)
        .with_context(|| format!("reading schema_version from {}", path.display()))?;
    Ok(probe.schema_version)
}

/// Parse the marker, validating that it's a schema-1 install. Returns
/// an error with a migrate hint when the on-disk marker is schema-2,
/// so a v1-only call site (CloningEngine) doesn't silently misinterpret
/// fish-speech state as GPT-SoVITS state.
pub fn read_install_state() -> Result<InstallState> {
    let path = marker_path().ok_or_else(|| anyhow!("could not resolve $VOICEFORGE_HOME"))?;
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let schema = peek_schema_version(&raw, &path)?;
    if schema != MARKER_SCHEMA_VERSION_V1 {
        bail!(
            "INSTALLED.toml schema_version={schema} but this caller expects v{} (GPT-SoVITS).
The install at {} is a different schema. If you want the legacy GPT-SoVITS
clone path, run `voiceforge install-cloning --engine gpt-sovits-v2 --force`.
Otherwise the v2 fish-speech runtime is wired in via FishEngine (PR-AB step 8).",
            MARKER_SCHEMA_VERSION_V1,
            path.display()
        );
    }
    let state: InstallState =
        toml::from_str(&raw).with_context(|| format!("parsing {} as schema-1", path.display()))?;
    Ok(state)
}

/// Parse the marker as a schema-2 (fish-speech) install. Mirrors
/// `read_install_state` but for the v2 path.
#[allow(dead_code)]
pub fn read_install_state_v2() -> Result<InstallStateV2> {
    let path = marker_path().ok_or_else(|| anyhow!("could not resolve $VOICEFORGE_HOME"))?;
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let schema = peek_schema_version(&raw, &path)?;
    if schema != MARKER_SCHEMA_VERSION_V2 {
        bail!(
            "INSTALLED.toml schema_version={schema} but this caller expects v{} (fish-speech S2 Pro).
The install at {} is the legacy GPT-SoVITS engine. Run
`voiceforge install-cloning --force` to upgrade (your v1 marker will be
backed up to INSTALLED.v1.bak so any GPT-SoVITS voices stay diagnosable).",
            MARKER_SCHEMA_VERSION_V2,
            path.display()
        );
    }
    let state: InstallStateV2 =
        toml::from_str(&raw).with_context(|| format!("parsing {} as schema-2", path.display()))?;
    Ok(state)
}

/// Parse the marker without committing to a schema up front. Peeks at
/// `schema_version` then dispatches to the right struct. Schema versions
/// other than {1,2} bail loud.
#[allow(dead_code)]
pub fn read_install_state_any() -> Result<InstallStateAny> {
    let path = marker_path().ok_or_else(|| anyhow!("could not resolve $VOICEFORGE_HOME"))?;
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let schema = peek_schema_version(&raw, &path)?;
    match schema {
        MARKER_SCHEMA_VERSION_V1 => {
            let v1: InstallState = toml::from_str(&raw)
                .with_context(|| format!("parsing {} as schema-1", path.display()))?;
            Ok(InstallStateAny::V1(v1))
        }
        MARKER_SCHEMA_VERSION_V2 => {
            let v2: InstallStateV2 = toml::from_str(&raw)
                .with_context(|| format!("parsing {} as schema-2", path.display()))?;
            Ok(InstallStateAny::V2(v2))
        }
        other => bail!(
            "INSTALLED.toml schema_version={other} is not supported (this build knows {} and {}).
Either downgrade voiceforge or re-run `voiceforge install-cloning --force`.",
            MARKER_SCHEMA_VERSION_V1,
            MARKER_SCHEMA_VERSION_V2
        ),
    }
}

/// Read the raw v1 backup TOML, if present. Returns `None` when no
/// backup exists. Returned as a String (not parsed) because the
/// migration-hint render only needs the existence + a SHA-prefix peek;
/// we don't enforce it parses cleanly (a corrupted backup must NOT
/// block the v2 install from being usable).
#[allow(dead_code)]
pub fn read_v1_backup_raw() -> Option<String> {
    let path = v1_backup_path()?;
    std::fs::read_to_string(&path).ok()
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

    // ========================================================================
    // Schema-2 marker reader (PR-AB step 6c)
    // ========================================================================

    fn write_v2_marker(home: &std::path::Path, body: &str) {
        let dir = home.join("cloning");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("INSTALLED.toml"), body).unwrap();
    }

    fn write_v1_backup(home: &std::path::Path, body: &str) {
        let dir = home.join("cloning");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("INSTALLED.v1.bak"), body).unwrap();
    }

    const V2_MARKER_BODY: &str = r#"
schema_version = 2
version = "0.4.0"
installed_at = "2026-05-11T00:00:00Z"
engine = "fish-speech-s2-pro"
fish_speech_sha = "3dd1f85c402ee6f0a17c2971d3b0dd8d881ca139"
python_path = "/opt/homebrew/bin/python3.11"
ffmpeg6_prefix = "/opt/homebrew/opt/ffmpeg@6"
venv_path = "/x/venv"
repo_path = "/x/repo"
checkpoint_path = "/x/repo/checkpoints/s2-pro"
whisper_model = "medium"

[model_sha256]
codec_pth = "74fc41c5"
"#;

    #[test]
    #[serial]
    fn read_install_state_v2_parses_real_v2_toml() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            write_v2_marker(tmp.path(), V2_MARKER_BODY);
            let st = read_install_state_v2().expect("parse v2");
            assert_eq!(st.schema_version, 2);
            assert_eq!(st.engine, "fish-speech-s2-pro");
            assert_eq!(
                st.fish_speech_sha,
                "3dd1f85c402ee6f0a17c2971d3b0dd8d881ca139"
            );
            assert_eq!(st.whisper_model, "medium");
            assert_eq!(st.model_sha256.get("codec_pth").unwrap(), "74fc41c5");
        });
    }

    #[test]
    #[serial]
    fn read_install_state_v2_rejects_v1_marker_with_helpful_error() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            write_marker(
                tmp.path(),
                r#"
schema_version = 1
gpt_sovits_sha = "08d627c3"
python_path = "/p"
ffmpeg6_prefix = "/f"
"#,
            );
            // The v1 marker is missing fish_speech_sha + the schema check
            // wins; either way the error must mention v2.
            let err = read_install_state_v2().unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("fish-speech") || msg.contains("schema"),
                "v2 reader on v1 marker must explain why; got: {msg}"
            );
        });
    }

    #[test]
    #[serial]
    fn read_install_state_v1_rejects_v2_marker_with_migrate_hint() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            write_v2_marker(tmp.path(), V2_MARKER_BODY);
            let err = read_install_state().unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("gpt-sovits-v2") || msg.contains("legacy"),
                "v1 reader on v2 marker must hint at the legacy engine; got: {msg}"
            );
        });
    }

    #[test]
    #[serial]
    fn read_install_state_any_dispatches_v1() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            write_marker(
                tmp.path(),
                r#"
schema_version = 1
version = "0.1.0"
installed_at = "2026-05-04T00:00:00Z"
gpt_sovits_sha = "08d627c3"
python_path = "/p"
ffmpeg6_prefix = "/f"
venv_path = "/v"
repo_path = "/r"
"#,
            );
            let st = read_install_state_any().expect("parse any");
            assert_eq!(st.schema_version(), 1);
            assert_eq!(st.engine_label(), "GPT-SoVITS v2 (legacy)");
            match st {
                InstallStateAny::V1(v1) => assert_eq!(v1.gpt_sovits_sha, "08d627c3"),
                InstallStateAny::V2(_) => panic!("expected V1 variant"),
            }
        });
    }

    #[test]
    #[serial]
    fn read_install_state_any_dispatches_v2() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            write_v2_marker(tmp.path(), V2_MARKER_BODY);
            let st = read_install_state_any().expect("parse any");
            assert_eq!(st.schema_version(), 2);
            assert_eq!(st.engine_label(), "fish-speech S2 Pro");
            match st {
                InstallStateAny::V2(v2) => assert_eq!(v2.engine, "fish-speech-s2-pro"),
                InstallStateAny::V1(_) => panic!("expected V2 variant"),
            }
        });
    }

    #[test]
    #[serial]
    fn read_install_state_any_rejects_unknown_schema() {
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
            let err = read_install_state_any().unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("99") && msg.contains("not supported"),
                "unknown-schema error must name the bad version; got: {msg}"
            );
        });
    }

    #[test]
    #[serial]
    fn is_installed_v2_false_when_only_v1_marker_present() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            write_marker(
                tmp.path(),
                r#"
schema_version = 1
gpt_sovits_sha = "x"
python_path = "y"
ffmpeg6_prefix = "z"
"#,
            );
            assert!(is_installed(), "v1 marker should mean is_installed = true");
            assert!(
                !is_installed_v2(),
                "v1 marker must NOT register as v2 installed"
            );
            assert!(is_installed_any(), "any-schema check should pass");
        });
    }

    #[test]
    #[serial]
    fn is_installed_v2_true_when_v2_marker_present() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            write_v2_marker(tmp.path(), V2_MARKER_BODY);
            assert!(is_installed_v2());
            assert!(
                !is_installed(),
                "v2 marker must NOT register as v1 installed (legacy CloningEngine
                must not silently misinterpret fish-speech state)"
            );
            assert!(is_installed_any());
        });
    }

    #[test]
    #[serial]
    fn read_v1_backup_raw_returns_some_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            write_v1_backup(
                tmp.path(),
                "schema_version = 1\ngpt_sovits_sha = \"old-install-sha\"\n",
            );
            let raw = read_v1_backup_raw().expect("v1 backup should be readable");
            assert!(raw.contains("schema_version = 1"));
            assert!(raw.contains("old-install-sha"));
        });
    }

    #[test]
    #[serial]
    fn read_v1_backup_raw_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            assert!(read_v1_backup_raw().is_none());
        });
    }

    #[test]
    #[serial]
    fn v1_backup_path_under_voiceforge_home() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let p = v1_backup_path().expect("path");
            assert!(p.starts_with(tmp.path()), "{}", p.display());
            assert!(p.ends_with("INSTALLED.v1.bak"));
        });
    }
}
