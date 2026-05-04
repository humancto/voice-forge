//! First-run bootstrap of the user's `~/.voiceforge/` layout.
//!
//! Idempotent: every subdir + the default config.toml is created with
//! `OpenOptions::create_new(true)` so two parallel `voiceforge`
//! invocations can race here without truncating each other's writes.
//! Existing user files are never overwritten.

use anyhow::{anyhow, bail, Context, Result};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::config::EMBEDDED_PRESETS;
use crate::paths;

/// Default `config.toml` shipped on first run. Compiled in.
const EMBEDDED_CONFIG_TOML: &str = "\
# ~/.voiceforge/config.toml
# Edit this file to change the default voice that `voiceforge say`
# and `voiceforge run` use when no --voice flag is passed.
active_voice = \"default\"
";

const SUBDIRS: &[&str] = &["presets", "cache", "voices", "embeddings", "logs"];

#[must_use = "BootstrapReport carries the first-run signal — drop it on the floor and the user never sees the banner"]
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BootstrapReport {
    pub home: PathBuf,
    pub created_home: bool,
    pub created_dirs: Vec<&'static str>,
    pub copied_presets: Vec<&'static str>,
    pub wrote_default_config: bool,
}

impl BootstrapReport {
    pub fn is_first_run(&self) -> bool {
        self.created_home
    }
}

/// Create the canonical `~/.voiceforge/` layout if missing. Safe to
/// call on every invocation; existing files are preserved.
pub fn ensure_voiceforge_home() -> Result<BootstrapReport> {
    let home =
        paths::user_home().ok_or_else(|| anyhow!("could not resolve $VOICEFORGE_HOME or $HOME"))?;

    if home.exists() && !home.is_dir() {
        bail!(
            "{} exists but is not a directory — refusing to clobber",
            home.display()
        );
    }

    let created_home = !home.exists();
    fs::create_dir_all(&home).with_context(|| format!("could not create {}", home.display()))?;

    let mut report = BootstrapReport {
        home: home.clone(),
        created_home,
        ..Default::default()
    };

    for sub in SUBDIRS {
        let path = home.join(sub);
        if path.exists() {
            if !path.is_dir() {
                bail!(
                    "{} exists but is not a directory — refusing to clobber",
                    path.display()
                );
            }
            continue;
        }
        fs::create_dir_all(&path)
            .with_context(|| format!("could not create {}", path.display()))?;
        report.created_dirs.push(sub);
    }

    let presets_dir = home.join("presets");
    for (id, content) in EMBEDDED_PRESETS {
        let preset_path = presets_dir.join(format!("{id}.json"));
        match write_if_absent(&preset_path, content.as_bytes()) {
            Ok(true) => report.copied_presets.push(id),
            Ok(false) => {} // user-customized or already there
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("could not write embedded preset {}", preset_path.display())
                });
            }
        }
    }

    let config_path = home.join("config.toml");
    match write_if_absent(&config_path, EMBEDDED_CONFIG_TOML.as_bytes()) {
        Ok(true) => report.wrote_default_config = true,
        Ok(false) => {}
        Err(e) => {
            return Err(e).with_context(|| format!("could not write {}", config_path.display()));
        }
    }

    Ok(report)
}

/// Write `bytes` to `path` only if no file exists there. Returns
/// `true` when we wrote, `false` when the file was already present.
/// Uses `O_CREAT | O_EXCL` semantics so two racing processes can't
/// truncate each other.
fn write_if_absent(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => {
            f.write_all(bytes)?;
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// Print a one-time banner to stderr if this invocation actually did
/// the first-run setup. No-op otherwise.
pub fn print_if_first_run(report: &BootstrapReport) {
    if !report.is_first_run() {
        return;
    }
    eprintln!("voiceforge: initialized {}", report.home.display());
    eprintln!(
        "presets: {} installed (edit {}/presets/ to customize)",
        report.copied_presets.len(),
        report.home.display()
    );
    eprintln!("config:  {}/config.toml", report.home.display());
    eprintln!("next:    voiceforge say --text \"VoiceForge is ready\"");
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
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    /// Guards against a future PR removing the `default` preset
    /// without updating `EMBEDDED_CONFIG_TOML`'s `active_voice`. If
    /// the embedded config ever points at a non-existent preset,
    /// fresh installs would silently default to a missing voice.
    #[test]
    fn embedded_config_active_voice_exists_in_embedded_presets() {
        let active = "default";
        assert!(
            EMBEDDED_CONFIG_TOML.contains(&format!("active_voice = \"{active}\"")),
            "embedded config.toml should default to active_voice = {active:?}"
        );
        assert!(
            EMBEDDED_PRESETS.iter().any(|(id, _)| *id == active),
            "embedded preset {active:?} must exist or fresh installs break"
        );
    }

    #[test]
    #[serial]
    fn creates_layout_in_empty_home() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("voiceforge");

        with_home(&home, |home| {
            let report = ensure_voiceforge_home().expect("bootstrap");

            assert!(report.is_first_run());
            assert!(home.is_dir());
            for sub in SUBDIRS {
                assert!(
                    home.join(sub).is_dir(),
                    "subdir {sub} should exist after bootstrap"
                );
            }
            assert_eq!(
                report.copied_presets.len(),
                EMBEDDED_PRESETS.len(),
                "every embedded preset should be copied"
            );
            assert!(report.wrote_default_config);
            assert!(home.join("config.toml").is_file());
            assert!(home.join("presets/default.json").is_file());
        });
    }

    #[test]
    #[serial]
    fn is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("voiceforge");

        with_home(&home, |_| {
            let first = ensure_voiceforge_home().expect("first");
            let second = ensure_voiceforge_home().expect("second");

            assert!(first.is_first_run());
            assert!(!second.is_first_run());
            assert!(second.copied_presets.is_empty());
            assert!(!second.wrote_default_config);
            assert!(second.created_dirs.is_empty());
        });
    }

    #[test]
    #[serial]
    fn preserves_user_customized_preset() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("voiceforge");
        fs::create_dir_all(home.join("presets")).unwrap();
        let custom = br#"{"id": "default", "display_name": "MY CUSTOM EDIT"}"#;
        fs::write(home.join("presets/default.json"), custom).unwrap();

        with_home(&home, |home| {
            let report = ensure_voiceforge_home().expect("bootstrap");
            let on_disk = fs::read(home.join("presets/default.json")).unwrap();
            assert_eq!(on_disk, custom, "user edit must survive bootstrap");
            assert!(
                !report.copied_presets.contains(&"default"),
                "default should NOT be in copied_presets"
            );
        });
    }

    #[test]
    #[serial]
    fn preserves_user_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("voiceforge");
        fs::create_dir_all(&home).unwrap();
        let custom = b"# hand-edited\nactive_voice = \"angry_duck\"\n";
        fs::write(home.join("config.toml"), custom).unwrap();

        with_home(&home, |home| {
            let report = ensure_voiceforge_home().expect("bootstrap");
            let on_disk = fs::read(home.join("config.toml")).unwrap();
            assert_eq!(on_disk, custom);
            assert!(!report.wrote_default_config);
        });
    }

    #[test]
    #[serial]
    fn creates_only_missing_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("voiceforge");
        fs::create_dir_all(home.join("cache")).unwrap();

        with_home(&home, |_| {
            let report = ensure_voiceforge_home().expect("bootstrap");
            assert!(!report.created_dirs.contains(&"cache"));
            // The other four should still be created.
            for sub in &["presets", "voices", "embeddings", "logs"] {
                assert!(
                    report.created_dirs.contains(sub),
                    "{sub} should appear in created_dirs"
                );
            }
        });
    }

    #[test]
    #[serial]
    fn rejects_non_dir_home() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home_path = tmp.path().join("voiceforge_is_a_file");
        fs::write(&home_path, b"not a dir").unwrap();

        with_home(&home_path, |_| {
            let err = ensure_voiceforge_home().unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("not a directory"),
                "expected non-directory error, got: {msg}"
            );
        });
    }

    #[test]
    #[serial]
    fn parallel_bootstrap_is_race_safe() {
        // Two threads racing on the same VOICEFORGE_HOME — both should
        // succeed, no truncated files, exactly one observes
        // is_first_run() depending on who wins the OS-level
        // create_dir race.
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("voiceforge");

        with_home(&home, |home| {
            std::thread::scope(|scope| {
                let h1 = scope.spawn(ensure_voiceforge_home);
                let h2 = scope.spawn(ensure_voiceforge_home);
                let r1 = h1.join().unwrap().expect("thread 1");
                let r2 = h2.join().unwrap().expect("thread 2");
                // Both must succeed; banner-printing semantics are
                // best-effort under contention (banner can fire 0, 1,
                // or 2 times — accept).
                let _ = (r1, r2);
            });

            // After the race, layout is intact and presets are
            // untruncated.
            for (id, content) in EMBEDDED_PRESETS {
                let p = home.join("presets").join(format!("{id}.json"));
                let on_disk = fs::read(&p).unwrap();
                assert_eq!(
                    on_disk,
                    content.as_bytes(),
                    "preset {id} must not be truncated by the race"
                );
            }
            let cfg = fs::read(home.join("config.toml")).unwrap();
            assert_eq!(cfg, EMBEDDED_CONFIG_TOML.as_bytes());
        });
    }
}
