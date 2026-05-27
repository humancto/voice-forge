//! Embedded runtime assets — install scripts, clone scripts, synth
//! workers, and the post-install smoke fixture — bundled directly
//! into the `voiceforge` binary via `include_bytes!` / `include_str!`.
//!
//! Without this, brew + curl installs of the binary have no sibling
//! `scripts/` dir, and three user-facing commands die immediately:
//!   - `voiceforge install-cloning` (cannot find install_cloning_*.sh)
//!   - `voiceforge clone <name> <src>` (cannot find clone_voice_*.sh)
//!   - `voiceforge say --voice <cloned-voice>` (cannot find the
//!     {fish_speech_synth,cloning_synth}.py worker)
//!
//! `resolve_runtime_script(name)` is the single resolver:
//!
//! 1. If we're running from a source checkout (the test workspace, or
//!    `cargo run` from the repo), prefer `<repo>/scripts/<name>` so
//!    edits to scripts/foo.sh take effect without a rebuild.
//! 2. Otherwise extract the embedded copy to
//!    `~/.voiceforge/cloning/.install/<name>` (mode 0700 on the dir,
//!    0755 on .sh/.py, 0644 on fixtures) and return the extracted
//!    path. Idempotent via sha256-skip in `write_if_changed`.
//!
//! Sha256s of the embedded payloads are pinned at build time by
//! `build.rs` (see `$OUT_DIR/embedded_sha.rs`). Source of truth is
//! the file content on disk; the generated constants exist only so
//! the runtime can sanity-check the payload it bundled.
//!
//! NOTE: `include_bytes!("../../../scripts/foo.sh")` reaches outside
//! the crate root. The voiceforge-cli crate is workspace-only and
//! ships via GitHub releases / Homebrew; it is NOT published to
//! crates.io. If that ever changes, move scripts/ under
//! `apps/voiceforge-cli/scripts/` or use `[package] include = [...]`
//! before publishing.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

// Build-time sha256s. See build.rs.
include!(concat!(env!("OUT_DIR"), "/embedded_sha.rs"));

/// Each embedded runtime script is described by one row of this
/// table. Order doesn't matter functionally; keep alphabetical for
/// reviewability.
pub struct EmbeddedScript {
    pub name: &'static str,
    pub bytes: &'static [u8],
    pub mode: u32,
    /// Build-time sha256 of `bytes` (pinned by build.rs). Asserted
    /// against the runtime byte digest in `embedded_sha_matches_payload`.
    #[allow(dead_code)] // read by tests; lookup-time integrity check is optional.
    pub sha256_hex: &'static str,
}

/// Every script the released binary needs at runtime. Kept in
/// lockstep with build.rs::SCRIPTS — if you add an entry here, add
/// the matching (file_name, const_suffix) pair there and rebuild.
pub const EMBEDDED_RUNTIME_SCRIPTS: &[EmbeddedScript] = &[
    EmbeddedScript {
        name: "clone_voice.sh",
        bytes: include_bytes!("../../../scripts/clone_voice.sh"),
        mode: 0o755,
        sha256_hex: SHA_CLONE_VOICE_SH,
    },
    EmbeddedScript {
        name: "clone_voice_fish.sh",
        bytes: include_bytes!("../../../scripts/clone_voice_fish.sh"),
        mode: 0o755,
        sha256_hex: SHA_CLONE_VOICE_FISH_SH,
    },
    EmbeddedScript {
        name: "cloning_synth.py",
        bytes: include_bytes!("../../../scripts/cloning_synth.py"),
        mode: 0o755,
        sha256_hex: SHA_CLONING_SYNTH_PY,
    },
    EmbeddedScript {
        name: "fish_speech_synth.py",
        bytes: include_bytes!("../../../scripts/fish_speech_synth.py"),
        mode: 0o755,
        sha256_hex: SHA_FISH_SPEECH_SYNTH_PY,
    },
    EmbeddedScript {
        name: "install_cloning.sh",
        bytes: include_bytes!("../../../scripts/install_cloning.sh"),
        mode: 0o755,
        sha256_hex: SHA_INSTALL_CLONING_SH,
    },
    EmbeddedScript {
        name: "install_cloning_fish.sh",
        bytes: include_bytes!("../../../scripts/install_cloning_fish.sh"),
        mode: 0o755,
        sha256_hex: SHA_INSTALL_CLONING_FISH_SH,
    },
];

/// Compat shim — tests pull the fish install script via this
/// constant. Identical payload to the registry entry. New code
/// should use `resolve_runtime_script("install_cloning_fish.sh")`.
#[allow(dead_code)]
pub const INSTALL_CLONING_FISH_SH: &str = include_str!("../../../scripts/install_cloning_fish.sh");

/// 8-second public-domain LibriVox reference clip used by the
/// post-install smoke test. mono 32 kHz, loudnorm'd to -16 LUFS.
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

/// The single resolver for runtime scripts (install / clone / synth).
///
/// Resolution order:
///   1. **Source checkout** — if `paths::repo_config_dir()` finds a
///      `configs/` ancestor and that ancestor has a `scripts/<name>`
///      file, return that path. Lets `cargo run` / `cargo test` users
///      edit `scripts/foo.sh` and re-run without a rebuild.
///   2. **Embedded extraction** — look `name` up in
///      `EMBEDDED_RUNTIME_SCRIPTS`, extract via `write_if_changed`
///      into `extract_dir()`, return the extracted path.
///
/// Returns an error only when the name is not in the registry AND
/// not on disk in a source checkout. That should be a packaging bug
/// (someone added a `scripts/foo.sh` caller without updating the
/// registry + build.rs).
pub fn resolve_runtime_script(name: &str) -> Result<PathBuf> {
    resolve_runtime_script_with_repo_lookup(name, crate::paths::repo_config_dir)
}

/// Test seam: lets the unit tests inject a `|| None` repo-lookup to
/// simulate a release-binary install (no source checkout reachable).
/// The public entry point is `resolve_runtime_script`.
pub fn resolve_runtime_script_with_repo_lookup<F: FnOnce() -> Option<PathBuf>>(
    name: &str,
    repo_lookup: F,
) -> Result<PathBuf> {
    // 1. Source-checkout precedence (devs editing scripts/foo.sh).
    if let Some(repo_configs) = repo_lookup() {
        if let Some(repo) = repo_configs.parent() {
            let candidate = repo.join("scripts").join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    // 2. Embedded extraction. The released binary always lands here.
    let entry = EMBEDDED_RUNTIME_SCRIPTS
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| {
            anyhow!(
                "scripts/{name} is not embedded and not present in a source checkout. \
                 This is a voiceforge packaging bug — please file an issue with the \
                 output of `voiceforge doctor`."
            )
        })?;

    let dir = extract_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = dir.join(entry.name);
    write_if_changed(&path, entry.bytes, entry.mode)?;
    Ok(path)
}

/// Extract every embedded asset to `extract_dir()`. Mode 0700 on
/// the directory + 0755 on the script + 0644 on the fixtures.
///
/// Idempotent: existing files with matching sha256 are left alone.
/// Mismatched sha256 triggers an overwrite (e.g., user upgraded
/// voiceforge and the embedded script changed).
///
/// Returned struct names only the fish install script + smoke
/// fixtures — the legacy interface that `install_smoke.rs` consumes.
/// Other runtime scripts (clone_voice_*, *_synth.py, install_cloning.sh)
/// are NOT extracted here — they're extracted lazily via
/// `resolve_runtime_script` when their respective commands run.
// Retained for tests + as a stable entry point if a future smoke /
// uninstall flow needs to materialize every embedded asset up-front.
// `install_smoke` references SMOKE_REFERENCE_{WAV,TXT} directly today
// and does NOT call extract_all (audit: 2026-05-27, no source caller).
#[allow(dead_code)]
pub fn extract_all() -> Result<ExtractedAssets> {
    use std::io::Write;

    // Smoke flow needs a stable extract-dir path for both the script
    // and the WAV/txt fixtures, regardless of whether a source
    // checkout is reachable. Bypass `resolve_runtime_script`'s
    // source-checkout precedence here so the smoke test always works
    // against the extracted copy.
    let dir = extract_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let script_path = dir.join("install_cloning_fish.sh");
    write_if_changed(&script_path, INSTALL_CLONING_FISH_SH.as_bytes(), 0o755)?;
    let smoke_wav_path = dir.join("smoke_reference_8s.wav");
    let smoke_txt_path = dir.join("smoke_reference_8s.txt");
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
#[allow(dead_code)] // returned by extract_all (see comment above extract_all).
#[derive(Debug, Clone)]
pub struct ExtractedAssets {
    pub script_path: PathBuf,
    pub smoke_wav_path: PathBuf,
    pub smoke_txt_path: PathBuf,
}

/// Atomic-rename write that skips when the existing file matches.
/// Sha256-checks before overwrite so re-running install is a fast
/// no-op when content hasn't changed.
fn write_if_changed(
    path: &std::path::Path,
    content: &[u8],
    #[cfg_attr(not(unix), allow(unused_variables))] mode: u32,
) -> Result<()> {
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
                                             // PR-AB step 6d (rust-expert plan v3): real `say`-generated
                                             // ~8s clip is ~500 KB. Bumped from the placeholder-era 32_000
                                             // floor; catches a regress to a tiny placeholder.
        const MIN_WAV_BYTES: usize = 250_000;
        const MIN_TXT_BYTES: usize = 50; // real transcript is ~190 chars
        assert!(
            INSTALL_CLONING_FISH_SH.len() >= MIN_SCRIPT_BYTES,
            "install script too small: {} bytes (min {MIN_SCRIPT_BYTES})",
            INSTALL_CLONING_FISH_SH.len()
        );
        assert!(
            SMOKE_REFERENCE_WAV.len() >= MIN_WAV_BYTES,
            "smoke wav too small: {} bytes (min {MIN_WAV_BYTES}). \
             Did you accidentally revert to the placeholder? Regenerate \
             via `say -v Samantha \"...\" -o /tmp/x.aiff && ffmpeg ...`",
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

    /// SHA256 lock for the canonical smoke fixture (PR-AB step 6d, R3
    /// fix). The fixture is `say`-generated locally on macOS, then
    /// committed; without this lock a developer who regenerates with
    /// a different `say` voice / text / ffmpeg version would silently
    /// ship different bytes. The committed fixture's SHA is the
    /// source of truth — regenerate-and-recommit flows MUST update
    /// this constant in lockstep.
    const SMOKE_REFERENCE_WAV_SHA256: &str =
        "163813eec3acf28b51b44f7e6341a1ad7e0de8078f062720cd9afa892821a3bc";

    #[test]
    fn embedded_smoke_wav_sha256_matches_lock() {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(SMOKE_REFERENCE_WAV);
        let actual = hex::encode(h.finalize());
        assert_eq!(
            actual, SMOKE_REFERENCE_WAV_SHA256,
            "embedded smoke wav SHA256 drift detected.\n\
             actual:   {actual}\n\
             expected: {SMOKE_REFERENCE_WAV_SHA256}\n\
             If you intentionally regenerated tests/fixtures/smoke_reference_8s.wav, \
             update SMOKE_REFERENCE_WAV_SHA256 in this file to match."
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

    // ========================================================================
    // v0.4.1 — registry + resolver tests (plan §3)
    // ========================================================================

    /// Sha256-hex of raw bytes (mirrors build.rs::hex_encode).
    fn sha256_hex_of(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        let d = h.finalize();
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(d.len() * 2);
        for b in d.iter() {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 0x0f) as usize] as char);
        }
        s
    }

    /// Plan §3 test 1: every entry in the registry extracts and the
    /// extracted file is byte-identical to the embedded payload with
    /// the right mode.
    #[test]
    #[serial]
    fn every_embedded_script_extracts() {
        with_tmp_home(|| {
            // Use the test seam with `|| None` so we force the
            // extract path regardless of the workspace state.
            for entry in EMBEDDED_RUNTIME_SCRIPTS {
                let path = resolve_runtime_script_with_repo_lookup(entry.name, || None)
                    .unwrap_or_else(|e| panic!("resolve {}: {e:#}", entry.name));
                assert!(path.is_file(), "{} missing after resolve", path.display());
                let actual = std::fs::read(&path).unwrap();
                assert_eq!(
                    actual.as_slice(),
                    entry.bytes,
                    "extracted bytes for {} do not match embedded payload",
                    entry.name
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
                    assert_eq!(
                        mode & 0o777,
                        entry.mode,
                        "{} mode = {:o}, want {:o}",
                        entry.name,
                        mode & 0o777,
                        entry.mode
                    );
                }
            }
        });
    }

    /// Plan §3 test 2: the build.rs-generated sha matches the
    /// embedded payload's runtime sha. Catches a stale-OUT_DIR
    /// regression where the constants and `include_bytes!` payload
    /// drift apart.
    #[test]
    fn embedded_sha_matches_payload() {
        for entry in EMBEDDED_RUNTIME_SCRIPTS {
            let actual = sha256_hex_of(entry.bytes);
            assert_eq!(
                actual, entry.sha256_hex,
                "sha drift for {}: payload={actual} const={}",
                entry.name, entry.sha256_hex
            );
        }
    }

    /// Plan §3 test 3: second call is a no-op (sha256-skip path).
    #[test]
    #[serial]
    fn resolve_runtime_script_idempotent_on_repeat_call() {
        with_tmp_home(|| {
            let first = resolve_runtime_script_with_repo_lookup("install_cloning_fish.sh", || None)
                .unwrap();
            let mtime1 = std::fs::metadata(&first).unwrap().modified().unwrap();
            // Sleep just enough that mtime would tick if rewrite happens.
            std::thread::sleep(std::time::Duration::from_millis(50));
            let second =
                resolve_runtime_script_with_repo_lookup("install_cloning_fish.sh", || None)
                    .unwrap();
            let mtime2 = std::fs::metadata(&second).unwrap().modified().unwrap();
            assert_eq!(first, second);
            assert_eq!(
                mtime1, mtime2,
                "second resolve rewrote file (mtime changed) — write_if_changed should skip"
            );
        });
    }

    /// Plan §3 test 4: corrupted on-disk payload is restored.
    #[test]
    #[serial]
    fn resolve_runtime_script_overwrites_on_content_drift() {
        with_tmp_home(|| {
            let path =
                resolve_runtime_script_with_repo_lookup("fish_speech_synth.py", || None).unwrap();
            // Corrupt it.
            std::fs::write(&path, b"# corrupted by test").unwrap();
            // Re-resolve.
            let path2 =
                resolve_runtime_script_with_repo_lookup("fish_speech_synth.py", || None).unwrap();
            assert_eq!(path, path2);
            let actual = std::fs::read(&path2).unwrap();
            let expected = EMBEDDED_RUNTIME_SCRIPTS
                .iter()
                .find(|e| e.name == "fish_speech_synth.py")
                .unwrap()
                .bytes;
            assert_eq!(
                actual.as_slice(),
                expected,
                "drifted bytes were NOT restored on re-resolve"
            );
        });
    }

    /// Plan §3 test 5: unknown name → clear packaging-bug error.
    #[test]
    #[serial]
    fn resolve_runtime_script_rejects_unknown_name() {
        with_tmp_home(|| {
            let err =
                resolve_runtime_script_with_repo_lookup("does_not_exist.sh", || None).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("does_not_exist.sh"), "got: {msg}");
            assert!(msg.contains("packaging bug"), "got: {msg}");
        });
    }

    /// Plan §3 test 6: source-checkout precedence — when the repo
    /// lookup finds a `<repo>/scripts/<name>` file, return that path,
    /// not the extracted one.
    #[test]
    #[serial]
    fn resolve_runtime_script_prefers_source_checkout_over_extract() {
        // Wrap in with_tmp_home so any extraction fallback (if
        // precedence ever breaks) lands in a tmpdir rather than the
        // host's real ~/.voiceforge.
        with_tmp_home(|| {
            let tmp = TempDir::new().unwrap();
            let fake_repo = tmp.path().join("fake-repo");
            let fake_scripts = fake_repo.join("scripts");
            std::fs::create_dir_all(&fake_scripts).unwrap();
            let fake_script = fake_scripts.join("install_cloning_fish.sh");
            std::fs::write(
                &fake_script,
                b"#!/usr/bin/env bash\n# fake source-checkout copy\n",
            )
            .unwrap();
            // The repo lookup returns the configs/ dir; the resolver
            // walks up to its parent and looks for scripts/.
            let fake_configs = fake_repo.join("configs");
            std::fs::create_dir_all(&fake_configs).unwrap();
            let resolved =
                resolve_runtime_script_with_repo_lookup("install_cloning_fish.sh", || {
                    Some(fake_configs.clone())
                })
                .unwrap();
            assert_eq!(resolved, fake_script);
            // Content is the fake (NOT the embedded payload) — proves
            // precedence in fact, not just in path naming.
            let content = std::fs::read(&resolved).unwrap();
            assert!(
                content.starts_with(b"#!/usr/bin/env bash\n# fake"),
                "resolver returned source-checkout path but content suggests embedded payload was extracted instead"
            );
        });
    }

    /// Plan §3 test 7: with no source checkout reachable, extraction
    /// path wins. This is the smoking-gun test for the v0.4.0 bug —
    /// proves a fresh `/usr/local/bin/voiceforge` install can find
    /// the script.
    #[test]
    #[serial]
    fn resolve_runtime_script_falls_back_to_extract_when_no_source_checkout() {
        with_tmp_home(|| {
            let path = resolve_runtime_script_with_repo_lookup("install_cloning_fish.sh", || None)
                .unwrap();
            // Must land in the extract dir, not anywhere else.
            let dir = extract_dir().unwrap();
            assert!(
                path.starts_with(&dir),
                "extract_path {} not under {}",
                path.display(),
                dir.display()
            );
            // And must be byte-identical to the embedded payload.
            let actual = std::fs::read(&path).unwrap();
            let expected = EMBEDDED_RUNTIME_SCRIPTS
                .iter()
                .find(|e| e.name == "install_cloning_fish.sh")
                .unwrap()
                .bytes;
            assert_eq!(actual.as_slice(), expected);
        });
    }

    /// Plan §3 test 8: every embedded script has a sane size floor
    /// (catches an accidental truncation / placeholder revert).
    #[test]
    fn every_embedded_script_min_size() {
        // Pin floors at ~50% of current sizes; tighten over time.
        // Current sizes (approx): install_cloning_fish.sh ~17 KB,
        // install_cloning.sh ~12 KB, clone_voice_fish.sh ~8 KB,
        // clone_voice.sh ~6 KB, fish_speech_synth.py ~13 KB,
        // cloning_synth.py ~5 KB.
        let floors: &[(&str, usize)] = &[
            ("install_cloning_fish.sh", 8000),
            ("install_cloning.sh", 6000),
            ("clone_voice_fish.sh", 4000),
            ("clone_voice.sh", 3000),
            ("fish_speech_synth.py", 6000),
            ("cloning_synth.py", 2500),
        ];
        for (name, floor) in floors {
            let entry = EMBEDDED_RUNTIME_SCRIPTS
                .iter()
                .find(|e| e.name == *name)
                .unwrap_or_else(|| panic!("missing registry entry: {name}"));
            assert!(
                entry.bytes.len() >= *floor,
                "{name} only {} bytes; floor is {floor} — accidental truncation?",
                entry.bytes.len()
            );
        }
    }

    /// Plan §3 test 9: shell scripts start with the bash shebang,
    /// python scripts start with the python3 shebang.
    #[test]
    fn every_embedded_script_starts_with_correct_shebang() {
        for entry in EMBEDDED_RUNTIME_SCRIPTS {
            if entry.name.ends_with(".sh") {
                assert!(
                    entry.bytes.starts_with(b"#!/usr/bin/env bash\n")
                        || entry.bytes.starts_with(b"#!/bin/bash\n"),
                    "{} missing bash shebang; first 32 bytes: {:?}",
                    entry.name,
                    String::from_utf8_lossy(&entry.bytes[..entry.bytes.len().min(32)])
                );
            } else if entry.name.ends_with(".py") {
                assert!(
                    entry.bytes.starts_with(b"#!/usr/bin/env python3")
                        || entry.bytes.starts_with(b"#!/usr/bin/env python\n"),
                    "{} missing python3 shebang; first 32 bytes: {:?}",
                    entry.name,
                    String::from_utf8_lossy(&entry.bytes[..entry.bytes.len().min(32)])
                );
            } else {
                panic!("unrecognized extension in registry entry {}", entry.name);
            }
        }
    }
}
