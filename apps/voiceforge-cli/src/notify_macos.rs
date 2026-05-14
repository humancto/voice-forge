//! macOS Notification Center bridge (ROADMAP 3.5).
//!
//! Mirrors every spoken line as a Notification Center banner via
//! `osascript`. Opt-in via `VOICEFORGE_MIRROR_NOTIFICATIONS=1`. Compiles
//! to a no-op on non-macOS platforms.
//!
//! ## Wiring
//!
//! `daemon_server::process_frame` resolves the (voice, text) tuple,
//! then calls `mirror.mirror(&voice, &text)` BEFORE
//! `engine.speak(...)`. The banner appears at synthesis-start, not at
//! synthesis-end (200-800 ms later). If synthesis errors after the
//! banner shows, the user sees a banner for a line that was attempted
//! but not heard — preferable to silent failure, matches the spec.
//!
//! ## Why a trait
//!
//! `AudioSink` already uses Arc-of-trait dependency injection so tests
//! can swap a `RecordingSink` for the real `RodioSink`. We mirror that
//! pattern here: prod uses `OsascriptMirror`; tests use
//! `RecordingMirror` to assert the daemon called the bridge with the
//! right args. Avoids env-var contention + thread-local pitfalls.
//!
//! ## Why fire-and-forget via `tokio::process::Command`
//!
//! `std::process::Command::spawn` + drop-Child leaks zombies in a
//! long-lived daemon (parent never `wait`s and never exits to be
//! reparented to launchd). `tokio::process::Command` registers with
//! the runtime's SIGCHLD handler; we `tokio::spawn` an awaiter that
//! `.wait().await`s the child so it's reaped — but synthesis is never
//! blocked on the spawn.

use std::sync::{Arc, OnceLock};

/// Object-safe bridge between `process_frame` and the actual notification
/// implementation. `mirror()` MUST NOT block — implementations either
/// spawn-and-detach or return immediately.
///
/// **Runtime requirement:** some implementations (notably
/// `OsascriptMirror`) require a Tokio runtime context. Today the only
/// call site is the daemon's `process_frame`, which is always polled
/// inside the daemon's runtime. Future call sites must respect this.
pub(crate) trait Mirror: Send + Sync {
    /// Best-effort mirror of `(voice, text)` to a platform notification.
    /// Implementations MUST NOT block. Failures are logged, not
    /// returned.
    fn mirror(&self, voice: &str, text: &str);
}

/// Production impl. On macOS, spawns `osascript` to display a
/// Notification Center banner. On other platforms, no-op.
pub(crate) struct OsascriptMirror;

impl Mirror for OsascriptMirror {
    /// # Panics
    ///
    /// On macOS: panics if called outside a Tokio runtime context
    /// (uses `tokio::process::Command::spawn` and `tokio::spawn`).
    /// Daemon's `process_frame` always satisfies this.
    fn mirror(&self, voice: &str, text: &str) {
        if !enabled() {
            return;
        }
        spawn_osascript(voice, text);
    }
}

/// Default Mirror used by the prod daemon. Returns
/// `Arc<OsascriptMirror>` boxed as `Arc<dyn Mirror>`.
pub(crate) fn default_mirror() -> Arc<dyn Mirror> {
    Arc::new(OsascriptMirror)
}

/// Cached on first call. Daemon-lifetime opt-in flag.
///
/// Caching avoids two problems:
///   1. Per-event env-var lookup overhead (process-wide lock on Unix).
///   2. Post-Rust-1.85, `std::env::set_var` is `unsafe` because reading
///      env from another thread while writing is UB. Caching means we
///      read the env exactly once.
pub fn enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| enabled_with(|k| std::env::var(k).ok()))
}

/// Test-friendly: pure function over an injected env reader. No cache.
/// Truthy values: `1`, `true`, `yes`, `on` (case-sensitive). Anything
/// else (including unset) is false.
pub(crate) fn enabled_with<F>(env: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    matches!(
        env("VOICEFORGE_MIRROR_NOTIFICATIONS").as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// AppleScript-string-literal escape.
///
/// Order is mandatory:
///   1. Replace any `c.is_control()` char (NUL, newline, tab, CR, ...)
///      with a single space. NUL would otherwise blow up `Command::arg`
///      via `CString::new`. Newlines render oddly in Notification
///      Center anyway.
///   2. Replace `\` with `\\` (must come before step 3, otherwise step
///      3's inserted `\` gets re-escaped).
///   3. Replace `"` with `\"`.
///
/// Neutralizes the obvious injection vector `" with title "evil` —
/// the closing `"` becomes `\"` and the AppleScript string stays open.
///
/// Built on macOS (used by `spawn_osascript`) and on all platforms in
/// `cfg(test)` (the unit tests cover the pure logic cross-platform).
/// Omitted from release Linux builds.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn escape_for_applescript(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if c.is_control() {
            out.push(' ');
        } else if c == '\\' {
            out.push_str("\\\\");
        } else if c == '"' {
            out.push_str("\\\"");
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(any(target_os = "macos", test))]
const MAX_BODY_BYTES: usize = 240;

/// Cap the body to 240 bytes (Notification Center truncates around 256;
/// 240 leaves headroom for the title + AppleScript wrapper). Reuses
/// `watch::truncate_to_bytes` so the binary has one canonical truncator.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn truncate_body(s: &str) -> String {
    crate::watch::truncate_to_bytes(s, MAX_BODY_BYTES)
}

/// Build the AppleScript source. Pure — no side effects, used in tests.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn build_script(voice: &str, text: &str) -> String {
    let title = format!("voiceforge \u{00B7} {}", voice);
    let body = truncate_body(text);
    format!(
        "display notification \"{}\" with title \"{}\"",
        escape_for_applescript(&body),
        escape_for_applescript(&title),
    )
}

#[cfg(target_os = "macos")]
fn spawn_osascript(voice: &str, text: &str) {
    let script = build_script(voice, text);
    let result = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match result {
        Ok(mut child) => {
            // Detach: never block synthesis on osascript completion. The
            // awaiter exists purely to reap the child via Tokio's SIGCHLD
            // handler — without it, we'd leak a zombie per spoken line.
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        Err(e) => {
            eprintln!("voiceforge: osascript spawn failed: {e}");
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn spawn_osascript(_voice: &str, _text: &str) {
    // Non-macOS: no-op. ROADMAP 3.5 is macOS-specific; future Linux
    // bridge would go here as a `notify-send` invocation.
}

/// PR-D D-2: notify on `voiceforge note` completion. Test-injectable
/// over `Mirror` so unit tests can assert the call without spawning
/// osascript.
pub(crate) fn notify_note_complete_with(
    mirror: &dyn Mirror,
    voice: &str,
    chunks: usize,
    secs: f64,
) {
    if !enabled() {
        return;
    }
    mirror.mirror(
        voice,
        &format!("note rendered ({chunks} chunks, {secs:.1}s)"),
    );
}

#[allow(dead_code)]
pub(crate) fn notify_note_complete(voice: &str, chunks: usize, secs: f64) {
    notify_note_complete_with(&*default_mirror(), voice, chunks, secs);
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::Mirror;
    use std::sync::Mutex;

    /// Records every (voice, text) pair the daemon hands to the mirror.
    /// Used by daemon integration tests to prove the call site fires.
    #[derive(Default)]
    pub(crate) struct RecordingMirror {
        calls: Mutex<Vec<(String, String)>>,
    }

    impl RecordingMirror {
        pub(crate) fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Mirror for RecordingMirror {
        fn mirror(&self, voice: &str, text: &str) {
            self.calls
                .lock()
                .unwrap()
                .push((voice.to_string(), text.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_passes_plain_text_unchanged() {
        assert_eq!(escape_for_applescript("hello world"), "hello world");
    }

    #[test]
    fn escape_escapes_double_quote() {
        assert_eq!(escape_for_applescript(r#"say "hi""#), r#"say \"hi\""#);
    }

    #[test]
    fn escape_escapes_backslash() {
        assert_eq!(escape_for_applescript(r"a\b"), r"a\\b");
    }

    #[test]
    fn escape_strips_newlines_and_tabs_to_space() {
        assert_eq!(escape_for_applescript("a\nb\tc\rd"), "a b c d");
    }

    #[test]
    fn escape_strips_nul() {
        assert_eq!(escape_for_applescript("a\0b"), "a b");
    }

    #[test]
    fn escape_neutralizes_injection_attempt() {
        // Attacker tries to terminate the body string and inject script.
        let evil = r#"" with title "PWNED"#;
        let out = escape_for_applescript(evil);
        // Every `"` in the output must be preceded by `\` (i.e. inside
        // a string literal, no escape can re-open the string).
        let bytes = out.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'"' {
                assert!(
                    i > 0 && bytes[i - 1] == b'\\',
                    "unescaped quote at {i}: {out:?}"
                );
            }
        }
    }

    #[test]
    fn enabled_with_returns_true_for_truthy_values() {
        for v in ["1", "true", "yes", "on"] {
            let v_owned = v.to_string();
            assert!(
                enabled_with(|k| if k == "VOICEFORGE_MIRROR_NOTIFICATIONS" {
                    Some(v_owned.clone())
                } else {
                    None
                }),
                "expected {v:?} → true",
            );
        }
    }

    #[test]
    fn enabled_with_returns_false_for_falsy_values() {
        for v in ["0", "false", "no", "off", "", "TRUE", "Yes", "anything"] {
            let v_owned = v.to_string();
            assert!(
                !enabled_with(|k| if k == "VOICEFORGE_MIRROR_NOTIFICATIONS" {
                    Some(v_owned.clone())
                } else {
                    None
                }),
                "expected {v:?} → false",
            );
        }
    }

    #[test]
    fn enabled_with_returns_false_when_unset() {
        assert!(!enabled_with(|_| None));
    }

    #[test]
    fn truncate_body_caps_at_240_bytes_utf8_safe() {
        // 4-byte chars × 100 = 400 bytes → must truncate at a char
        // boundary, never panic.
        let s: String = "🎙".repeat(100);
        let out = truncate_body(&s);
        assert!(out.len() <= 240);
        // Output is still valid UTF-8 (would have panicked otherwise).
        assert!(out.chars().all(|c| c == '🎙'));
    }

    #[test]
    fn build_script_includes_title_and_body_escaped() {
        let s = build_script("peter", r#"build "failed""#);
        // Title must contain the literal middle-dot byte sequence and
        // the voice name (we render `voiceforge · peter`).
        assert!(
            s.contains("voiceforge \u{00B7} peter"),
            "title missing: {s:?}"
        );
        // Body must have the inner double-quotes escaped.
        assert!(s.contains(r#"build \"failed\""#));
        // No raw double-quote inside the body or title.
        // (The outer wrapper has exactly 4 unescaped `"`s.)
        let unescaped: usize = s
            .as_bytes()
            .iter()
            .enumerate()
            .filter(|(i, &b)| b == b'"' && (*i == 0 || s.as_bytes()[i - 1] != b'\\'))
            .count();
        assert_eq!(unescaped, 4, "expected exactly 4 unescaped quotes: {s:?}");
    }

    #[test]
    fn recording_mirror_captures_calls() {
        use test_support::RecordingMirror;
        let m = RecordingMirror::default();
        m.mirror("peter", "hello");
        m.mirror("default", "world");
        assert_eq!(
            m.calls(),
            vec![
                ("peter".to_string(), "hello".to_string()),
                ("default".to_string(), "world".to_string()),
            ]
        );
    }
}
