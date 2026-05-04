//! TTS engine facade.
//!
//! `Engine` holds three sub-engines (embedded, optional server,
//! optional cloning) and dispatches per-call based on which voice the
//! caller named. The same `Engine` instance can serve `default` →
//! embedded, `peter` (a cloned voice) → cloning, all in one process.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex as TokioMutex;

use crate::paths;
use crate::{install_cloning, voices};

/// Hard cap on synthesized text length. Beyond this, both `say` and
/// `espeak-ng` happily block for minutes on a megabyte of input. The
/// reaction lines this ships for are short.
const MAX_TEXT_LEN: usize = 10_000;

/// Process timeout for any single OS-native TTS subprocess.
const TTS_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct Engine {
    embedded: EmbeddedEngine,
    server: Option<ServerEngine>,
    cloning: Option<CloningEngine>,
}

impl Engine {
    pub async fn speak(&self, text: &str, voice: &str) -> Result<PathBuf> {
        if text.len() > MAX_TEXT_LEN {
            bail!(
                "TTS input too long ({} bytes, max {}); split or shorten before calling speak()",
                text.len(),
                MAX_TEXT_LEN
            );
        }
        if text.trim().is_empty() {
            bail!("TTS input is empty or whitespace-only");
        }

        // Per-call dispatch:
        //   1. cloned voice (and cloning installed)  → CloningEngine
        //   2. server URL set                        → ServerEngine
        //   3. otherwise                             → EmbeddedEngine
        if let Some(cloning) = &self.cloning {
            if voices::voice_exists(voice) {
                return cloning.speak(text, voice).await;
            }
        }
        if let Some(server) = &self.server {
            return server.speak(text, voice).await;
        }
        self.embedded.speak(text, voice).await
    }
}

/// Build an `Engine` for this process invocation. Always constructs the
/// embedded backend; conditionally adds server (when
/// `VOICEFORGE_TTS_URL` is set) and cloning (when the install marker
/// is present).
pub fn select_engine() -> Result<Engine> {
    let embedded = EmbeddedEngine::new()?;

    let server = std::env::var("VOICEFORGE_TTS_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .map(ServerEngine::new);

    let cloning = if install_cloning::is_installed() {
        Some(CloningEngine::new()?)
    } else {
        None
    };

    Ok(Engine {
        embedded,
        server,
        cloning,
    })
}

// ---- Server -----------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ServerEngine {
    base_url: String,
}

#[derive(Debug, Serialize)]
struct TtsRequest<'a> {
    text: &'a str,
    voice: &'a str,
}

#[derive(Debug, Deserialize)]
struct TtsResponse {
    audio_path: String,
    cache_hit: bool,
}

impl ServerEngine {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    pub async fn speak(&self, text: &str, voice: &str) -> Result<PathBuf> {
        let url = format!("{}/tts", self.base_url);
        let response = reqwest::Client::new()
            .post(&url)
            .json(&TtsRequest { text, voice })
            .send()
            .await
            .with_context(|| {
                format!(
                    "could not reach TTS server at {} (start services/tts-server/server.py or unset VOICEFORGE_TTS_URL for the embedded engine)",
                    self.base_url
                )
            })?;

        let status = response.status();
        if !status.is_success() {
            bail!("TTS server returned error status: {}", status);
        }

        let body: TtsResponse = response.json().await?;
        if body.cache_hit {
            println!("Using cached audio");
        }
        Ok(PathBuf::from(body.audio_path))
    }
}

// ---- Embedded ---------------------------------------------------------

/// Builds the OS-native synthesizer command. Default impl uses the
/// real `say` / `espeak-ng`; tests inject a fake to avoid spawning a
/// real synthesizer in CI.
pub type SynthBuilder = Box<dyn Fn(&Synth) -> Command + Send + Sync>;

/// Inputs the synth-builder closure needs to construct the command.
/// Fields are read inside user-provided closures so the compiler
/// can't see them; the `allow` is a real load-bearing one.
#[allow(dead_code)]
pub struct Synth<'a> {
    pub text: &'a str,
    pub voice: &'a str,
    pub output_aiff_or_wav: &'a Path,
}

pub struct EmbeddedEngine {
    cache_dir: PathBuf,
    backend: Backend,
    /// `None` = use the platform default (`say` on macOS, `espeak-ng`
    /// on Linux, error on others). Tests pass a fake here.
    synth_override: Option<SynthBuilder>,
}

impl std::fmt::Debug for EmbeddedEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedEngine")
            .field("cache_dir", &self.cache_dir)
            .field("backend", &self.backend)
            .field(
                "synth_override",
                &self.synth_override.as_ref().map(|_| "<closure>"),
            )
            .finish()
    }
}

impl EmbeddedEngine {
    pub fn new() -> Result<Self> {
        let home = paths::user_home()
            .ok_or_else(|| anyhow!("could not resolve $VOICEFORGE_HOME or $HOME"))?;
        let cache_dir = home.join("cache");
        let backend = Backend::for_current_os()?;
        Ok(Self {
            cache_dir,
            backend,
            synth_override: None,
        })
    }

    /// Test-only constructor: pin the cache dir + the backend tag +
    /// inject a fake synth-builder. The synth builder must spawn a
    /// process that writes a valid WAV to its `output_aiff_or_wav`
    /// path.
    #[cfg(test)]
    pub fn for_testing(cache_dir: PathBuf, backend: Backend, synth: SynthBuilder) -> Self {
        Self {
            cache_dir,
            backend,
            synth_override: Some(synth),
        }
    }

    pub fn cache_path_for(&self, text: &str, voice: &str) -> PathBuf {
        let key = cache_key(text, voice, self.backend.id());
        self.cache_dir.join(format!("{key}.wav"))
    }

    pub async fn speak(&self, text: &str, voice: &str) -> Result<PathBuf> {
        let out = self.cache_path_for(text, voice);
        if out.exists() {
            return Ok(out);
        }

        std::fs::create_dir_all(&self.cache_dir)
            .with_context(|| format!("could not create cache dir {}", self.cache_dir.display()))?;

        // Atomic write: synth into a sibling .tmp, then rename. A
        // SIGINT mid-synthesis can't poison the cache.
        let tmp_wav = out.with_extension("wav.tmp");
        // Best-effort cleanup of any prior aborted run.
        let _ = std::fs::remove_file(&tmp_wav);

        match (&self.synth_override, &self.backend) {
            (Some(builder), _) => {
                let synth = Synth {
                    text,
                    voice,
                    output_aiff_or_wav: &tmp_wav,
                };
                let mut cmd = builder(&synth);
                run_with_timeout(&mut cmd, "synth (test)").await?;
            }
            (None, Backend::MacosSay) => {
                let aiff = tmp_wav.with_extension("aiff.tmp");
                let _ = std::fs::remove_file(&aiff);

                let mut say = Command::new("say");
                say.arg("-o").arg(&aiff).arg("--").arg(text);
                run_with_timeout(&mut say, "say")
                    .await
                    .with_context(|| "macOS `say` failed (is it on PATH? it ships with macOS)")?;

                let mut afconvert = Command::new("afconvert");
                afconvert
                    .arg("-f")
                    .arg("WAVE")
                    .arg("-d")
                    .arg("LEI16")
                    .arg(&aiff)
                    .arg(&tmp_wav);
                let result = run_with_timeout(&mut afconvert, "afconvert").await;
                let _ = std::fs::remove_file(&aiff);
                result
                    .with_context(|| "afconvert failed (ships with macOS as part of CoreAudio)")?;
            }
            (None, Backend::LinuxEspeak) => {
                let mut espeak = Command::new("espeak-ng");
                espeak.arg("-w").arg(&tmp_wav).arg("--").arg(text);
                run_with_timeout(&mut espeak, "espeak-ng")
                    .await
                    .with_context(|| "espeak-ng failed (try `sudo apt install espeak-ng`)")?;
            }
            (None, Backend::Unsupported(name)) => {
                bail!(
                    "embedded TTS not yet implemented on {name} (ROADMAP 1.1.2). Set VOICEFORGE_TTS_URL to use the server path."
                );
            }
        }

        std::fs::rename(&tmp_wav, &out).with_context(|| {
            format!(
                "could not move synthesized audio into cache: {}",
                out.display()
            )
        })?;
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    MacosSay,
    LinuxEspeak,
    /// Carries the OS name for a clear error.
    Unsupported(String),
}

impl Backend {
    pub fn for_current_os() -> Result<Self> {
        if cfg!(target_os = "macos") {
            Ok(Self::MacosSay)
        } else if cfg!(target_os = "linux") {
            Ok(Self::LinuxEspeak)
        } else {
            Ok(Self::Unsupported(std::env::consts::OS.to_string()))
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::MacosSay => "embedded.macos.say",
            Self::LinuxEspeak => "embedded.linux.espeak",
            Self::Unsupported(_) => "embedded.unsupported",
        }
    }
}

/// Note: embedded keys are intentionally different from the Python
/// server's keys (server includes preset params: temperature, speed,
/// model_version — see `services/tts-server/server.py`'s `cache_key`).
/// The two caches live in disjoint roots (`~/.voiceforge/cache/` vs
/// `services/tts-server/audio_cache/`) and are independent by design.
fn cache_key(text: &str, voice: &str, backend_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher.update(b"|");
    hasher.update(voice.as_bytes());
    hasher.update(b"|");
    hasher.update(backend_id.as_bytes());
    hex::encode(hasher.finalize())
}

// ---- Cloning ---------------------------------------------------------
//
// Long-lived NDJSON child running scripts/cloning_synth.py inside the
// install-cloning venv. Spawned lazily on first cloned-voice call,
// reused for the rest of the process. Communicates one request per
// line on stdin, one response per line on stdout.

const CLONING_MODEL_LOAD_TIMEOUT: Duration = Duration::from_secs(60);
const CLONING_SYNTH_TIMEOUT: Duration = Duration::from_secs(120);

pub struct CloningEngine {
    cache_dir: PathBuf,
    install: install_cloning::InstallState,
    /// `None` until first request; populated lazily.
    child: Arc<TokioMutex<Option<SynthChild>>>,
}

struct SynthChild {
    /// Held only for `kill_on_drop` to keep the child alive for the
    /// SynthChild's lifetime — never read directly.
    #[allow(dead_code)]
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl std::fmt::Debug for CloningEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloningEngine")
            .field("cache_dir", &self.cache_dir)
            .field("install", &self.install)
            .field("child", &"<lazy>")
            .finish()
    }
}

impl CloningEngine {
    pub fn new() -> Result<Self> {
        let install = install_cloning::read_install_state()?;
        let home = paths::user_home()
            .ok_or_else(|| anyhow!("could not resolve $VOICEFORGE_HOME or $HOME"))?;
        let cache_dir = home.join("cache");
        Ok(Self {
            cache_dir,
            install,
            child: Arc::new(TokioMutex::new(None)),
        })
    }

    fn cache_path_for(&self, text: &str, voice: &str, profile_created_at: &str) -> PathBuf {
        let key = cloning_cache_key(text, voice, profile_created_at);
        self.cache_dir.join(format!("{key}.wav"))
    }

    pub async fn speak(&self, text: &str, voice: &str) -> Result<PathBuf> {
        let profile =
            voices::load_voice(voice).with_context(|| format!("loading cloned voice {voice}"))?;
        let out = self.cache_path_for(text, voice, &profile.created_at);

        if out.exists() {
            return Ok(out);
        }
        std::fs::create_dir_all(&self.cache_dir)
            .with_context(|| format!("could not create cache dir {}", self.cache_dir.display()))?;

        let mut guard = self.child.lock().await;
        if guard.is_none() {
            *guard = Some(self.spawn_child().await?);
        }
        let child = guard.as_mut().expect("just spawned");

        let req = serde_json::json!({
            "text": text,
            "voice": voice,
            "out": out.to_string_lossy(),
        });
        let line = format!("{req}\n");
        child
            .stdin
            .write_all(line.as_bytes())
            .await
            .context("writing to cloning_synth.py stdin")?;
        child.stdin.flush().await.context("flushing synth stdin")?;

        // Read until we see a synth-response line (sample_rate present)
        // or an error. Skip notification lines (loaded_seconds, ready).
        let started = Instant::now();
        loop {
            if started.elapsed() > CLONING_SYNTH_TIMEOUT {
                bail!("cloning synth timed out after {:?}", CLONING_SYNTH_TIMEOUT);
            }
            let line = match child.stdout_lines.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => bail!("cloning synth child closed stdout"),
                Err(e) => return Err(e).context("reading synth stdout"),
            };
            let v: serde_json::Value = serde_json::from_str(&line)
                .with_context(|| format!("parsing synth response: {line}"))?;
            if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
                let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
                bail!("synth failed: {err}");
            }
            // Synth response carries sample_rate; notifications don't.
            if v.get("sample_rate").is_some() {
                return Ok(out);
            }
            // else: it was a notification (ready / loaded_seconds);
            // keep reading.
        }
    }

    async fn spawn_child(&self) -> Result<SynthChild> {
        let python = install_cloning::cloning_venv_python()
            .ok_or_else(|| anyhow!("could not resolve cloning venv python"))?;
        let script = install_cloning::cloning_synth_script().ok_or_else(|| {
            anyhow!(
                "could not locate scripts/cloning_synth.py — install voiceforge from source for now (ROADMAP 1.7 will package it)"
            )
        })?;

        let mut cmd = tokio::process::Command::new(&python);
        cmd.arg(&script)
            .env(
                "DYLD_FALLBACK_LIBRARY_PATH",
                format!("{}/lib", self.install.ffmpeg6_prefix),
            )
            .env(
                "PYTHONPATH",
                format!("{repo}:{repo}/GPT_SoVITS", repo = self.install.repo_path),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning {} {}", python.display(), script.display()))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("synth child has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("synth child has no stdout"))?;
        let stdout_lines = BufReader::new(stdout).lines();

        let mut sc = SynthChild {
            child,
            stdin,
            stdout_lines,
        };

        // Wait for `{"ok": true, "ready": true}` (printed on startup).
        let started = Instant::now();
        loop {
            if started.elapsed() > CLONING_MODEL_LOAD_TIMEOUT {
                bail!(
                    "cloning synth child failed to print ready within {:?}",
                    CLONING_MODEL_LOAD_TIMEOUT
                );
            }
            let line = match sc.stdout_lines.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => bail!("cloning synth child exited before ready"),
                Err(e) => return Err(e).context("reading ready line"),
            };
            let v: serde_json::Value = serde_json::from_str(&line)
                .with_context(|| format!("parsing ready line: {line}"))?;
            if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
                let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
                bail!("synth child startup failed: {err}");
            }
            if v.get("ready").is_some() {
                break;
            }
            // else: keep reading
        }
        Ok(sc)
    }
}

impl Drop for CloningEngine {
    fn drop(&mut self) {
        // Best-effort: the tokio::Mutex doesn't have try_lock_owned, so
        // the child is killed by tokio::process::Child::kill_on_drop set
        // at spawn time. Nothing else to do here.
    }
}

fn cloning_cache_key(text: &str, voice: &str, created_at: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher.update(b"|");
    hasher.update(voice.as_bytes());
    hasher.update(b"|");
    hasher.update(created_at.as_bytes());
    hasher.update(b"|");
    hasher.update(b"cloning.gpt-sovits-v2");
    hex::encode(hasher.finalize())
}

// run_with_timeout MUST come before the #[cfg(test)] mod (clippy
// items_after_test_module). It's the last non-test item in this file.
async fn run_with_timeout(cmd: &mut Command, label: &str) -> Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("could not spawn {label}"))?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                let mut stderr = String::new();
                if let Some(mut s) = child.stderr.take() {
                    use tokio::io::AsyncReadExt;
                    let _ = s.read_to_string(&mut stderr).await;
                }
                bail!("{label} failed: {} (stderr: {})", status, stderr.trim());
            }
            Ok(None) => {
                if started.elapsed() >= TTS_TIMEOUT {
                    let _ = child.kill().await;
                    bail!("{label} timed out after {:?}", TTS_TIMEOUT);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => {
                let _ = child.kill().await;
                return Err(e).with_context(|| format!("{label} wait failed"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::process::Command;

    fn shell_escape(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    fn fake_synth(counter: Arc<AtomicUsize>) -> SynthBuilder {
        Box::new(move |s: &Synth| {
            counter.fetch_add(1, Ordering::SeqCst);
            let out = s.output_aiff_or_wav.to_owned();
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(format!(
                "printf 'RIFF\\0\\0\\0\\0WAVEfmt ' > {}",
                shell_escape(out.to_str().expect("utf8 path"))
            ));
            cmd
        })
    }

    fn engine_in(tmp: &Path, counter: Arc<AtomicUsize>) -> EmbeddedEngine {
        EmbeddedEngine::for_testing(tmp.to_path_buf(), Backend::MacosSay, fake_synth(counter))
    }

    #[tokio::test]
    async fn first_call_invokes_synth_and_writes_cache() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = Arc::new(AtomicUsize::new(0));
        let engine = engine_in(tmp.path(), counter.clone());

        let path = engine.speak("hello", "default").await.expect("speak");

        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(path.exists());
        assert!(path.starts_with(tmp.path()));
        assert_eq!(path.extension().and_then(|s| s.to_str()), Some("wav"));
    }

    #[tokio::test]
    async fn second_call_with_same_input_is_cache_hit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = Arc::new(AtomicUsize::new(0));
        let engine = engine_in(tmp.path(), counter.clone());

        let p1 = engine.speak("hello", "default").await.expect("first");
        let p2 = engine.speak("hello", "default").await.expect("second");

        assert_eq!(p1, p2);
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "synth not invoked on cache hit"
        );
    }

    #[tokio::test]
    async fn different_text_yields_different_cache_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = Arc::new(AtomicUsize::new(0));
        let engine = engine_in(tmp.path(), counter.clone());

        let a = engine.speak("first", "default").await.expect("a");
        let b = engine.speak("second", "default").await.expect("b");

        assert_ne!(a, b);
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn different_voice_yields_different_cache_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = Arc::new(AtomicUsize::new(0));
        let engine = engine_in(tmp.path(), counter.clone());

        let a = engine.speak("hi", "voice_one").await.expect("a");
        let b = engine.speak("hi", "voice_two").await.expect("b");

        assert_ne!(a, b);
    }

    fn facade_with(embedded: EmbeddedEngine) -> Engine {
        Engine {
            embedded,
            server: None,
            cloning: None,
        }
    }

    #[tokio::test]
    async fn rejects_too_long_text() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = Arc::new(AtomicUsize::new(0));
        let engine = facade_with(engine_in(tmp.path(), counter.clone()));

        let huge = "x".repeat(MAX_TEXT_LEN + 1);
        let err = engine.speak(&huge, "default").await.unwrap_err();
        assert!(format!("{err:#}").contains("too long"));
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rejects_empty_text() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = Arc::new(AtomicUsize::new(0));
        let engine = facade_with(engine_in(tmp.path(), counter.clone()));

        let err = engine.speak("   ", "default").await.unwrap_err();
        assert!(format!("{err:#}").contains("empty"));
    }

    #[tokio::test]
    async fn aborted_synth_does_not_poison_cache() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let synth: SynthBuilder = Box::new(|_s: &Synth| {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg("exit 1");
            cmd
        });
        let engine =
            EmbeddedEngine::for_testing(tmp.path().to_path_buf(), Backend::MacosSay, synth);

        let cache_path = engine.cache_path_for("hello", "default");
        let err = engine.speak("hello", "default").await.unwrap_err();
        assert!(format!("{err:#}").contains("synth"));
        assert!(
            !cache_path.exists(),
            "cache must not exist after failed synth"
        );
    }

    #[tokio::test]
    #[serial]
    async fn select_engine_includes_server_when_url_set() {
        let prev = std::env::var("VOICEFORGE_TTS_URL").ok();
        std::env::set_var("VOICEFORGE_TTS_URL", "http://example.invalid");
        let engine = select_engine().expect("select");
        assert!(
            engine.server.is_some(),
            "facade should include server backend"
        );
        match prev {
            Some(v) => std::env::set_var("VOICEFORGE_TTS_URL", v),
            None => std::env::remove_var("VOICEFORGE_TTS_URL"),
        }
    }

    #[tokio::test]
    #[serial]
    async fn select_engine_omits_server_when_url_unset() {
        let prev = std::env::var("VOICEFORGE_TTS_URL").ok();
        std::env::remove_var("VOICEFORGE_TTS_URL");
        let engine = select_engine().expect("select");
        assert!(engine.server.is_none());
        if let Some(v) = prev {
            std::env::set_var("VOICEFORGE_TTS_URL", v);
        }
    }

    #[tokio::test]
    #[serial]
    async fn engine_routes_unknown_voice_to_embedded_fallback() {
        // No cloning installed in test env; unknown voice must fall
        // through to the embedded backend (i.e. not error).
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = Arc::new(AtomicUsize::new(0));
        let engine = facade_with(engine_in(tmp.path(), counter.clone()));
        // "default" is not a cloned voice → embedded path runs the fake
        // synth, which writes a 1-byte stand-in WAV.
        let path = engine.speak("hello", "default").await.expect("speak");
        assert!(path.exists());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
