//! `voiceforge doctor` — system health check.
//!
//! Returns a `DoctorReport` of named checks; renders to a text table
//! or stable, schema-versioned JSON. Exit code: 0 on no errors, 1 on
//! any `CheckStatus::Error`.

use anyhow::Result;
use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

use crate::config;
use crate::install_cloning;
use crate::paths;
use crate::tts::Backend;

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    pub voiceforge_version: &'static str,
    pub checks: Vec<Check>,
}

impl DoctorReport {
    pub fn has_error(&self) -> bool {
        self.checks.iter().any(|c| c.status == CheckStatus::Error)
    }

    pub fn count(&self, status: CheckStatus) -> usize {
        self.checks.iter().filter(|c| c.status == status).count()
    }
}

pub async fn run_doctor() -> DoctorReport {
    let mut checks = Vec::new();

    checks.push(check_binary_path());
    checks.push(check_home());
    checks.push(check_audio());
    checks.push(check_embedded_tts().await);
    checks.push(check_python_server().await);
    checks.push(check_cache());
    checks.push(check_presets());
    checks.push(check_config_toml());
    checks.push(check_ffmpeg().await);
    checks.push(check_yt_dlp().await);
    checks.push(check_cloning());
    checks.push(check_daemon_socket().await);
    checks.push(check_notification_bridge());
    checks.push(check_reaction_provider().await);
    checks.push(check_casts());

    DoctorReport {
        schema_version: SCHEMA_VERSION,
        voiceforge_version: env!("CARGO_PKG_VERSION"),
        checks,
    }
}

// -- check helpers ----------------------------------------------------

fn ok(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        status: CheckStatus::Ok,
        detail: detail.into(),
    }
}

fn warn(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        status: CheckStatus::Warn,
        detail: detail.into(),
    }
}

fn err(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        status: CheckStatus::Error,
        detail: detail.into(),
    }
}

fn check_binary_path() -> Check {
    match std::env::current_exe() {
        Ok(p) => ok("binary", p.display().to_string()),
        Err(e) => warn("binary", format!("could not resolve current_exe: {e}")),
    }
}

fn check_home() -> Check {
    let Some(home) = paths::user_home() else {
        return warn("home", "$VOICEFORGE_HOME and $HOME both unset");
    };
    if !home.exists() {
        return warn(
            "home",
            format!(
                "{} does not exist (will be created on next bootstrap)",
                home.display()
            ),
        );
    }
    if !home.is_dir() {
        return err(
            "home",
            format!("{} exists but is not a directory", home.display()),
        );
    }
    ok("home", home.display().to_string())
}

fn check_audio() -> Check {
    match rodio::OutputStream::try_default() {
        Ok(_) => ok("audio backend", "rodio default output"),
        Err(e) => warn("audio backend", format!("no default output device: {e}")),
    }
}

async fn check_embedded_tts() -> Check {
    let backend = match Backend::for_current_os() {
        Ok(b) => b,
        Err(e) => return warn("embedded TTS", format!("{e:#}")),
    };
    let bin = match &backend {
        Backend::MacosSay => "say",
        Backend::LinuxEspeak => "espeak-ng",
        Backend::Unsupported(name) => {
            return warn("embedded TTS", format!("unsupported OS: {name}"));
        }
    };
    match which_async(bin).await {
        Some(path) => ok("embedded TTS", format!("{bin} ({})", path.display())),
        None => warn(
            "embedded TTS",
            format!("{bin} not on PATH (server is the only option)"),
        ),
    }
}

async fn check_python_server() -> Check {
    let url =
        std::env::var("VOICEFORGE_TTS_URL").unwrap_or_else(|_| "http://127.0.0.1:5555".to_string());
    let health = format!("{}/health", url.trim_end_matches('/'));

    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(1))
        .build()
    {
        Ok(c) => c,
        Err(e) => return warn("python server", format!("could not build http client: {e}")),
    };

    match client.get(&health).send().await {
        Ok(resp) if resp.status().is_success() => {
            // Verify the body matches; catches "wrong server on the right port."
            match resp.json::<serde_json::Value>().await {
                Ok(v) if v.get("status").and_then(|s| s.as_str()) == Some("ok") => {
                    ok("python server", format!("running at {url}"))
                }
                Ok(_) => warn(
                    "python server",
                    format!("{health} responded but body shape is wrong"),
                ),
                Err(e) => warn(
                    "python server",
                    format!("{health} responded but body did not parse: {e}"),
                ),
            }
        }
        Ok(resp) => warn("python server", format!("{health} → {}", resp.status())),
        Err(_) => warn(
            "python server",
            format!("not reachable at {url} (opt-in; only needed for cloning)"),
        ),
    }
}

fn check_cache() -> Check {
    let Some(home) = paths::user_home() else {
        return warn("cache", "no $HOME");
    };
    let cache = home.join("cache");
    if !cache.is_dir() {
        return ok("cache", format!("{} (not yet created)", cache.display()));
    }
    let mut count = 0u64;
    let mut bytes = 0u64;
    if let Ok(entries) = std::fs::read_dir(&cache) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    count += 1;
                    bytes += meta.len();
                }
            }
        }
    }
    ok(
        "cache",
        format!(
            "{} ({} files, {})",
            cache.display(),
            count,
            human_bytes(bytes)
        ),
    )
}

fn check_presets() -> Check {
    match config::load_presets() {
        Ok(presets) if presets.is_empty() => warn("presets", "no presets installed"),
        Ok(presets) => ok("presets", format!("{} installed", presets.len())),
        Err(e) => err("presets", format!("could not load presets: {e:#}")),
    }
}

fn check_config_toml() -> Check {
    let Some(home) = paths::user_home() else {
        return warn("config.toml", "no $HOME");
    };
    let path = home.join("config.toml");
    if !path.is_file() {
        return warn("config.toml", format!("{} not yet created", path.display()));
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) => {
            return err(
                "config.toml",
                format!("could not read {}: {e}", path.display()),
            )
        }
    };
    // Pull out active_voice with a tiny line scan — avoids dragging
    // `toml` in just for one field. When config grows, swap to a
    // proper parser.
    let active = raw.lines().find_map(|l| {
        // Token-equality, not prefix-match — otherwise a hypothetical
        // `active_voice_backup = "x"` would shadow the real key.
        l.split_once('=').and_then(|(k, v)| {
            if k.trim() == "active_voice" {
                Some(v.trim().trim_matches('"').to_string())
            } else {
                None
            }
        })
    });
    let Some(active) = active else {
        return warn(
            "config.toml",
            format!("{} has no active_voice", path.display()),
        );
    };
    match config::load_presets() {
        Ok(presets) if presets.iter().any(|p| p.id == active) => {
            ok("config.toml", format!("active_voice = \"{active}\""))
        }
        Ok(_) => err(
            "config.toml",
            format!("active_voice \"{active}\" does not match any installed preset"),
        ),
        Err(e) => err("config.toml", format!("preset load failed: {e:#}")),
    }
}

/// Probes `~/.voiceforge/voiceforge.sock` to report daemon liveness.
/// Reuses `daemon_server::probe_socket` so the timeout matches what
/// the daemon itself uses for stale-detect — otherwise doctor would
/// report `not running` under transient load and confuse users.
async fn check_daemon_socket() -> Check {
    let path = match crate::daemon_server::DaemonConfig::default_path() {
        Ok(p) => p,
        Err(e) => return warn("daemon", format!("could not resolve socket path: {e:#}")),
    };
    if !path.exists() {
        return ok("daemon", format!("not running ({} absent)", path.display()));
    }
    match crate::daemon_server::probe_socket(&path).await {
        Ok(true) => ok("daemon", format!("running at {}", path.display())),
        Ok(false) => warn(
            "daemon",
            format!(
                "stale socket file at {} (no listener); will be cleaned up on next `voiceforge daemon`",
                path.display()
            ),
        ),
        Err(e) => warn(
            "daemon",
            format!("probe failed at {}: {e:#}", path.display()),
        ),
    }
}

/// ROADMAP 3.5: macOS Notification Center bridge.
///
/// On macOS, reports whether `osascript` is on PATH and whether the
/// `VOICEFORGE_MIRROR_NOTIFICATIONS` env var is set. On other
/// platforms, reports "not applicable".
fn check_notification_bridge() -> Check {
    if !cfg!(target_os = "macos") {
        return ok("notification bridge", "not applicable on this platform");
    }
    let env_state = std::env::var("VOICEFORGE_MIRROR_NOTIFICATIONS")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| format!("set to {v:?}"))
        .unwrap_or_else(|| "unset (mirroring off)".to_string());
    let osascript_present = which::which("osascript").is_ok();
    if !osascript_present {
        return warn(
            "notification bridge",
            format!("osascript not on PATH; env: {env_state}"),
        );
    }
    ok(
        "notification bridge",
        format!("osascript on PATH; env: {env_state}"),
    )
}

/// ROADMAP 4.1: report active reaction provider + LLM endpoint state.
///
/// Probes only TCP+TLS reachability of `VOICEFORGE_LLM_URL` (no chat
/// completion call — would cost tokens and might be slow). Reports
/// active provider name + timeout + strict mode.
async fn check_reaction_provider() -> Check {
    // Construct the actual prod provider so `name()` reflects what
    // the daemon would use. Cheap — no network call here.
    use std::sync::Arc;
    let rules = Arc::new(crate::rules::Rules::default_builtin());
    let casts =
        Arc::new(crate::cast::Casts::load().unwrap_or_else(|_| crate::cast::Casts::empty()));
    let provider = crate::reaction::select_provider(Arc::clone(&rules), Arc::clone(&casts));
    let provider_name = provider.name();

    let url = std::env::var("VOICEFORGE_LLM_URL")
        .ok()
        .filter(|v| !v.trim().is_empty());

    let Some(url) = url else {
        return ok("reaction provider", format!("{provider_name} (rules.json)"));
    };

    let timeout_ms = std::env::var("VOICEFORGE_LLM_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(2000);
    let strict = matches!(
        std::env::var("VOICEFORGE_LLM_STRICT").as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    );
    let has_api_key = std::env::var("VOICEFORGE_LLM_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_some()
        || std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_some();

    let parsed = match reqwest::Url::parse(&url) {
        Ok(u) => u,
        Err(e) => {
            return warn(
                "reaction provider",
                format!(
                    "llm{} ({}ms timeout, key={}); URL unparseable: {e}",
                    if strict { "-strict" } else { "" },
                    timeout_ms,
                    if has_api_key { "set" } else { "unset" }
                ),
            );
        }
    };

    let host = parsed.host_str().unwrap_or("?");
    let port = parsed.port_or_known_default().unwrap_or(0);
    let reachable = tokio::time::timeout(
        std::time::Duration::from_millis(1000),
        tokio::net::TcpStream::connect(format!("{host}:{port}")),
    )
    .await
    .ok()
    .and_then(|r| r.ok())
    .is_some();

    let detail = format!(
        "{provider_name} -> {host}:{port} ({}ms timeout, key={}, host {})",
        timeout_ms,
        if has_api_key { "set" } else { "unset" },
        if reachable {
            "reachable"
        } else {
            "UNREACHABLE"
        },
    );
    if reachable {
        ok("reaction provider", detail)
    } else {
        warn("reaction provider", detail)
    }
}

/// ROADMAP 4.3: report configured multi-voice casts. Previews each
/// event's cast inline so users can sanity-check casts.toml without
/// running the daemon.
fn check_casts() -> Check {
    let casts = match crate::cast::Casts::load() {
        Ok(c) => c,
        Err(e) => return warn("casts", format!("failed to load casts.toml: {e:#}")),
    };
    if casts.is_empty() {
        return ok("casts", "0 (no casts.toml)".to_string());
    }
    let mut entries: Vec<String> = casts
        .iter()
        .map(|(event, cfg)| format!("{event}=[{}]", cfg.voices.join(",")))
        .collect();
    entries.sort();
    let source = casts
        .source()
        .map(|p| format!(" from {}", p.display()))
        .unwrap_or_default();
    let mut detail = format!("{} ({}){source}", casts.len(), entries.join(", "));
    // Mismatch warning: casts only fire through the LLM provider.
    use std::sync::Arc;
    let rules = Arc::new(crate::rules::Rules::default_builtin());
    let provider = crate::reaction::select_provider(Arc::clone(&rules), Arc::new(casts));
    if provider.name() == "static" {
        detail.push_str(" — WARNING: no LLM provider configured; casts will not fire");
        return warn("casts", detail);
    }
    ok("casts", detail)
}

fn check_cloning() -> Check {
    use install_cloning::InstallStateAny;

    let state = match install_cloning::read_install_state_any() {
        Ok(s) => s,
        Err(_) => {
            return warn(
                "cloning",
                "not installed — run `voiceforge install-cloning` to enable voice cloning",
            );
        }
    };

    // If a v1 backup is present alongside a v2 install the user upgraded
    // from GPT-SoVITS; surface a migration hint so any cloned voices
    // they had on the old engine don't silently break. The backup file
    // is written by scripts/install_cloning_fish.sh step "v1 marker
    // backup" before the v2 install proceeds.
    let migration_hint = match &state {
        InstallStateAny::V2(_) if install_cloning::read_v1_backup_raw().is_some() => {
            Some(" (v1 backup present at INSTALLED.v1.bak — old GPT-SoVITS voices need migration)")
        }
        _ => None,
    };

    let detail = match &state {
        InstallStateAny::V1(s) => format!(
            "GPT-SoVITS @ {} (ffmpeg6: {})",
            &s.gpt_sovits_sha[..s.gpt_sovits_sha.len().min(7)],
            s.ffmpeg6_prefix
        ),
        InstallStateAny::V2(s) => {
            let sha7 = &s.fish_speech_sha[..s.fish_speech_sha.len().min(7)];
            let whisper = if s.whisper_model.is_empty() {
                String::new()
            } else {
                format!(", whisper: {}", s.whisper_model)
            };
            format!(
                "fish-speech S2 Pro @ {sha7} (ffmpeg6: {}{whisper})",
                s.ffmpeg6_prefix
            )
        }
    };
    let detail = match migration_hint {
        Some(hint) => format!("{detail}{hint}"),
        None => detail,
    };
    ok("cloning", detail)
}

async fn check_ffmpeg() -> Check {
    match Command::new("ffmpeg")
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
    {
        Ok(s) if s.success() => match which_async("ffmpeg").await {
            Some(p) => ok("ffmpeg", p.display().to_string()),
            None => ok("ffmpeg", "on PATH"),
        },
        Ok(_) => warn("ffmpeg", "exited non-zero"),
        Err(_) => warn(
            "ffmpeg",
            "not on PATH (only needed for `voiceforge ingest`)",
        ),
    }
}

/// Optional dependency for `voiceforge clone <URL>` and
/// `voiceforge ingest <URL>` (ROADMAP 2.3 URL ingest). Warn (not
/// error) when missing — local-file ingest still works.
async fn check_yt_dlp() -> Check {
    match which_async("yt-dlp").await {
        Some(p) => ok("yt-dlp", p.display().to_string()),
        None => warn(
            "yt-dlp",
            "not on PATH (only needed for URL ingest; install via `brew install yt-dlp` or `pipx install yt-dlp`)",
        ),
    }
}

async fn which_async(bin: &str) -> Option<PathBuf> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {}", shell_quote(bin)))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn human_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let n = n as f64;
    if n >= GB {
        format!("{:.1} GB", n / GB)
    } else if n >= MB {
        format!("{:.1} MB", n / MB)
    } else if n >= KB {
        format!("{:.1} KB", n / KB)
    } else {
        format!("{n} B")
    }
}

// -- rendering --------------------------------------------------------

pub fn render_human<W: Write>(report: &DoctorReport, w: &mut W) -> std::io::Result<()> {
    writeln!(
        w,
        "voiceforge {} — system check\n",
        report.voiceforge_version
    )?;

    let name_w = report
        .checks
        .iter()
        .map(|c| c.name.len())
        .max()
        .unwrap_or(0);
    for check in &report.checks {
        let tag = match check.status {
            CheckStatus::Ok => "[OK]",
            CheckStatus::Warn => "[WARN]",
            CheckStatus::Error => "[ERROR]",
        };
        writeln!(
            w,
            "{tag:<7} {name:<width$}  {detail}",
            tag = tag,
            name = check.name,
            width = name_w,
            detail = check.detail,
        )?;
    }

    writeln!(
        w,
        "\nok: {}   warn: {}   error: {}",
        report.count(CheckStatus::Ok),
        report.count(CheckStatus::Warn),
        report.count(CheckStatus::Error),
    )?;
    Ok(())
}

pub fn render_json<W: Write>(report: &DoctorReport, w: &mut W) -> Result<()> {
    serde_json::to_writer_pretty(&mut *w, report)?;
    writeln!(w)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn with_home<F: FnOnce(&std::path::Path)>(home: &std::path::Path, f: F) {
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

    #[tokio::test]
    #[serial]
    async fn report_has_expected_check_names() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), |_| {});
        std::env::set_var("VOICEFORGE_HOME", tmp.path());
        let report = run_doctor().await;
        let names: Vec<&str> = report.checks.iter().map(|c| c.name).collect();
        for expected in &[
            "binary",
            "home",
            "audio backend",
            "embedded TTS",
            "python server",
            "cache",
            "presets",
            "config.toml",
            "ffmpeg",
            "yt-dlp",
            "daemon",
        ] {
            assert!(
                names.contains(expected),
                "missing check {expected:?} in {names:?}"
            );
        }
        std::env::remove_var("VOICEFORGE_HOME");
    }

    #[tokio::test]
    #[serial]
    async fn json_output_round_trips_and_has_schema_version() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("VOICEFORGE_HOME", tmp.path());
        let report = run_doctor().await;
        let mut buf = Vec::new();
        render_json(&report, &mut buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert!(parsed["voiceforge_version"].is_string());
        assert!(parsed["checks"].is_array());
        // Check status enum serializes lowercase.
        for check in parsed["checks"].as_array().unwrap() {
            let status = check["status"].as_str().unwrap();
            assert!(
                matches!(status, "ok" | "warn" | "error"),
                "bad status: {status}"
            );
        }
        std::env::remove_var("VOICEFORGE_HOME");
    }

    #[tokio::test]
    #[serial]
    async fn human_output_includes_summary_line() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("VOICEFORGE_HOME", tmp.path());
        let report = run_doctor().await;
        let mut buf = Vec::new();
        render_human(&report, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("ok:"));
        assert!(s.contains("warn:"));
        assert!(s.contains("error:"));
        std::env::remove_var("VOICEFORGE_HOME");
    }

    #[test]
    #[serial]
    fn home_check_errors_when_path_is_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("voiceforge_is_a_file");
        std::fs::write(&bogus, b"not a dir").unwrap();
        std::env::set_var("VOICEFORGE_HOME", &bogus);
        let check = check_home();
        assert_eq!(check.status, CheckStatus::Error);
        std::env::remove_var("VOICEFORGE_HOME");
    }

    #[test]
    #[serial]
    fn config_toml_check_errors_on_unknown_active_voice() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::write(
            home.join("config.toml"),
            b"active_voice = \"this_voice_does_not_exist\"\n",
        )
        .unwrap();
        std::env::set_var("VOICEFORGE_HOME", home);
        let check = check_config_toml();
        assert_eq!(check.status, CheckStatus::Error);
        assert!(check.detail.contains("this_voice_does_not_exist"));
        std::env::remove_var("VOICEFORGE_HOME");
    }

    #[test]
    #[serial]
    fn config_toml_check_ignores_keys_that_only_share_a_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        // active_voice_backup must NOT shadow the real key.
        std::fs::write(
            home.join("config.toml"),
            b"active_voice_backup = \"this_voice_does_not_exist\"\nactive_voice = \"default\"\n",
        )
        .unwrap();
        std::env::set_var("VOICEFORGE_HOME", home);
        let check = check_config_toml();
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(check.detail.contains("default"));
        std::env::remove_var("VOICEFORGE_HOME");
    }

    #[test]
    #[serial]
    fn config_toml_check_ok_when_active_voice_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::write(home.join("config.toml"), b"active_voice = \"default\"\n").unwrap();
        std::env::set_var("VOICEFORGE_HOME", home);
        let check = check_config_toml();
        assert_eq!(check.status, CheckStatus::Ok);
        std::env::remove_var("VOICEFORGE_HOME");
    }

    #[tokio::test]
    #[serial]
    async fn python_server_warn_when_port_is_unreachable() {
        // Ephemeral-port trick: bind to :0, grab the port, drop the
        // listener so the port is now free. Deterministic, portable.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        std::env::set_var("VOICEFORGE_TTS_URL", format!("http://127.0.0.1:{port}"));
        let check = check_python_server().await;
        std::env::remove_var("VOICEFORGE_TTS_URL");

        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("not reachable") || check.detail.contains("connect"));
    }

    #[tokio::test]
    #[serial]
    async fn daemon_check_reports_not_running_when_socket_absent() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("VOICEFORGE_HOME", tmp.path());
        let check = check_daemon_socket().await;
        std::env::remove_var("VOICEFORGE_HOME");
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(
            check.detail.contains("not running"),
            "unexpected detail: {}",
            check.detail
        );
    }

    #[tokio::test]
    #[serial]
    async fn daemon_check_reports_stale_when_file_is_not_a_socket() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("VOICEFORGE_HOME", tmp.path());
        // Create a regular file at the socket path. probe_socket connects,
        // gets ECONNREFUSED / EOPNOTSUPP, returns Ok(false) → stale.
        std::fs::write(tmp.path().join("voiceforge.sock"), b"not a socket").unwrap();
        let check = check_daemon_socket().await;
        std::env::remove_var("VOICEFORGE_HOME");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.detail.contains("stale"),
            "unexpected detail: {}",
            check.detail
        );
    }
}
