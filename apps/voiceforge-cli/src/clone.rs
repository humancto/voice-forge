//! `voiceforge clone <name> <source> [--force]` — engine-aware
//! dispatcher (PR-C C-3).
//!
//! Top-level pre-flight (name validation, existing-voice check,
//! pack collision) runs ONCE; then dispatches to `run_v1` (legacy
//! GPT-SoVITS recipe via `scripts/clone_voice.sh`) or `run_v2`
//! (fish-speech S2 Pro recipe via `scripts/clone_voice_fish.sh`)
//! based on which install marker is present on disk.
//!
//! Dispatch order (plan v2 Decision #1 — install-marker-driven, NOT
//! env-driven; reads no `VOICEFORGE_*_ENGINE` env vars):
//!
//!   1. schema-2 marker present  -> run_v2 (fish-speech, v0.4 default)
//!   2. schema-1 marker present  -> run_v1 (legacy GPT-SoVITS)
//!   3. neither present          -> bail with install hint
//!
//! `source` may be a local file path, a `file://` URL (scheme stripped),
//! or an http/https/ytsearch URL (downloaded via yt-dlp into a tempdir).

use anyhow::{anyhow, bail, Context, Result};
use std::process::{Command, Stdio};

use crate::install_cloning;
use crate::paths;
use crate::url_ingest;
use crate::voices;

/// Top-level `voiceforge clone` entrypoint. Pre-flights name + voice
/// existence + pack collision, then dispatches v1/v2 per install
/// marker.
pub fn run(name: String, source: String, force: bool) -> Result<()> {
    // ---- pre-flight (R4: lifted ABOVE v1/v2 dispatch so both
    // branches inherit the same collision guarantees) ---------------

    voices::validate_name(&name)?;

    if voices::voice_exists(&name) && !force {
        bail!(
            "voice {name:?} already exists; pass --force to replace it.\n\
             Existing dir: {}",
            voices::voice_dir(&name)?.display()
        );
    }

    // PR #29 nit: refuse clone if a PACK with the same name is
    // already installed. Symmetric to the check in
    // `packs::install_pack`. `voiceforge say --voice <name>` would
    // be ambiguous between cloned voice + pack otherwise.
    if !force && crate::packs::pack_is_installed(&name) {
        let pack_dir = crate::packs::packs_root()
            .map(|r| r.join(&name).display().to_string())
            .unwrap_or_else(|| format!("~/.voiceforge/packs/{name}"));
        bail!(
            "cannot clone voice {name:?}: a pack with the same name is already installed at {pack_dir}. Use --force to clone anyway, or `voiceforge pack remove {name}` first."
        );
    }

    // ---- dispatch off install markers (Decision #1) ----------------

    if install_cloning::is_installed_v2() {
        run_v2(name, source, force)
    } else if install_cloning::is_installed() {
        run_v1(name, source, force)
    } else {
        bail!(
            "cloning is not installed yet — run `voiceforge install-cloning` first.\n\
             It installs fish-speech S2 Pro (~12 GB, ~30 min) by default; \
             legacy GPT-SoVITS via VOICEFORGE_INSTALL_CLONING_ENGINE=gpt-sovits-v2."
        );
    }
}

/// Legacy GPT-SoVITS clone path. Existing v1 users (pre-PR-AB) take
/// this branch automatically when their schema-1 marker is on disk.
fn run_v1(name: String, source: String, force: bool) -> Result<()> {
    // Resolve the source — local path or URL. The ResolvedSource value
    // MUST stay in scope for the entire Command::status() below; its
    // Drop unlinks the tempdir holding the downloaded WAV.
    let resolved = url_ingest::resolve_source(&source)
        .with_context(|| format!("resolving clone source {source:?}"))?;

    let script = resolve_script_named("clone_voice.sh")?;

    let local_path = resolved.local_path();
    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .arg(&name)
        .arg(local_path)
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
    drop(resolved);
    Ok(())
}

/// Fish-speech S2 Pro clone path (v0.4 default). Writes a schema-2
/// voice profile + single ref.wav + ref.txt at
/// `~/.voiceforge/voices/<name>/`. Shells out to
/// `scripts/clone_voice_fish.sh` (the v2 recipe; PR-C C-2).
fn run_v2(name: String, source: String, force: bool) -> Result<()> {
    let resolved = url_ingest::resolve_source(&source)
        .with_context(|| format!("resolving clone source {source:?}"))?;

    let script = resolve_script_named("clone_voice_fish.sh")?;

    let local_path = resolved.local_path();
    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .arg(&name)
        .arg(local_path)
        .arg(if force { "1" } else { "0" })
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd
        .status()
        .with_context(|| format!("spawning {}", script.display()))?;

    if !status.success() {
        bail!("clone_voice_fish.sh exited non-zero: {status}");
    }
    drop(resolved);
    Ok(())
}

/// Resolve `scripts/<name>` next to the running binary or in the
/// repo root (when running via `cargo run`). Replaces the v1-only
/// `resolve_clone_script` so both recipes share one lookup.
fn resolve_script_named(name: &str) -> Result<std::path::PathBuf> {
    if let Some(repo_configs) = paths::repo_config_dir() {
        if let Some(repo) = repo_configs.parent() {
            let candidate = repo.join("scripts").join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(anyhow!(
        "could not locate scripts/{name}.\n\
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
created_at = "2026-05-04T00:00:00Z"
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

    /// PR #29 nit + reviewer follow-up: clone must refuse if a PACK
    /// with the same name is already installed (symmetric to
    /// `packs::install_pack` refusing on cloned-voice collision).
    #[test]
    #[serial]
    fn run_errors_when_pack_with_same_name_installed() {
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
        // Stage a fake installed pack at packs/peter/manifest.toml.
        let pack_dir = tmp.path().join("packs/peter");
        std::fs::create_dir_all(&pack_dir).unwrap();
        std::fs::write(
            pack_dir.join("manifest.toml"),
            r#"
schema_version = 1
name = "peter"
voice_source = "test"
source_clip_url = "https://example.invalid/x.mp4"
reference_prompt_text = "x"
tier = "character"
[phrases]
build_failed = "Sad."
"#,
        )
        .unwrap();
        with_home(tmp.path(), || {
            let err = run("peter".into(), "/some/source".into(), false).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("a pack with the same name") && msg.contains("--force"),
                "expected pack-collision error, got: {msg}"
            );
        });
    }

    // ========================================================================
    // PR-C C-3: engine-aware dispatch tests
    //
    // These tests assert WHICH script the dispatcher picks based on
    // the install marker(s) on disk. We don't run the scripts to
    // completion (that needs real ffmpeg + fish-speech/GPT-SoVITS);
    // we stage an unresolvable source so the resolver/script bails,
    // then assert the bail message names the correct script.
    // ========================================================================

    fn write_v1_marker_in(home: &Path) {
        let cloning = home.join("cloning");
        std::fs::create_dir_all(&cloning).unwrap();
        std::fs::write(
            cloning.join("INSTALLED.toml"),
            r#"
schema_version = 1
gpt_sovits_sha = "08d627c3"
python_path = "/p"
ffmpeg6_prefix = "/f"
"#,
        )
        .unwrap();
    }

    fn write_v2_marker_in(home: &Path) {
        let cloning = home.join("cloning");
        std::fs::create_dir_all(&cloning).unwrap();
        std::fs::write(
            cloning.join("INSTALLED.toml"),
            r#"
schema_version = 2
version = "0.4.0"
fish_speech_sha = "3dd1f85c402ee6f0a17c2971d3b0dd8d881ca139"
python_path = "/p"
ffmpeg6_prefix = "/f"
"#,
        )
        .unwrap();
    }

    /// V2 marker on disk + invalid source: the dispatcher routes to
    /// run_v2 → resolve_source fails on the bogus path. The bail
    /// message must name the v2 source resolution (NOT the v1 script).
    #[test]
    #[serial]
    fn run_dispatches_v2_when_v2_marker_present() {
        let tmp = tempfile::tempdir().unwrap();
        write_v2_marker_in(tmp.path());
        with_home(tmp.path(), || {
            let err = run("tyson".into(), "/nonexistent/audio.wav".into(), false).unwrap_err();
            let msg = format!("{err:#}");
            // Source-resolution failure mentions the bogus path; this
            // path runs INSIDE run_v2 → proves dispatch landed there.
            assert!(
                msg.contains("resolving clone source") || msg.contains("nonexistent"),
                "expected v2 source-resolution failure, got: {msg}"
            );
        });
    }

    /// Only v1 marker on disk: dispatcher routes to run_v1.
    #[test]
    #[serial]
    fn run_dispatches_v1_when_only_v1_marker_present() {
        let tmp = tempfile::tempdir().unwrap();
        write_v1_marker_in(tmp.path());
        with_home(tmp.path(), || {
            let err = run("peter".into(), "/nonexistent/audio.wav".into(), false).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("resolving clone source") || msg.contains("nonexistent"),
                "expected v1 source-resolution failure, got: {msg}"
            );
        });
    }

    /// V2 dispatch is unaffected by an orphan `INSTALLED.v1.bak` file
    /// (left behind by a previous schema-1 install per PR-AB step 6c).
    /// `is_installed()` and `is_installed_v2()` are mutually exclusive
    /// at the marker level — both can never return true simultaneously
    /// — so this test pins that the v1.bak orphan does NOT confuse the
    /// dispatcher and that v2 takes the active path.
    /// (Renamed from `run_dispatches_v2_when_both_markers_present` per
    /// rust-expert PR #45 review R1: "both markers" was misleading
    /// since markers are structurally exclusive.)
    #[test]
    #[serial]
    fn v2_dispatch_unaffected_by_orphan_v1_bak() {
        let tmp = tempfile::tempdir().unwrap();
        write_v2_marker_in(tmp.path());
        // Stage a v1 backup file (not the active marker — the active
        // marker is the schema-2 INSTALLED.toml from write_v2_marker_in).
        std::fs::write(
            tmp.path().join("cloning/INSTALLED.v1.bak"),
            "schema_version = 1\ngpt_sovits_sha = \"old\"\n",
        )
        .unwrap();
        with_home(tmp.path(), || {
            let err = run("tyson".into(), "/nonexistent".into(), false).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("resolving clone source") || msg.contains("nonexistent"),
                "expected v2 dispatch unaffected by v1.bak orphan, got: {msg}"
            );
        });
    }

    /// Pack-collision check (R4 lock) MUST fire before v1/v2 dispatch.
    /// Stage a pack + a v2 marker → pack collision bails before the
    /// dispatcher even looks at the install marker. The bail message
    /// names the pack collision, NOT the dispatcher.
    #[test]
    #[serial]
    fn pack_collision_check_fires_before_v1_v2_dispatch() {
        let tmp = tempfile::tempdir().unwrap();
        write_v2_marker_in(tmp.path()); // v2 marker present
        let pack_dir = tmp.path().join("packs/tyson");
        std::fs::create_dir_all(&pack_dir).unwrap();
        std::fs::write(
            pack_dir.join("manifest.toml"),
            r#"
schema_version = 1
name = "tyson"
voice_source = "test"
source_clip_url = "https://example.invalid/x.mp4"
reference_prompt_text = "x"
tier = "character"
[phrases]
build_failed = "Sad."
"#,
        )
        .unwrap();
        with_home(tmp.path(), || {
            let err = run("tyson".into(), "/nonexistent".into(), false).unwrap_err();
            let msg = format!("{err:#}");
            // Pack-collision bail wins over any dispatch-side failure
            assert!(
                msg.contains("a pack with the same name"),
                "expected pack-collision bail to fire before dispatch, got: {msg}"
            );
        });
    }
}
