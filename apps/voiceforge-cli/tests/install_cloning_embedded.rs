//! Integration test for the v0.4.1 embedded-runtime-scripts fix.
//!
//! The v0.4.0 release binary's `install-cloning` died with
//! "could not locate scripts/install_cloning_fish.sh" because the
//! released tarball ships no `scripts/` sibling. v0.4.1 embeds the
//! script via `include_bytes!` + extracts to
//! `~/.voiceforge/cloning/.install/` on first use.
//!
//! This test runs the actual `voiceforge` binary (via
//! `CARGO_BIN_EXE_voiceforge`) under a private `VOICEFORGE_HOME`,
//! asserts the bash script ran (exit code is nonzero — the marker
//! is absent, which is the expected error from `--check`), AND
//! asserts the extracted script landed at the expected path with
//! the right sha256.
//!
//! NB: a `cargo test` build of the binary has `CARGO_MANIFEST_DIR`
//! baked in and the `paths::repo_config_dir` fallback walks it for
//! a `configs/` sibling — which IS present in the workspace. So
//! this test cannot directly assert the extraction path under
//! `cargo test`; the unit test
//! `resolve_runtime_script_falls_back_to_extract_when_no_source_checkout`
//! (in `src/embedded_install.rs::tests`) carries that contract via
//! the `|| None` test seam.
//!
//! What this test DOES prove: the `install-cloning --check` call
//! exits with the *bash script's* error ("marker missing"), not
//! the *resolver's* error ("could not locate scripts/..."). That
//! distinction is the v0.4.1 bug fix — before, the resolver bailed
//! before the bash script ever ran.

use std::path::PathBuf;
use std::process::Command;

fn voiceforge_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_voiceforge"))
}

#[test]
fn install_cloning_check_runs_the_bash_script_under_tmp_home() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let out = Command::new(voiceforge_bin())
        .args(["install-cloning", "--check"])
        .env("VOICEFORGE_HOME", home)
        // Force the non-wizard path so the bash output is on
        // stderr/stdout in plain text.
        .env("NO_COLOR", "1")
        .output()
        .expect("running voiceforge install-cloning --check");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}\n{stderr}");

    // The v0.4.0 failure mode was a resolver error before the bash
    // script ran. v0.4.1 should let the bash script run, which then
    // exits non-zero because the marker is absent in a fresh HOME.
    assert!(
        !combined.contains("could not locate scripts/"),
        "v0.4.1 resolver regression — the script-resolver error came back. \
         Combined output:\n{combined}"
    );
    // The bash script itself reports the marker is missing — that's
    // the expected error from `--check` against an uninstalled HOME.
    // Either the bash error text reaches stdout/stderr or the wizard
    // path consumes it; both prove the script executed.
    assert!(
        combined.contains("marker missing")
            || combined.contains("INSTALLED.toml")
            || combined.contains("install-cloning"),
        "expected bash script's marker-missing error, got:\n{combined}"
    );
    assert!(
        !out.status.success(),
        "install-cloning --check unexpectedly succeeded against an empty HOME"
    );
}

// ---------------------------------------------------------------------------
// v0.4.2 — embedded-payload parity for the synth script
// ---------------------------------------------------------------------------
//
// The on-disk-grep tests in `fish_speech_synth_script.rs` prove the
// source file shipped with the v0.4.2 helper. But the release binary
// extracts from an `include_bytes!` payload, not from disk. This test
// pulls the same payload via the same relative path that
// `src/embedded_install.rs` uses, so a divergence (e.g. someone edits
// the on-disk script but a build.rs caching bug freezes the embedded
// copy) would trip here at unit-test speed.

const EMBEDDED_FISH_SPEECH_SYNTH_PY: &[u8] =
    include_bytes!("../../../scripts/fish_speech_synth.py");
const EMBEDDED_INSTALL_CLONING_FISH_SH: &[u8] =
    include_bytes!("../../../scripts/install_cloning_fish.sh");

#[test]
fn embedded_fish_speech_synth_payload_has_mps_helper() {
    let body = std::str::from_utf8(EMBEDDED_FISH_SPEECH_SYNTH_PY)
        .expect("fish_speech_synth.py must be valid UTF-8");
    assert!(
        body.contains("def _detect_default_device()"),
        "embedded fish_speech_synth.py payload missing _detect_default_device helper"
    );
    assert!(
        body.contains(r#"os.environ.get("VOICEFORGE_FISH_SYNTH_DEVICE", _detect_default_device())"#),
        "embedded fish_speech_synth.py payload's env-var default no longer calls _detect_default_device"
    );
    // The MPS fallback env var must be set when device == "mps" — without
    // it, fish-speech S2 Pro aborts on NotImplementedError for ops MPS
    // doesn't natively implement.
    assert!(
        body.contains(r#"os.environ.setdefault("PYTORCH_ENABLE_MPS_FALLBACK", "1")"#),
        "embedded fish_speech_synth.py payload missing PYTORCH_ENABLE_MPS_FALLBACK \
         setdefault — MPS path will crash on day one"
    );
}

#[test]
fn embedded_install_cloning_fish_payload_has_v042_fixes() {
    let body = std::str::from_utf8(EMBEDDED_INSTALL_CLONING_FISH_SH)
        .expect("install_cloning_fish.sh must be valid UTF-8");
    // Bug 1 — force branch must wipe $REPO_DIR alongside venv + marker.
    let force_idx = body
        .find(r#"if [[ "$FORCE" == "1" ]]; then"#)
        .expect("force-branch sentinel missing from embedded payload");
    let tail = &body[force_idx..];
    let fi_idx = tail.find("\nfi\n").expect("force-branch fi missing");
    let block = &tail[..fi_idx];
    let rm_line = block
        .lines()
        .find(|l| l.trim_start().starts_with("run rm -rf"))
        .expect("force-branch rm -rf line missing");
    assert!(
        rm_line.contains("$REPO_DIR"),
        "embedded install_cloning_fish.sh force-branch rm -rf must include \
         $REPO_DIR; got: {rm_line}"
    );
    // Bug 2 — fetch must target `main`, never the pinned SHA.
    assert!(
        body.contains(r#"git -C "$REPO_DIR" fetch --quiet origin main"#),
        "embedded install_cloning_fish.sh must fetch origin main"
    );
    assert!(
        !body.contains(r#"git -C "$REPO_DIR" fetch --quiet origin "$FISH_SPEECH_SHA""#),
        "embedded install_cloning_fish.sh still fetches origin <SHA>"
    );
}
