//! `voiceforge clone <name> <source> [--force]` — wraps
//! `scripts/clone_voice.sh`.

use anyhow::{anyhow, bail, Context, Result};
use std::process::{Command, Stdio};

use crate::install_cloning;
use crate::paths;
use crate::voices;

/// Spawn the clone-voice bash recipe and stream its output.
pub fn run(name: String, source: String, force: bool) -> Result<()> {
    if !install_cloning::is_installed() {
        bail!(
            "cloning is not installed yet — run `voiceforge install-cloning` first.\n\
             It downloads the GPT-SoVITS v2 stack (~1.7 GB) and is idempotent."
        );
    }

    voices::validate_name(&name)?;

    if voices::voice_exists(&name) && !force {
        bail!(
            "voice {name:?} already exists; pass --force to replace it.\n\
             Existing dir: {}",
            voices::voice_dir(&name)?.display()
        );
    }

    let script = resolve_clone_script()?;

    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .arg(&name)
        .arg(&source)
        .arg(if force { "1" } else { "0" })
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd
        .status()
        .with_context(|| format!("spawning {}", script.display()))?;

    if !status.success() {
        bail!("clone_voice.sh exited non-zero: {status}");
    }
    Ok(())
}

/// Find `scripts/clone_voice.sh` next to the running binary or in the
/// repo root (when running via `cargo run`).
fn resolve_clone_script() -> Result<std::path::PathBuf> {
    if let Some(repo_configs) = paths::repo_config_dir() {
        if let Some(repo) = repo_configs.parent() {
            let candidate = repo.join("scripts/clone_voice.sh");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(anyhow!(
        "could not locate scripts/clone_voice.sh.\n\
         The clone recipe ships with the source checkout. If you installed\n\
         via the binary release, follow the manual steps in docs (binary\n\
         packaging is the next ROADMAP item after 1.7)."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::path::Path;

    fn with_home<F: FnOnce()>(home: &Path, f: F) {
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
    fn run_errors_when_install_marker_absent() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let err = run("peter".into(), "/some/source".into(), false).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("install-cloning"),
                "expected install hint, got: {msg}"
            );
        });
    }

    #[test]
    #[serial]
    fn run_errors_on_invalid_name() {
        let tmp = tempfile::tempdir().unwrap();
        // Stage a fake INSTALLED.toml so we get past the install check
        // and exercise validate_name.
        std::fs::create_dir_all(tmp.path().join("cloning")).unwrap();
        std::fs::write(
            tmp.path().join("cloning/INSTALLED.toml"),
            r#"
schema_version = 1
gpt_sovits_sha = "x"
python_path = "/x"
ffmpeg6_prefix = "/x"
"#,
        )
        .unwrap();
        with_home(tmp.path(), || {
            let err = run("../etc".into(), "/x".into(), false).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("invalid char"),
                "expected name validation error, got: {msg}"
            );
        });
    }

    #[test]
    #[serial]
    fn run_errors_on_existing_voice_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("cloning")).unwrap();
        std::fs::write(
            tmp.path().join("cloning/INSTALLED.toml"),
            r#"
schema_version = 1
gpt_sovits_sha = "x"
python_path = "/x"
ffmpeg6_prefix = "/x"
"#,
        )
        .unwrap();
        let voice_dir = tmp.path().join("voices/peter");
        std::fs::create_dir_all(&voice_dir).unwrap();
        std::fs::write(
            voice_dir.join("profile.toml"),
            r#"
schema_version = 1
name = "peter"
source = "x"
duration_seconds = 60.0
recipe = "gpt-sovits-v2-multi-aux-ref"
aux_count = 5
"#,
        )
        .unwrap();
        with_home(tmp.path(), || {
            let err = run("peter".into(), "/some/source".into(), false).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("already exists") && msg.contains("--force"),
                "expected exists+force error, got: {msg}"
            );
        });
    }
}
