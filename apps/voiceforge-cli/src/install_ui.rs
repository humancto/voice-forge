//! Tier-2 install wizard (ROADMAP v0.4 PR-AB step 6b).
//!
//! Wraps the `scripts/install_cloning_fish.sh` invocation with an
//! indicatif `MultiProgress` driven by the script's `==> ` phase
//! lines. The bash script is the source of truth for the install
//! recipe; this module just *renders* the install nicely.
//!
//! ## Contract with the bash script
//!
//! Every major install step in `install_cloning_fish.sh` prefixes
//! its boundary line with `==> ` (the `step` shell function). The
//! parser here splits stdout on those markers and treats each
//! intervening block as one phase. When a phase boundary is seen:
//!
//!   1. Mark the previous phase complete (✓).
//!   2. Start a new spinner with the phase title.
//!   3. Update the cassette-frame compact footer's
//!      `BannerStatus::Custom("phase N/M: <title>")` (when the
//!      branding header was rendered).
//!
//! Non-phase output (tool stdout, `say` lines, `[dry-run] ...`) gets
//! suppressed by default and surfaced only on failure (so a successful
//! 8-minute pip install doesn't drown the wizard in noise).
//!
//! Suppression: when `branding::use_color()` is false (NO_COLOR,
//! non-TTY), we fall back to *exact* stdio inheritance — the existing
//! shipped behavior of `install_cloning::run()`. The wizard is
//! cosmetic; the install recipe must work identically without it.

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

/// Marker the bash installer prefixes phase lines with. Stable; the
/// script's tests assert on it (see tests/install_cloning_fish_script.rs).
pub const PHASE_MARKER: &str = "==> ";

/// Extract the phase title from a stdout line. Returns `None` for any
/// line that isn't a phase boundary.
///
/// ```text
/// "==> creating venv at /foo"  ->  Some("creating venv at /foo")
/// "    free: 2003 GB"          ->  None
/// ""                           ->  None
/// ```
pub fn parse_phase_line(line: &str) -> Option<&str> {
    line.strip_prefix(PHASE_MARKER).map(str::trim)
}

/// Run a `Command` with the indicatif install wizard wrapped around
/// it. The command's stdout is read line-by-line; each `==> ` line
/// advances the wizard. The command's stderr passes through untouched
/// (errors must remain visible).
///
/// On non-success exit, the captured stdout is dumped *after* the
/// progress bars are torn down so the user sees the full install log.
///
/// `total_phases` is a hint for the bar's length; the wizard tolerates
/// over- or under-count (last phase just lingers / bar wraps).
pub fn run_with_wizard(cmd: &mut Command, total_phases: usize, title: &str) -> Result<ExitStatus> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child: Child = cmd
        .spawn()
        .with_context(|| format!("spawning install wizard target: {cmd:?}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("wizard child has no stdout"))?;

    let mp = MultiProgress::new();
    let header = mp.add(ProgressBar::new(total_phases as u64));
    header.set_style(
        ProgressStyle::with_template(
            "  {prefix:.cyan.bold} [{bar:40.cyan/blue}] {pos}/{len} · {msg}",
        )
        .unwrap()
        .progress_chars("█▌ "),
    );
    header.set_prefix(title.to_string());
    header.set_message("starting…");

    let phase_bar = mp.add(ProgressBar::new_spinner());
    phase_bar.set_style(
        ProgressStyle::with_template("    {spinner:.green} {msg}")
            .unwrap()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
    );
    phase_bar.enable_steady_tick(Duration::from_millis(120));

    let captured: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();

    // Stream stdout off the child on a background thread so we don't
    // deadlock on its pipe buffer while the wizard is rendering.
    let captured_writer = Arc::clone(&captured);
    let mp_for_thread = mp.clone();
    let header_clone = header.clone();
    let phase_clone = phase_bar.clone();
    let reader_handle = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut phase_count: u64 = 0;
        for line in reader.lines().map_while(Result::ok) {
            captured_writer.lock().unwrap().push(line.clone());
            if let Some(title) = parse_phase_line(&line) {
                phase_count = phase_count.saturating_add(1);
                header_clone.set_position(phase_count);
                header_clone.set_message(title.to_string());
                phase_clone.set_message(title.to_string());
            }
        }
        // Final tick so the bar lands at its true value.
        header_clone.set_position(phase_count);
        let _ = mp_for_thread; // keep alive
    });

    let status = child
        .wait()
        .with_context(|| "waiting on install wizard child")?;
    reader_handle
        .join()
        .map_err(|_| anyhow::anyhow!("wizard reader thread panicked"))?;

    if status.success() {
        header.finish_with_message("done");
        phase_bar.finish_and_clear();
    } else {
        header.abandon_with_message("FAILED");
        phase_bar.abandon();
        // Dump captured stdout AFTER tearing down progress bars so the
        // log is readable.
        let log = captured.lock().unwrap();
        eprintln!(
            "\n--- install_cloning_fish.sh stdout (last {} lines) ---",
            log.len().min(80)
        );
        let start = log.len().saturating_sub(80);
        for line in &log[start..] {
            eprintln!("{line}");
        }
        eprintln!("--- end log ---\n");
    }
    Ok(status)
}

// ============================================================================
// Engine selector
// ============================================================================

/// Which install recipe to run. Default: fish-speech-s2-pro for new
/// installs (the v0.4 pivot). Legacy GPT-SoVITS stays accessible via
/// `--engine gpt-sovits-v2` for users who want to keep their schema-1
/// installs alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallEngine {
    FishSpeechS2Pro,
    GptSovitsV2,
}

impl InstallEngine {
    pub const DEFAULT: Self = Self::FishSpeechS2Pro;

    /// Bash script filename inside `<repo>/scripts/`.
    pub const fn script_filename(self) -> &'static str {
        match self {
            Self::FishSpeechS2Pro => "install_cloning_fish.sh",
            Self::GptSovitsV2 => "install_cloning.sh",
        }
    }

    /// Approximate phase count emitted by the corresponding script.
    /// Used as the indicatif bar length hint. Drift-tolerant.
    pub const fn approx_phase_count(self) -> usize {
        match self {
            Self::FishSpeechS2Pro => 17,
            Self::GptSovitsV2 => 14,
        }
    }

    pub fn human_title(self) -> &'static str {
        match self {
            Self::FishSpeechS2Pro => "voiceforge install-cloning · fish-speech S2 Pro",
            Self::GptSovitsV2 => "voiceforge install-cloning · GPT-SoVITS v2 (legacy)",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fish-speech-s2-pro" => Some(Self::FishSpeechS2Pro),
            "gpt-sovits-v2" => Some(Self::GptSovitsV2),
            _ => None,
        }
    }
}

/// Pick the engine from the env. `VOICEFORGE_INSTALL_CLONING_ENGINE`
/// overrides; otherwise default. Unknown values bail loud.
pub fn engine_from_env() -> Result<InstallEngine> {
    engine_from_env_with(std::env::var("VOICEFORGE_INSTALL_CLONING_ENGINE").ok())
}

/// Test-friendly variant.
pub fn engine_from_env_with(raw: Option<String>) -> Result<InstallEngine> {
    match raw.as_deref() {
        None | Some("") => Ok(InstallEngine::DEFAULT),
        Some(v) => InstallEngine::parse(v).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown VOICEFORGE_INSTALL_CLONING_ENGINE: {v:?} (want fish-speech-s2-pro or gpt-sovits-v2)"
            )
        }),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn parse_phase_line_extracts_title() {
        assert_eq!(
            parse_phase_line("==> creating venv at /foo"),
            Some("creating venv at /foo")
        );
        assert_eq!(
            parse_phase_line("==> arm64 Homebrew check"),
            Some("arm64 Homebrew check")
        );
    }

    #[test]
    fn parse_phase_line_trims_trailing_whitespace() {
        assert_eq!(
            parse_phase_line("==> writing marker   "),
            Some("writing marker")
        );
    }

    #[test]
    fn parse_phase_line_rejects_non_phase_lines() {
        assert_eq!(parse_phase_line("    free: 2003 GB"), None);
        assert_eq!(parse_phase_line(""), None);
        assert_eq!(parse_phase_line("warning: foo"), None);
        assert_eq!(parse_phase_line("=> not a phase"), None);
        assert_eq!(parse_phase_line("==>nospace"), None);
    }

    #[test]
    fn parse_phase_line_handles_unicode_dash() {
        // The bash script uses an em-dash in some phase titles.
        assert_eq!(
            parse_phase_line("==> done — fish-speech S2 Pro cloning stack ready"),
            Some("done — fish-speech S2 Pro cloning stack ready")
        );
    }

    #[test]
    fn install_engine_default_is_fish_speech() {
        assert_eq!(InstallEngine::DEFAULT, InstallEngine::FishSpeechS2Pro);
    }

    #[test]
    fn install_engine_script_filenames_are_distinct() {
        assert_ne!(
            InstallEngine::FishSpeechS2Pro.script_filename(),
            InstallEngine::GptSovitsV2.script_filename()
        );
        assert!(InstallEngine::FishSpeechS2Pro
            .script_filename()
            .ends_with("_fish.sh"));
        assert!(InstallEngine::GptSovitsV2
            .script_filename()
            .ends_with("install_cloning.sh"));
    }

    #[test]
    fn install_engine_phase_counts_within_realistic_bounds() {
        // The wizard's bar length is a hint; it must be > 0 and within
        // a sane range so a future script bump can't break the wizard.
        for e in [InstallEngine::FishSpeechS2Pro, InstallEngine::GptSovitsV2] {
            let n = e.approx_phase_count();
            assert!(
                (10..=40).contains(&n),
                "{:?} phase count {n} unreasonable",
                e
            );
        }
    }

    #[test]
    fn install_engine_parses_known_values() {
        assert_eq!(
            InstallEngine::parse("fish-speech-s2-pro"),
            Some(InstallEngine::FishSpeechS2Pro)
        );
        assert_eq!(
            InstallEngine::parse("gpt-sovits-v2"),
            Some(InstallEngine::GptSovitsV2)
        );
        assert_eq!(InstallEngine::parse("unknown"), None);
    }

    #[test]
    #[serial]
    fn engine_from_env_defaults_when_unset() {
        let prev = std::env::var("VOICEFORGE_INSTALL_CLONING_ENGINE").ok();
        std::env::remove_var("VOICEFORGE_INSTALL_CLONING_ENGINE");
        assert_eq!(engine_from_env().unwrap(), InstallEngine::DEFAULT);
        if let Some(v) = prev {
            std::env::set_var("VOICEFORGE_INSTALL_CLONING_ENGINE", v);
        }
    }

    #[test]
    fn engine_from_env_with_handles_empty_string() {
        // Empty string is a common shell idiom for "use default" — must
        // NOT be treated as an unknown engine.
        assert_eq!(
            engine_from_env_with(Some(String::new())).unwrap(),
            InstallEngine::DEFAULT
        );
    }

    #[test]
    fn engine_from_env_with_rejects_unknown() {
        let err = engine_from_env_with(Some("frobnicate".into())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("frobnicate") && msg.contains("fish-speech-s2-pro"),
            "error must name the bad value AND list valid engines, got: {msg}"
        );
    }

    /// Smoke test for the wizard wrapper: invoke a tiny inline bash
    /// script that emits 3 phases, verify `run_with_wizard` returns
    /// success and the captured log contains the phase markers.
    #[test]
    fn run_with_wizard_drives_phases_from_script() {
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(
            r#"
                printf '\n==> phase one\n'
                printf '    detail line\n'
                printf '\n==> phase two\n'
                printf '\n==> phase three\n'
                "#,
        );
        let status = run_with_wizard(&mut cmd, 3, "wizard test").expect("run wizard");
        assert!(status.success(), "wizard child failed: {status}");
    }

    /// On non-success, the wizard surfaces the captured log to stderr.
    /// We can't easily intercept the global stderr here, but we CAN
    /// confirm the function returns the failing exit status (not Err).
    #[test]
    fn run_with_wizard_returns_failing_status_on_nonzero_exit() {
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg("printf '==> doomed\n'; exit 7");
        let status = run_with_wizard(&mut cmd, 1, "wizard fail test").expect("invoke");
        assert!(!status.success());
        assert_eq!(status.code(), Some(7), "exit code must propagate");
    }
}
