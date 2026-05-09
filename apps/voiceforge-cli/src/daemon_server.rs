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
