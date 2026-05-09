//! `voiceforge hook` -- stdin NDJSON forwarder for AI-agent
//! integrations (Claude Code, Cursor, Codex, etc.).
//!
//! See `.planning/voiceforge-hook.plan.md` for the design audit
//! (rust-expert plan v2 APPROVE).

use anyhow::{anyhow, Result};
use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::daemon_client::{self, SendError, SendOutcome, SendRequest};

/// Hard cap on `message` field length client-side. Protects daemon
/// log volume + reply size. Truncation happens BEFORE serialization
/// so what we transmit on the wire is bounded.
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone)]
pub struct HookConfig {
    pub event_from: Option<String>,
    pub message_from: Option<String>,
    pub voice: Option<String>,
    pub profile: Option<String>,
    pub passthrough: bool,
    pub quiet: bool,
    pub fail_ratio: f64,
    pub fail_window: usize,
    pub fail_min: usize,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            event_from: None,
            message_from: None,
            voice: None,
            profile: None,
            passthrough: false,
            quiet: false,
            fail_ratio: 0.5,
            fail_window: 100,
            fail_min: 10,
            connect_timeout: Duration::from_millis(1000),
            read_timeout: Duration::from_secs(5),
        }
    }
}

/// Production entry point. Wraps stdin/stdout with the right buffering
/// and delegates to `run_with_io`.
pub async fn run(cfg: HookConfig, socket_path: &Path) -> i32 {
    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    run_with_io(cfg, socket_path, reader, &mut stdout).await
}

/// Test seam: read from any AsyncBufRead, write passthrough to any
/// AsyncWrite. Returns the process exit code.
pub(crate) async fn run_with_io<R, W>(
    cfg: HookConfig,
    socket_path: &Path,
    reader: R,
    stdout: &mut W,
) -> i32
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = reader.lines();
    let mut window: VecDeque<bool> = VecDeque::with_capacity(cfg.fail_window);
    let mut first_attempt = true;

    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => return 0, // EOF
            Err(e) => {
                if !cfg.quiet {
                    eprintln!("voiceforge hook: stdin read error: {e}");
                }
                return 4;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        // Parse the line. Malformed -> drop + warn; passthrough does
        // NOT include malformed lines (downstream consumers expect
        // structured input).
        let parsed: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                if !cfg.quiet {
                    eprintln!("voiceforge hook: dropping malformed JSON line: {e}");
                }
                continue;
            }
        };

        // Passthrough BEFORE forward so a slow daemon doesn't gate the
        // downstream consumer.
        if cfg.passthrough && stdout.write_all(line.as_bytes()).await.is_ok() {
            let _ = stdout.write_all(b"\n").await;
            let _ = stdout.flush().await;
        }

        let req = build_request(&cfg, &parsed, &line);

        match daemon_client::send(socket_path, &req, cfg.connect_timeout, cfg.read_timeout).await {
            Ok(SendOutcome::Ok { .. }) => {
                push_outcome(&mut window, true, cfg.fail_window);
                first_attempt = false;
            }
            Ok(SendOutcome::Rejected { error }) => {
                if !cfg.quiet {
                    eprintln!("voiceforge hook: daemon rejected frame: {error}");
                }
                push_outcome(&mut window, false, cfg.fail_window);
                first_attempt = false;
            }
            Err(SendError::NotReachable(msg)) => {
                if first_attempt {
                    eprintln!("voiceforge hook: {msg}");
                    return 2;
                }
                if !cfg.quiet {
                    eprintln!("voiceforge hook: daemon not reachable: {msg}");
                }
                push_outcome(&mut window, false, cfg.fail_window);
            }
            Err(SendError::Protocol(msg)) => {
                if !cfg.quiet {
                    eprintln!("voiceforge hook: daemon protocol error: {msg}");
                }
                push_outcome(&mut window, false, cfg.fail_window);
                first_attempt = false;
            }
        }

        // Threshold check.
        if window.len() >= cfg.fail_min {
            let failures = window.iter().filter(|ok| !**ok).count();
            let ratio = failures as f64 / window.len() as f64;
            if ratio >= cfg.fail_ratio {
                eprintln!(
                    "voiceforge hook: daemon unhealthy ({} of last {} frames failed; ratio {:.2} >= threshold {:.2})",
                    failures,
                    window.len(),
                    ratio,
                    cfg.fail_ratio,
                );
                return 1;
            }
        }
    }
}

fn push_outcome(window: &mut VecDeque<bool>, ok: bool, cap: usize) {
    window.push_back(ok);
    while window.len() > cap {
        window.pop_front();
    }
}

fn build_request(cfg: &HookConfig, parsed: &serde_json::Value, raw_line: &str) -> SendRequest {
    let (profile_event, profile_message) = match cfg.profile.as_deref() {
        Some(p) => apply_profile(p, parsed),
        None => (None, None),
    };

    let event = cfg
        .event_from
        .as_deref()
        .and_then(|p| extract_field(parsed, p))
        .or(profile_event)
        .or_else(|| {
            parsed
                .get("event")
                .and_then(|v| v.as_str())
                .map(String::from)
        });

    let text = parsed
        .get("text")
        .and_then(|v| v.as_str())
        .map(String::from);

    let voice = cfg.voice.clone().or_else(|| {
        parsed
            .get("voice")
            .and_then(|v| v.as_str())
            .map(String::from)
    });

    let message = cfg
        .message_from
        .as_deref()
        .and_then(|p| extract_field(parsed, p))
        .or(profile_message)
        .or_else(|| {
            parsed
                .get("message")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| Some(raw_line.to_string()))
        .map(|m| truncate_to_bytes(&m, MAX_MESSAGE_BYTES));

    SendRequest {
        event,
        text,
        voice,
        message,
    }
}

/// Extract a string from a dotted JSON path. Spec:
/// - Traverses Value::Object only.
/// - Numeric path segments / array indexing -> None (non-feature).
/// - Value::Null -> None.
/// - Value::String -> the string.
/// - Value::Number / Value::Bool -> to_string().
/// - Value::Object / Value::Array at the leaf -> None.
/// - Missing intermediate -> None.
pub(crate) fn extract_field(value: &serde_json::Value, path: &str) -> Option<String> {
    let mut current = value;
    for segment in path.split('.') {
        match current {
            serde_json::Value::Object(map) => {
                current = map.get(segment)?;
            }
            _ => return None,
        }
    }
    match current {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Profile-aware (event, message) extractor for known upstreams.
/// Initial set: `claude-code`. Returns (event, message) options.
pub(crate) fn apply_profile(
    profile: &str,
    value: &serde_json::Value,
) -> (Option<String>, Option<String>) {
    match profile {
        "claude-code" => {
            // Claude Code's hook payload: hook_event_name = the event
            // type (Notification, PreToolUse, etc.); message = optional
            // free-form text. Fall through to the raw line for message.
            let event = extract_field(value, "hook_event_name");
            let message = extract_field(value, "message");
            (event, message)
        }
        _ => (None, None),
    }
}

/// Truncate a string to at most `max_bytes` UTF-8 bytes, snapping at
/// a char boundary so the result is still valid UTF-8.
fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

pub fn known_profiles() -> &'static [&'static str] {
    &["claude-code"]
}

pub fn validate_profile(profile: &str) -> Result<()> {
    if known_profiles().contains(&profile) {
        Ok(())
    } else {
        Err(anyhow!(
            "unknown profile {:?}; supported: {}",
            profile,
            known_profiles().join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_sink::AudioSink;
    use crate::daemon_server::test_support::{fixture, spawn_serve};
    use std::sync::Arc;
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    fn fast_cfg() -> HookConfig {
        HookConfig {
            connect_timeout: Duration::from_millis(500),
            read_timeout: Duration::from_secs(2),
            ..HookConfig::default()
        }
    }

    fn empty_stdout() -> Vec<u8> {
        Vec::new()
    }

    // 1. Round-trip a single event frame via run_with_io.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_forwards_single_event_frame() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg_d, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg_d.socket_path.clone();
        let h = spawn_serve(cfg_d, engine, rules, sink.clone() as Arc<dyn AudioSink>).await;

        let input = b"{\"event\":\"build_failed\"}\n".to_vec();
        let reader = BufReader::new(&input[..]);
        let mut stdout = empty_stdout();

        let exit = run_with_io(fast_cfg(), &socket, reader, &mut stdout).await;
        assert_eq!(exit, 0);

        // Wait for the spawn_blocking play call.
        for _ in 0..50 {
            if sink.count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(sink.count(), 1);
        h.abort();
    }

    // 2. text-only frame.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_forwards_text_frame() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg_d, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg_d.socket_path.clone();
        let h = spawn_serve(cfg_d, engine, rules, sink.clone() as Arc<dyn AudioSink>).await;

        let input = b"{\"text\":\"hello there\"}\n".to_vec();
        let reader = BufReader::new(&input[..]);
        let mut stdout = empty_stdout();

        let exit = run_with_io(fast_cfg(), &socket, reader, &mut stdout).await;
        assert_eq!(exit, 0);
        h.abort();
    }

    // 3. Multi-line stream.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_handles_multi_line_stream() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg_d, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg_d.socket_path.clone();
        let h = spawn_serve(cfg_d, engine, rules, sink.clone() as Arc<dyn AudioSink>).await;

        let input = (0..5)
            .map(|i| format!("{{\"text\":\"msg-{i}\"}}\n"))
            .collect::<String>()
            .into_bytes();
        let reader = BufReader::new(&input[..]);
        let mut stdout = empty_stdout();

        let exit = run_with_io(fast_cfg(), &socket, reader, &mut stdout).await;
        assert_eq!(exit, 0);

        for _ in 0..50 {
            if sink.count() >= 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(sink.count(), 5);
        h.abort();
    }

    // 4. --event-from with nested path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_with_event_from_extracts_nested_field() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg_d, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg_d.socket_path.clone();
        let h = spawn_serve(cfg_d, engine, rules, sink.clone() as Arc<dyn AudioSink>).await;

        let mut cfg = fast_cfg();
        cfg.event_from = Some("hook.event_name".into());
        let input = b"{\"hook\":{\"event_name\":\"command_failed\"}}\n".to_vec();
        let reader = BufReader::new(&input[..]);
        let mut stdout = empty_stdout();
        let exit = run_with_io(cfg, &socket, reader, &mut stdout).await;
        assert_eq!(exit, 0);

        for _ in 0..50 {
            if sink.count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(sink.count(), 1);
        h.abort();
    }

    // 5. --voice override.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_with_voice_override_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg_d, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg_d.socket_path.clone();
        let h = spawn_serve(cfg_d, engine, rules, sink.clone() as Arc<dyn AudioSink>).await;

        let mut cfg = fast_cfg();
        cfg.voice = Some("peter".into());
        // Frame has its own voice; --voice should win.
        let input = b"{\"text\":\"hi\",\"voice\":\"foo\"}\n".to_vec();
        let reader = BufReader::new(&input[..]);
        let mut stdout = empty_stdout();
        let exit = run_with_io(cfg, &socket, reader, &mut stdout).await;
        assert_eq!(exit, 0);
        h.abort();
    }

    // 6. Passthrough writes BEFORE forward.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_passthrough_writes_input_to_stdout() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg_d, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg_d.socket_path.clone();
        let h = spawn_serve(cfg_d, engine, rules, sink as Arc<dyn AudioSink>).await;

        let mut cfg = fast_cfg();
        cfg.passthrough = true;
        let input = b"{\"text\":\"x\"}\n{\"text\":\"y\"}\n".to_vec();
        let reader = BufReader::new(&input[..]);
        let mut stdout: Vec<u8> = Vec::new();
        let exit = run_with_io(cfg, &socket, reader, &mut stdout).await;
        assert_eq!(exit, 0);

        let out = String::from_utf8(stdout).unwrap();
        assert!(out.contains("{\"text\":\"x\"}"));
        assert!(out.contains("{\"text\":\"y\"}"));
        h.abort();
    }

    // 7. Malformed mid-stream -> drop + continue.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_drops_malformed_frame_warns_continues() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg_d, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg_d.socket_path.clone();
        let h = spawn_serve(cfg_d, engine, rules, sink.clone() as Arc<dyn AudioSink>).await;

        let input = b"{\"text\":\"a\"}\nthis is not json\n{\"text\":\"b\"}\n".to_vec();
        let reader = BufReader::new(&input[..]);
        let mut stdout = empty_stdout();
        let exit = run_with_io(fast_cfg(), &socket, reader, &mut stdout).await;
        assert_eq!(exit, 0);

        for _ in 0..50 {
            if sink.count() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(sink.count(), 2);
        h.abort();
    }

    // 8. Quiet suppresses warnings (we can't easily capture stderr;
    //    just ensure exit code is still 0 with quiet on).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_quiet_suppresses_warnings() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg_d, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg_d.socket_path.clone();
        let h = spawn_serve(cfg_d, engine, rules, sink as Arc<dyn AudioSink>).await;

        let mut cfg = fast_cfg();
        cfg.quiet = true;
        let input = b"not json\n{\"text\":\"x\"}\n".to_vec();
        let reader = BufReader::new(&input[..]);
        let mut stdout = empty_stdout();
        let exit = run_with_io(cfg, &socket, reader, &mut stdout).await;
        assert_eq!(exit, 0);
        h.abort();
    }

    // 9. First-frame NotReachable -> exit 2 immediately.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_returns_exit_2_on_first_frame_not_reachable() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("nope.sock");

        let mut cfg = fast_cfg();
        cfg.connect_timeout = Duration::from_millis(150);

        let input = b"{\"text\":\"x\"}\n".to_vec();
        let reader = BufReader::new(&input[..]);
        let mut stdout = empty_stdout();
        let exit = run_with_io(cfg, &socket, reader, &mut stdout).await;
        assert_eq!(exit, 2);
    }

    // 10. Threshold tripped: stub server that immediately closes
    //     each connection -> Protocol error counted as failure.
    //     Send 12 frames after a brief warm-up frame from a real daemon
    //     would be ideal, but switching mid-stream is complex. Simpler:
    //     stub that on connection 1 replies ok, then on subsequent
    //     connections closes immediately.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_returns_exit_1_when_failure_threshold_tripped() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("flaky.sock");
        let socket_for_listener = socket.clone();
        let listener_task = tokio::spawn(async move {
            let listener = UnixListener::bind(&socket_for_listener).expect("bind");
            let mut connection_count = 0;
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                connection_count += 1;
                if connection_count == 1 {
                    // First connection: drain + send valid ok reply.
                    use tokio::io::AsyncReadExt;
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream
                        .write_all(b"{\"ok\":true,\"spoken\":\"hi\",\"voice\":\"v\"}\n")
                        .await;
                    let _ = stream.flush().await;
                } else {
                    // Subsequent: drain + close (Protocol error on client).
                    use tokio::io::AsyncReadExt;
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    drop(stream);
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut cfg = fast_cfg();
        cfg.fail_min = 5; // trip earlier so the test runs quickly
        cfg.fail_window = 20;
        cfg.fail_ratio = 0.5;
        cfg.quiet = true;

        // 1 success + 12 failures -> ratio 12/13 > 0.5 once we have >=5 attempts
        let mut input = String::new();
        for _ in 0..13 {
            input.push_str("{\"text\":\"x\"}\n");
        }
        let reader = BufReader::new(input.as_bytes());
        let mut stdout = empty_stdout();
        let exit = run_with_io(cfg, &socket, reader, &mut stdout).await;
        assert_eq!(exit, 1);
        listener_task.abort();
    }

    // 11. --profile claude-code maps hook_event_name.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_with_profile_claude_code_maps_hook_event_name() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg_d, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg_d.socket_path.clone();
        let h = spawn_serve(cfg_d, engine, rules, sink.clone() as Arc<dyn AudioSink>).await;

        let mut cfg = fast_cfg();
        cfg.profile = Some("claude-code".into());
        let input =
            b"{\"hook_event_name\":\"build_failed\",\"message\":\"npm test died\"}\n".to_vec();
        let reader = BufReader::new(&input[..]);
        let mut stdout = empty_stdout();
        let exit = run_with_io(cfg, &socket, reader, &mut stdout).await;
        assert_eq!(exit, 0);

        for _ in 0..50 {
            if sink.count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(sink.count(), 1);
        h.abort();
    }

    // -- pure unit tests for extract_field / truncate / build_request --

    #[test]
    fn extract_field_handles_dotted_path() {
        let v: serde_json::Value = serde_json::from_str(r#"{"a":{"b":{"c":"deep"}}}"#).unwrap();
        assert_eq!(extract_field(&v, "a.b.c"), Some("deep".into()));
        assert_eq!(extract_field(&v, "a.b"), None); // object at leaf -> None
    }

    #[test]
    fn extract_field_returns_none_on_array_or_missing_or_null() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"arr":[1,2],"nul":null,"a":{"b":"x"}}"#).unwrap();
        assert_eq!(extract_field(&v, "arr"), None); // array -> None
        assert_eq!(extract_field(&v, "arr.0"), None); // numeric index -> None
        assert_eq!(extract_field(&v, "nul"), None);
        assert_eq!(extract_field(&v, "a.missing"), None);
        assert_eq!(extract_field(&v, "z"), None);
    }

    #[test]
    fn extract_field_stringifies_numbers_and_bools() {
        let v: serde_json::Value = serde_json::from_str(r#"{"n":42,"f":3.14,"b":true}"#).unwrap();
        assert_eq!(extract_field(&v, "n"), Some("42".into()));
        assert_eq!(extract_field(&v, "f"), Some("3.14".into()));
        assert_eq!(extract_field(&v, "b"), Some("true".into()));
    }

    #[test]
    fn message_truncated_to_4kib_before_serialize() {
        let cfg = HookConfig::default();
        // 8 KiB raw line, no message field -> falls back to raw, truncated.
        let raw = format!("{{\"x\":\"{}\"}}", "a".repeat(8 * 1024));
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
        let req = build_request(&cfg, &parsed, &raw);
        let msg = req.message.expect("message present");
        assert!(msg.len() <= MAX_MESSAGE_BYTES);
    }

    #[test]
    fn truncate_to_bytes_respects_char_boundaries() {
        // emoji is 4 bytes; cap at 5 should yield 1 emoji + nothing else.
        let s = "🎯🎯🎯";
        let t = truncate_to_bytes(s, 5);
        assert!(t.len() <= 5);
        assert!(t == "🎯");
    }

    #[test]
    fn known_profiles_includes_claude_code() {
        assert!(known_profiles().contains(&"claude-code"));
        assert!(validate_profile("claude-code").is_ok());
        assert!(validate_profile("nope").is_err());
    }
}
