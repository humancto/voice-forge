//! Dependency audit (ROADMAP v0.4 PR-AB step 3).
//!
//! Single source of truth for "what does voiceforge need on this
//! machine to actually work?" Consumed by:
//!
//! - `voiceforge audit` — humans run this when something is off
//! - `voiceforge doctor` — per-dep rows in the existing health check
//! - `voiceforge install-cloning` — uses the report to know what's
//!   missing + what to install
//!
//! Each dep returns a `DepStatus` with a typed `Remediation` that
//! holds **data, not formatted strings** — the renderer picks
//! macOS-brew / Linux-apt / Linux-dnf / manual-URL based on the
//! current OS. This keeps Linux support a "fill in the apt/dnf
//! commands" exercise in v0.4.1 instead of a structural rewrite.
//!
//! ## Wire-format-stable JSON
//!
//! `voiceforge audit --json` is a documented contract for shell
//! scripts + CI checks. The structure is versioned via
//! `schema_version`; changes that break consumers bump the version.

use serde::Serialize;
use std::path::PathBuf;

const SCHEMA_VERSION: u32 = 1;

/// Top-level audit report. Serializes to JSON for machine consumption.
#[derive(Debug, Clone, Serialize)]
pub struct DepAuditReport {
    pub schema_version: u32,
    pub voiceforge_version: &'static str,
    pub deps: Vec<DepStatus>,
}

impl DepAuditReport {
    pub fn ok_count(&self) -> usize {
        self.deps
            .iter()
            .filter(|d| d.status == DepCheckStatus::Ok)
            .count()
    }
    pub fn warn_count(&self) -> usize {
        self.deps
            .iter()
            .filter(|d| d.status == DepCheckStatus::Warn)
            .count()
    }
    pub fn error_count(&self) -> usize {
        self.deps
            .iter()
            .filter(|d| d.status == DepCheckStatus::Error)
            .count()
    }
    pub fn has_blocker(&self) -> bool {
        self.deps
            .iter()
            .any(|d| d.required && d.status == DepCheckStatus::Error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DepCheckStatus {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct DepStatus {
    /// Stable identifier — e.g. "ffmpeg", "python3.11", "fish-speech-weights".
    pub name: &'static str,
    /// True if the dep is required for `voiceforge install-cloning` to
    /// finish; false for optional / nice-to-have. `has_blocker()` only
    /// counts required deps.
    pub required: bool,
    pub status: DepCheckStatus,
    /// Human-readable one-line detail (e.g. "/opt/homebrew/bin/ffmpeg
    /// (version 6.1)" or "not found on PATH").
    pub detail: String,
    /// How to fix it. `None` when there's nothing actionable
    /// (status == Ok) or when the dep is so foundational no automated
    /// remediation makes sense.
    pub remediation: Option<Remediation>,
}

/// Per-OS remediation commands. **Data, not formatted strings** so
/// the renderer can pick the right command for the user's OS.
/// Adding Linux distro support in v0.4.1 is a fill-in-the-blanks
/// exercise on this struct, not a structural rewrite.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Remediation {
    /// Shell command to run on macOS (typically a `brew install ...`).
    pub macos: Option<&'static str>,
    /// Shell command to run on Debian/Ubuntu (`apt install ...`).
    pub linux_apt: Option<&'static str>,
    /// Shell command to run on Fedora/RHEL (`dnf install ...`).
    pub linux_dnf: Option<&'static str>,
    /// Last-resort URL with manual install instructions. Used when
    /// no package manager has the dep, or for things like Xcode CLT
    /// that aren't `brew install`-able.
    pub manual_url: Option<&'static str>,
}

impl Remediation {
    /// Pick the right remediation command for the current OS.
    /// Returns `None` if no remediation is available for this OS.
    pub fn for_current_os(&self) -> Option<&'static str> {
        if cfg!(target_os = "macos") {
            self.macos.or(self.manual_url)
        } else if cfg!(target_os = "linux") {
            // Try apt first (Debian/Ubuntu — more common), then dnf,
            // then fall back to manual.
            self.linux_apt.or(self.linux_dnf).or(self.manual_url)
        } else {
            self.manual_url
        }
    }
}

// ============================================================================
// Public entry point
// ============================================================================

/// Run the full dep audit and return a structured report. Cheap —
/// each check is a single `which` / `path.exists()` / version-probe;
/// total < 100ms.
pub fn audit_dependencies() -> DepAuditReport {
    let deps = vec![
        check_package_manager(),
        check_python_3_11(),
        check_ffmpeg6(),
        check_yt_dlp(),
        check_poppler_utils(),
        check_xcode_clt(),
        check_fish_speech_venv(),
        check_fish_speech_weights(),
        check_whisper_model(),
        check_caffeinate(),
    ];
    DepAuditReport {
        schema_version: SCHEMA_VERSION,
        voiceforge_version: env!("CARGO_PKG_VERSION"),
        deps,
    }
}

// ============================================================================
// Per-dep checks
// ============================================================================

fn check_package_manager() -> DepStatus {
    if cfg!(target_os = "macos") {
        match which_cmd("brew") {
            Some(p) => DepStatus {
                name: "package manager",
                required: true,
                status: DepCheckStatus::Ok,
                detail: format!("brew at {}", p.display()),
                remediation: None,
            },
            None => DepStatus {
                name: "package manager",
                required: true,
                status: DepCheckStatus::Error,
                detail: "brew not on PATH".into(),
                remediation: Some(Remediation {
                    manual_url: Some("https://brew.sh"),
                    ..Default::default()
                }),
            },
        }
    } else if cfg!(target_os = "linux") {
        if which_cmd("apt-get").is_some() {
            DepStatus {
                name: "package manager",
                required: true,
                status: DepCheckStatus::Ok,
                detail: "apt-get found".into(),
                remediation: None,
            }
        } else if which_cmd("dnf").is_some() {
            DepStatus {
                name: "package manager",
                required: true,
                status: DepCheckStatus::Ok,
                detail: "dnf found".into(),
                remediation: None,
            }
        } else {
            DepStatus {
                name: "package manager",
                required: true,
                status: DepCheckStatus::Warn,
                detail: "neither apt-get nor dnf on PATH — pip-only install".into(),
                remediation: None,
            }
        }
    } else {
        DepStatus {
            name: "package manager",
            required: false,
            status: DepCheckStatus::Warn,
            detail: "voiceforge only supports macOS + Linux".into(),
            remediation: None,
        }
    }
}

fn check_python_3_11() -> DepStatus {
    // Try common locations: brew prefix, pyenv shim, mise shim, PATH.
    for cmd in ["python3.11", "python3"] {
        if let Some(p) = which_cmd(cmd) {
            return DepStatus {
                name: "python3.11",
                required: true,
                status: DepCheckStatus::Ok,
                detail: format!("{} at {}", cmd, p.display()),
                remediation: None,
            };
        }
    }
    DepStatus {
        name: "python3.11",
        required: true,
        status: DepCheckStatus::Error,
        detail: "python3.11 not on PATH".into(),
        remediation: Some(Remediation {
            macos: Some("brew install python@3.11"),
            linux_apt: Some("sudo apt install python3.11"),
            linux_dnf: Some("sudo dnf install python3.11"),
            manual_url: Some("https://www.python.org/downloads/"),
        }),
    }
}

fn check_ffmpeg6() -> DepStatus {
    // We want ffmpeg version 6 specifically because fish-speech
    // needs it. ffmpeg@6 may live at `/opt/homebrew/opt/ffmpeg@6/bin`
    // (brew keg-only) rather than on PATH.
    let ffmpeg = which_cmd("ffmpeg").or_else(|| {
        let candidate = PathBuf::from("/opt/homebrew/opt/ffmpeg@6/bin/ffmpeg");
        if candidate.is_file() {
            Some(candidate)
        } else {
            None
        }
    });
    match ffmpeg {
        Some(p) => DepStatus {
            name: "ffmpeg",
            required: true,
            status: DepCheckStatus::Ok,
            detail: format!("at {}", p.display()),
            remediation: None,
        },
        None => DepStatus {
            name: "ffmpeg",
            required: true,
            status: DepCheckStatus::Error,
            detail: "ffmpeg not on PATH (need version 6+ for fish-speech)".into(),
            remediation: Some(Remediation {
                macos: Some("brew install ffmpeg@6"),
                linux_apt: Some("sudo apt install ffmpeg"),
                linux_dnf: Some("sudo dnf install ffmpeg"),
                manual_url: Some("https://ffmpeg.org/download.html"),
            }),
        },
    }
}

fn check_yt_dlp() -> DepStatus {
    match which_cmd("yt-dlp") {
        Some(p) => DepStatus {
            name: "yt-dlp",
            required: true,
            status: DepCheckStatus::Ok,
            detail: format!("at {}", p.display()),
            remediation: None,
        },
        None => DepStatus {
            name: "yt-dlp",
            required: true,
            status: DepCheckStatus::Error,
            detail: "yt-dlp not on PATH (needed for `voiceforge clone <youtube-url>`)".into(),
            remediation: Some(Remediation {
                macos: Some("brew install yt-dlp"),
                linux_apt: Some("sudo apt install yt-dlp"),
                linux_dnf: Some("sudo dnf install yt-dlp"),
                manual_url: Some("https://github.com/yt-dlp/yt-dlp#installation"),
            }),
        },
    }
}

fn check_poppler_utils() -> DepStatus {
    // `pdftotext` is part of poppler-utils. Required for v0.4.1 PDF
    // intake; pre-installed in v0.4 to avoid second install.
    match which_cmd("pdftotext") {
        Some(p) => DepStatus {
            name: "poppler-utils",
            required: false,
            status: DepCheckStatus::Ok,
            detail: format!("pdftotext at {}", p.display()),
            remediation: None,
        },
        None => DepStatus {
            name: "poppler-utils",
            required: false,
            status: DepCheckStatus::Warn,
            detail: "pdftotext not on PATH (needed for `voiceforge note --in book.pdf` in v0.4.1)"
                .into(),
            remediation: Some(Remediation {
                macos: Some("brew install poppler"),
                linux_apt: Some("sudo apt install poppler-utils"),
                linux_dnf: Some("sudo dnf install poppler-utils"),
                manual_url: None,
            }),
        },
    }
}

fn check_xcode_clt() -> DepStatus {
    if !cfg!(target_os = "macos") {
        return DepStatus {
            name: "xcode CLT",
            required: false,
            status: DepCheckStatus::Ok,
            detail: "not applicable on this platform".into(),
            remediation: None,
        };
    }
    // `xcode-select -p` returns the active developer dir; non-zero
    // exit means CLT not installed. We can't easily run subprocesses
    // here without an async runtime, so check the conventional path.
    let conventional = PathBuf::from("/Library/Developer/CommandLineTools");
    if conventional.is_dir() {
        DepStatus {
            name: "xcode CLT",
            required: true,
            status: DepCheckStatus::Ok,
            detail: format!("at {}", conventional.display()),
            remediation: None,
        }
    } else {
        DepStatus {
            name: "xcode CLT",
            required: true,
            status: DepCheckStatus::Error,
            detail: "Xcode Command Line Tools not installed (needed for whisper compile deps)"
                .into(),
            remediation: Some(Remediation {
                macos: Some("xcode-select --install"),
                manual_url: Some(
                    "https://developer.apple.com/download/all/?q=command%20line%20tools",
                ),
                ..Default::default()
            }),
        }
    }
}

fn check_fish_speech_venv() -> DepStatus {
    let venv_python = home_path("cloning/venv/bin/python");
    if venv_python.is_file() {
        DepStatus {
            name: "fish-speech venv",
            required: true,
            status: DepCheckStatus::Ok,
            detail: format!("at {}", venv_python.display()),
            remediation: None,
        }
    } else {
        DepStatus {
            name: "fish-speech venv",
            required: true,
            status: DepCheckStatus::Error,
            detail: "venv not created — run install-cloning".into(),
            remediation: Some(Remediation {
                macos: Some("voiceforge install-cloning"),
                linux_apt: Some("voiceforge install-cloning"),
                linux_dnf: Some("voiceforge install-cloning"),
                manual_url: None,
            }),
        }
    }
}

fn check_fish_speech_weights() -> DepStatus {
    let weights_dir = home_path("cloning/weights/fish-speech");
    if weights_dir.is_dir() {
        DepStatus {
            name: "fish-speech weights",
            required: true,
            status: DepCheckStatus::Ok,
            detail: format!("at {}", weights_dir.display()),
            remediation: None,
        }
    } else {
        DepStatus {
            name: "fish-speech weights",
            required: true,
            status: DepCheckStatus::Error,
            detail: "weights not downloaded (~10 GB) — run install-cloning".into(),
            remediation: Some(Remediation {
                macos: Some("voiceforge install-cloning"),
                linux_apt: Some("voiceforge install-cloning"),
                linux_dnf: Some("voiceforge install-cloning"),
                manual_url: None,
            }),
        }
    }
}

fn check_whisper_model() -> DepStatus {
    let model_path = home_path("cloning/whisper/medium.pt");
    if model_path.is_file() {
        DepStatus {
            name: "whisper model",
            required: true,
            status: DepCheckStatus::Ok,
            detail: format!("medium model at {}", model_path.display()),
            remediation: None,
        }
    } else {
        DepStatus {
            name: "whisper model",
            required: true,
            status: DepCheckStatus::Error,
            detail: "whisper medium model not downloaded (~1.5 GB) — run install-cloning".into(),
            remediation: Some(Remediation {
                macos: Some("voiceforge install-cloning"),
                linux_apt: Some("voiceforge install-cloning"),
                linux_dnf: Some("voiceforge install-cloning"),
                manual_url: None,
            }),
        }
    }
}

fn check_caffeinate() -> DepStatus {
    if !cfg!(target_os = "macos") {
        return DepStatus {
            name: "caffeinate",
            required: false,
            status: DepCheckStatus::Ok,
            detail: "not applicable on this platform".into(),
            remediation: None,
        };
    }
    match which_cmd("caffeinate") {
        Some(_) => DepStatus {
            name: "caffeinate",
            required: false,
            status: DepCheckStatus::Ok,
            detail: "available (prevents sleep during long synth runs)".into(),
            remediation: None,
        },
        None => DepStatus {
            name: "caffeinate",
            required: false,
            status: DepCheckStatus::Warn,
            detail: "caffeinate not found — laptop may sleep mid-synth".into(),
            remediation: None,
        },
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Find a command on `PATH`. Returns the resolved path or `None`.
/// We use `which::which` (already a dep) so the lookup respects the
/// caller's actual `PATH`, including non-standard brew prefixes,
/// pyenv shims, mise shims, etc.
fn which_cmd(cmd: &str) -> Option<PathBuf> {
    which::which(cmd).ok()
}

/// Resolve a path under the voiceforge home dir. Falls back to
/// `~/.voiceforge/<suffix>` if the env var is unset.
fn home_path(suffix: &str) -> PathBuf {
    match crate::paths::user_home() {
        Some(home) => home.join(suffix),
        None => PathBuf::from(format!("~/.voiceforge/{suffix}")),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remediation_for_current_os_picks_macos_on_macos() {
        let r = Remediation {
            macos: Some("brew install foo"),
            linux_apt: Some("apt install foo"),
            ..Default::default()
        };
        let got = r.for_current_os();
        if cfg!(target_os = "macos") {
            assert_eq!(got, Some("brew install foo"));
        } else if cfg!(target_os = "linux") {
            assert_eq!(got, Some("apt install foo"));
        }
    }

    #[test]
    fn remediation_for_current_os_falls_back_through_chain() {
        // No macos, no linux_apt — should reach linux_dnf on Linux,
        // and manual_url on macOS.
        let r = Remediation {
            macos: None,
            linux_apt: None,
            linux_dnf: Some("dnf install foo"),
            manual_url: Some("https://example/install"),
        };
        let got = r.for_current_os();
        if cfg!(target_os = "macos") {
            assert_eq!(got, Some("https://example/install"));
        } else if cfg!(target_os = "linux") {
            assert_eq!(got, Some("dnf install foo"));
        }
    }

    #[test]
    fn remediation_for_current_os_returns_none_when_empty() {
        let r = Remediation::default();
        assert_eq!(r.for_current_os(), None);
    }

    #[test]
    fn audit_dependencies_returns_all_known_deps() {
        let report = audit_dependencies();
        let names: Vec<&str> = report.deps.iter().map(|d| d.name).collect();
        // Whatever the machine state, every dep MUST appear in the
        // report — they're either Ok, Warn, or Error, never absent.
        for required_name in [
            "package manager",
            "python3.11",
            "ffmpeg",
            "yt-dlp",
            "fish-speech venv",
            "fish-speech weights",
            "whisper model",
        ] {
            assert!(
                names.contains(&required_name),
                "missing dep {required_name:?} in {names:?}"
            );
        }
    }

    #[test]
    fn audit_report_count_helpers_match_status_distribution() {
        let report = DepAuditReport {
            schema_version: SCHEMA_VERSION,
            voiceforge_version: "test",
            deps: vec![
                DepStatus {
                    name: "ok-thing",
                    required: true,
                    status: DepCheckStatus::Ok,
                    detail: "".into(),
                    remediation: None,
                },
                DepStatus {
                    name: "warn-thing",
                    required: false,
                    status: DepCheckStatus::Warn,
                    detail: "".into(),
                    remediation: None,
                },
                DepStatus {
                    name: "error-required",
                    required: true,
                    status: DepCheckStatus::Error,
                    detail: "".into(),
                    remediation: None,
                },
                DepStatus {
                    name: "error-optional",
                    required: false,
                    status: DepCheckStatus::Error,
                    detail: "".into(),
                    remediation: None,
                },
            ],
        };
        assert_eq!(report.ok_count(), 1);
        assert_eq!(report.warn_count(), 1);
        assert_eq!(report.error_count(), 2);
        assert!(
            report.has_blocker(),
            "has_blocker must be true when a required dep is Error"
        );
    }

    #[test]
    fn audit_report_has_blocker_false_when_only_optional_errors() {
        let report = DepAuditReport {
            schema_version: SCHEMA_VERSION,
            voiceforge_version: "test",
            deps: vec![DepStatus {
                name: "optional-error",
                required: false,
                status: DepCheckStatus::Error,
                detail: "".into(),
                remediation: None,
            }],
        };
        assert!(!report.has_blocker(), "optional errors are not blockers");
    }

    #[test]
    fn audit_report_serializes_to_versioned_json() {
        let report = audit_dependencies();
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert!(json["deps"].is_array());
        // Wire format check: each dep has the documented shape.
        for dep in json["deps"].as_array().unwrap() {
            assert!(dep["name"].is_string());
            assert!(dep["required"].is_boolean());
            assert!(matches!(
                dep["status"].as_str(),
                Some("ok") | Some("warn") | Some("error")
            ));
            assert!(dep["detail"].is_string());
        }
    }
}
