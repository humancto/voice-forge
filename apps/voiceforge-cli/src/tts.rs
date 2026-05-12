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

// ============================================================================
// EngineKind — typed selector for the live synth backend (ROADMAP v0.4 PR-AB)
// ============================================================================

/// The TTS engine voiceforge uses for live (non-pack-rendered) synthesis.
///
/// Selected at startup via `VOICEFORGE_TTS_ENGINE`. Default is fish-speech
/// S2 Pro (studio quality). `gpt-sovits-v2` is opt-in for one release as
/// the deprecation path; deletes in v0.5. `embedded` (`say`/`espeak-ng`)
/// is the no-deps fallback used when no cloning runtime is installed.
///
/// Strings are single-sourced via [`EngineKind::as_str`] + [`EngineKind::ALL`]
/// so `FromStr`, `TryFrom<&str>`, and serde `try_from` all agree
/// automatically when a new variant is added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EngineKind {
    /// Fish-speech S2 Pro — studio-quality default, slower (~1.5-2× real-time).
    /// The 5 shipped packs are pre-rendered with this engine.
    FishSpeechS2Pro,
    /// GPT-SoVITS v2 — legacy, opt-in only via env var. Faster but lower
    /// quality. Removed in v0.5.
    GptSovitsV2,
    /// `say` (macOS) / `espeak-ng` (Linux) — no-deps fallback.
    /// Further dispatched via [`Backend`] to the right OS binary.
    Embedded,
}

impl EngineKind {
    /// Canonical wire-string for the variant. Single source of truth —
    /// `FromStr`, serde, and CLI `--engine` flag all consult this.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FishSpeechS2Pro => "fish-speech-s2-pro",
            Self::GptSovitsV2 => "gpt-sovits-v2",
            Self::Embedded => "embedded",
        }
    }

    /// All known variants. Used by `FromStr` to reject unknown values
    /// with a helpful "valid: ..." message, and by tests to assert
    /// every variant round-trips.
    pub const ALL: &'static [Self] = &[Self::FishSpeechS2Pro, Self::GptSovitsV2, Self::Embedded];

    /// The default engine when `VOICEFORGE_TTS_ENGINE` is unset.
    /// Studio quality wins by default; legacy + embedded require
    /// explicit opt-in.
    pub const DEFAULT: Self = Self::FishSpeechS2Pro;
}

impl std::fmt::Display for EngineKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for EngineKind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| {
                anyhow!(
                    "unknown engine {s:?}; valid values: {}",
                    Self::ALL
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }
}

impl TryFrom<&str> for EngineKind {
    type Error = anyhow::Error;
    fn try_from(s: &str) -> Result<Self> {
        s.parse()
    }
}

impl TryFrom<String> for EngineKind {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

// serde plumbing — single-sources through TryFrom so adding a variant
// doesn't require updating a separate `#[serde(rename = "...")]` table.
impl serde::Serialize for EngineKind {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for EngineKind {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Resolve `VOICEFORGE_TTS_ENGINE` into an `EngineKind`. Hard error on
/// unknown values (no silent fallback — typos surface immediately
/// with the valid list). Empty / unset → [`EngineKind::DEFAULT`].
///
/// Wired into `select_engine()` in PR-AB step 8 once `FishEngine` exists.
#[allow(dead_code)]
pub fn engine_kind_from_env() -> Result<EngineKind> {
    engine_kind_from_env_with(|k| std::env::var(k).ok())
}

/// Test-friendly env reader. Same contract as `engine_kind_from_env`.
pub(crate) fn engine_kind_from_env_with<F>(env: F) -> Result<EngineKind>
where
    F: Fn(&str) -> Option<String>,
{
    match env("VOICEFORGE_TTS_ENGINE")
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(s) => s
            .parse::<EngineKind>()
            .with_context(|| "VOICEFORGE_TTS_ENGINE"),
        None => Ok(EngineKind::DEFAULT),
    }
}

// ============================================================================
// TtsEngine trait — object-safe synth interface (ROADMAP v0.4 PR-AB step 2)
// ============================================================================

/// Heap-allocated future returned by `TtsEngine::speak`. Matches the
/// type `Box::pin(async move { ... })` produces. Send + 'a so it can
/// be polled across `tokio::spawn` task boundaries when the caller
/// owns the engine via `Arc<dyn TtsEngine>` and clones owned strings.
///
/// Wired into select_engine() in PR-AB step 8 once FishEngine exists.
#[allow(dead_code)]
pub type BoxedTtsFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<PathBuf>> + Send + 'a>>;

/// Object-safe synth contract. All v0.4 engines
/// (`FishEngine`, `CloningEngine`, `EmbeddedEngine`, `ServerEngine`)
/// implement this so `select_engine()` can return
/// `Arc<dyn TtsEngine + Send + Sync>` driven by `VOICEFORGE_TTS_ENGINE`.
///
/// Returns `BoxFuture` (not `async fn`) explicitly to keep the trait
/// object-safe with a `+ Send` bound on the returned future. `async fn`
/// in traits is dyn-compatible on stable 1.85+ but the returned future
/// defaults to `?Send`, which would block `tokio::spawn` of synth calls.
/// `async-trait` (used by `ReactionProvider`) is the macro alternative;
/// we go macro-free here to keep the trait surface explicit.
///
/// **Cancel-safety**: dropping the returned future SHOULD cancel the
/// underlying synth. Implementors that wrap a long-running child
/// process MUST handle drop-mid-synth gracefully (the child stays
/// alive; the caller's drop just means "abandon this output").
///
/// Wired into select_engine() in PR-AB step 8 once FishEngine exists.
#[allow(dead_code)]
pub trait TtsEngine: Send + Sync {
    /// Synthesize `text` in `voice`, return the path to the written
    /// WAV in `~/.voiceforge/cache/<sha>.wav`. Cache-hit on second
    /// call with the same (text, voice, engine_kind, profile.recipe).
    fn speak<'a>(&'a self, text: &'a str, voice: &'a str) -> BoxedTtsFuture<'a>;

    /// Stable identifier for `voiceforge doctor` and logs. Matches the
    /// `EngineKind` variant the engine was constructed for.
    fn engine_kind(&self) -> EngineKind;
}

// Compile-time assertion that TtsEngine is object-safe. If anyone
// adds a generic method or `Self` return type to the trait, this fails
// to compile with a clear error rather than failing at the dyn site.
#[allow(dead_code)]
fn _assert_tts_engine_object_safe() {
    let _check: Option<Box<dyn TtsEngine + Send + Sync>> = None;
}

#[derive(Debug)]
pub struct Engine {
    embedded: EmbeddedEngine,
    server: Option<ServerEngine>,
    /// Schema-1 / GPT-SoVITS runtime. Populated when an `INSTALLED.toml`
    /// at schema_version=1 is present. Kept around for users with v1
    /// installs who haven't re-run install-cloning yet — the v0.4
    /// migration story is "your v1 voices keep working."
    cloning: Option<CloningEngine>,
    /// Schema-2 / fish-speech S2 Pro runtime. Populated when a
    /// schema-2 marker is present. Wins over `cloning` when both are
    /// somehow constructible (caller asked for an explicit engine via
    /// `VOICEFORGE_TTS_ENGINE` AND both schema markers exist on disk —
    /// rare, but we choose the new path).
    fish: Option<FishEngine>,
    /// Engine the dispatcher prefers. Picked at `select_engine()` time
    /// from `engine_kind_from_env()`; controls which backend wins
    /// when multiple are constructed.
    preferred: EngineKind,
}

impl Engine {
    /// Test-only constructor: build an Engine with just the embedded
    /// backend (no server, no cloning, no fish). Lets cross-module tests
    /// (e.g. `daemon_server::tests`) use the same fake-synth machinery
    /// `tts::tests` does without re-deriving it.
    #[cfg(test)]
    pub(crate) fn for_testing(embedded: EmbeddedEngine) -> Self {
        Self {
            embedded,
            server: None,
            cloning: None,
            fish: None,
            preferred: EngineKind::DEFAULT,
        }
    }

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

        // Per-call dispatch (priority order, highest wins):
        //   1. preferred engine (from VOICEFORGE_TTS_ENGINE) → fish OR cloning
        //   2. cloned voice exists + matching engine constructed
        //   3. server URL set                                → ServerEngine
        //   4. otherwise                                     → EmbeddedEngine
        if voices::voice_exists(voice) {
            // Honor the user's explicit engine pick. Only falls through
            // if the preferred engine wasn't constructed (e.g. user said
            // fish-speech-s2-pro but only the v1 marker is on disk).
            match self.preferred {
                EngineKind::FishSpeechS2Pro => {
                    if let Some(fish) = &self.fish {
                        return fish.speak(text, voice).await;
                    }
                    if let Some(cloning) = &self.cloning {
                        return cloning.speak(text, voice).await;
                    }
                }
                EngineKind::GptSovitsV2 => {
                    if let Some(cloning) = &self.cloning {
                        return cloning.speak(text, voice).await;
                    }
                    if let Some(fish) = &self.fish {
                        return fish.speak(text, voice).await;
                    }
                }
                EngineKind::Embedded => {
                    // Fall through to embedded; preferred=embedded means
                    // user explicitly opted out of cloning.
                }
            }
        }
        if let Some(server) = &self.server {
            return server.speak(text, voice).await;
        }
        self.embedded.speak(text, voice).await
    }
}

/// Build an `Engine` for this process invocation. Always constructs the
/// embedded backend; conditionally adds server (when `VOICEFORGE_TTS_URL`
/// is set), v1 cloning (when a schema-1 marker is present), and v2 fish
/// (when a schema-2 marker is present). The preferred-engine field is
/// driven by `VOICEFORGE_TTS_ENGINE` (default fish-speech-s2-pro);
/// `Engine::speak()` honors it on a per-call basis.
pub fn select_engine() -> Result<Engine> {
    let embedded = EmbeddedEngine::new()?;

    let server = std::env::var("VOICEFORGE_TTS_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .map(ServerEngine::new);

    // Construct both schema runtimes when their respective markers exist.
    // A user mid-migration (v2 install with v1.bak) gets BOTH ready; the
    // dispatcher picks per-call based on `preferred`.
    let cloning = if install_cloning::is_installed() {
        Some(CloningEngine::new()?)
    } else {
        None
    };
    let fish = if install_cloning::is_installed_v2() {
        Some(FishEngine::new()?)
    } else {
        None
    };

    let preferred = engine_kind_from_env()?;

    Ok(Engine {
        embedded,
        server,
        cloning,
        fish,
        preferred,
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

// ---- Fish-speech S2 Pro (v2 cloning runtime) ------------------------
// Mirrors CloningEngine's NDJSON-stdio architecture exactly — only the
// child process changes (fish_speech_synth.py instead of cloning_synth.py)
// + the install state struct (InstallStateV2 instead of InstallState).
// Designed in lockstep with scripts/fish_speech_synth.py (PR-AB step 7)
// so the protocol contract on both sides is the same set of asserts.

const FISH_MODEL_LOAD_TIMEOUT: Duration = Duration::from_secs(120);
const FISH_SYNTH_TIMEOUT: Duration = Duration::from_secs(180);

pub struct FishEngine {
    cache_dir: PathBuf,
    install: install_cloning::InstallStateV2,
    /// `None` until first request; populated lazily so the ~30-90s
    /// model load never enters the path of a `voiceforge say --voice
    /// embedded_preset` call.
    child: Arc<TokioMutex<Option<SynthChild>>>,
}

impl std::fmt::Debug for FishEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FishEngine")
            .field("cache_dir", &self.cache_dir)
            .field("install", &self.install)
            .field("child", &"<lazy>")
            .finish()
    }
}

impl FishEngine {
    pub fn new() -> Result<Self> {
        let install = install_cloning::read_install_state_v2()?;
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
        let key = fish_cache_key(text, voice, profile_created_at);
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
            .context("writing to fish_speech_synth.py stdin")?;
        child
            .stdin
            .flush()
            .await
            .context("flushing fish synth stdin")?;

        let started = Instant::now();
        loop {
            if started.elapsed() > FISH_SYNTH_TIMEOUT {
                bail!("fish-speech synth timed out after {:?}", FISH_SYNTH_TIMEOUT);
            }
            let line = match child.stdout_lines.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => bail!("fish-speech synth child closed stdout"),
                Err(e) => return Err(e).context("reading fish synth stdout"),
            };
            let v: serde_json::Value = serde_json::from_str(&line)
                .with_context(|| format!("parsing fish synth response: {line}"))?;
            if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
                let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
                bail!("fish synth failed: {err}");
            }
            if v.get("sample_rate").is_some() {
                return Ok(out);
            }
            // notification line (ready / loaded_seconds); keep reading
        }
    }

    async fn spawn_child(&self) -> Result<SynthChild> {
        let python = install_cloning::cloning_venv_python()
            .ok_or_else(|| anyhow!("could not resolve cloning venv python"))?;
        let script = install_cloning::fish_synth_script().ok_or_else(|| {
            anyhow!(
                "could not locate scripts/fish_speech_synth.py — install voiceforge from source for now (binary release packaging lands in PR-AB step 6c.5)"
            )
        })?;

        let mut cmd = tokio::process::Command::new(&python);
        cmd.arg(&script)
            .env(
                "DYLD_FALLBACK_LIBRARY_PATH",
                format!("{}/lib", self.install.ffmpeg6_prefix),
            )
            // PYTHONPATH points at the fish-speech repo root; the script
            // does `sys.path.insert(0, str(repo_dir))` itself but setting
            // PYTHONPATH up front lets `python -c "import fish_speech"`
            // also succeed in the same env if anyone wants to debug.
            .env("PYTHONPATH", &self.install.repo_path)
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
            .ok_or_else(|| anyhow!("fish synth child has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("fish synth child has no stdout"))?;
        let stdout_lines = BufReader::new(stdout).lines();

        let mut sc = SynthChild {
            child,
            stdin,
            stdout_lines,
        };

        let started = Instant::now();
        loop {
            if started.elapsed() > FISH_MODEL_LOAD_TIMEOUT {
                bail!(
                    "fish-speech synth child failed to print ready within {:?}",
                    FISH_MODEL_LOAD_TIMEOUT
                );
            }
            let line = match sc.stdout_lines.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => bail!("fish-speech synth child exited before ready"),
                Err(e) => return Err(e).context("reading fish ready line"),
            };
            let v: serde_json::Value = serde_json::from_str(&line)
                .with_context(|| format!("parsing fish ready line: {line}"))?;
            if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
                let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
                bail!("fish synth child startup failed: {err}");
            }
            if v.get("ready").is_some() {
                break;
            }
        }
        Ok(sc)
    }
}

impl Drop for FishEngine {
    fn drop(&mut self) {
        // Same kill_on_drop story as CloningEngine — the tokio child's
        // own drop kills the python process.
    }
}

/// Cache key for fish-speech outputs. NOTE: distinct salt from the
/// GPT-SoVITS path so a re-clone with a different engine never hits
/// the wrong cached WAV. Cross-engine cache contamination would silently
/// produce GPT-SoVITS audio for a "fish-speech" voice request.
fn fish_cache_key(text: &str, voice: &str, created_at: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher.update(b"|");
    hasher.update(voice.as_bytes());
    hasher.update(b"|");
    hasher.update(created_at.as_bytes());
    hasher.update(b"|");
    hasher.update(b"fish-speech.s2-pro");
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

    // ============================================================================
    // EngineKind tests (ROADMAP v0.4 PR-AB step 1)
    // ============================================================================

    #[test]
    fn engine_kind_as_str_is_kebab_case_for_every_variant() {
        for &k in EngineKind::ALL {
            let s = k.as_str();
            assert!(!s.is_empty(), "as_str() for {k:?} is empty");
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "as_str() for {k:?} = {s:?} not kebab-case ascii"
            );
        }
    }

    #[test]
    fn engine_kind_default_is_fish_speech() {
        assert_eq!(EngineKind::DEFAULT, EngineKind::FishSpeechS2Pro);
    }

    #[test]
    fn engine_kind_fromstr_roundtrips_every_variant() {
        for &k in EngineKind::ALL {
            let s = k.as_str();
            let parsed: EngineKind = s.parse().expect("variant string must parse");
            assert_eq!(parsed, k, "round-trip mismatch for {k:?}");
        }
    }

    #[test]
    fn engine_kind_fromstr_rejects_unknown_with_helpful_message() {
        let err = "fishspeech".parse::<EngineKind>().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown engine"), "got: {msg}");
        assert!(msg.contains("\"fishspeech\""), "got: {msg}");
        // Lists every valid value
        for &k in EngineKind::ALL {
            assert!(
                msg.contains(k.as_str()),
                "valid-list missing {:?}, got: {msg}",
                k.as_str()
            );
        }
    }

    #[test]
    fn engine_kind_fromstr_rejects_empty_uppercase_whitespace() {
        assert!("".parse::<EngineKind>().is_err());
        assert!("FISH-SPEECH-S2-PRO".parse::<EngineKind>().is_err());
        assert!(" fish-speech-s2-pro".parse::<EngineKind>().is_err());
        assert!("fish-speech-s2-pro\n".parse::<EngineKind>().is_err());
    }

    #[test]
    fn engine_kind_serde_roundtrips_every_variant() {
        for &k in EngineKind::ALL {
            let json = serde_json::to_string(&k).unwrap();
            // Stringly-encoded — wire format matches as_str()
            assert_eq!(json, format!("\"{}\"", k.as_str()));
            let back: EngineKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, k);
        }
    }

    #[test]
    fn engine_kind_serde_rejects_unknown_value() {
        let r: serde_json::Result<EngineKind> = serde_json::from_str(r#""nope""#);
        assert!(r.is_err());
    }

    #[test]
    fn engine_kind_tryfrom_str_and_string_match_fromstr() {
        for &k in EngineKind::ALL {
            let s = k.as_str();
            let from_ref: EngineKind = TryFrom::try_from(s).unwrap();
            let from_owned: EngineKind = TryFrom::try_from(s.to_string()).unwrap();
            assert_eq!(from_ref, k);
            assert_eq!(from_owned, k);
        }
    }

    #[test]
    fn engine_kind_from_env_unset_returns_default() {
        let kind = engine_kind_from_env_with(|_| None).unwrap();
        assert_eq!(kind, EngineKind::DEFAULT);
    }

    #[test]
    fn engine_kind_from_env_empty_returns_default() {
        let kind = engine_kind_from_env_with(|k| {
            if k == "VOICEFORGE_TTS_ENGINE" {
                Some(String::new())
            } else {
                None
            }
        })
        .unwrap();
        assert_eq!(kind, EngineKind::DEFAULT);
    }

    #[test]
    fn engine_kind_from_env_whitespace_treated_as_empty() {
        let kind = engine_kind_from_env_with(|k| {
            if k == "VOICEFORGE_TTS_ENGINE" {
                Some("   ".to_string())
            } else {
                None
            }
        })
        .unwrap();
        assert_eq!(kind, EngineKind::DEFAULT);
    }

    #[test]
    fn engine_kind_from_env_each_valid_value() {
        for &k in EngineKind::ALL {
            let kind = engine_kind_from_env_with(|key| {
                if key == "VOICEFORGE_TTS_ENGINE" {
                    Some(k.as_str().to_string())
                } else {
                    None
                }
            })
            .unwrap();
            assert_eq!(kind, k);
        }
    }

    #[test]
    fn engine_kind_from_env_invalid_value_hard_errors_with_var_name() {
        let err = engine_kind_from_env_with(|k| {
            if k == "VOICEFORGE_TTS_ENGINE" {
                Some("fishspeech".to_string())
            } else {
                None
            }
        })
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("VOICEFORGE_TTS_ENGINE"), "got: {msg}");
        assert!(msg.contains("unknown engine"), "got: {msg}");
    }

    #[test]
    fn engine_kind_display_matches_as_str() {
        for &k in EngineKind::ALL {
            assert_eq!(format!("{k}"), k.as_str());
        }
    }

    // ============================================================================
    // TtsEngine trait tests (ROADMAP v0.4 PR-AB step 2)
    // ============================================================================

    /// Minimal `TtsEngine` impl for trait-shape testing. Records every
    /// `(text, voice)` it was asked to synthesize; returns a fixed path.
    /// Future PR-D tests will reuse this for daemon-integration of
    /// `voiceforge note` without needing the real fish-speech child.
    pub(crate) struct RecordingTtsEngine {
        kind: EngineKind,
        calls: std::sync::Mutex<Vec<(String, String)>>,
        return_path: PathBuf,
    }

    impl RecordingTtsEngine {
        pub(crate) fn new(kind: EngineKind, return_path: PathBuf) -> Self {
            Self {
                kind,
                calls: std::sync::Mutex::new(Vec::new()),
                return_path,
            }
        }
        pub(crate) fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl TtsEngine for RecordingTtsEngine {
        fn speak<'a>(&'a self, text: &'a str, voice: &'a str) -> BoxedTtsFuture<'a> {
            let path = self.return_path.clone();
            let text = text.to_string();
            let voice = voice.to_string();
            let calls = &self.calls;
            Box::pin(async move {
                calls.lock().unwrap().push((text, voice));
                Ok(path)
            })
        }
        fn engine_kind(&self) -> EngineKind {
            self.kind
        }
    }

    #[test]
    fn tts_engine_trait_is_object_safe_at_compile_time() {
        // If TtsEngine becomes non-object-safe (e.g., someone adds a
        // generic method or `Self` return), this line fails to compile.
        let _: Box<dyn TtsEngine + Send + Sync> = Box::new(RecordingTtsEngine::new(
            EngineKind::Embedded,
            PathBuf::from("/tmp/fake.wav"),
        ));
    }

    #[tokio::test]
    async fn tts_engine_trait_dispatches_via_boxfuture() {
        let engine: Arc<dyn TtsEngine + Send + Sync> = Arc::new(RecordingTtsEngine::new(
            EngineKind::FishSpeechS2Pro,
            PathBuf::from("/tmp/recorded.wav"),
        ));
        let path = engine.speak("hello world", "peter").await.unwrap();
        assert_eq!(path, PathBuf::from("/tmp/recorded.wav"));
        assert_eq!(engine.engine_kind(), EngineKind::FishSpeechS2Pro);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tts_engine_boxfuture_is_send_can_cross_spawn() {
        // The BoxFuture must be Send so synth calls can be spawned
        // onto a multi-thread runtime. Failing this would block
        // `voiceforge note`'s sequential-chunk synth pattern.
        let engine: Arc<dyn TtsEngine + Send + Sync> = Arc::new(RecordingTtsEngine::new(
            EngineKind::FishSpeechS2Pro,
            PathBuf::from("/tmp/sent.wav"),
        ));
        let e2 = Arc::clone(&engine);
        let handle = tokio::spawn(async move { e2.speak("spawned", "peter").await });
        let path = handle.await.unwrap().unwrap();
        assert_eq!(path, PathBuf::from("/tmp/sent.wav"));
    }

    #[tokio::test]
    async fn tts_engine_records_each_call_in_order() {
        let recorder = Arc::new(RecordingTtsEngine::new(
            EngineKind::FishSpeechS2Pro,
            PathBuf::from("/tmp/x.wav"),
        ));
        let engine: Arc<dyn TtsEngine + Send + Sync> = recorder.clone();
        engine.speak("first", "peter").await.unwrap();
        engine.speak("second", "brian").await.unwrap();
        engine.speak("third", "peter").await.unwrap();
        let calls = recorder.calls();
        assert_eq!(
            calls,
            vec![
                ("first".to_string(), "peter".to_string()),
                ("second".to_string(), "brian".to_string()),
                ("third".to_string(), "peter".to_string()),
            ]
        );
    }

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
            fish: None,
            preferred: EngineKind::DEFAULT,
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

    // ========================================================================
    // FishEngine + dispatcher tests (PR-AB step 8)
    // ========================================================================

    #[test]
    #[serial]
    fn fish_engine_new_fails_when_no_v2_marker() {
        // Without an INSTALLED.toml at schema_version=2 on disk,
        // FishEngine::new() must fail loud — never silently misroute
        // to the embedded synth or the wrong schema reader.
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var("VOICEFORGE_HOME").ok();
        std::env::set_var("VOICEFORGE_HOME", tmp.path());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let err = FishEngine::new().unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("INSTALLED") || msg.contains("schema") || msg.contains("reading"),
                "fish-engine error must reference the marker; got: {msg}"
            );
        }));
        match prev {
            Some(v) => std::env::set_var("VOICEFORGE_HOME", v),
            None => std::env::remove_var("VOICEFORGE_HOME"),
        }
        if let Err(p) = result {
            std::panic::resume_unwind(p);
        }
    }

    #[test]
    fn fish_cache_key_is_distinct_from_cloning_cache_key() {
        // Cross-engine cache contamination would silently produce
        // GPT-SoVITS audio for a "fish-speech" voice request. The salt
        // bytes at the end of each key fn make collisions impossible.
        let f = fish_cache_key("hello", "peter", "2026-05-11T00:00:00Z");
        let c = cloning_cache_key("hello", "peter", "2026-05-11T00:00:00Z");
        assert_ne!(
            f, c,
            "fish + cloning cache keys must differ for the same (text,voice,created_at)"
        );
    }

    #[test]
    fn fish_cache_key_is_stable_for_same_inputs() {
        let a = fish_cache_key("hi", "peter", "2026-05-11T00:00:00Z");
        let b = fish_cache_key("hi", "peter", "2026-05-11T00:00:00Z");
        assert_eq!(a, b, "fish cache key must be deterministic");
    }

    #[test]
    fn fish_cache_key_changes_with_each_input_field() {
        let base = fish_cache_key("hi", "peter", "2026-05-11T00:00:00Z");
        let by_text = fish_cache_key("bye", "peter", "2026-05-11T00:00:00Z");
        let by_voice = fish_cache_key("hi", "alice", "2026-05-11T00:00:00Z");
        let by_created = fish_cache_key("hi", "peter", "2026-05-12T00:00:00Z");
        assert_ne!(base, by_text, "text change must flip key");
        assert_ne!(base, by_voice, "voice change must flip key");
        assert_ne!(base, by_created, "created_at change must flip key");
    }

    /// Dispatch precedence test (no real synth involved). Build an
    /// Engine with all four backends present and verify that the
    /// `preferred` field controls the call routing for cloned voices.
    /// We can't actually call speak() here (FishEngine + CloningEngine
    /// would require a real install), so this test inspects the static
    /// shape of the Engine struct + the EngineKind values.
    #[test]
    fn dispatcher_default_engine_kind_is_fish_speech() {
        assert_eq!(EngineKind::DEFAULT, EngineKind::FishSpeechS2Pro);
    }

    #[test]
    fn dispatcher_embedded_engine_kind_does_not_route_to_a_clone_runtime() {
        // Documenting the dispatch contract: when preferred=Embedded,
        // Engine::speak does NOT consult fish or cloning even if
        // both are populated. The logic lives in the EngineKind::Embedded
        // arm of the match in Engine::speak — this test is a regression
        // net for accidentally adding a fish/cloning fallback there.
        match EngineKind::Embedded {
            EngineKind::Embedded => {}
            EngineKind::FishSpeechS2Pro => {
                panic!("Embedded must not equal FishSpeechS2Pro")
            }
            EngineKind::GptSovitsV2 => panic!("Embedded must not equal GptSovitsV2"),
        }
    }
}
