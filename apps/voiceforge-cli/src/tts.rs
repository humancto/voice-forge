//! TTS engine selection: embedded (OS-native) by default, or the
//! Python Flask server when `VOICEFORGE_TTS_URL` is set.
//!
//! Two variants are baked into a single enum — the set is fixed
//! forever (embedded vs server) and dynamic dispatch buys nothing.
//! Inherent `async fn` per struct, no trait, no `Box<dyn>`.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;

use crate::paths;

/// Hard cap on synthesized text length. Beyond this, both `say` and
/// `espeak-ng` happily block for minutes on a megabyte of input. The
/// reaction lines this ships for are short.
const MAX_TEXT_LEN: usize = 10_000;

/// Process timeout for any single OS-native TTS subprocess.
const TTS_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum Engine {
    Embedded(EmbeddedEngine),
    Server(ServerEngine),
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

        match self {
            Engine::Embedded(e) => e.speak(text, voice).await,
            Engine::Server(e) => e.speak(text, voice).await,
        }
    }
}

/// Pick the engine for this process invocation.
///
///   `VOICEFORGE_TTS_URL` set    → `ServerEngine`
///   `VOICEFORGE_TTS_URL` unset  → `EmbeddedEngine`
pub fn select_engine() -> Result<Engine> {
    if let Ok(url) = std::env::var("VOICEFORGE_TTS_URL") {
        if !url.is_empty() {
            return Ok(Engine::Server(ServerEngine::new(url)));
        }
    }
    Ok(Engine::Embedded(EmbeddedEngine::new()?))
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

fn cache_key(text: &str, voice: &str, backend_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher.update(b"|");
    hasher.update(voice.as_bytes());
    hasher.update(b"|");
    hasher.update(backend_id.as_bytes());
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

    #[tokio::test]
    async fn rejects_too_long_text() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = Arc::new(AtomicUsize::new(0));
        let engine = Engine::Embedded(engine_in(tmp.path(), counter.clone()));

        let huge = "x".repeat(MAX_TEXT_LEN + 1);
        let err = engine.speak(&huge, "default").await.unwrap_err();
        assert!(format!("{err:#}").contains("too long"));
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rejects_empty_text() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = Arc::new(AtomicUsize::new(0));
        let engine = Engine::Embedded(engine_in(tmp.path(), counter.clone()));

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
    async fn select_engine_picks_server_when_url_set() {
        // Process-global env mutation — fine in isolation here, but
        // guard against parallel tests that touch the same var.
        // (No others in this module touch VOICEFORGE_TTS_URL.)
        let prev = std::env::var("VOICEFORGE_TTS_URL").ok();
        std::env::set_var("VOICEFORGE_TTS_URL", "http://example.invalid");
        let engine = select_engine().expect("select");
        assert!(matches!(engine, Engine::Server(_)));
        match prev {
            Some(v) => std::env::set_var("VOICEFORGE_TTS_URL", v),
            None => std::env::remove_var("VOICEFORGE_TTS_URL"),
        }
    }
}
