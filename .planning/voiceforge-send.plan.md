# Plan v2: voiceforge send (ROADMAP 1.9)

> Plan v1 → rust-expert REVISE with 7 items. v2 folds them in.

## Goal

Connect to `~/.voiceforge/voiceforge.sock`, write one NDJSON frame,
print the daemon's reply, exit. Companion to 1.8.

```
$ voiceforge send build_failed
spoken: "That did not go well." (voice: angry_duck)

$ voiceforge send --text "deploy is live" --voice peter --json
{"ok":true,"spoken":"deploy is live","voice":"peter"}

$ voiceforge send build_failed
voiceforge send: daemon not reachable at /Users/me/.voiceforge/voiceforge.sock
hint: start it with `voiceforge daemon &`
exit 2

$ voiceforge daemon & voiceforge send build_failed
# 25ms-backoff retry-loop tides over the bind race; works on first try.
spoken: "..." (voice: ...)
```

## CLI shape

```
voiceforge send [<event>] [--text <str>] [--voice <name>] [--message <str>] [--json]
```

- Positional `event` is optional. At least one of `event` or `--text`
  must be present (mirrors the daemon's frame contract).
- `--json` prints the raw reply line on stdout. Default prints a short
  human summary on `ok:true`.

## Output streams

- **`ok:true` reply** → stdout. Either `--json` raw line or human
  `spoken: "..." (voice: ...)`.
- **`ok:false` reply** → stderr (`voiceforge send: <error>`).
- **Connect / I/O / malformed-reply errors** → stderr.

Keeps stdout clean for `voiceforge send foo | jq` style pipelines.

## Exit codes

| code | meaning                                                            |
| ---- | ------------------------------------------------------------------ |
| 0    | daemon replied `ok:true`                                           |
| 1    | daemon replied `ok:false` (frame rejected by daemon)               |
| 2    | daemon not reachable (no socket, connect refused past deadline)    |
| 3    | bad CLI args (clap default — neither event nor text supplied)      |
| 4    | post-connect failure (read timeout, partial line at EOF, non-JSON, |
|      | missing `ok` field — daemon is there but broken/garbage)           |

## Connect-retry loop

`voiceforge daemon &; voiceforge send foo` is the dominant pattern.
ECONNREFUSED is the "bind hasn't completed yet" signal when the file
exists but the listener isn't accepting. Loop:

```
deadline = now + connect_timeout
loop:
    match UnixStream::connect(path).await:
        Ok(s)                                    → return s
        Err(NotFound | ConnectionRefused) if now < deadline:
            sleep(25ms); continue
        Err(_)                                   → exit 2
    if now >= deadline                           → exit 2
```

`connect_timeout` = `VOICEFORGE_SEND_TIMEOUT_MS` env override, default
1000 ms, clamped `[50, 30_000]`. Only ECONNREFUSED/NotFound retry —
permission-denied / I/O errors fail immediately.

## Read deadline

`tokio::time::timeout(read_deadline, BufReader::read_line(...))`.
`read_deadline` = `VOICEFORGE_SEND_READ_TIMEOUT_MS` env override,
default 5000 ms (synthesis can take a beat), clamped `[100, 60_000]`.
Timeout → exit 4.

64 KiB cap on `read_line`. Sanity bound on reply size; daemon's
contract caps replies well under this. Bounded reads are good hygiene
regardless of trust.

## Files

### Modified

- `apps/voiceforge-cli/src/main.rs` — `Commands::Send {event, text,
voice, message, json}` variant + dispatch. The dispatch maps the
  result+outcome combinations to the 5 exit codes above.
- `apps/voiceforge-cli/src/daemon_server.rs` — extract a
  `#[cfg(test)] pub(crate) mod test_support` containing `RecordingSink`,
  `fixture()`, `spawn_serve()`. (The lib-crate split that lets
  integration tests under `tests/` import non-cfg-gated symbols is
  noted as a future move when cross-target tests grow; for now the
  `#[cfg(test)] pub(crate)` extraction is cheaper and equally
  effective.)

### New

- `apps/voiceforge-cli/src/daemon_client.rs` (~120 lines) — the wire
  format client. Uses `daemon_server::DaemonConfig::default_path` for
  the default socket. Exposes:

  ```rust
  pub struct SendRequest {
      pub event: Option<String>,
      pub text: Option<String>,
      pub voice: Option<String>,
      pub message: Option<String>,
  }

  /// Daemon's protocol-level reply. Distinct from Result::Err so the
  /// CLI can map ok:true → exit 0 and ok:false → exit 1 cleanly.
  pub enum SendOutcome {
      Ok { spoken: String, voice: String },
      Rejected { error: String },
  }

  /// Returns Ok(SendOutcome) when the daemon was reachable AND replied
  /// with parseable JSON. Returns Err(...) for transport failures
  /// (timeout, connection refused past deadline, non-JSON reply, EOF
  /// before reply). Caller maps Err variants to exit 2 vs exit 4.
  pub async fn send(socket_path: &Path, req: &SendRequest) -> Result<SendOutcome, SendError>;

  pub enum SendError {
      NotReachable(String),   // → exit 2
      Protocol(String),       // → exit 4 (post-connect garbage)
  }
  ```

  Implementation: connect-retry loop, write request + `\n`, read one
  line under deadline, parse JSON, dispatch on `ok` field.

  `#[serde(skip_serializing_if = "Option::is_none")]` on `SendRequest`
  fields so the wire frame stays minimal.

## Tests

`apps/voiceforge-cli/src/daemon_client.rs` `#[cfg(test)] mod tests`,
using `daemon_server::test_support::{fixture, spawn_serve}`:

1. `send_ok_event_round_trips` — start daemon, send `event:build_failed`,
   assert `Ok(Ok { spoken, voice })` with `spoken` in the rules table.
2. `send_text_with_voice_round_trips` — send `text:hi voice:peter`,
   assert `Ok { spoken: "hi", voice: "peter" }`.
3. `send_returns_rejected_on_empty_frame` — send neither event nor
   text, assert `Ok(Rejected { error })`.
4. `send_returns_not_reachable_when_no_daemon` — call `send` against
   a tempdir path with no daemon, assert `Err(NotReachable(_))`. Set
   a tight `VOICEFORGE_SEND_TIMEOUT_MS=200` so the test runs quickly.
5. `send_retries_during_bind_race` — spawn a task that creates the
   `UnixListener` after a 100 ms delay, then call `send` with the
   default 1 s timeout; assert success.
6. `send_returns_protocol_error_on_non_json_reply` — stub UnixListener
   that accepts then writes `not json\n`; assert `Err(Protocol(_))`.
7. `send_returns_protocol_error_on_eof_before_reply` — stub listener
   accepts then closes; assert `Err(Protocol(_))`.
8. `send_returns_protocol_error_on_missing_ok_field` — stub writes
   `{"spoken":"x"}\n`; assert `Err(Protocol(_))`.
9. `send_returns_protocol_error_on_read_timeout` — stub accepts then
   sleeps forever; with `VOICEFORGE_SEND_READ_TIMEOUT_MS=200`, assert
   `Err(Protocol(_))` within the deadline.

Tests 6-9 use a hand-rolled `UnixListener` stub (~20 LoC helper),
not the full daemon — they need raw control over the reply.

## Verify

1. `cargo test daemon_client::` — 9 pass.
2. `cargo test` — full sweep, no regressions.
3. `cargo clippy --all-targets -- -D warnings` — clean.
4. `cargo fmt --check` — clean.
5. **Manual**:
   ```sh
   voiceforge daemon & voiceforge send build_failed     # one-liner race
   voiceforge send --text "test" --voice default --json
   echo $?                                               # 0
   kill -TERM %1
   voiceforge send build_failed; echo $?                 # 2
   ```

## Atomic commits

1. `refactor(daemon_server): extract test_support module`
2. `feat(client): NDJSON Unix-socket client + voiceforge send subcommand`

## What plan v1 got wrong (audit log)

1. **Test fixture sharing.** v1 mentioned the extraction but
   didn't pick a pattern. v2: `#[cfg(test)] pub(crate)` mod, defer
   the lib-split until cross-target tests demand it.
2. **Connect-race.** v1 said "1s timeout is enough." v2: explicit
   25ms-backoff retry loop on ECONNREFUSED/NotFound so the
   `daemon &; send` one-liner just works.
3. **Connect timeout configurability.** v1 hand-waved. v2: env var
   `VOICEFORGE_SEND_TIMEOUT_MS`, clamped, 1000 ms default.
4. **Exit codes.** v1 had 4 codes; conflated "daemon replied garbage"
   with "daemon not reachable." v2: 5 codes, exit 4 for post-connect
   protocol failure.
5. **Read deadline.** v1 missing entirely. v2: separate env var
   `VOICEFORGE_SEND_READ_TIMEOUT_MS`, 5 s default.
6. **Output streams.** v1 silent. v2: stdout for `ok:true`, stderr
   for `ok:false` and transport errors.
7. **Test coverage.** v1 had 5 tests, missed protocol-error cases.
   v2: 9 tests including non-JSON, partial-line-EOF, missing-ok-field,
   read timeout, and the connect-race itself.
8. **`SendOutcome::Err` ambiguity.** v1's `SendOutcome::Err {error}`
   conflated daemon-rejected and transport-failed. v2: split into
   `SendOutcome::Rejected` (protocol-level) and `SendError::{NotReachable,
Protocol}` (transport-level), with clear exit-code mapping.
