//! Embedded install assets (ROADMAP v0.4 PR-AB step 4).
//!
//! Bundles `scripts/install_cloning_fish.sh` and the smoke test
//! fixture directly into the compiled `voiceforge` binary via
//! `include_str!` / `include_bytes!`. Without this, brew + curl
//! installs of the binary have no sibling `scripts/` dir, and
//! `voiceforge install-cloning` always dies with "could not locate
//! scripts/install_cloning.sh" — the onboarding-audit's S1 finding.
//!
//! On invocation we extract the script to
//! `~/.voiceforge/cloning/.install/install_cloning_fish.sh` (mode
//! 0700) and shell out to it. Smoke fixture extracts the same way
//! to `~/.voiceforge/cloning/.install/smoke_reference_8s.{wav,txt}`.
//!
//! Both extracts are idempotent — re-running install reuses the
//! files. The `.install/` subdirectory is .gitignore'd at the user
//! HOME level so it's not accidentally committed in dev setups.
//!
//! `#[allow(dead_code)]` is applied at the module level until PR-AB
//! step 6 wires `install_cloning::run()` through `extract_all()`.

#![allow(dead_code)]

use anyhow::{Context, Result};
use std::path::PathBuf;

/// The fish-speech install script. Content is filled in by PR-AB
/// step 6 (which writes `scripts/install_cloning_fish.sh`); for now
/// this is a placeholder that errors loud if invoked. The constant
/// itself is real and used by tests + the extraction helper today
/// so the wiring works end-to-end before step 6 lands.
pub const INSTALL_CLONING_FISH_SH: &str = include_str!("../../../scripts/install_cloning_fish.sh");

/// 8-second public-domain LibriVox reference clip used by the
/// post-install smoke test. mono 32 kHz, loudnorm'd to -16 LUFS.
/// Filled in by PR-AB step 6.
pub const SMOKE_REFERENCE_WAV: &[u8] =
    include_bytes!("../../../tests/fixtures/smoke_reference_8s.wav");

/// Transcript of the smoke reference clip. Used by fish-speech as
/// the reference text for the smoke synth call.
pub const SMOKE_REFERENCE_TXT: &str =
    include_str!("../../../tests/fixtures/smoke_reference_8s.txt");

/// Where embedded assets land after extraction. Subdir of
/// `~/.voiceforge/cloning/` so a `voiceforge install-cloning --uninstall`
/// cleans it as part of the existing pruning.
pub fn extract_dir() -> Result<PathBuf> {
    let home = crate::paths::user_home().context("could not resolve $VOICEFORGE_HOME or $HOME")?;
    Ok(home.join("cloning").join(".install"))
}

/// Extract every embedded asset to `extract_dir()`. Mode 0700 on
/// the directory + 0755 on the script + 0644 on the fixtures.
///
/// Idempotent: existing files with matching sha256 are left alone.
/// Mismatched sha256 triggers an overwrite (e.g., user upgraded
/// voiceforge and the embedded script changed).
pub fn extract_all() -> Result<ExtractedAssets> {
    use std::io::Write;

    let dir = extract_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }

    let script_path = dir.join("install_cloning_fish.sh");
    let smoke_wav_path = dir.join("smoke_reference_8s.wav");
    let smoke_txt_path = dir.join("smoke_reference_8s.txt");

    write_if_changed(&script_path, INSTALL_CLONING_FISH_SH.as_bytes(), 0o755)?;
    write_if_changed(&smoke_wav_path, SMOKE_REFERENCE_WAV, 0o644)?;
    write_if_changed(&smoke_txt_path, SMOKE_REFERENCE_TXT.as_bytes(), 0o644)?;

    let _ = std::io::stdout().flush();
    Ok(ExtractedAssets {
        script_path,
        smoke_wav_path,
        smoke_txt_path,
    })
}

/// Result of `extract_all`. Caller uses the script_path to shell
/// out + smoke_*_path to feed the post-install smoke test.
#[derive(Debug, Clone)]
pub struct ExtractedAssets {
    pub script_path: PathBuf,
    pub smoke_wav_path: PathBuf,
    pub smoke_txt_path: PathBuf,
}

/// Atomic-rename write that skips when the existing file matches.
/// Sha256-checks before overwrite so re-running install is a fast
/// no-op when content hasn't changed.
fn write_if_changed(path: &std::path::Path, content: &[u8], mode: u32) -> Result<()> {
    if let Ok(existing) = std::fs::read(path) {
        if sha256_eq(&existing, content) {
            // Verify mode is also correct; fix if not.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let meta = std::fs::metadata(path)?;
                if meta.permissions().mode() & 0o777 != mode {
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
                }
            }
            return Ok(());
        }
    }

    let parent = path
        .parent()
        .with_context(|| format!("path has no parent: {}", path.display()))?;
    let tmp = parent.join(format!(
        ".{}.partial.{}",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, content).with_context(|| format!("writing temp {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    let _ = mode; // suppress unused on non-unix
    Ok(())
}

fn sha256_eq(a: &[u8], b: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    if a.len() != b.len() {
        return false;
    }
    let mut ha = Sha256::new();
    ha.update(a);
    let mut hb = Sha256::new();
    hb.update(b);
    ha.finalize() == hb.finalize()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    fn with_tmp_home<F: FnOnce()>(f: F) {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("VOICEFORGE_HOME").ok();
        std::env::set_var("VOICEFORGE_HOME", tmp.path());
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
    fn embedded_constants_meet_minimum_size_thresholds() {
        // include_str!/include_bytes! resolve at compile time; if
        // the source files don't exist, the build fails before this
        // test runs. We instead assert minimum sane sizes so an
        // accidental truncation (e.g. someone replaces the install
        // script with a one-line stub) trips the test.
        const MIN_SCRIPT_BYTES: usize = 200; // even the placeholder is >300 bytes
        const MIN_WAV_BYTES: usize = 32_000; // ~0.5s @ 32 kHz mono 16-bit
        const MIN_TXT_BYTES: usize = 10;
        assert!(
            INSTALL_CLONING_FISH_SH.len() >= MIN_SCRIPT_BYTES,
            "install script too small: {} bytes (min {MIN_SCRIPT_BYTES})",
            INSTALL_CLONING_FISH_SH.len()
        );
        assert!(
            SMOKE_REFERENCE_WAV.len() >= MIN_WAV_BYTES,
            "smoke wav too small: {} bytes (min {MIN_WAV_BYTES})",
            SMOKE_REFERENCE_WAV.len()
        );
        assert!(
            SMOKE_REFERENCE_TXT.len() >= MIN_TXT_BYTES,
            "smoke txt too small: {} bytes (min {MIN_TXT_BYTES})",
            SMOKE_REFERENCE_TXT.len()
        );
        // RIFF header check on the embedded WAV — if someone replaces
        // the placeholder with a non-WAV file, this fails loudly.
        assert_eq!(
            &SMOKE_REFERENCE_WAV[..4],
            b"RIFF",
            "smoke wav embed lacks RIFF header"
        );
        assert_eq!(
            &SMOKE_REFERENCE_WAV[8..12],
            b"WAVE",
            "smoke wav embed lacks WAVE marker"
        );
    }

    #[test]
    #[serial]
    fn extract_all_writes_three_files() {
        with_tmp_home(|| {
            let assets = extract_all().expect("extract");
            assert!(assets.script_path.is_file());
            assert!(assets.smoke_wav_path.is_file());
            assert!(assets.smoke_txt_path.is_file());
        });
    }

    #[test]
    #[serial]
    fn extract_all_is_idempotent() {
        with_tmp_home(|| {
            let a1 = extract_all().expect("first extract");
            let m1 = std::fs::metadata(&a1.script_path).unwrap();
            // Second extract on same content must not rewrite.
            let a2 = extract_all().expect("second extract");
            let m2 = std::fs::metadata(&a2.script_path).unwrap();
            // mtime equality is the weakest check; sha equality is
            // implied by write_if_changed's contract.
            assert_eq!(
                m1.modified().ok(),
                m2.modified().ok(),
                "idempotent re-extract changed mtime"
            );
        });
    }

    #[test]
    #[serial]
    fn extract_all_sets_script_executable() {
        with_tmp_home(|| {
            let assets = extract_all().expect("extract");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let meta = std::fs::metadata(&assets.script_path).unwrap();
                let mode = meta.permissions().mode() & 0o777;
                assert_eq!(mode, 0o755, "script mode = {:o}, want 0755", mode);
            }
        });
    }

    #[test]
    #[serial]
    fn extract_all_overwrites_when_content_differs() {
        with_tmp_home(|| {
            let assets = extract_all().expect("first extract");
            // Corrupt the script file
            std::fs::write(&assets.script_path, b"corrupted").unwrap();
            // Re-extract MUST overwrite back to the embedded content
            let _ = extract_all().expect("re-extract");
            let actual = std::fs::read(&assets.script_path).unwrap();
            assert_eq!(
                actual,
                INSTALL_CLONING_FISH_SH.as_bytes(),
                "re-extract did not overwrite corrupted file"
            );
        });
    }

    #[test]
    fn extract_dir_is_under_voiceforge_home() {
        // Sanity: the directory we extract into lives under the
        // voiceforge home, not /tmp or PWD. Important for the
        // uninstaller (it cleans ~/.voiceforge/cloning/).
        let prev = std::env::var("VOICEFORGE_HOME").ok();
        std::env::set_var("VOICEFORGE_HOME", "/tmp/voiceforge_home_audit_test");
        let dir = extract_dir().expect("extract_dir");
        assert!(
            dir.starts_with("/tmp/voiceforge_home_audit_test"),
            "extract_dir = {} not under VOICEFORGE_HOME",
            dir.display()
        );
        assert!(dir.ends_with(".install"), "must end with .install/");
        match prev {
            Some(v) => std::env::set_var("VOICEFORGE_HOME", v),
            None => std::env::remove_var("VOICEFORGE_HOME"),
        }
    }
}
