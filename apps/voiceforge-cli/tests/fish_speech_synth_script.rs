//! Integration tests for `scripts/fish_speech_synth.py` (the v2
//! fish-speech NDJSON worker; ROADMAP v0.4 PR-AB step 7).
//!
//! End-to-end synth requires a working fish-speech install, ~10 GB of
//! weights, and ~30s of model-load time per run. We skip that here
//! and only exercise the *protocol contract* the Rust client (FishEngine,
//! lands in step 8) depends on:
//!
//!   1. `python3 -m py_compile` succeeds (catches syntax regressions).
//!   2. Missing `INSTALLED.toml` -> emits `{"ok": false, ...}` to
//!      stdout AND exits with code 2 (so the Rust client distinguishes
//!      "no install" from "synth failure").
//!   3. Wrong schema_version (v1 marker) -> bails with code 2 + a
//!      `re-install --force` hint.
//!   4. fish_speech_sha mismatch -> bails with code 2.
//!   5. Stdout bytes BEFORE any framework chatter must be valid NDJSON
//!      (the protocol-stdout-stash trick at the top of the script).
//!
//! All tests use a unique tempdir for `VOICEFORGE_HOME` so they can
//! run in parallel.

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

// Local copy of the SHA pin for use in test fixtures + assertions
// below. R4 fix (rust-expert review pass 1): the
// `pinned_sha_matches_canonical_rust_constant` test below grep-asserts
// that this value matches the canonical declaration in
// install_cloning.rs::FISH_SPEECH_PINNED_SHA so a one-place drift
// trips immediately (bash + python + this constant + the canonical
// Rust constant all stay in lockstep).
const PINNED_FISH_SPEECH_SHA: &str = "3dd1f85c402ee6f0a17c2971d3b0dd8d881ca139";

fn script_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("scripts").join("fish_speech_synth.py"))
        .expect("locate fish_speech_synth.py")
}

fn write_marker(home: &Path, body: &str) {
    let dir = home.join("cloning");
    std::fs::create_dir_all(&dir).expect("mkdir cloning");
    std::fs::write(dir.join("INSTALLED.toml"), body).expect("write marker");
}

#[test]
fn script_is_executable_and_present() {
    let path = script_path();
    assert!(path.is_file(), "script missing at {}", path.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o111;
        assert!(mode != 0, "script is not executable: {}", path.display());
    }
}

#[test]
fn script_passes_python_syntax_check() {
    // Quick regression net for syntax breakage. `py_compile` is in the
    // stdlib so this only requires python3, which voiceforge already
    // requires for the cloning runtime — same precedent as the v1
    // cloning_synth.py (which has zero in-repo tests but ships).
    let out = Command::new("python3")
        .arg("-m")
        .arg("py_compile")
        .arg(script_path())
        .output()
        .expect("run py_compile");
    assert!(
        out.status.success(),
        "py_compile failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
fn missing_marker_emits_ndjson_error_and_exits_2() {
    let tmp = TempDir::new().unwrap();
    // No INSTALLED.toml in the home — script must bail BEFORE trying
    // to import fish_speech.
    let out = Command::new("python3")
        .arg(script_path())
        .env("VOICEFORGE_HOME", tmp.path())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("invoke synth script");
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit code 2 (install-not-ready), got {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first_line = stdout.lines().next().unwrap_or("");
    let parsed: serde_json::Value =
        serde_json::from_str(first_line).expect("first stdout line must be NDJSON");
    assert_eq!(parsed["ok"], false, "missing-marker must emit ok:false");
    assert!(
        parsed["error"]
            .as_str()
            .unwrap_or("")
            .contains("cloning marker missing"),
        "error must name the missing artifact: {parsed:?}"
    );
}

#[test]
fn schema_v1_marker_bails_with_upgrade_hint() {
    let tmp = TempDir::new().unwrap();
    write_marker(
        tmp.path(),
        r#"
schema_version = 1
gpt_sovits_sha = "08d627c3"
python_path = "/p"
ffmpeg6_prefix = "/f"
"#,
    );
    let out = Command::new("python3")
        .arg(script_path())
        .env("VOICEFORGE_HOME", tmp.path())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("invoke synth script");
    assert_eq!(
        out.status.code(),
        Some(2),
        "schema-1 marker must exit 2; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap_or(""))
        .expect("first stdout line must be NDJSON");
    let err = parsed["error"].as_str().unwrap_or("");
    assert!(
        err.contains("schema_version") && err.contains("install-cloning"),
        "v1-on-v2-script error must reference both the version mismatch AND the
         install-cloning command, got: {err}"
    );
}

#[test]
fn fish_speech_sha_mismatch_bails_with_reinstall_hint() {
    let tmp = TempDir::new().unwrap();
    // Schema is v2, but the SHA pin is wrong — simulates a torn install
    // where the Python script and the marker are out of sync.
    write_marker(
        tmp.path(),
        r#"
schema_version = 2
fish_speech_sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
python_path = "/p"
ffmpeg6_prefix = "/f"
repo_path = "/r"
checkpoint_path = "/c"
whisper_model = "medium"
engine = "fish-speech-s2-pro"
"#,
    );
    let out = Command::new("python3")
        .arg(script_path())
        .env("VOICEFORGE_HOME", tmp.path())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("invoke synth script");
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap_or(""))
        .expect("first stdout line must be NDJSON");
    let err = parsed["error"].as_str().unwrap_or("");
    assert!(
        err.contains("SHA mismatch") && err.contains("install-cloning"),
        "SHA mismatch error must reference the install command, got: {err}"
    );
    // The mismatch error must mention BOTH SHAs so the user sees what
    // the script expected vs what they have on disk.
    assert!(
        err.contains("deadbeef") && err.contains(&PINNED_FISH_SPEECH_SHA[..8]),
        "SHA mismatch error must include both SHAs, got: {err}"
    );
}

#[test]
fn pinned_sha_matches_install_script() {
    // Single-source-of-truth check: the Python pin and the bash pin
    // must match. If they drift, a working install ships with a
    // synth-side SHA failure on every request — silently broken on
    // every clone after the next install.
    let bash_path = script_path().with_file_name("install_cloning_fish.sh");
    let bash = std::fs::read_to_string(&bash_path).expect("read bash installer");
    assert!(
        bash.contains(&format!(r#"FISH_SPEECH_SHA="{PINNED_FISH_SPEECH_SHA}""#)),
        "install_cloning_fish.sh's FISH_SPEECH_SHA does not match the python pin
         {PINNED_FISH_SPEECH_SHA}.\nThe two MUST be bumped in lockstep."
    );

    let py = std::fs::read_to_string(script_path()).expect("read python script");
    assert!(
        py.contains(&format!(
            r#"EXPECTED_FISH_SPEECH_SHA = "{PINNED_FISH_SPEECH_SHA}""#
        )),
        "fish_speech_synth.py's pin diverged from the test constant"
    );

    // R4 fix (rust-expert review pass 1): also assert the canonical
    // Rust constant in install_cloning.rs matches. Without this, a
    // bumper could update bash + python + this test constant in
    // lockstep and STILL leave the Rust source-of-truth stale.
    let rs_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("install_cloning.rs");
    let rs = std::fs::read_to_string(&rs_path).expect("read install_cloning.rs");
    assert!(
        rs.contains(&format!(
            r#"FISH_SPEECH_PINNED_SHA: &str = "{PINNED_FISH_SPEECH_SHA}""#
        )),
        "install_cloning.rs::FISH_SPEECH_PINNED_SHA diverged from the test
         constant {PINNED_FISH_SPEECH_SHA}. All four pin sites (bash, python,
         this test constant, and the Rust source-of-truth constant) MUST
         match."
    );
}

#[test]
fn first_stdout_byte_starts_with_brace() {
    // The protocol contract: every stdout line is JSON, no leading
    // banner / log line. The script's `_PROTOCOL_STDOUT = sys.stdout;
    // sys.stdout = sys.stderr` prelude protects this. Verify by
    // capturing bytes 0..1 of stdout for a missing-marker invocation
    // (the simplest path that produces output without needing fish_speech).
    let tmp = TempDir::new().unwrap();
    let out = Command::new("python3")
        .arg(script_path())
        .env("VOICEFORGE_HOME", tmp.path())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("invoke synth script");
    assert!(
        out.stdout.first() == Some(&b'{'),
        "first stdout byte must be '{{' (NDJSON), got: {:?}\nfull stdout: {}",
        out.stdout.first(),
        String::from_utf8_lossy(&out.stdout)
    );
}

// ---------------------------------------------------------------------------
// v0.4.2 — device autodetect (MPS on Apple Silicon)
// ---------------------------------------------------------------------------
//
// The v0.4.1 default was `device = "cpu"`, which gave ~1/62× realtime on
// an M2 (~17 minutes for 16.4 s of cloned audio). v0.4.2 ships a
// `_detect_default_device()` helper that picks `mps` when torch's MPS
// backend is available + built; ~3× speedup on M-series with the same
// model and the same studio-quality output (whisper-verified
// character-perfect on the Tyson clone).
//
// VOICEFORGE_FISH_SYNTH_DEVICE remains the explicit override (e.g.
// memory-constrained 16 GB M-series may want to stay on cpu).

#[test]
fn fish_speech_synth_defines_device_autodetect_helper() {
    // Sha-pin-independent regression: assert the helper function exists
    // and the env-var default call site points at it. Pure text grep
    // against the on-disk script — no torch import needed.
    let body = std::fs::read_to_string(script_path()).expect("read fish_speech_synth.py");
    assert!(
        body.contains("def _detect_default_device()"),
        "v0.4.2 MPS default regression — _detect_default_device() helper missing"
    );
    assert!(
        body.contains(
            r#"os.environ.get("VOICEFORGE_FISH_SYNTH_DEVICE", _detect_default_device())"#
        ),
        "v0.4.2 MPS default regression — env-var default no longer calls _detect_default_device()"
    );
    // The hardcoded "cpu" default must NOT be back.
    assert!(
        !body.contains(r#"os.environ.get("VOICEFORGE_FISH_SYNTH_DEVICE", "cpu")"#),
        "v0.4.2 regression — env-var default reverted to hardcoded \"cpu\""
    );
}

#[test]
fn detect_default_device_prefers_mps_when_torch_reports_available() {
    // Behavior test: spawn python with a stub `torch` package on
    // PYTHONPATH that makes mps.is_available()/is_built() return True,
    // import the helper directly from the script, assert it returns "mps".
    // No real torch dependency required — the stub satisfies the imports.
    let tmp = TempDir::new().unwrap();
    let stub_dir = tmp.path().join("stubs");
    let torch_dir = stub_dir.join("torch");
    let backends_dir = torch_dir.join("backends");
    let mps_dir = backends_dir.join("mps");
    std::fs::create_dir_all(&mps_dir).unwrap();
    // torch/__init__.py
    std::fs::write(
        torch_dir.join("__init__.py"),
        "from . import backends\nclass _Cuda:\n    @staticmethod\n    def is_available():\n        return False\ncuda = _Cuda()\n",
    )
    .unwrap();
    // torch/backends/__init__.py
    std::fs::write(backends_dir.join("__init__.py"), "from . import mps\n").unwrap();
    // torch/backends/mps/__init__.py — claim MPS is available + built.
    std::fs::write(
        mps_dir.join("__init__.py"),
        "def is_available():\n    return True\ndef is_built():\n    return True\n",
    )
    .unwrap();

    // Driver script: import the helper out of fish_speech_synth.py and
    // print its return value. NB: the script's module body does
    // `sys.stdout = sys.stderr` (the NDJSON protocol stash) at import
    // time, so we MUST print through `sys.__stdout__` after exec_module
    // — otherwise the value lands on stderr and stdout reads empty.
    let driver = format!(
        r#"
import importlib.util, sys
spec = importlib.util.spec_from_file_location("fss", r"{script}")
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)
print(mod._detect_default_device(), file=sys.__stdout__)
"#,
        script = script_path().display()
    );
    let out = Command::new("python3")
        .arg("-c")
        .arg(&driver)
        .env("PYTHONPATH", &stub_dir)
        // Make sure the real torch (if installed in the host's site-packages)
        // does NOT shadow our stub. PYTHONPATH is searched before site-packages
        // for the current interpreter, so the stub wins.
        .output()
        .expect("invoke python driver");
    assert!(
        out.status.success(),
        "driver failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim(),
        "mps",
        "_detect_default_device() must return \"mps\" when torch.backends.mps says so; got {stdout:?}"
    );
}

#[test]
fn detect_default_device_falls_back_to_cpu_when_torch_missing() {
    // No torch on PYTHONPATH at all — the `import torch` inside the
    // helper raises ImportError, the `except Exception: pass` swallows
    // it, and the function returns "cpu". Use a clean PYTHONPATH so the
    // host's torch (if any) doesn't satisfy the import.
    let tmp = TempDir::new().unwrap();
    let driver = format!(
        r#"
import importlib.util, sys
# Forcibly remove any pre-loaded torch from sys.modules so the import
# inside the helper truly fails. (The test harness's parent python may
# have imported torch via some other path.)
for k in list(sys.modules):
    if k == "torch" or k.startswith("torch."):
        del sys.modules[k]
spec = importlib.util.spec_from_file_location("fss", r"{script}")
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)
# Poison sys.path so torch can never be located, then probe the helper.
# Print through __stdout__ — the script body redirected sys.stdout to
# stderr for NDJSON protocol cleanliness.
sys.path = [r"{tmp}"]
print(mod._detect_default_device(), file=sys.__stdout__)
"#,
        script = script_path().display(),
        tmp = tmp.path().display(),
    );
    let out = Command::new("python3")
        .arg("-c")
        .arg(&driver)
        // Empty PYTHONPATH; we already clamp sys.path inside the driver.
        .env("PYTHONPATH", tmp.path())
        .output()
        .expect("invoke python driver");
    assert!(
        out.status.success(),
        "driver failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim(),
        "cpu",
        "_detect_default_device() must fall back to \"cpu\" when torch is unimportable; got {stdout:?}"
    );
}
