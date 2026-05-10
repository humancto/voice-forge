---
roadmap_item: 4.1 ReactionProvider trait (Static + Llm)
status: plan v2 — rust-expert REVISE folded
---

# 4.1 ReactionProvider trait

## Goal (from ROADMAP)

> `ReactionProvider` trait with `Static` (rules.json) + `Llm` (OpenAI-compatible
> endpoint via `VOICEFORGE_LLM_URL`). Static is default, LLM falls back to
> static on failure.

Today the daemon's `process_frame` calls `rules::choose_reaction(...)` to pick
a `(voice, line)` from `rules.json`. That's the "Static" path. 4.1 abstracts
that behind a trait so an LLM-backed provider can be swapped in. Static
remains default. LLM is opt-in via `VOICEFORGE_LLM_URL` and **always** falls
back to Static on any error.

## Plan v2 deltas (rust-expert REVISE fold)

| #   | Item                                                                   | Resolution                                                                                                                                                                                                                                                                            |
| --- | ---------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| B1  | `line.len() <= 240` is bytes, not chars                                | Use `line.chars().count() <= 200`. Also reject `is_empty()` / `trim().is_empty()`.                                                                                                                                                                                                    |
| B2  | Voice membership case-sensitive → silent fallback on cosmetic mismatch | Normalize voice names (lowercase + strip whitespace + replace `-` with `_`) into a `HashSet<String>`; compare normalized. Log unknown voice with the exact returned name.                                                                                                             |
| B3  | OpenAI envelope vs bare `{voice,line}` ambiguous                       | Deterministic try-order: (1) try `{voice, line}` direct, (2) on `serde_json::Error` try OpenAI envelope `{choices:[{message:{content:"<json>"}}]}` and recursively parse `content`. Document. Test both.                                                                              |
| B4  | mockito doesn't actually exercise network errors                       | Connection-refusal: bind a `TcpListener`, capture port, drop listener, point reqwest at the dead port. Timeout: mockito `with_chunked_body` + sleep past the configured timeout.                                                                                                      |
| R1  | AFIT vs async-trait reasoning was wrong                                | Actual reason: RPITIT (`fn react -> impl Future + Send`) **is not dyn-compatible** on stable. We need `Arc<dyn ReactionProvider>`, so `async-trait` (which generates `Pin<Box<dyn Future + Send>>`) is the correct call. Pin to `async-trait = "0.1.89"`.                             |
| R2  | `AtomicU8` consecutive-failure breaker is too crude                    | Time-windowed: 5 failures within 60s → trip; trip lasts 5 minutes; then half-open (one probe). Two atomics: `AtomicU32` failure count (rolling-window approximation) + `AtomicU64` reset-instant nanoseconds since `Instant::now()`. Define "session" = until next half-open success. |
| R3  | 5s timeout too long for interactive speech                             | Default `VOICEFORGE_LLM_TIMEOUT_MS=2000`. Set on per-request `RequestBuilder::timeout`, not `Client::timeout` (client is shared). Doctor reports the value.                                                                                                                           |
| R5  | API key env var                                                        | Order: `VOICEFORGE_LLM_API_KEY` → `OPENAI_API_KEY` → none. No `ANTHROPIC_API_KEY` etc. — let users set the explicit voiceforge var for non-OpenAI keys.                                                                                                                               |
| R8  | Env-var tests will collide under parallel execution                    | All tests touching `VOICEFORGE_LLM_*` use `#[serial_test::serial]`. Cached at construction; tests must `set_var` BEFORE constructing the provider.                                                                                                                                    |
| M1  | Stderr noise budget untested                                           | Test asserts ≤1 stderr log per failure mode after 100 consecutive failures. Use a captured-stderr harness or just assert via the `tracing` machinery if we adopt it (we don't — `eprintln` matches the codebase).                                                                     |
| M3  | OpenAI envelope success path untested                                  | Mockito test with body = `{"choices":[{"message":{"content":"{\"voice\":\"angry_duck\",\"line\":\"...\"}"}}]}`.                                                                                                                                                                       |
| M4  | HTTP 429 / 5xx untested                                                | Mockito returns 429 with body — assert fallback fires, no attempt to deserialize the error body as success schema.                                                                                                                                                                    |
| M5  | Doctor probe spec underdefined                                         | TCP+TLS handshake with 1s timeout via `tokio::net::TcpStream::connect` to URL host:port. Reports "URL set, host reachable" / "URL set, unreachable" / "URL unset". Does NOT make a chat completion call.                                                                              |
| M6  | Cancel-safety undocumented                                             | One-line doc on the trait method: cancel-safe; drop-and-retry has no observable side effects.                                                                                                                                                                                         |
| M7  | Daemon integration test must prove provider output is used             | Inject `RecordingProvider` returning `("test_voice", "test_line")`; assert daemon spoke that exact pair, not whatever rules.json would have picked.                                                                                                                                   |
| M8  | `--strict-llm` mode                                                    | `VOICEFORGE_LLM_STRICT=1` env var. Default off (silent fallback). When on, daemon returns `Reply::err` on LLM failure instead of falling back. Doctor reports active mode.                                                                                                            |
| Nit | `Vec<String>` for voices                                               | `HashSet<String>` of normalized names for membership; `Arc<[String]>` of original names for the prompt (cheap clone).                                                                                                                                                                 |
| Nit | "8-12 words" prompt vs 240 char cap mismatch                           | Tighten cap to 200 chars; keep word guideline as "8-12 words, no longer than 200 characters" in prompt.                                                                                                                                                                               |
| Nit | `OnceLock<AtomicBool>` for the failure-logged flag                     | Overengineered — module-scope `static FAILURE_LOGGED: AtomicBool = AtomicBool::new(false);`.                                                                                                                                                                                          |
| Nit | `provider.name()` for doctor                                           | Add `fn name(&self) -> &'static str` on trait → `"static"` / `"llm"`. Two lines, big debugging win.                                                                                                                                                                                   |

## Surface (final)

- **NEW** `apps/voiceforge-cli/src/reaction.rs` (~330 lines + tests):

  ```rust
  use async_trait::async_trait;

  /// Picks (voice, line) for an event.
  ///
  /// Cancel-safe: drop-and-retry is safe; no observable side effects from
  /// a dropped call (reqwest cancels the in-flight request, return type
  /// has no half-completed state).
  #[async_trait]
  pub trait ReactionProvider: Send + Sync {
      async fn react(&self, event: &str) -> (String, String);
      /// Stable identifier for `voiceforge doctor`.
      fn name(&self) -> &'static str;
  }

  /// Wraps `rules::Rules`. Today's behavior, extracted into a trait.
  pub struct StaticProvider {
      rules: Arc<Rules>,
      fallback: (String, String),
  }

  /// LLM-backed: POSTs to an OpenAI-compatible chat endpoint, parses
  /// the response with deterministic try-order, and on ANY failure
  /// silently falls back to the wrapped Static provider (or returns
  /// an error in strict mode).
  pub struct LlmProvider {
      client: reqwest::Client,
      url: String,
      api_key: Option<String>,
      model: Option<String>,
      voices_normalized: HashSet<String>,    // membership lookup
      voices_for_prompt: Arc<[String]>,      // original names for prompt
      timeout: Duration,
      strict: bool,
      breaker: CircuitBreaker,
      static_fallback: Arc<StaticProvider>,
  }

  /// Time-windowed: 5 failures within 60s trips for 5 min, then
  /// half-open. Two atomics, lock-free. Race-tolerant by design
  /// (worst case: a few wasted LLM calls before the breaker trips —
  /// acceptable, documented).
  pub(crate) struct CircuitBreaker {
      failure_count: AtomicU32,
      window_start_nanos: AtomicU64,         // since process Instant epoch
      reset_at_nanos: AtomicU64,             // 0 = closed; nonzero = trip-until
  }

  /// Reads env, returns the right provider:
  ///   VOICEFORGE_LLM_URL set       → LlmProvider wrapping StaticProvider
  ///   VOICEFORGE_LLM_URL unset     → StaticProvider directly
  ///   VOICEFORGE_LLM_API_KEY       → optional Authorization
  ///   OPENAI_API_KEY               → fallback for above
  ///   VOICEFORGE_LLM_MODEL         → optional model name
  ///   VOICEFORGE_LLM_TIMEOUT_MS    → default 2000
  ///   VOICEFORGE_LLM_STRICT        → 1/true/yes/on → strict mode
  pub fn select_provider(rules: Arc<Rules>) -> Arc<dyn ReactionProvider>;
  ```

- **MODIFIED** `apps/voiceforge-cli/src/daemon_server.rs`:
  - `serve(... , provider: Arc<dyn ReactionProvider>)` instead of `rules`.
  - `process_frame` uses `provider.react(event).await` for the event branch.
  - Text-only frames bypass the provider (text is verbatim).
  - `handle_connection` + `spawn_serve` updated similarly.
  - `spawn_serve_with_provider` test fixture (existing `spawn_serve` keeps backward compat via `select_provider(rules)`).

- **MODIFIED** `apps/voiceforge-cli/src/daemon.rs`:
  - `let provider = reaction::select_provider(rules);` → `serve(... , provider, ...)`.

- **MODIFIED** `apps/voiceforge-cli/src/main.rs`: `mod reaction;`.

- **MODIFIED** `apps/voiceforge-cli/src/doctor.rs`: row reports
  `provider.name()` + `VOICEFORGE_LLM_URL` reachability + timeout +
  strict mode.

- **MODIFIED** `Cargo.toml`: `async-trait = "0.1.89"`.

- **MODIFIED** `README.md`: capability paragraph + LLM env var table.

## Design choices (folded)

### Why `async-trait` and not AFIT?

rustc 1.91 has dyn-compatible AFIT, but only for **uses that don't need
explicit `Send` bounds**. The trait we need looks like:

```rust
fn react(&self, event: &str) -> impl Future<Output = (String, String)> + Send + '_;
```

That RPITIT signature is **not dyn-compatible** on stable. Since
`select_provider` returns `Arc<dyn ReactionProvider>` for runtime
selection, we need the `Pin<Box<dyn Future + Send>>` adapter that
`async-trait` generates. One Box per call is rounding error against the
500-3000 ms LLM round-trip.

### Fallback semantics: silent by default, strict by env

`VOICEFORGE_LLM_STRICT` unset (default): any LLM failure → log once + fall
through to `StaticProvider`. The user gets a line; the daemon never goes
silent. The first failure logs to stderr; subsequent failures of the same
mode are counted, not logged.

`VOICEFORGE_LLM_STRICT=1`: any LLM failure → return the error. The daemon
still replies (with `Reply::err`), but no spoken line is produced. Useful
for CI and debugging "is the LLM actually being called?"

### Time-windowed circuit breaker

```
state CLOSED:
    on success: reset failure_count to 0
    on failure:
        failure_count += 1
        if failure_count >= 5 within 60s window:
            transition to OPEN, set reset_at = now + 5min

state OPEN:
    on any call: skip LLM, return fallback immediately (silent mode)
                 OR Reply::err (strict mode)
    when now >= reset_at: transition to HALF_OPEN

state HALF_OPEN:
    on first call: try LLM
        if success → CLOSED, reset counters
        if failure → OPEN, reset_at = now + 5min
```

Implementation: two atomics, no mutex. The window is a "rolling 60s"
approximation — when `failure_count` increments, we check whether
`now - window_start >= 60s`; if so, reset both. This isn't a true
sliding window but is racy-safe and right within ~10s on average.

"Session" is clearly defined: from one CLOSED state to the next CLOSED
state. After 5 minutes OPEN + one successful HALF_OPEN probe, we're back
to a fresh session. The daemon doesn't need to restart.

### Latency budget

Default 2000 ms via `VOICEFORGE_LLM_TIMEOUT_MS`. Set on
`RequestBuilder::timeout` per request, not `Client::timeout` (the client
is shared and we don't want a stale timeout sticking). At 2s the user
will perceive the speech as "slightly delayed" but not "broken." Local
Ollama users can bump to 5-10s.

### Voice list

Read once at provider construction:

1. Voice names from `rules.json` (canonical defaults).
2. Installed pack names from `~/.voiceforge/packs/<pack>/manifest.toml`.
3. Cloned voice names from `~/.voiceforge/voices/`.

Two collections:

- `voices_normalized: HashSet<String>` for O(1) case-insensitive
  membership lookup. Normalize: `trim()` → `to_lowercase()` →
  `replace('-', "_")`.
- `voices_for_prompt: Arc<[String]>` for the prompt body (preserves
  original casing for the LLM to copy).

If LLM returns a voice not in `voices_normalized`, log
`voiceforge: LLM returned unknown voice {returned!r}; falling back`
(once per unique voice via a `Mutex<HashSet<String>>` of
already-warned names) and fall back. The full original name in the log
lets the user diagnose pack-installed-after-startup cases.

### Schema validation

```rust
#[derive(Deserialize)]
struct LlmDirect { voice: String, line: String }

#[derive(Deserialize)]
struct OpenAiEnvelope {
    choices: Vec<OpenAiChoice>,
}
#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}
#[derive(Deserialize)]
struct OpenAiMessage {
    content: String,  // expected to itself be JSON matching LlmDirect
}
```

Try-order:

1. `serde_json::from_str::<LlmDirect>(body)`.
2. On `Err`, `serde_json::from_str::<OpenAiEnvelope>(body)` →
   `envelope.choices.first()?.message.content` →
   `serde_json::from_str::<LlmDirect>(content)`.
3. On `Err`, fallback.

After parse, validate:

- `!voice.trim().is_empty()`
- `!line.trim().is_empty()`
- `line.chars().count() <= 200`
- `voice` in `voices_normalized` (after normalize)

Any failure → fallback.

### System prompt

```
You're a reactions engine for `voiceforge`, a CLI that speaks short
quips when developer events happen. The user just hit event="<event>".
Pick ONE voice from this list and write ONE short, in-character line
(8-12 words, present tense, no markdown, no emoji, max 200 characters).

Voices: <comma-separated voices_for_prompt>

Respond with raw JSON only, no prose:
{"voice": "<one of the voices>", "line": "<the spoken line>"}
```

When the endpoint supports it, set `response_format: {"type":
"json_object"}`. Pass `temperature: 0.7` (some characterful variance
without going off the rails).

### Why baseline `OPENAI_API_KEY` as fallback?

Common dev convention. Users with `OPENAI_API_KEY` already exported
shouldn't have to also set `VOICEFORGE_LLM_API_KEY`. Order is:
explicit voiceforge var > convention. Document explicitly so users
understand the precedence.

### Concurrent requests + breaker race

Best-effort breaker: if 5 events fire simultaneously while the LLM is
down, all 5 race past the trip threshold before any of them increments
to the trip count. Worst case: 5 wasted LLM calls instead of 1 before
the breaker actually opens. Acceptable. Don't put a mutex around the
breaker — the cost would dwarf the savings. Documented in code comment.

## Test plan (final)

**Pure unit tests (~14):**

- [ ] `static_provider_returns_rules_pick`
- [ ] `static_provider_falls_back_when_event_missing`
- [ ] `static_provider_name_is_static`
- [ ] `llm_provider_falls_back_on_connection_refused` (bind `TcpListener` → drop → reqwest at dead port)
- [ ] `llm_provider_falls_back_on_timeout` (mockito with `with_chunked_body` delay > timeout)
- [ ] `llm_provider_falls_back_on_unparseable_response` (mockito returns "not json")
- [ ] `llm_provider_falls_back_on_unknown_voice` (LLM returns voice not in list, normalized)
- [ ] `llm_provider_falls_back_on_empty_line` (LLM returns `""` for line)
- [ ] `llm_provider_falls_back_on_too_long_line` (LLM returns 250-char line)
- [ ] `llm_provider_falls_back_on_http_429`
- [ ] `llm_provider_falls_back_on_http_500`
- [ ] `llm_provider_returns_llm_pick_on_direct_success` (`{voice, line}` body)
- [ ] `llm_provider_returns_llm_pick_on_openai_envelope_success` (`{choices:[...]}` body)
- [ ] `llm_provider_handles_voice_name_case_normalization` (LLM returns `"Angry_Duck"`, member set has `"angry_duck"`)
- [ ] `llm_provider_includes_authorization_when_voiceforge_key_set`
- [ ] `llm_provider_includes_authorization_when_openai_key_set_and_voiceforge_unset`
- [ ] `llm_provider_omits_authorization_when_no_keys_set`
- [ ] `llm_provider_strict_mode_returns_error_instead_of_fallback`
- [ ] `select_provider_returns_static_when_url_unset`
- [ ] `select_provider_returns_llm_when_url_set`

**Circuit breaker tests:**

- [ ] `breaker_trips_after_5_failures_within_window`
- [ ] `breaker_does_not_trip_with_failures_outside_window` (mock the clock or use slow timer)
- [ ] `breaker_open_skips_llm_call_returns_fallback`
- [ ] `breaker_half_open_probes_after_5_minutes` (use `tokio::time::pause` + advance)
- [ ] `breaker_half_open_success_resets_to_closed`

**Stderr noise budget:**

- [ ] `failure_logs_at_most_once_per_failure_mode_per_session` — drive 100 consecutive `connection_refused`, count stderr lines

**Daemon integration tests:**

- [ ] `serve_uses_provider_for_event_branch` — inject `RecordingProvider` returning `("test_voice", "test_line")`, assert daemon spoke that exact pair (not what rules.json would have picked)
- [ ] `serve_text_frames_bypass_provider` — text-only frame should NOT call `react()`

**All env-var tests use `#[serial_test::serial]`.**

**Gates:**

- [ ] `cargo test --all` green on macOS + Linux
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] `cargo fmt --check` clean

## Edge cases

1. **Endpoint that's not OpenAI-compatible** — Anthropic etc. Document
   that we only handle the chat-completions envelope. Anthropic users
   point at a proxy or wait for a future Anthropic provider.
2. **Real OpenAI** at `https://api.openai.com/v1/chat/completions` — set
   `VOICEFORGE_LLM_URL` to that, set `OPENAI_API_KEY`. Works.
3. **Local Ollama** at `http://localhost:11434/v1/chat/completions` —
   set URL, no API key needed, set timeout to 5000ms.
4. **Concurrent requests** — each `process_frame` is its own tokio task;
   `reqwest::Client` is `Arc`-safe; no shared mutable state in the
   provider except `breaker` (atomics) + `unknown_voice_warned`
   (`Mutex<HashSet<String>>`).
5. **Pack installed after daemon start** — voice list is stale.
   Fallback fires + log includes the unknown voice name. User sees the
   problem and restarts the daemon.
6. **Endpoint dies mid-session** — breaker trips after 5 failures in
   60s, daemon stays responsive via fallback for 5 minutes, half-open
   probe re-enables when the endpoint recovers.

## Out of scope

- Streaming LLM responses (4.2 covers streaming TTS).
- Multi-voice cast (4.3).
- Caching across restarts.
- Custom system-prompt overrides (good for v2).
- Cost accounting.
- Anthropic-shaped envelope (`content[0].text` etc.).
- Retry on 429 / 5xx (we fall back instead — simpler, predictable).

## Rollback

Unset `VOICEFORGE_LLM_URL` → Static path, behavior unchanged from today.
Or revert the PR — the trait wrapper falls away cleanly.
