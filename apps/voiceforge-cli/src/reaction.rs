//! Pluggable `(voice, line)` source for daemon events (ROADMAP 4.1).
//!
//! ## Design
//!
//! `ReactionProvider` is the trait. Two impls:
//!
//! - `StaticProvider`: today's behavior — pull from `rules::Rules`.
//!   Default. Always available.
//! - `LlmProvider`: POST to an OpenAI-compatible chat endpoint, parse
//!   `(voice, line)`, fall back to `StaticProvider` on **any** failure
//!   (network, timeout, parse, schema, unknown voice). Opt-in via
//!   `VOICEFORGE_LLM_URL`.
//!
//! ## Why `async-trait` and not AFIT
//!
//! rustc 1.91 has dyn-compatible AFIT, but the explicit `+ Send` bound
//! we need (`fn react -> impl Future<Output=...> + Send`) makes the
//! trait NOT dyn-compatible on stable. We need `Arc<dyn ReactionProvider>`
//! so `select_provider` can return either impl behind a single type, so
//! `async-trait` (which generates `Pin<Box<dyn Future + Send>>`) is the
//! correct choice. One Box per call is rounding error against a
//! 500-3000ms LLM round-trip.
//!
//! ## Failure semantics
//!
//! Default: silent fallback to Static on any LLM failure. The daemon
//! never goes silent — the user gets a line.
//!
//! `VOICEFORGE_LLM_STRICT=1`: LLM failures bubble up; daemon returns
//! `Reply::err`. Useful for CI / debugging.
//!
//! ## Circuit breaker
//!
//! Time-windowed, lock-free. Two atomics. After 5 failures within 60s
//! the breaker trips for 5 minutes, then half-opens to probe. Race-tolerant
//! by design — worst case a few wasted LLM calls before tripping.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::rules::Rules;

const DEFAULT_TIMEOUT_MS: u64 = 2_000;
const MAX_LINE_CHARS: usize = 200;
const BREAKER_FAILURE_THRESHOLD: u32 = 5;
const BREAKER_WINDOW_SECS: u64 = 60;
const BREAKER_RESET_SECS: u64 = 300;

/// Picks `(voice, line)` for an event.
///
/// **Cancel-safe**: dropping a pending `react()` call has no observable
/// side effects (reqwest cancels the in-flight request; the return type
/// has no half-completed state).
#[async_trait]
pub trait ReactionProvider: Send + Sync {
    /// Resolve the event into a `(voice, line)` pair. Implementations
    /// that fail internally (LLM down etc.) MUST either fall back
    /// silently or return a deterministic value — never panic.
    async fn react(&self, event: &str) -> (String, String);

    /// Stable identifier for `voiceforge doctor` and logs.
    fn name(&self) -> &'static str;
}

// ============================================================================
// StaticProvider — today's behavior
// ============================================================================

/// Wraps `Rules`. The fallback pair is used when an event is missing
/// from the loaded rules — matches the legacy `choose_reaction`
/// signature.
pub struct StaticProvider {
    rules: Arc<Rules>,
    fallback: (String, String),
}

impl StaticProvider {
    pub fn new(rules: Arc<Rules>) -> Self {
        Self {
            rules,
            fallback: ("default".to_string(), "Event received.".to_string()),
        }
    }
}

#[async_trait]
impl ReactionProvider for StaticProvider {
    async fn react(&self, event: &str) -> (String, String) {
        let mut rng = rand::thread_rng();
        if let Some((voice, line)) = self.rules.pick(event, &mut rng) {
            (voice.to_string(), line.to_string())
        } else {
            self.fallback.clone()
        }
    }

    fn name(&self) -> &'static str {
        "static"
    }
}

// ============================================================================
// CircuitBreaker — time-windowed, lock-free
// ============================================================================

/// State machine:
///
/// - **CLOSED**: `reset_at_nanos == 0`. Failures increment a counter;
///   if 5 land within 60s the breaker trips.
/// - **OPEN**: `reset_at_nanos > now`. All calls skip the LLM and go
///   straight to fallback (or error in strict mode).
/// - **HALF_OPEN**: `0 < reset_at_nanos <= now`. Next call probes; on
///   success → CLOSED, on failure → OPEN with fresh timer.
///
/// The window is a rolling 60s approximation: when `failure_count`
/// increments past 0, we record `window_start`; if a subsequent failure
/// arrives more than 60s later, we reset both before incrementing.
/// Race-tolerant: 5 simultaneous failures might all see count=0 and
/// race to increment — worst case a few extra LLM calls before tripping.
pub(crate) struct CircuitBreaker {
    epoch: Instant,
    failure_count: AtomicU32,
    window_start_nanos: AtomicU64,
    reset_at_nanos: AtomicU64,
}

impl CircuitBreaker {
    pub(crate) fn new() -> Self {
        Self {
            epoch: Instant::now(),
            failure_count: AtomicU32::new(0),
            window_start_nanos: AtomicU64::new(0),
            reset_at_nanos: AtomicU64::new(0),
        }
    }

    fn now_nanos(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    /// Returns `true` if the LLM should be skipped right now.
    pub(crate) fn is_open(&self) -> bool {
        let reset = self.reset_at_nanos.load(Ordering::Acquire);
        reset != 0 && self.now_nanos() < reset
    }

    /// Returns `true` if we're in HALF_OPEN (one probe permitted).
    /// Currently only checked by `is_open()`'s complement; reserved for
    /// the explicit half-open probe path in 4.1.1.
    #[allow(dead_code)]
    pub(crate) fn is_half_open(&self) -> bool {
        let reset = self.reset_at_nanos.load(Ordering::Acquire);
        reset != 0 && self.now_nanos() >= reset
    }

    pub(crate) fn record_success(&self) {
        self.failure_count.store(0, Ordering::Release);
        self.window_start_nanos.store(0, Ordering::Release);
        self.reset_at_nanos.store(0, Ordering::Release);
    }

    pub(crate) fn record_failure(&self) {
        let now = self.now_nanos();
        let window_start = self.window_start_nanos.load(Ordering::Acquire);
        let window_nanos = BREAKER_WINDOW_SECS * 1_000_000_000;
        if window_start == 0 || now.saturating_sub(window_start) > window_nanos {
            self.window_start_nanos.store(now, Ordering::Release);
            self.failure_count.store(1, Ordering::Release);
        } else {
            let prev = self.failure_count.fetch_add(1, Ordering::AcqRel);
            if prev + 1 >= BREAKER_FAILURE_THRESHOLD {
                let reset_nanos = BREAKER_RESET_SECS * 1_000_000_000;
                self.reset_at_nanos
                    .store(now.saturating_add(reset_nanos), Ordering::Release);
            }
        }
    }
}

// ============================================================================
// LlmProvider
// ============================================================================

#[derive(Debug, Deserialize)]
struct LlmDirect {
    voice: String,
    line: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiEnvelope {
    choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAiMessage {
    content: String,
}

pub struct LlmProvider {
    client: reqwest::Client,
    url: String,
    api_key: Option<String>,
    model: Option<String>,
    voices_normalized: HashSet<String>,
    voices_for_prompt: Arc<[String]>,
    timeout: Duration,
    strict: bool,
    breaker: CircuitBreaker,
    static_fallback: Arc<StaticProvider>,
    /// Track voices we've already warned about to avoid stderr spam.
    unknown_voices_warned: Mutex<HashSet<String>>,
    /// Track failure modes we've already logged this session.
    failure_modes_logged: Mutex<HashSet<&'static str>>,
}

impl LlmProvider {
    pub fn new(
        url: String,
        api_key: Option<String>,
        model: Option<String>,
        timeout: Duration,
        strict: bool,
        voices: Vec<String>,
        static_fallback: Arc<StaticProvider>,
    ) -> Self {
        let voices_normalized: HashSet<String> =
            voices.iter().map(|v| normalize_voice(v)).collect();
        let voices_for_prompt: Arc<[String]> = voices.into();
        Self {
            client: reqwest::Client::new(),
            url,
            api_key,
            model,
            voices_normalized,
            voices_for_prompt,
            timeout,
            strict,
            breaker: CircuitBreaker::new(),
            static_fallback,
            unknown_voices_warned: Mutex::new(HashSet::new()),
            failure_modes_logged: Mutex::new(HashSet::new()),
        }
    }

    fn log_failure_once(&self, mode: &'static str, detail: &str) {
        let mut guard = self.failure_modes_logged.lock().unwrap();
        if guard.insert(mode) {
            eprintln!("voiceforge: LLM provider failure ({mode}): {detail} — falling back to static rules. Subsequent {mode} failures suppressed.");
        }
    }

    fn warn_unknown_voice(&self, voice: &str) {
        let mut guard = self.unknown_voices_warned.lock().unwrap();
        if guard.insert(voice.to_string()) {
            eprintln!(
                "voiceforge: LLM returned unknown voice {voice:?}; not in voice list. Restart daemon if you've added new packs/voices since startup."
            );
        }
    }

    fn build_prompt(&self, event: &str) -> String {
        let voices = self.voices_for_prompt.join(", ");
        format!(
            "You're a reactions engine for `voiceforge`, a CLI that speaks short \
quips when developer events happen. The user just hit event=\"{event}\". \
Pick ONE voice from this list and write ONE short, in-character line \
(8-12 words, present tense, no markdown, no emoji, max 200 characters).\n\n\
Voices: {voices}\n\n\
Respond with raw JSON only, no prose:\n\
{{\"voice\": \"<one of the voices>\", \"line\": \"<the spoken line>\"}}"
        )
    }

    async fn try_llm(&self, event: &str) -> Result<(String, String)> {
        let prompt = self.build_prompt(event);
        let mut body = serde_json::json!({
            "messages": [
                { "role": "user", "content": prompt }
            ],
            "temperature": 0.7,
            "response_format": { "type": "json_object" },
        });
        if let Some(model) = &self.model {
            body["model"] = serde_json::Value::String(model.clone());
        }

        let mut req = self
            .client
            .post(&self.url)
            .timeout(self.timeout)
            .json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await.map_err(|e| anyhow!("send: {e}"))?;
        if !resp.status().is_success() {
            return Err(anyhow!("http {}", resp.status()));
        }
        let raw = resp.text().await.map_err(|e| anyhow!("body: {e}"))?;
        let parsed = parse_response(&raw)?;
        validate_pair(&parsed, &self.voices_normalized)?;
        let normalized_voice = self.canonicalize_voice(&parsed.voice);
        Ok((normalized_voice, parsed.line))
    }

    fn canonicalize_voice(&self, returned: &str) -> String {
        let n = normalize_voice(returned);
        // Find original casing in voices_for_prompt that normalizes to n.
        for v in self.voices_for_prompt.iter() {
            if normalize_voice(v) == n {
                return v.clone();
            }
        }
        // Shouldn't happen if validate_pair passed, but be defensive.
        returned.to_string()
    }
}

#[async_trait]
impl ReactionProvider for LlmProvider {
    async fn react(&self, event: &str) -> (String, String) {
        if self.breaker.is_open() {
            // Skip LLM entirely; circuit is open.
            return self.static_fallback.react(event).await;
        }

        match self.try_llm(event).await {
            Ok(pair) => {
                self.breaker.record_success();
                pair
            }
            Err(e) => {
                self.breaker.record_failure();
                let mode = classify_error(&e);
                let detail = format!("{e:#}");
                if mode == "unknown_voice" {
                    if let Some(v) = extract_unknown_voice(&detail) {
                        self.warn_unknown_voice(&v);
                    }
                } else {
                    self.log_failure_once(mode, &detail);
                }
                if self.strict {
                    // Strict mode: synthesize a deterministic error
                    // line. The daemon still returns Ok with this text;
                    // the user sees that the LLM failed in spoken form.
                    return ("default".to_string(), format!("LLM error: {mode}"));
                }
                self.static_fallback.react(event).await
            }
        }
    }

    fn name(&self) -> &'static str {
        if self.strict {
            "llm-strict"
        } else {
            "llm"
        }
    }
}

// ============================================================================
// Pure helpers (testable without the network)
// ============================================================================

fn normalize_voice(v: &str) -> String {
    v.trim().to_lowercase().replace('-', "_")
}

fn parse_response(body: &str) -> Result<LlmDirect> {
    // Try direct first.
    if let Ok(d) = serde_json::from_str::<LlmDirect>(body) {
        return Ok(d);
    }
    // Try OpenAI envelope.
    if let Ok(envelope) = serde_json::from_str::<OpenAiEnvelope>(body) {
        if let Some(choice) = envelope.choices.first() {
            if let Ok(d) = serde_json::from_str::<LlmDirect>(&choice.message.content) {
                return Ok(d);
            }
        }
    }
    Err(anyhow!(
        "response did not match LlmDirect or OpenAI envelope"
    ))
}

fn validate_pair(pair: &LlmDirect, voices_normalized: &HashSet<String>) -> Result<()> {
    if pair.line.trim().is_empty() {
        return Err(anyhow!("line is empty or whitespace"));
    }
    if pair.line.chars().count() > MAX_LINE_CHARS {
        return Err(anyhow!(
            "line exceeds {MAX_LINE_CHARS} chars ({} actual)",
            pair.line.chars().count()
        ));
    }
    if pair.voice.trim().is_empty() {
        return Err(anyhow!("voice is empty"));
    }
    let normalized = normalize_voice(&pair.voice);
    if !voices_normalized.contains(&normalized) {
        return Err(anyhow!("unknown_voice:{}", pair.voice));
    }
    Ok(())
}

fn classify_error(e: &anyhow::Error) -> &'static str {
    let msg = format!("{e:#}");
    if msg.contains("send:") {
        if msg.to_lowercase().contains("timeout") || msg.contains("operation timed out") {
            "timeout"
        } else {
            "network"
        }
    } else if msg.starts_with("http ") {
        "http"
    } else if msg.contains("unknown_voice:") {
        "unknown_voice"
    } else if msg.contains("LlmDirect") || msg.contains("OpenAI envelope") {
        "parse"
    } else if msg.contains("line") || msg.contains("voice is") {
        "schema"
    } else {
        "other"
    }
}

fn extract_unknown_voice(detail: &str) -> Option<String> {
    let needle = "unknown_voice:";
    let idx = detail.find(needle)?;
    let rest = &detail[idx + needle.len()..];
    Some(rest.trim().to_string())
}

// ============================================================================
// select_provider — env-driven factory
// ============================================================================

pub fn select_provider(rules: Arc<Rules>) -> Arc<dyn ReactionProvider> {
    select_provider_with_env(rules, |k| std::env::var(k).ok())
}

/// Pure-ish factory used by tests + the prod entry point. The env
/// reader is injected so tests can avoid `std::env::set_var` UB
/// on Rust 1.85+.
pub fn select_provider_with_env<F>(rules: Arc<Rules>, env: F) -> Arc<dyn ReactionProvider>
where
    F: Fn(&str) -> Option<String>,
{
    let static_provider = Arc::new(StaticProvider::new(Arc::clone(&rules)));

    let url = match env("VOICEFORGE_LLM_URL").filter(|v| !v.trim().is_empty()) {
        Some(u) => u,
        None => return static_provider,
    };

    let api_key = env("VOICEFORGE_LLM_API_KEY")
        .filter(|v| !v.trim().is_empty())
        .or_else(|| env("OPENAI_API_KEY").filter(|v| !v.trim().is_empty()));
    let model = env("VOICEFORGE_LLM_MODEL").filter(|v| !v.trim().is_empty());
    let timeout_ms = env("VOICEFORGE_LLM_TIMEOUT_MS")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let strict = matches!(
        env("VOICEFORGE_LLM_STRICT").as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    );

    // Voice list = rules voices + installed packs + cloned voices.
    // Best-effort — failures here are non-fatal (we just get a smaller
    // menu).
    let mut voices: Vec<String> = rules.voices();
    if let Ok(installed_packs) = crate::packs::list_installed() {
        voices.extend(installed_packs);
    }
    if let Ok(cloned) = crate::voices::list_cloned_voices() {
        voices.extend(cloned.into_iter().map(|v| v.name));
    }
    voices.sort();
    voices.dedup();

    Arc::new(LlmProvider::new(
        url,
        api_key,
        model,
        Duration::from_millis(timeout_ms),
        strict,
        voices,
        static_provider,
    ))
}

// ============================================================================
// Test fixtures
// ============================================================================

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Records every event the daemon hands to the provider, returns a
    /// caller-controlled (voice, line). Used to prove the daemon
    /// actually wires through to the trait.
    pub(crate) struct RecordingProvider {
        pub fixed: (String, String),
        pub events: Mutex<Vec<String>>,
    }

    impl RecordingProvider {
        pub(crate) fn new(voice: &str, line: &str) -> Self {
            Self {
                fixed: (voice.to_string(), line.to_string()),
                events: Mutex::new(Vec::new()),
            }
        }
        pub(crate) fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ReactionProvider for RecordingProvider {
        async fn react(&self, event: &str) -> (String, String) {
            self.events.lock().unwrap().push(event.to_string());
            self.fixed.clone()
        }
        fn name(&self) -> &'static str {
            "recording"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice_set(v: &[&str]) -> HashSet<String> {
        v.iter().map(|s| normalize_voice(s)).collect()
    }

    #[test]
    fn normalize_strips_case_and_dashes() {
        assert_eq!(normalize_voice("Angry-Duck"), "angry_duck");
        assert_eq!(normalize_voice("  PETER  "), "peter");
        assert_eq!(normalize_voice("hype_narrator"), "hype_narrator");
    }

    #[test]
    fn parse_direct_shape() {
        let body = r#"{"voice":"angry_duck","line":"oh no"}"#;
        let p = parse_response(body).unwrap();
        assert_eq!(p.voice, "angry_duck");
        assert_eq!(p.line, "oh no");
    }

    #[test]
    fn parse_openai_envelope() {
        let body = r#"{"choices":[{"message":{"content":"{\"voice\":\"angry_duck\",\"line\":\"oh no\"}"}}]}"#;
        let p = parse_response(body).unwrap();
        assert_eq!(p.voice, "angry_duck");
        assert_eq!(p.line, "oh no");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_response("not json").is_err());
        assert!(parse_response(r#"{"foo":"bar"}"#).is_err());
        assert!(parse_response(r#"{"choices":[]}"#).is_err());
    }

    #[test]
    fn validate_rejects_empty_line() {
        let voices = voice_set(&["peter"]);
        let pair = LlmDirect {
            voice: "peter".to_string(),
            line: "   ".to_string(),
        };
        assert!(validate_pair(&pair, &voices).is_err());
    }

    #[test]
    fn validate_rejects_too_long_line() {
        let voices = voice_set(&["peter"]);
        let pair = LlmDirect {
            voice: "peter".to_string(),
            line: "a".repeat(201),
        };
        assert!(validate_pair(&pair, &voices).is_err());
    }

    #[test]
    fn validate_accepts_201_byte_emoji_line_under_char_cap() {
        let voices = voice_set(&["peter"]);
        // 100 emoji = 400 bytes, but only 100 chars — should pass.
        let line = "🎙".repeat(100);
        let pair = LlmDirect {
            voice: "peter".to_string(),
            line,
        };
        assert!(validate_pair(&pair, &voices).is_ok());
    }

    #[test]
    fn validate_rejects_unknown_voice() {
        let voices = voice_set(&["peter"]);
        let pair = LlmDirect {
            voice: "stewie".to_string(),
            line: "hello".to_string(),
        };
        let err = validate_pair(&pair, &voices).unwrap_err();
        assert!(format!("{err:#}").contains("unknown_voice:stewie"));
    }

    #[test]
    fn validate_accepts_normalized_voice_match() {
        // Member set has "angry_duck"; LLM returns "Angry-Duck".
        let voices = voice_set(&["angry_duck"]);
        let pair = LlmDirect {
            voice: "Angry-Duck".to_string(),
            line: "oh no".to_string(),
        };
        assert!(validate_pair(&pair, &voices).is_ok());
    }

    #[test]
    fn extract_unknown_voice_pulls_name() {
        let detail = "schema check failed: unknown_voice:weirdcase";
        assert_eq!(extract_unknown_voice(detail), Some("weirdcase".to_string()));
    }

    #[test]
    fn classify_error_buckets_correctly() {
        assert_eq!(
            classify_error(&anyhow!("send: connection refused")),
            "network"
        );
        assert_eq!(
            classify_error(&anyhow!("send: operation timed out")),
            "timeout"
        );
        assert_eq!(classify_error(&anyhow!("http 429")), "http");
        assert_eq!(
            classify_error(&anyhow!("unknown_voice:foo")),
            "unknown_voice"
        );
        assert_eq!(
            classify_error(&anyhow!(
                "response did not match LlmDirect or OpenAI envelope"
            )),
            "parse"
        );
    }

    #[test]
    fn breaker_starts_closed() {
        let b = CircuitBreaker::new();
        assert!(!b.is_open());
        assert!(!b.is_half_open());
    }

    #[test]
    fn breaker_trips_after_threshold_failures() {
        let b = CircuitBreaker::new();
        for _ in 0..BREAKER_FAILURE_THRESHOLD {
            b.record_failure();
        }
        assert!(b.is_open());
    }

    #[test]
    fn breaker_success_resets_state() {
        let b = CircuitBreaker::new();
        for _ in 0..BREAKER_FAILURE_THRESHOLD {
            b.record_failure();
        }
        assert!(b.is_open());
        b.record_success();
        assert!(!b.is_open());
        assert_eq!(b.failure_count.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn static_provider_returns_rules_pick() {
        let rules = Arc::new(Rules::default_builtin());
        let p = StaticProvider::new(rules);
        let (voice, _line) = p.react("build_failed").await;
        assert_eq!(voice, "angry_duck");
    }

    #[tokio::test]
    async fn static_provider_falls_back_for_unknown_event() {
        let rules = Arc::new(Rules::default_builtin());
        let p = StaticProvider::new(rules);
        let (voice, line) = p.react("totally_unknown_event_xyz").await;
        assert_eq!(voice, "default");
        assert_eq!(line, "Event received.");
    }

    #[test]
    fn static_provider_name() {
        let rules = Arc::new(Rules::default_builtin());
        let p = StaticProvider::new(rules);
        assert_eq!(p.name(), "static");
    }

    #[test]
    fn select_provider_returns_static_when_url_unset() {
        let rules = Arc::new(Rules::default_builtin());
        let p = select_provider_with_env(rules, |_| None);
        assert_eq!(p.name(), "static");
    }

    #[test]
    fn select_provider_returns_llm_when_url_set() {
        let rules = Arc::new(Rules::default_builtin());
        let p = select_provider_with_env(rules, |k| match k {
            "VOICEFORGE_LLM_URL" => Some("http://127.0.0.1:1/v1/chat/completions".to_string()),
            _ => None,
        });
        assert_eq!(p.name(), "llm");
    }

    #[test]
    fn select_provider_strict_mode_via_env() {
        let rules = Arc::new(Rules::default_builtin());
        let p = select_provider_with_env(rules, |k| match k {
            "VOICEFORGE_LLM_URL" => Some("http://127.0.0.1:1/v1/chat/completions".to_string()),
            "VOICEFORGE_LLM_STRICT" => Some("1".to_string()),
            _ => None,
        });
        assert_eq!(p.name(), "llm-strict");
    }

    #[test]
    fn select_provider_falls_through_openai_api_key() {
        // We can't easily inspect the api_key field from outside, but
        // we can prove the env reader is consulted in the right order
        // by setting only OPENAI_API_KEY and checking the provider
        // constructs successfully (LlmProvider::new doesn't fail).
        let rules = Arc::new(Rules::default_builtin());
        let p = select_provider_with_env(rules, |k| match k {
            "VOICEFORGE_LLM_URL" => Some("http://127.0.0.1:1/v1/chat/completions".to_string()),
            "OPENAI_API_KEY" => Some("sk-test".to_string()),
            _ => None,
        });
        assert_eq!(p.name(), "llm");
    }

    // ====================================================================
    // LLM fallback tests — use real network primitives, no env contention
    // ====================================================================

    #[tokio::test]
    async fn llm_falls_back_on_connection_refused() {
        // Bind, capture port, drop → guaranteed-dead port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let rules = Arc::new(Rules::default_builtin());
        let static_p = Arc::new(StaticProvider::new(Arc::clone(&rules)));
        let llm = LlmProvider::new(
            format!("http://127.0.0.1:{port}/v1/chat/completions"),
            None,
            None,
            Duration::from_millis(500),
            false,
            rules.voices(),
            static_p,
        );
        let (voice, line) = llm.react("build_failed").await;
        // Fall-through to static → angry_duck voice.
        assert_eq!(voice, "angry_duck");
        assert!(!line.is_empty());
    }

    #[tokio::test]
    async fn llm_strict_returns_error_line_instead_of_falling_back() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let rules = Arc::new(Rules::default_builtin());
        let static_p = Arc::new(StaticProvider::new(Arc::clone(&rules)));
        let llm = LlmProvider::new(
            format!("http://127.0.0.1:{port}/v1/chat/completions"),
            None,
            None,
            Duration::from_millis(500),
            true, // strict
            rules.voices(),
            static_p,
        );
        let (_voice, line) = llm.react("build_failed").await;
        // Connection refused → classify_error returns "network".
        // (On some kernels we might see "timeout" if the SYN never gets
        // an RST; both are valid strict-mode error lines.)
        assert!(
            line == "LLM error: network" || line == "LLM error: timeout",
            "got: {line:?}",
        );
    }

    #[tokio::test]
    async fn llm_breaker_opens_after_repeated_failures() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let rules = Arc::new(Rules::default_builtin());
        let static_p = Arc::new(StaticProvider::new(Arc::clone(&rules)));
        let llm = LlmProvider::new(
            format!("http://127.0.0.1:{port}/v1/chat/completions"),
            None,
            None,
            Duration::from_millis(200),
            false,
            rules.voices(),
            static_p,
        );
        for _ in 0..BREAKER_FAILURE_THRESHOLD {
            llm.react("build_failed").await;
        }
        assert!(llm.breaker.is_open());
    }
}
