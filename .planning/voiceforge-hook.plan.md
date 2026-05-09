# Plan v2: voiceforge hook (ROADMAP 3.3)

> Plan v1 -> rust-expert REVISE with 6 items + 3 nits. v2 folds them in.

## Goal

Read JSON Lines from stdin, forward each to the daemon. Lets Claude
Code, Cursor, Codex, etc. pipe their hook events into voiceforge with
zero shell-script glue.

```
$ claude-code hooks set notification 'voiceforge hook --profile claude-code'
$ echo '{"event":"command_failed","message":"npm test died"}' | voiceforge hook
$ tail -f events.ndjson | voiceforge hook --event-from notification_type --passthrough | jq
```

Distinct from `voiceforge ingest` (audio) and `voiceforge send`
(single-frame). `hook` is the streaming/multi-frame surface.

## CLI shape

```
voiceforge hook \
  [--event-from <jsonpath>] \
  [--message-from <jsonpath>] \
  [--voice <name>] \
  [--profile <known-schema>] \
  [--passthrough] \
  [--quiet]
```

Flags:

- `--event-from <jsonpath>`: pull the event name from a dotted JSON
  path. Default: read input as already-shaped frame.
- `--message-from <jsonpath>`: pull free-form context. Default: the
  whole input line, truncated to 4 KiB.
- `--voice <name>`: override per-frame voice for the whole stream.
- `--profile <known-schema>`: alias for a known upstream's field
  layout. Initial set: `claude-code` (maps `hook_event_name` -> daemon
  `event`, `message` -> daemon `message`). Future: `cursor`, `codex`.
  Mutually exclusive with `--event-from`/`--message-from`.
- `--passthrough`: write each input line to stdout BEFORE forwarding
  (so a downstream `jq` doesn't block on a hung daemon). Malformed
  lines are NOT passed through (drop + warn unless `--quiet`).
- `--quiet`: suppress per-frame stderr warnings.

Env tunables (item 7):
- `VOICEFORGE_HOOK_FAIL_RATIO` (default 0.5)
- `VOICEFORGE_HOOK_FAIL_WINDOW` (default 100)
- `VOICEFORGE_HOOK_FAIL_MIN` (default 10)

## Exit codes

- `0` stream consumed cleanly (EOF reached)
- `1` failure threshold tripped post-bootstrap (>=`MIN` attempts AND
  failure ratio >=`RATIO` over last `WINDOW` attempts)
- `2` daemon NotReachable on the FIRST frame -- exit immediately
  (item 2). Don't wait 10s for the threshold to trip.
- `3` bad CLI args (clap)

## Wire mapping

For each non-blank input line:

1. Parse as JSON. Parse failure -> stderr warn (unless `--quiet`),
   drop frame, continue.
2. If `--passthrough`, write line + `\n` to stdout NOW (item 3).
3. Build `daemon_client::SendRequest`:
   - `event` = `--event-from` value, OR profile mapping, OR input's
     `event` field, OR `None`.
   - `text` = input's `text` field if present.
   - `voice` = `--voice` flag wins, else input's `voice` field.
   - `message` = `--message-from` value, OR profile mapping, OR
     truncated raw line (4 KiB cap applied BEFORE serializing -
     item 8).
4. Call `daemon_client::send`. Track outcome in sliding window.
5. First-frame `NotReachable` -> exit 2 immediately. Subsequent
   failures count toward the threshold.

## Files

### New

- `apps/voiceforge-cli/src/hook.rs` (~250 lines + tests):
  - `pub struct HookConfig { event_from, message_from, voice, profile, passthrough, quiet, fail_ratio, fail_window, fail_min }`
  - `pub async fn run(cfg, socket_path) -> i32` -- production
    wrapper. Builds `BufReader::new(tokio::io::stdin())` (item 1) and
    a `tokio::io::stdout()`, calls `run_with_io`.
  - `pub(crate) async fn run_with_io<R, W>(cfg, socket_path, reader, stdout) -> i32`
    where `R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin` (item 1).
    Test seam.
  - `extract_field(value: &serde_json::Value, path: &str) -> Option<String>`:
    pure function. Spec'd precisely (item 4):
    - Dotted path, traverses Value::Object only.
    - Array indexing or numeric-keyed paths -> `None` (explicit
      non-feature).
    - `Value::Null` -> `None`.
    - `Value::Number` / `Value::Bool` -> `to_string()`.
    - `Value::String` -> the string.
    - Missing intermediate -> `None`.
  - `apply_profile(profile: &str, value: &Value) -> (Option<String>, Option<String>)`:
    profile-aware (event, message) extractor. Initial supported:
    `claude-code` (event = `hook_event_name`, message = `message`
    OR raw line if missing).

### Modified

- `apps/voiceforge-cli/src/main.rs` -- `Commands::Hook { ... }` +
  `run_hook` dispatcher (env-knob reads, calls `hook::run`).

## Failure threshold

Sliding window of last `WINDOW` attempts (deque). After each frame:
- If first-frame and `NotReachable` -> exit 2.
- Else push outcome (success/failure) to window. Trim to `WINDOW`.
- If window length >= `MIN` and failure-ratio >= `RATIO`, exit 1
  with the most recent error in stderr.

## Tests

`apps/voiceforge-cli/src/hook.rs` `#[cfg(test)] mod tests`:

1. `run_forwards_single_event_frame` -- feed `{"event":"build_failed"}\n`
   into `run_with_io` against test_support::fixture daemon; assert
   sink recorded one play.
2. `run_forwards_text_frame` -- text-only frame; assert reply
   spoken == text.
3. `run_handles_multi_line_stream` -- 5 frames, assert 5 sink records.
4. `run_with_event_from_extracts_nested_field` -- input
   `{"hook":{"event_name":"command_failed"}}\n` + `--event-from
   hook.event_name`; assert daemon sees event=command_failed.
5. `run_with_voice_override_wins` -- `--voice peter` + input voice=foo;
   assert daemon voice=peter.
6. `run_passthrough_writes_input_to_stdout_BEFORE_forwarding` --
   capture stdout; assert line written before send completes (test
   uses a slow stub daemon; assert stdout has the line within 50ms
   of input even if reply takes 500ms).
7. `run_drops_malformed_frame_warns_continues` -- 3 lines, middle
   one bad JSON; 2 sink records, exit 0.
8. `run_quiet_suppresses_warnings` -- same as 7 with `--quiet`;
   assert empty stderr.
9. `run_returns_exit_2_on_first_frame_not_reachable` -- no daemon,
   single frame, tight 200ms connect timeout; assert exit 2.
10. `run_returns_exit_1_when_failure_threshold_tripped` -- frame 1
    succeeds (real daemon up), then daemon goes away, send 12 more;
    assert exit 1 with "daemon unhealthy" in stderr.
11. `run_with_profile_claude_code_maps_hook_event_name` -- input
    `{"hook_event_name":"Notification","message":"x"}` +
    `--profile claude-code`; assert daemon sees event=Notification,
    message=x.
12. `extract_field_handles_dotted_path` -- pure unit.
13. `extract_field_returns_none_on_array_or_missing_or_null` -- pure unit.
14. `extract_field_stringifies_numbers_and_bools` -- pure unit.
15. `message_truncated_to_4kib_before_serialize` -- 8 KiB raw line;
    daemon receives a 4 KiB message.

## Out of scope (explicit non-features per item 5)

- Multi-frame-per-connection `SessionClient` -- per-frame Unix-socket
  connect is ~50us locally, no measurable win. Defer to 3.3.x if a
  real workload demands it.
- HTTP/WebSocket source (3.3.1).
- Pre-filter rules (regex match against event names) (3.3.2).
- Burst-rate limiter (3.3.3 -- daemon already has the semaphore cap).
- Array indexing in `extract_field` -- explicit non-feature.

## Manual smoke (post-merge)

1. `voiceforge daemon &`
2. `printf '%s\n' '{"event":"build_failed"}' '{"event":"build_success"}' | voiceforge hook`
   -> daemon speaks both.
3. `voiceforge hook --event-from hook.event_name <<< '{"hook":{"event_name":"command_failed"}}'`
   -> speaks command_failed.
4. `voiceforge hook --profile claude-code <<< '{"hook_event_name":"Notification","message":"hi"}'`
   -> speaks Notification line.
5. Daemon down + first frame -> exit 2 within ~1s.

## Atomic commits

1. `feat(hook): voiceforge hook -- stdin NDJSON forwarder + claude-code profile`

## What plan v1 got wrong (audit log)

1. `tokio::io::stdin()` is not `AsyncBufRead`. v2: wrap in
   `BufReader::new` for prod, `BufReader::new(&[u8])` for tests;
   bound is `R: AsyncBufRead + Unpin`.
2. Exit-2 collided with the threshold path -- a down daemon would
   wait 10x1s before exiting. v2: first-frame NotReachable -> exit 2
   immediately; threshold governs only post-bootstrap degradation.
3. Passthrough fired AFTER send; v2: BEFORE, so a hung daemon
   doesn't gate downstream consumers.
4. `extract_field` semantics undefined; v2: precise spec (objects
   only, no arrays, null->None, numbers/bools to_string).
5. Considered a multi-frame `SessionClient` -- v2: explicit
   "out of scope, per-frame is fine."
6. Missed the `--profile claude-code` ergonomics; v2: ships in
   this PR.
7. Threshold not env-tunable; v2: three env knobs.
8. 4 KiB cap was vague about WHERE; v2: applied client-side BEFORE
   serializing.
