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
        let l = l.trim();
        if l.starts_with("active_voice") {
            l.split('=')
                .nth(1)
                .map(|s| s.trim().trim_matches('"').to_string())
        } else {
            None
        }
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
}
