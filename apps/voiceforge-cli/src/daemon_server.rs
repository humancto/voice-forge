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
use crate::rules::{choose_reaction, Rules};
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

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Reply {
    Ok {
        ok: bool, // always true; `untagged` needs the discriminant in-shape
        spoken: String,
        voice: String,
    },
    Err {
        ok: bool, // always false
        error: String,
    },
}

impl Reply {
    fn ok(spoken: impl Into<String>, voice: impl Into<String>) -> Self {
        Reply::Ok {
            ok: true,
            spoken: spoken.into(),
            voice: voice.into(),
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
    rules: Arc<Rules>,
    sink: Arc<dyn AudioSink>,
) -> Result<()> {
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
                let rules = Arc::clone(&rules);
                let sink = Arc::clone(&sink);
                let semaphore = Arc::clone(&semaphore);
                tokio::spawn(handle_connection(stream, engine, rules, sink, semaphore));
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
    rules: Arc<Rules>,
    sink: Arc<dyn AudioSink>,
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

        let reply = match process_frame(&buf, &engine, &rules, &sink, &semaphore).await {
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
    rules: &Arc<Rules>,
    sink: &Arc<dyn AudioSink>,
    semaphore: &Arc<Semaphore>,
) -> Result<Reply> {
    let frame: Frame =
        serde_json::from_slice(raw).context("frame is not valid JSON matching the schema")?;

    let (text, voice) = resolve_text_and_voice(&frame, rules)?;

    // Acquire the permit BEFORE synthesis so a flood doesn't queue up
    // unbounded synthesis work either.
    let permit = Arc::clone(semaphore)
        .acquire_owned()
        .await
        .map_err(|e| anyhow!("semaphore closed: {e}"))?;

    let audio_path = engine.speak(&text, &voice).await?;

    // rodio playback is sync. Move it onto the blocking pool, hand the
    // permit *into* the closure so it's released when playback ends
    // (even if the async caller is dropped).
    let sink_clone = Arc::clone(sink);
    let path_clone = audio_path.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit; // dropped on closure exit
        let result = sink_clone.play(&path_clone);
        if let Err(e) = result {
            eprintln!("voiceforge daemon: audio playback failed: {e:#}");
        }
    });

    Ok(Reply::ok(text, voice))
}

/// Pick the (text, voice) pair to actually speak based on the request:
///
/// 1. If `text` is set in the request, use it verbatim. Voice = request
///    voice if set, else "default".
/// 2. Else if `event` is set, dispatch through `rules`. Voice override
///    in the request still wins over the rule's voice.
/// 3. Else error: neither field present.
fn resolve_text_and_voice(frame: &Frame, rules: &Arc<Rules>) -> Result<(String, String)> {
    if let Some(text) = frame.text.as_deref() {
        let voice = frame.voice.clone().unwrap_or_else(|| "default".to_string());
        return Ok((text.to_string(), voice));
    }

    let event = frame
        .event
        .as_deref()
        .ok_or_else(|| anyhow!("frame must contain either \"event\" or \"text\""))?;

    let mut rng = rand::thread_rng();
    let fallback_voice = "default";
    let fallback_text = "Event received.";
    let (rule_voice, text) =
        choose_reaction(rules, event, (fallback_voice, fallback_text), &mut rng);

    let voice = frame.voice.clone().unwrap_or(rule_voice);
    Ok((text, voice))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tts::{Backend, EmbeddedEngine, SynthBuilder};
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    /// Test sink that records the WAV paths the daemon hands it. No
    /// audio device touched — `play` is a no-op that bumps a counter.
    #[derive(Default)]
    struct RecordingSink {
        played: Mutex<Vec<PathBuf>>,
        count: AtomicUsize,
    }

    impl RecordingSink {
        fn count(&self) -> usize {
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
    fn fake_synth() -> SynthBuilder {
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

    /// Build a minimal-but-real Engine + Rules + sink triple for tests.
    /// Returns the four pieces needed to spin up `serve` in a tempdir.
    fn fixture(tmp_root: &Path) -> (DaemonConfig, Arc<Engine>, Arc<Rules>, Arc<RecordingSink>) {
        let socket_path = tmp_root.join("voiceforge.sock");
        let cfg = DaemonConfig { socket_path };

        let cache_dir = tmp_root.join("cache");
        let embedded = EmbeddedEngine::for_testing(cache_dir, Backend::MacosSay, fake_synth());
        let engine = Arc::new(Engine::for_testing(embedded));

        let rules = Arc::new(Rules::default_builtin());
        let sink = Arc::new(RecordingSink::default());

        (cfg, engine, rules, sink)
    }

    /// Spawn `serve` on a tokio task and wait until the socket file
    /// exists (bind succeeded). Returns the join handle so the test
    /// can drop it / abort it cleanly.
    async fn spawn_serve(
        cfg: DaemonConfig,
        engine: Arc<Engine>,
        rules: Arc<Rules>,
        sink: Arc<dyn AudioSink>,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let socket_path = cfg.socket_path.clone();
        let handle = tokio::spawn(async move { serve(cfg, engine, rules, sink).await });
        // Wait for bind. Tight bound — this is a local socket on the same FS.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if probe_socket(&socket_path).await.unwrap_or(false) {
                return handle;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("daemon never bound at {}", socket_path.display());
    }

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
        let result = serve(cfg, engine, rules, sink as Arc<dyn AudioSink>).await;
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
}
