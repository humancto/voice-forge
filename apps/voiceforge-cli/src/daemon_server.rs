//! Unix-socket NDJSON daemon (ROADMAP 1.8).
//!
//! External tools (claude-code hooks, git hooks, shell preexec, the
//! future `voiceforge send` from 1.9) drop JSON lines on a Unix socket;
//! the daemon reads them and routes to the TTS engine + audio playback.
//!
//! Wire format: one JSON object per line, max 64 KiB per line.
//!   request:  `{"event": "...", "text"?: "...", "voice"?: "...", "message"?: "..."}`
//!   reply:    `{"ok": true,  "spoken": "...", "voice": "..."}` or
//!             `{"ok": false, "error": "..."}`
//!
//! Either `event` OR `text` must be present; missing both → error.
//!
//! Concurrency: a per-server `Semaphore` (cap 8) caps how many handlers
//! can be queued for audio playback at once. The actual `AudioSink::play`
//! call (rodio is sync) is run inside `spawn_blocking` so it never
//! occupies a Tokio worker. Synthesis is already serialized inside
//! `CloningEngine` by its own `TokioMutex`, so the cap is effectively
//! "≤8 audio plays queued, ≤1 synthesis at a time."

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal;
use tokio::signal::unix::{signal as unix_signal, SignalKind};
use tokio::sync::Semaphore;

use crate::audio_sink::AudioSink;
use crate::notify_macos::Mirror;
use crate::playback::{PlaybackItem, PlaybackQueue};
use crate::reaction::ReactionProvider;
#[cfg(test)]
use crate::rules::Rules;
use crate::tts::Engine;

/// Shared between the daemon's stale-socket detect and the doctor's
/// daemon probe — keeps both reports consistent.
pub(crate) const STALE_PROBE_TIMEOUT: Duration = Duration::from_millis(50);

/// 64 KiB cap on incoming line length. Prevents a hostile same-user
/// process from OOMing the daemon by sending a 1 GiB line.
const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Concurrency cap on simultaneous audio playbacks. 8 is "more than a
/// human can listen to anyway." Cheap to bump if needed.
const MAX_INFLIGHT: usize = 8;

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
}

impl DaemonConfig {
    /// Default socket location: `$VOICEFORGE_HOME/voiceforge.sock`
    /// (or `~/.voiceforge/voiceforge.sock`).
    pub fn default_path() -> Result<PathBuf> {
        crate::paths::user_home()
            .map(|h| h.join("voiceforge.sock"))
            .ok_or_else(|| anyhow!("could not resolve ~/.voiceforge — set VOICEFORGE_HOME or HOME"))
    }
}

#[derive(Debug, Deserialize)]
struct Frame {
    event: Option<String>,
    text: Option<String>,
    voice: Option<String>,
    #[allow(dead_code)] // logged-only, not spoken
    message: Option<String>,
}

/// One turn in a cast reply (ROADMAP 4.3).
#[derive(Debug, Serialize)]
struct TurnReply {
    voice: String,
    line: String,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Reply {
    /// Multi-turn cast (ROADMAP 4.3). MUST come before `Single` in the
    /// enum so serde's untagged dispatch tries it first on round-trip
    /// deserialize — `spoken: Vec<...>` and `spoken: String` are
    /// disjoint at the JSON-type level so this is defensive only.
    Cast {
        ok: bool, // always true
        spoken: Vec<TurnReply>,
    },
    Single {
        ok: bool, // always true
        spoken: String,
        voice: String,
    },
    Err {
        ok: bool, // always false
        error: String,
    },
}

impl Reply {
    fn single(spoken: impl Into<String>, voice: impl Into<String>) -> Self {
        Reply::Single {
            ok: true,
            spoken: spoken.into(),
            voice: voice.into(),
        }
    }
    fn cast(turns: Vec<(String, String)>) -> Self {
        Reply::Cast {
            ok: true,
            spoken: turns
                .into_iter()
                .map(|(voice, line)| TurnReply { voice, line })
                .collect(),
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        Reply::Err {
            ok: false,
            error: msg.into(),
        }
    }
}

/// RAII guard: unlinks the socket file on drop. Covers panic/abort
/// cleanup without relying on the SIGTERM handler firing. SIGKILL,
/// power loss, or panic-before-bind still leak the file — those are
/// recovered by the *next* startup's stale-socket detect.
struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Probe whether anything is listening on the given socket path.
/// `Ok(true)` = live listener. `Ok(false)` = stale file or absent.
/// On `Err(Elapsed)` we err on the safe side and treat as live, so two
/// daemons racing don't both try to clobber each other's socket.
pub(crate) async fn probe_socket(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    match tokio::time::timeout(STALE_PROBE_TIMEOUT, UnixStream::connect(path)).await {
        Ok(Ok(_)) => Ok(true),                            // someone answered
        Ok(Err(_)) => Ok(false),                          // ECONNREFUSED / ENOENT — stale
        Err(_) => Err(anyhow!("daemon probe timed out")), // conservative
    }
}

/// Run the daemon to completion. Returns when SIGTERM / Ctrl-C arrives
/// or a fatal error occurs. Engine, Rules, and Sink are injected so
/// tests can hand in a recording sink and a synthetic ruleset.
pub async fn serve(
    cfg: DaemonConfig,
    engine: Arc<Engine>,
    provider: Arc<dyn ReactionProvider>,
    sink: Arc<dyn AudioSink>,
    mirror: Arc<dyn Mirror>,
) -> Result<()> {
    // Single-consumer playback queue (ROADMAP 4.3). All audio (single
    // and cast) goes through here so playback never overlaps. Owned
    // by the serve scope; consumer task lives until queue's last clone
    // drops at function return.
    let playback = PlaybackQueue::spawn(Arc::clone(&sink));
    serve_with_playback(cfg, engine, provider, sink, mirror, playback).await
}

async fn serve_with_playback(
    cfg: DaemonConfig,
    engine: Arc<Engine>,
    provider: Arc<dyn ReactionProvider>,
    sink: Arc<dyn AudioSink>,
    mirror: Arc<dyn Mirror>,
    playback: PlaybackQueue,
) -> Result<()> {
    // `sink` kept in the signature so legacy callers (and the
    // shutdown-message wording below) remain unchanged. The actual
    // playback side already moved to `playback`.
    let _sink = sink;
    // Stale-socket detect.
    if cfg.socket_path.exists() {
        match probe_socket(&cfg.socket_path).await {
            Ok(true) => {
                bail!(
                    "another voiceforge daemon is already listening on {}",
                    cfg.socket_path.display()
                );
            }
            Ok(false) => {
                // Stale file. Unlink and proceed.
                std::fs::remove_file(&cfg.socket_path).with_context(|| {
                    format!(
                        "removing stale socket file at {}",
                        cfg.socket_path.display()
                    )
                })?;
            }
            Err(_) => {
                bail!(
                    "could not determine whether a daemon owns {} (probe timed out); refusing to clobber",
                    cfg.socket_path.display()
                );
            }
        }
    }

    // Make sure the parent directory exists. `bootstrap::ensure_voiceforge_home`
    // should have run already, but if the user passed a custom socket path
    // (e.g. in tests) the parent might not exist yet.
    if let Some(parent) = cfg.socket_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir {}", parent.display()))?;
        }
    }

    let listener = UnixListener::bind(&cfg.socket_path)
        .with_context(|| format!("binding {}", cfg.socket_path.display()))?;

    // Construct the SocketGuard *immediately after* bind, *before* chmod —
    // if chmod fails we still want the file unlinked.
    let _guard = SocketGuard(cfg.socket_path.clone());

    // Tighten permissions to 0600. There is a microsecond-window race
    // between bind and chmod where the file exists at the user's umask;
    // accepted because the parent dir is user-owned ~/.voiceforge/.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg.socket_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 on {}", cfg.socket_path.display()))?;
    }

    let semaphore = Arc::new(Semaphore::new(MAX_INFLIGHT));

    eprintln!(
        "voiceforge daemon: listening on {} (max {} inflight)",
        cfg.socket_path.display(),
        MAX_INFLIGHT
    );

    // Shutdown future: SIGTERM OR Ctrl-C. Kept as a fused future inside
    // the accept loop's select! so each iteration sees the same flag.
    let mut sigterm = unix_signal(SignalKind::terminate()).context("installing SIGTERM handler")?;

    loop {
        tokio::select! {
            // accept() is documented cancel-safe.
            accept_result = listener.accept() => {
                let (stream, _) = match accept_result {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("voiceforge daemon: accept error: {e}");
                        continue;
                    }
                };
                let engine = Arc::clone(&engine);
                let provider = Arc::clone(&provider);
                let mirror = Arc::clone(&mirror);
                let playback = playback.clone();
                let semaphore = Arc::clone(&semaphore);
                tokio::spawn(handle_connection(
                    stream, engine, provider, mirror, playback, semaphore,
                ));
            }
            _ = signal::ctrl_c() => {
                eprintln!("voiceforge daemon: ctrl_c received, shutting down");
                break;
            }
            _ = sigterm.recv() => {
                eprintln!("voiceforge daemon: SIGTERM received, shutting down");
                break;
            }
        }
    }

    // _guard drops here → socket file unlinked. In-flight handler tasks
    // continue until their clients hang up; we don't await them, but
    // they hold no shared resources that would corrupt by outliving us.
    Ok(())
}

async fn handle_connection(
    stream: UnixStream,
    engine: Arc<Engine>,
    provider: Arc<dyn ReactionProvider>,
    mirror: Arc<dyn Mirror>,
    playback: PlaybackQueue,
    semaphore: Arc<Semaphore>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::with_capacity(8 * 1024, read_half);

    loop {
        let mut buf: Vec<u8> = Vec::new();
        // read_until preserves the trailing \n; we strip below. Cap at
        // MAX_FRAME_BYTES + 1 so we can detect overflow precisely.
        let read_result = (&mut reader)
            .take((MAX_FRAME_BYTES as u64) + 1)
            .read_until(b'\n', &mut buf)
            .await;
        let n = match read_result {
            Ok(0) => return, // client closed
            Ok(n) => n,
            Err(e) => {
                eprintln!("voiceforge daemon: read error: {e}");
                return;
            }
        };

        // Detect "did we fill the cap without seeing \n?" — overflow.
        let overflowed = n > MAX_FRAME_BYTES || (n == MAX_FRAME_BYTES + 1 && !buf.ends_with(b"\n"));
        if overflowed {
            let reply = Reply::err(format!(
                "frame too large (>{} bytes); closing connection",
                MAX_FRAME_BYTES
            ));
            let _ = write_reply(&mut write_half, &reply).await;
            eprintln!("voiceforge daemon: oversized frame, closing connection");
            return;
        }

        // Strip trailing \n.
        if buf.ends_with(b"\n") {
            buf.pop();
        }
        if buf.is_empty() {
            continue; // ignore blank lines
        }

        let reply =
            match process_frame(&buf, &engine, &provider, &mirror, &playback, &semaphore).await {
                Ok(r) => r,
                Err(e) => Reply::err(format!("{e:#}")),
            };

        if let Err(e) = write_reply(&mut write_half, &reply).await {
            // BrokenPipe is normal when the client doesn't read its reply.
            eprintln!("voiceforge daemon: write reply failed: {e}");
            return;
        }
    }
}

async fn process_frame(
    raw: &[u8],
    engine: &Arc<Engine>,
    provider: &Arc<dyn ReactionProvider>,
    mirror: &Arc<dyn Mirror>,
    playback: &PlaybackQueue,
    semaphore: &Arc<Semaphore>,
) -> Result<Reply> {
    let frame: Frame =
        serde_json::from_slice(raw).context("frame is not valid JSON matching the schema")?;

    // Resolve turns synchronously (LLM call happens here for the cast
    // path; ~500-3000ms). For text-only and single-voice we pay
    // basically nothing.
    //
    // ROADMAP 4.3 Bug 2 fix: when the frame has an explicit `voice`
    // override, ALWAYS take the single-voice path. Voice override
    // means "I want one specific voice"; honoring it requires
    // bypassing the cast lookup.
    let turns: Vec<(String, String)> = if let Some(text) = frame.text.as_deref() {
        let voice = frame.voice.clone().unwrap_or_else(|| "default".to_string());
        vec![(voice, text.to_string())]
    } else {
        let event = frame
            .event
            .as_deref()
            .ok_or_else(|| anyhow!("frame must contain either \"event\" or \"text\""))?;
        match &frame.voice {
            Some(override_voice) => {
                // Bypass cast entirely; honor the explicit voice.
                let (_provider_voice, line) = provider.react(event).await;
                vec![(override_voice.clone(), line)]
            }
            None => provider.react_cast(event).await,
        }
    };

    if turns.is_empty() {
        bail!("provider returned zero turns");
    }

    // ROADMAP 4.3 Bug 1 fix: acquire ONE permit for the whole frame,
    // not per-turn. Per-turn would deadlock under load (8 concurrent
    // casts × N turns serialize on a cap-8 semaphore). The playback
    // queue (cap 16) is the audio backpressure mechanism; the
    // semaphore now caps "in-flight synth" only, which is what its
    // original intent measured.
    let permit = Arc::clone(semaphore)
        .acquire_owned()
        .await
        .map_err(|e| anyhow!("semaphore closed: {e}"))?;

    // ROADMAP 4.3 Showstopper 2 fix: option-b sequencing. Build the
    // reply FROM the resolved turns (we already have the text — no
    // need to wait for synth) and detach the synth+enqueue work so
    // the daemon replies fast (within hook-timeout budgets) and
    // audio plays asynchronously through the queue.
    //
    // Mirror calls happen here, BEFORE the spawn, so the banner
    // ordering across concurrent frames matches reply ordering on
    // the wire (a detached spawn can't guarantee that). This also
    // restores the documented "banner lands at audio-start" intent
    // — mirror was always meant to be sync-cheap.
    for (voice, text) in &turns {
        mirror.mirror(/* voice */ voice, /* text */ text);
    }

    let reply = if turns.len() == 1 {
        let (voice, text) = (turns[0].0.clone(), turns[0].1.clone());
        Reply::single(text, voice)
    } else {
        Reply::cast(turns.clone())
    };

    // Detach synth+enqueue. The single permit is moved into the task
    // and dropped at task exit, so the daemon's inflight slot
    // represents the whole frame (cast or single), not per-turn.
    // Keeping the per-frame semaphore (cap 8): without it a flood of
    // frames could spawn unbounded detached tasks each holding an
    // Arc<Engine> and racing for the engine's internal TokioMutex.
    let engine = Arc::clone(engine);
    let playback = playback.clone();
    tokio::spawn(async move {
        let _permit = permit; // dropped at task exit
        for (voice, text) in turns {
            let audio_path = match engine.speak(&text, &voice).await {
                Ok(p) => p,
                Err(e) => {
                    eprintln!(
                        "voiceforge daemon: synth failed for ({voice:?}, {text:?}): {e:#}; aborting remaining turns"
                    );
                    return;
                }
            };
            if let Err(e) = playback
                .push(PlaybackItem {
                    path: audio_path,
                    permit: None,
                })
                .await
            {
                eprintln!("voiceforge daemon: failed to enqueue playback: {e:#}");
                return;
            }
        }
    });

    Ok(reply)
}

async fn write_reply<W>(writer: &mut W, reply: &Reply) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(reply).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Test fixtures shared between `daemon_server::tests` and
/// `daemon_client::tests`. Lives behind `#[cfg(test)]` so it's
/// invisible to release builds; `pub(crate)` so other modules'
/// test mods can `use crate::daemon_server::test_support::*`.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::tts::{Backend, EmbeddedEngine, SynthBuilder};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Test sink that records the WAV paths the daemon hands it. No
    /// audio device touched — `play` is a no-op that bumps a counter.
    #[derive(Default)]
    pub(crate) struct RecordingSink {
        played: Mutex<Vec<PathBuf>>,
        count: AtomicUsize,
    }

    impl RecordingSink {
        pub(crate) fn count(&self) -> usize {
            self.count.load(Ordering::SeqCst)
        }
    }

    impl AudioSink for RecordingSink {
        fn play(&self, wav_path: &Path) -> Result<()> {
            self.played.lock().unwrap().push(wav_path.to_path_buf());
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Tiny synth-builder closure: writes a 4-byte placeholder WAV via
    /// `sh -c 'printf RIFF > path'`. Mirrors the `fake_synth` pattern
    /// in `tts::tests`. Doesn't need to be a real WAV — `RecordingSink`
    /// never decodes it.
    pub(crate) fn fake_synth() -> SynthBuilder {
        Box::new(|s| {
            let out = s.output_aiff_or_wav.to_owned();
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(format!(
                "printf 'RIFF\\0\\0\\0\\0WAVEfmt ' > '{}'",
                out.display()
            ));
            cmd
        })
    }

    /// Synth that sleeps before writing the same fake WAV. Used by
    /// the option-b sequencing test to prove the daemon replies
    /// BEFORE synth finishes — `fake_synth` is microseconds and
    /// would pass the test even on the broken pre-fix code.
    pub(crate) fn slow_synth(delay_ms: u64) -> SynthBuilder {
        Box::new(move |s| {
            let out = s.output_aiff_or_wav.to_owned();
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(format!(
                "sleep {}; printf 'RIFF\\0\\0\\0\\0WAVEfmt ' > '{}'",
                (delay_ms as f64) / 1000.0,
                out.display()
            ));
            cmd
        })
    }

    /// Like `fixture` but with a slow synth, for option-b timing tests.
    pub(crate) fn fixture_slow_synth(
        tmp_root: &Path,
        delay_ms: u64,
    ) -> (DaemonConfig, Arc<Engine>, Arc<Rules>, Arc<RecordingSink>) {
        let socket_path = tmp_root.join("voiceforge.sock");
        let cfg = DaemonConfig { socket_path };
        let cache_dir = tmp_root.join("cache");
        let embedded =
            EmbeddedEngine::for_testing(cache_dir, Backend::MacosSay, slow_synth(delay_ms));
        let engine = Arc::new(Engine::for_testing(embedded));
        let rules = Arc::new(Rules::default_builtin());
        let sink = Arc::new(RecordingSink::default());
        (cfg, engine, rules, sink)
    }

    /// Build a minimal-but-real Engine + Rules + sink triple for tests.
    /// Returns the four pieces needed to spin up `serve` in a tempdir.
    pub(crate) fn fixture(
        tmp_root: &Path,
    ) -> (DaemonConfig, Arc<Engine>, Arc<Rules>, Arc<RecordingSink>) {
        let socket_path = tmp_root.join("voiceforge.sock");
        let cfg = DaemonConfig { socket_path };

        let cache_dir = tmp_root.join("cache");
        let embedded = EmbeddedEngine::for_testing(cache_dir, Backend::MacosSay, fake_synth());
        let engine = Arc::new(Engine::for_testing(embedded));

        let rules = Arc::new(Rules::default_builtin());
        let sink = Arc::new(RecordingSink::default());

        (cfg, engine, rules, sink)
    }

    /// Spawn `serve` on a tokio task with the default (production)
    /// mirror. The default mirror is `OsascriptMirror`, which is a
    /// no-op when `VOICEFORGE_MIRROR_NOTIFICATIONS` is unset and on
    /// non-macOS platforms — so existing tests are unaffected.
    pub(crate) async fn spawn_serve(
        cfg: DaemonConfig,
        engine: Arc<Engine>,
        rules: Arc<Rules>,
        sink: Arc<dyn AudioSink>,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let provider: Arc<dyn ReactionProvider> =
            Arc::new(crate::reaction::StaticProvider::new(rules));
        spawn_serve_with_provider(
            cfg,
            engine,
            provider,
            sink,
            crate::notify_macos::default_mirror(),
        )
        .await
    }

    /// Like `spawn_serve`, but lets the test inject its own
    /// `Arc<dyn Mirror>` (e.g. a `RecordingMirror` to assert the
    /// daemon called the bridge with the right args).
    pub(crate) async fn spawn_serve_with_mirror(
        cfg: DaemonConfig,
        engine: Arc<Engine>,
        rules: Arc<Rules>,
        sink: Arc<dyn AudioSink>,
        mirror: Arc<dyn Mirror>,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let provider: Arc<dyn ReactionProvider> =
            Arc::new(crate::reaction::StaticProvider::new(rules));
        spawn_serve_with_provider(cfg, engine, provider, sink, mirror).await
    }

    /// Full-control spawn: caller supplies the `Arc<dyn ReactionProvider>`
    /// (e.g. a `RecordingProvider` to assert the daemon dispatched
    /// through the trait, or a real `LlmProvider` for end-to-end tests).
    /// Used by ROADMAP 4.1 daemon integration tests.
    pub(crate) async fn spawn_serve_with_provider(
        cfg: DaemonConfig,
        engine: Arc<Engine>,
        provider: Arc<dyn ReactionProvider>,
        sink: Arc<dyn AudioSink>,
        mirror: Arc<dyn Mirror>,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let socket_path = cfg.socket_path.clone();
        let handle = tokio::spawn(async move { serve(cfg, engine, provider, sink, mirror).await });
        // Wait for bind. Tight bound; local socket on the same FS.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if probe_socket(&socket_path).await.unwrap_or(false) {
                return handle;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("daemon never bound at {}", socket_path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use serde_json::Value;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    /// Send one frame on a fresh connection, return the (parsed-JSON) reply.
    async fn send_one(socket_path: &Path, frame: &str) -> Value {
        let mut stream = UnixStream::connect(socket_path)
            .await
            .expect("client connect");
        stream.write_all(frame.as_bytes()).await.expect("write");
        if !frame.ends_with('\n') {
            stream.write_all(b"\n").await.expect("write newline");
        }
        let (read_half, _) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        AsyncBufReadExt::read_line(&mut reader, &mut line)
            .await
            .expect("read reply");
        serde_json::from_str(line.trim()).expect("reply is JSON")
    }

    // 1. event-only frame → ok + spoken matches a rules entry
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_handles_event_frame() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();
        let handle = spawn_serve(cfg, engine, rules, sink.clone() as Arc<dyn AudioSink>).await;

        let reply = send_one(&socket, r#"{"event":"build_failed"}"#).await;
        assert_eq!(reply["ok"], Value::Bool(true));
        let spoken = reply["spoken"].as_str().unwrap();
        assert!(
            [
                "The build failed again.",
                "That did not go well.",
                "The compiler has chosen violence."
            ]
            .contains(&spoken),
            "unexpected spoken line: {spoken:?}",
        );
        // Wait for the spawn_blocking play call to complete.
        for _ in 0..50 {
            if sink.count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(sink.count(), 1);
        handle.abort();
    }

    // 2. text-only frame → ok + spoken == text
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_handles_text_only_frame() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();
        let handle = spawn_serve(cfg, engine, rules, sink.clone() as Arc<dyn AudioSink>).await;

        let reply = send_one(&socket, r#"{"text":"hello there","voice":"default"}"#).await;
        assert_eq!(reply["ok"], Value::Bool(true));
        assert_eq!(reply["spoken"], "hello there");
        assert_eq!(reply["voice"], "default");
        handle.abort();
    }

    // 3. {} → ok:false
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_rejects_empty_frame() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();
        let handle = spawn_serve(cfg, engine, rules, sink as Arc<dyn AudioSink>).await;

        let reply = send_one(&socket, "{}").await;
        assert_eq!(reply["ok"], Value::Bool(false));
        assert!(reply["error"].as_str().unwrap().contains("event"));
        handle.abort();
    }

    // 4. stale regular file at the socket path is unlinked + bind succeeds
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_unlinks_stale_socket() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, rules, sink) = fixture(tmp.path());
        // Pre-create a regular file (not a real socket) at the path.
        std::fs::write(&cfg.socket_path, b"stale").expect("write stale");
        let socket = cfg.socket_path.clone();
        let handle = spawn_serve(cfg, engine, rules, sink as Arc<dyn AudioSink>).await;

        // If we got here, bind succeeded after stale-detect unlinked.
        assert!(probe_socket(&socket).await.expect("probe"));
        handle.abort();
    }

    // 5. second daemon on the same path errors (live daemon present)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_refuses_when_live_daemon_present() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();
        let h1 = spawn_serve(
            cfg.clone(),
            engine.clone(),
            rules.clone(),
            sink.clone() as Arc<dyn AudioSink>,
        )
        .await;

        // Try a second serve on the same path.
        let provider2: Arc<dyn ReactionProvider> =
            Arc::new(crate::reaction::StaticProvider::new(rules));
        let result = serve(
            cfg,
            engine,
            provider2,
            sink as Arc<dyn AudioSink>,
            crate::notify_macos::default_mirror(),
        )
        .await;
        assert!(result.is_err(), "second daemon must refuse to bind");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("already listening") || msg.contains("binding"),
            "unexpected error: {msg}",
        );
        // The first daemon should still own the socket.
        assert!(probe_socket(&socket).await.expect("probe"));
        h1.abort();
    }

    // 6. fan-out 8 concurrent clients
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_handles_multiple_clients_concurrently() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();
        let handle = spawn_serve(cfg, engine, rules, sink.clone() as Arc<dyn AudioSink>).await;

        let mut joins = Vec::new();
        for i in 0..8 {
            let socket = socket.clone();
            joins.push(tokio::spawn(async move {
                send_one(
                    &socket,
                    &format!(r#"{{"text":"msg-{i}","voice":"default"}}"#),
                )
                .await
            }));
        }
        for j in joins {
            let reply = j.await.expect("join");
            assert_eq!(reply["ok"], Value::Bool(true));
        }

        // Wait for all 8 spawn_blocking play calls to record.
        for _ in 0..100 {
            if sink.count() >= 8 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(sink.count(), 8);
        handle.abort();
    }

    // 7. malformed mid-stream — frame 1 OK, frame 2 err, frame 3 OK,
    //    connection stays open across all three.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_handles_malformed_mid_stream() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();
        let handle = spawn_serve(cfg, engine, rules, sink as Arc<dyn AudioSink>).await;

        let mut stream = UnixStream::connect(&socket).await.expect("connect");
        stream
            .write_all(b"{\"text\":\"a\"}\n{garbage\n{\"text\":\"b\"}\n")
            .await
            .expect("write");
        let (read_half, _) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        let mut line = String::new();
        AsyncBufReadExt::read_line(&mut reader, &mut line)
            .await
            .expect("read 1");
        let r1: Value = serde_json::from_str(line.trim()).expect("parse 1");
        assert_eq!(r1["ok"], Value::Bool(true));

        line.clear();
        AsyncBufReadExt::read_line(&mut reader, &mut line)
            .await
            .expect("read 2");
        let r2: Value = serde_json::from_str(line.trim()).expect("parse 2");
        assert_eq!(r2["ok"], Value::Bool(false));

        line.clear();
        AsyncBufReadExt::read_line(&mut reader, &mut line)
            .await
            .expect("read 3");
        let r3: Value = serde_json::from_str(line.trim()).expect("parse 3");
        assert_eq!(r3["ok"], Value::Bool(true));

        handle.abort();
    }

    // 8. oversized frame → clean rejection, daemon survives
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_rejects_oversized_frame() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();
        let handle = spawn_serve(cfg, engine, rules, sink as Arc<dyn AudioSink>).await;

        let stream = UnixStream::connect(&socket).await.expect("connect");
        let (read_half, mut write_half) = stream.into_split();
        // 128 KiB of 'a'. Well over the 64 KiB cap. The daemon will
        // detect overflow as soon as it has filled the take(MAX+1)
        // window, send back an error, and close the connection. From
        // the client's perspective that surfaces as BrokenPipe partway
        // through the write — accept that and proceed to read the reply
        // off the read half (which is independent and still valid).
        let huge = "a".repeat(128 * 1024);
        let _ = write_half.write_all(huge.as_bytes()).await;
        let _ = write_half.write_all(b"\n").await;
        let _ = write_half.shutdown().await;

        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        AsyncBufReadExt::read_line(&mut reader, &mut line)
            .await
            .expect("read reply");
        let reply: Value = serde_json::from_str(line.trim()).expect("parse");
        assert_eq!(reply["ok"], Value::Bool(false));
        assert!(reply["error"].as_str().unwrap().contains("too large"));

        // Daemon must still accept new connections.
        let r = send_one(&socket, r#"{"text":"after-oversized"}"#).await;
        assert_eq!(r["ok"], Value::Bool(true));
        handle.abort();
    }

    // 9. client disconnects before reading reply — daemon survives
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_survives_client_disconnect_before_reply() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();
        let handle = spawn_serve(cfg, engine, rules, sink as Arc<dyn AudioSink>).await;

        {
            let mut stream = UnixStream::connect(&socket).await.expect("connect");
            stream
                .write_all(b"{\"text\":\"bye\"}\n")
                .await
                .expect("write");
            stream.shutdown().await.expect("shutdown");
            // Drop the stream immediately without reading the reply.
        }
        // Give the daemon a moment to notice the BrokenPipe.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Daemon must still accept new connections.
        let r = send_one(&socket, r#"{"text":"after-disconnect"}"#).await;
        assert_eq!(r["ok"], Value::Bool(true));
        handle.abort();
    }

    // 10. partial line write — daemon waits for the rest, then processes
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_handles_partial_line_write() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();
        let handle = spawn_serve(cfg, engine, rules, sink as Arc<dyn AudioSink>).await;

        let mut stream = UnixStream::connect(&socket).await.expect("connect");
        stream
            .write_all(b"{\"text\":\"hel")
            .await
            .expect("write half 1");
        stream.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_millis(80)).await;
        stream.write_all(b"lo\"}\n").await.expect("write half 2");

        let (read_half, _) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        AsyncBufReadExt::read_line(&mut reader, &mut line)
            .await
            .expect("read reply");
        let reply: Value = serde_json::from_str(line.trim()).expect("parse");
        assert_eq!(reply["ok"], Value::Bool(true));
        assert_eq!(reply["spoken"], "hello");
        handle.abort();
    }

    // ROADMAP 3.5: prove the daemon hands every spoken (voice, text)
    // pair to the injected `Mirror`. Uses `RecordingMirror` so we don't
    // care whether `osascript` exists or what env vars are set —
    // straight assertion on the call surface.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_calls_mirror_for_each_spoken_line() {
        use crate::notify_macos::test_support::RecordingMirror;
        use crate::notify_macos::Mirror;

        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();
        let recorder: Arc<RecordingMirror> = Arc::new(RecordingMirror::default());
        let mirror: Arc<dyn Mirror> = recorder.clone();

        let handle = spawn_serve_with_mirror(
            cfg,
            engine,
            rules,
            sink.clone() as Arc<dyn AudioSink>,
            mirror,
        )
        .await;

        // text-only frame: voice defaults to "default", text is verbatim.
        let reply = send_one(&socket, r#"{"text":"hello world","voice":"peter"}"#).await;
        assert_eq!(reply["ok"], Value::Bool(true));

        // event frame: voice + text resolved from rules.
        let reply2 = send_one(&socket, r#"{"event":"build_failed"}"#).await;
        assert_eq!(reply2["ok"], Value::Bool(true));

        // Give the handler tasks a moment to flush.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let calls = recorder.calls();
        assert_eq!(
            calls.len(),
            2,
            "expected exactly 2 mirror calls, got {calls:?}"
        );
        assert_eq!(calls[0], ("peter".to_string(), "hello world".to_string()));
        assert_eq!(calls[1].0, "angry_duck", "build_failed → angry_duck voice");
        assert!(
            [
                "The build failed again.",
                "That did not go well.",
                "The compiler has chosen violence."
            ]
            .contains(&calls[1].1.as_str()),
            "unexpected text: {:?}",
            calls[1].1,
        );

        handle.abort();
    }

    // ROADMAP 4.1: prove process_frame's event branch dispatches through
    // the injected `Arc<dyn ReactionProvider>`, NOT through rules.json.
    // Uses `RecordingProvider` to return a deterministic (voice, line)
    // and asserts the daemon spoke exactly that pair.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_dispatches_event_frame_through_provider() {
        use crate::reaction::test_support::RecordingProvider;
        use crate::reaction::ReactionProvider;

        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, _rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();
        let recorder: Arc<RecordingProvider> = Arc::new(RecordingProvider::new(
            "synthetic_voice",
            "synthetic line under test",
        ));
        let provider: Arc<dyn ReactionProvider> = recorder.clone();

        let handle = spawn_serve_with_provider(
            cfg,
            engine,
            provider,
            sink.clone() as Arc<dyn AudioSink>,
            crate::notify_macos::default_mirror(),
        )
        .await;

        let reply = send_one(&socket, r#"{"event":"build_failed"}"#).await;
        assert_eq!(reply["ok"], Value::Bool(true));
        assert_eq!(reply["spoken"], "synthetic line under test");
        assert_eq!(reply["voice"], "synthetic_voice");

        // Provider was called with the exact event id, not the resolved
        // text-or-voice.
        let events = recorder.events();
        assert_eq!(events, vec!["build_failed".to_string()]);

        handle.abort();
    }

    // ROADMAP 4.1: text-only frames must BYPASS the provider — text is
    // verbatim, voice is "default" (or the request override).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_text_only_frame_bypasses_provider() {
        use crate::reaction::test_support::RecordingProvider;
        use crate::reaction::ReactionProvider;

        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, _rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();
        let recorder: Arc<RecordingProvider> =
            Arc::new(RecordingProvider::new("ignored", "ignored"));
        let provider: Arc<dyn ReactionProvider> = recorder.clone();

        let handle = spawn_serve_with_provider(
            cfg,
            engine,
            provider,
            sink.clone() as Arc<dyn AudioSink>,
            crate::notify_macos::default_mirror(),
        )
        .await;

        let reply = send_one(&socket, r#"{"text":"hello world","voice":"peter"}"#).await;
        assert_eq!(reply["ok"], Value::Bool(true));
        assert_eq!(reply["spoken"], "hello world");
        assert_eq!(reply["voice"], "peter");

        // Provider was NOT called.
        assert!(
            recorder.events().is_empty(),
            "text frames must not call the reaction provider; got: {:?}",
            recorder.events()
        );

        handle.abort();
    }

    // ROADMAP 4.3: prove cast turns play through the playback queue
    // strictly serially (turn 2 starts AFTER turn 1 ends). Uses a
    // BlockingRecordingSink with a 50ms delay per play so the
    // ordering window is large enough to be measurable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_plays_cast_turns_in_strict_sequence() {
        use crate::playback::test_support::BlockingRecordingSink;
        use crate::reaction::test_support::RecordingCastProvider;
        use std::time::Duration;

        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, _rules, _sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();

        // Cast that returns 3 turns.
        let provider: Arc<dyn ReactionProvider> = Arc::new(RecordingCastProvider::new(vec![
            ("peter".to_string(), "first".to_string()),
            ("brian".to_string(), "second".to_string()),
            ("peter".to_string(), "third".to_string()),
        ]));

        // 50ms-per-play sink to make sequencing observable.
        let blocking_sink = Arc::new(BlockingRecordingSink::new(Duration::from_millis(50)));
        let handle = spawn_serve_with_provider(
            cfg,
            engine,
            provider,
            blocking_sink.clone() as Arc<dyn AudioSink>,
            crate::notify_macos::default_mirror(),
        )
        .await;

        let reply = send_one(&socket, r#"{"event":"build_failed"}"#).await;
        assert_eq!(reply["ok"], Value::Bool(true));
        // Reply uses the cast shape: spoken is an array.
        let spoken = reply["spoken"].as_array().expect("spoken must be array");
        assert_eq!(spoken.len(), 3);
        assert_eq!(spoken[0]["voice"], "peter");
        assert_eq!(spoken[1]["voice"], "brian");
        assert_eq!(spoken[2]["voice"], "peter");

        // Wait for queue to drain. 3 × 50ms theoretical, but slow CI
        // runners need more headroom — poll until all 3 events land
        // or 5s deadline.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if blocking_sink.events().len() >= 3 || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let events = blocking_sink.events();
        assert_eq!(events.len(), 3, "expected 3 plays, got {}", events.len());
        // Strict ordering: each next play STARTED at or after the
        // previous play ENDED.
        for w in events.windows(2) {
            assert!(
                w[1].1 >= w[0].2,
                "cast turn started before previous ended: {w:?}"
            );
        }

        handle.abort();
    }

    // ROADMAP 4.3 Showstopper 2: prove the daemon replies BEFORE the
    // cast finishes synthesizing (option-b sequencing). Uses
    // `fixture_slow_synth` which sleeps 200ms per synth call so a
    // 3-turn cast = 600ms of synth. fake_synth is microseconds and
    // would pass even on the broken pre-fix code (synth was
    // synchronous in process_frame); slow_synth is the only way to
    // prove the daemon doesn't await synth before replying.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_replies_before_cast_audio_finishes() {
        use crate::reaction::test_support::RecordingCastProvider;
        use std::time::Duration;

        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, _rules, sink) = fixture_slow_synth(tmp.path(), 200);
        let socket = cfg.socket_path.clone();

        let provider: Arc<dyn ReactionProvider> = Arc::new(RecordingCastProvider::new(vec![
            ("peter".to_string(), "a".to_string()),
            ("brian".to_string(), "b".to_string()),
            ("peter".to_string(), "c".to_string()),
        ]));
        let handle = spawn_serve_with_provider(
            cfg,
            engine,
            provider,
            sink.clone() as Arc<dyn AudioSink>,
            crate::notify_macos::default_mirror(),
        )
        .await;

        let send_started = std::time::Instant::now();
        let reply = send_one(&socket, r#"{"event":"build_failed"}"#).await;
        let reply_latency = send_started.elapsed();

        assert_eq!(reply["ok"], Value::Bool(true));
        // 3 turns × 200ms synth = 600ms total. Reply must come back
        // in less than ONE turn's synth duration; assert < 150ms
        // (well under the first turn's 200ms).
        assert!(
            reply_latency < Duration::from_millis(150),
            "reply took {reply_latency:?}; option-b sequencing broken — daemon should reply BEFORE the first synth finishes"
        );

        handle.abort();
    }

    // ROADMAP 4.3 Bug 2: explicit voice override on an event with a
    // configured cast must take the single-voice path (honor the
    // override). Otherwise users who say "speak in Peter's voice"
    // get a cast they didn't ask for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_voice_override_bypasses_cast() {
        use crate::reaction::test_support::RecordingCastProvider;

        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, _rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();

        // Provider returns a 3-turn cast from react_cast, but a
        // single-turn from react. The override path uses react.
        let provider: Arc<dyn ReactionProvider> = Arc::new(RecordingCastProvider::new(vec![
            ("peter".to_string(), "cast-line-1".to_string()),
            ("brian".to_string(), "cast-line-2".to_string()),
        ]));

        let handle = spawn_serve_with_provider(
            cfg,
            engine,
            provider,
            sink.clone() as Arc<dyn AudioSink>,
            crate::notify_macos::default_mirror(),
        )
        .await;

        // Override voice; daemon must use the SINGLE-voice path,
        // even though react_cast would return 3 turns.
        let reply = send_one(
            &socket,
            r#"{"event":"build_failed","voice":"override_voice"}"#,
        )
        .await;
        assert_eq!(reply["ok"], Value::Bool(true));
        // Single-voice reply shape (string spoken + voice field).
        assert!(
            reply["spoken"].is_string(),
            "voice override must produce single-voice reply, got: {reply}"
        );
        assert_eq!(reply["voice"], "override_voice");

        handle.abort();
    }
}
