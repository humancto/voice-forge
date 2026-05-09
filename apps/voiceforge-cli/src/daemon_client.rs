//! NDJSON client for the daemon (ROADMAP 1.9).
//!
//! Companion to `daemon_server`. Connects to a Unix socket, writes
//! one JSON frame, reads one reply, returns. The CLI dispatcher in
//! `main.rs` maps the result/outcome combinations to exit codes:
//!
//!   Ok(SendOutcome::Ok)        → exit 0
//!   Ok(SendOutcome::Rejected)  → exit 1   (daemon said `ok:false`)
//!   Err(NotReachable)          → exit 2   (no socket / connect refused)
//!   Err(Protocol)              → exit 4   (post-connect failure)
//!
//! Connect uses a 25 ms-backoff retry loop on ECONNREFUSED/NotFound
//! up to a caller-supplied deadline so the `voiceforge daemon &
//! voiceforge send foo` one-liner just works.

use serde::Serialize;
use std::io;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Cap on the size of a single reply line. Sanity bound — daemon
/// replies are tiny (a JSON object with a couple of strings); a
/// runaway daemon shouldn't OOM the client.
const MAX_REPLY_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Default, Serialize)]
pub struct SendRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    Ok { spoken: String, voice: String },
    Rejected { error: String },
}

#[derive(Debug, Error)]
pub enum SendError {
    /// No socket file, no listener, or connect-refused past deadline.
    /// CLI maps to exit 2.
    #[error("daemon not reachable: {0}")]
    NotReachable(String),
    /// Daemon was reachable but reply was unparseable / truncated /
    /// missing required fields, or read deadline elapsed. CLI maps
    /// to exit 4.
    #[error("daemon protocol error: {0}")]
    Protocol(String),
}

/// Connect to `socket_path`, write `req` as a single JSON line, read
/// one reply line under `read_timeout`, parse and return.
///
/// `connect_timeout` bounds the retry loop. ECONNREFUSED / NotFound
/// while the listener is still binding are retried with 25 ms
/// backoff; permission-denied / other errors fail immediately.
pub async fn send(
    socket_path: &Path,
    req: &SendRequest,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<SendOutcome, SendError> {
    let mut stream = connect_with_retry(socket_path, connect_timeout).await?;

    // Serialize + write.
    let mut frame = serde_json::to_vec(req)
        .map_err(|e| SendError::Protocol(format!("serializing request: {e}")))?;
    frame.push(b'\n');
    stream
        .write_all(&frame)
        .await
        .map_err(|e| SendError::Protocol(format!("writing request: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| SendError::Protocol(format!("flushing request: {e}")))?;

    // Read one reply line under the deadline. take(MAX+1) caps the
    // buffer growth so a runaway daemon can't OOM us.
    let (read_half, _write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half).take(MAX_REPLY_BYTES + 1);
    let mut line = String::new();
    let read_result = tokio::time::timeout(read_timeout, reader.read_line(&mut line)).await;

    let n = match read_result {
        Err(_) => {
            return Err(SendError::Protocol(format!(
                "read timed out after {} ms",
                read_timeout.as_millis()
            )));
        }
        Ok(Err(e)) => return Err(SendError::Protocol(format!("reading reply: {e}"))),
        Ok(Ok(n)) => n,
    };

    if n == 0 {
        return Err(SendError::Protocol(
            "daemon closed connection without replying".into(),
        ));
    }
    if n as u64 > MAX_REPLY_BYTES {
        return Err(SendError::Protocol(format!(
            "reply exceeded {} bytes",
            MAX_REPLY_BYTES
        )));
    }

    parse_reply(line.trim_end_matches('\n'))
}

/// Connect retry loop. Returns the connected stream OR `NotReachable`.
/// Only ECONNREFUSED / NotFound are retried — permission errors and
/// other I/O failures bail immediately.
async fn connect_with_retry(path: &Path, timeout: Duration) -> Result<UnixStream, SendError> {
    let start = tokio::time::Instant::now();
    let deadline = start + timeout;

    loop {
        match UnixStream::connect(path).await {
            Ok(s) => return Ok(s),
            Err(e) if is_retryable(&e) => {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    return Err(SendError::NotReachable(format!(
                        "{} ({}); is the daemon running? `voiceforge daemon &`",
                        path.display(),
                        e.kind()
                    )));
                }
                let remaining = deadline - now;
                let nap = remaining.min(Duration::from_millis(25));
                tokio::time::sleep(nap).await;
            }
            Err(e) => {
                return Err(SendError::NotReachable(format!(
                    "{}: {}",
                    path.display(),
                    e
                )));
            }
        }
    }
}

fn is_retryable(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
    )
}

/// Parse a daemon reply line. The wire format guarantees `ok:bool` is
/// present; on `true` we look for `spoken` + `voice`, on `false` we
/// look for `error`. Anything else is a Protocol error.
fn parse_reply(line: &str) -> Result<SendOutcome, SendError> {
    let v: serde_json::Value = serde_json::from_str(line)
        .map_err(|e| SendError::Protocol(format!("reply was not JSON: {e} ({line:?})")))?;

    let ok = v
        .get("ok")
        .and_then(|x| x.as_bool())
        .ok_or_else(|| SendError::Protocol(format!("reply missing `ok` field: {line:?}")))?;

    if ok {
        let spoken = v
            .get("spoken")
            .and_then(|x| x.as_str())
            .ok_or_else(|| {
                SendError::Protocol(format!("ok reply missing `spoken` field: {line:?}"))
            })?
            .to_string();
        let voice = v
            .get("voice")
            .and_then(|x| x.as_str())
            .ok_or_else(|| {
                SendError::Protocol(format!("ok reply missing `voice` field: {line:?}"))
            })?
            .to_string();
        Ok(SendOutcome::Ok { spoken, voice })
    } else {
        let error = v
            .get("error")
            .and_then(|x| x.as_str())
            .unwrap_or("(no error message)")
            .to_string();
        Ok(SendOutcome::Rejected { error })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_sink::AudioSink;
    use crate::daemon_server::test_support::{fixture, spawn_serve};
    use std::sync::Arc;
    use tokio::net::UnixListener;

    /// Default timeouts for tests that aren't specifically timing-sensitive.
    fn fast_timeouts() -> (Duration, Duration) {
        (Duration::from_millis(500), Duration::from_secs(2))
    }

    // 1. Round-trip an event frame against the real daemon.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn send_ok_event_round_trips() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();
        let h = spawn_serve(cfg, engine, rules, sink as Arc<dyn AudioSink>).await;

        let (ct, rt) = fast_timeouts();
        let req = SendRequest {
            event: Some("build_failed".into()),
            ..Default::default()
        };
        let outcome = send(&socket, &req, ct, rt).await.expect("send");
        match outcome {
            SendOutcome::Ok { spoken, voice } => {
                assert_eq!(voice, "angry_duck");
                assert!(
                    [
                        "The build failed again.",
                        "That did not go well.",
                        "The compiler has chosen violence.",
                    ]
                    .contains(&spoken.as_str()),
                    "unexpected spoken: {spoken:?}",
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        h.abort();
    }

    // 2. text + voice round-trip.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn send_text_with_voice_round_trips() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();
        let h = spawn_serve(cfg, engine, rules, sink as Arc<dyn AudioSink>).await;

        let (ct, rt) = fast_timeouts();
        let req = SendRequest {
            text: Some("hello".into()),
            voice: Some("peter".into()),
            ..Default::default()
        };
        let outcome = send(&socket, &req, ct, rt).await.expect("send");
        assert_eq!(
            outcome,
            SendOutcome::Ok {
                spoken: "hello".into(),
                voice: "peter".into()
            }
        );
        h.abort();
    }

    // 3. empty frame → daemon replies ok:false → Rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn send_returns_rejected_on_empty_frame() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (cfg, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg.socket_path.clone();
        let h = spawn_serve(cfg, engine, rules, sink as Arc<dyn AudioSink>).await;

        let (ct, rt) = fast_timeouts();
        let req = SendRequest::default();
        let outcome = send(&socket, &req, ct, rt).await.expect("send");
        match outcome {
            SendOutcome::Rejected { error } => assert!(error.contains("event")),
            other => panic!("expected Rejected, got {other:?}"),
        }
        h.abort();
    }

    // 4. no daemon at all → NotReachable past tight deadline.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn send_returns_not_reachable_when_no_daemon() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("nope.sock");

        let req = SendRequest {
            event: Some("x".into()),
            ..Default::default()
        };
        let err = send(
            &socket,
            &req,
            Duration::from_millis(200),
            Duration::from_secs(1),
        )
        .await
        .expect_err("must fail");
        match err {
            SendError::NotReachable(_) => {}
            other => panic!("expected NotReachable, got {other:?}"),
        }
    }

    // 5. bind race — listener appears 100 ms after `send` starts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn send_retries_during_bind_race() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("delayed.sock");

        let socket_for_listener = socket.clone();
        // Spawn the binder BEFORE calling send (rust-expert note 6).
        let listener_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let listener = UnixListener::bind(&socket_for_listener).expect("bind");
            // Accept one connection, write a valid reply, drop.
            let (mut stream, _) = listener.accept().await.expect("accept");
            // Drain the request first.
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            stream
                .write_all(b"{\"ok\":true,\"spoken\":\"hi\",\"voice\":\"v\"}\n")
                .await
                .expect("write reply");
            stream.flush().await.expect("flush");
        });
        tokio::task::yield_now().await;

        let req = SendRequest {
            text: Some("hi".into()),
            ..Default::default()
        };
        let outcome = send(
            &socket,
            &req,
            Duration::from_millis(1000),
            Duration::from_secs(2),
        )
        .await
        .expect("send");
        assert_eq!(
            outcome,
            SendOutcome::Ok {
                spoken: "hi".into(),
                voice: "v".into()
            }
        );
        listener_task.await.expect("listener task");
    }

    // Helper: stub UnixListener that runs a one-shot accept + custom replier.
    struct StubServer {
        socket: std::path::PathBuf,
        handle: tokio::task::JoinHandle<()>,
    }
    impl StubServer {
        async fn spawn<F>(socket: std::path::PathBuf, replier: F) -> Self
        where
            F: FnOnce(
                    tokio::net::UnixStream,
                )
                    -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
                + Send
                + 'static,
        {
            let listener = UnixListener::bind(&socket).expect("bind");
            let handle = tokio::spawn(async move {
                if let Ok((stream, _)) = listener.accept().await {
                    replier(stream).await;
                }
            });
            // Tiny yield so the listener is in accept() before send() probes.
            tokio::time::sleep(Duration::from_millis(10)).await;
            Self { socket, handle }
        }
    }
    impl Drop for StubServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    // 6. non-JSON reply → Protocol error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn send_returns_protocol_error_on_non_json_reply() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stub = StubServer::spawn(tmp.path().join("s.sock"), |mut stream| {
            Box::pin(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(b"not json at all\n").await;
                let _ = stream.flush().await;
            })
        })
        .await;

        let (ct, rt) = fast_timeouts();
        let req = SendRequest {
            text: Some("x".into()),
            ..Default::default()
        };
        let err = send(&stub.socket, &req, ct, rt)
            .await
            .expect_err("must err");
        match err {
            SendError::Protocol(msg) => assert!(msg.contains("not JSON") || msg.contains("JSON")),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    // 7. EOF before any reply.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn send_returns_protocol_error_on_eof_before_reply() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stub = StubServer::spawn(tmp.path().join("s.sock"), |mut stream| {
            Box::pin(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                // Drop the stream without writing a reply.
                drop(stream);
            })
        })
        .await;

        let (ct, rt) = fast_timeouts();
        let req = SendRequest {
            text: Some("x".into()),
            ..Default::default()
        };
        let err = send(&stub.socket, &req, ct, rt)
            .await
            .expect_err("must err");
        match err {
            SendError::Protocol(msg) => assert!(msg.contains("closed")),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    // 8. reply missing `ok` field.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn send_returns_protocol_error_on_missing_ok_field() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stub = StubServer::spawn(tmp.path().join("s.sock"), |mut stream| {
            Box::pin(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(b"{\"spoken\":\"x\"}\n").await;
                let _ = stream.flush().await;
            })
        })
        .await;

        let (ct, rt) = fast_timeouts();
        let req = SendRequest {
            text: Some("x".into()),
            ..Default::default()
        };
        let err = send(&stub.socket, &req, ct, rt)
            .await
            .expect_err("must err");
        match err {
            SendError::Protocol(msg) => assert!(msg.contains("ok")),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    // 9. Read timeout: stub accepts then sleeps forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn send_returns_protocol_error_on_read_timeout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stub = StubServer::spawn(tmp.path().join("s.sock"), |mut stream| {
            Box::pin(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                // Hold the stream open without writing a reply.
                tokio::time::sleep(Duration::from_secs(60)).await;
                drop(stream);
            })
        })
        .await;

        let req = SendRequest {
            text: Some("x".into()),
            ..Default::default()
        };
        let err = send(
            &stub.socket,
            &req,
            Duration::from_millis(500),
            Duration::from_millis(200), // tight read deadline
        )
        .await
        .expect_err("must err");
        match err {
            SendError::Protocol(msg) => {
                assert!(msg.contains("timed out") || msg.contains("timeout"))
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
    }
}
