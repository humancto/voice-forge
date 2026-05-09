# Plan v2: Unix-socket daemon (ROADMAP 1.8)

> Plan v1 → rust-expert REVISE with 12 specific items + 3 nits.
> Plan v2 folds all 12 in. Reviewer should re-run on this version.

## Goal

Replace the placeholder daemon (which speaks "alive" every 30 s) with
a real event-driven server. External tools (claude-code hooks, git
hooks, shell preexec, the future `voiceforge send` from 1.9) drop JSON
events on a Unix socket; the daemon reads them and routes to the TTS
engine + audio playback.

```text
~/.voiceforge/voiceforge.sock
                   │
   {"event":"build_failed", ...}\n
                   ▼
         voiceforge daemon
                   │
   choose_reaction(rules, event, fallback, rng)
                   │
                   ▼
         engine.speak (Arc<Engine>) → AudioSink::play
                   │
                   ▼
       reply: {"ok":true,"spoken":"…"}\n
```

## Wire format

NDJSON. One frame per line. Line cap: **64 KiB** (oversized frame →
`{"ok":false,"error":"frame too large"}` then close the connection).

Request:

```json
{ "event": "build_failed", "message": "npm test died", "voice": "angry_duck" }
```

Fields:

- `event` (string, required-or-`text`) — dispatched through `rules`;
  unknown events fall back to `text`.
- `text` (string, optional) — explicit text to speak. Wins over rules.
- `voice` (string, optional) — explicit voice override.
- `message` (string, optional) — informational; logged, not spoken.

Either `event` OR `text` must be present, else 400-style reply.

Reply (one line per request):

```json
{ "ok": true, "spoken": "The build failed again.", "voice": "angry_duck" }
```

```json
{ "ok": false, "error": "unknown event \"foo\" and no text provided" }
```

## Out of scope

- Auth / multi-user safety (socket is mode 0600 in user-owned dir).
- Streaming audio over the socket. Daemon plays locally; client
  receives only the ack.
- Persistent state — no log file, no replay queue. Defer to 1.8.1.
- `voiceforge send` client — that is 1.9.
- `flock` lockfile — see "Stale-socket TOCTOU" below; deliberate non-goal.

## Files

### New

- `apps/voiceforge-cli/src/daemon_server.rs`:

  ```rust
  pub struct DaemonConfig { pub socket_path: PathBuf }

  /// Run the daemon to completion (graceful shutdown on SIGTERM / Ctrl-C).
  ///
  /// Engine and Rules are injected so tests can hand in a recording sink
  /// and a synthetic ruleset without touching the filesystem.
  pub async fn serve(
      cfg: DaemonConfig,
      engine: Arc<Engine>,
      rules: Arc<Rules>,
      sink: Arc<dyn AudioSink>,
  ) -> anyhow::Result<()>;
  ```

  Behavior:
  - **Stale-socket detect.** If the socket file exists, try to connect
    (~50 ms timeout). On success → another daemon is live; refuse with
    a clear error. On `ConnectionRefused` / no listener → stale, unlink
    and re-bind.
  - **Bind, then chmod 0600.** `UnixListener::bind(...)` followed by
    `fs::set_permissions(path, 0o600)`. The microsecond-window race in
    a user-owned dir is accepted. (Rejected `umask` dance: process-
    global state, `unsafe` per-call, races with other threads.)
  - **`SocketGuard(PathBuf)` RAII.** Constructed after successful bind;
    `Drop` impl unlinks the socket file. Covers panic cleanup without
    relying on the signal handler.
  - **Accept loop.** `select!` between `listener.accept()` (cancel-safe
    per Tokio docs) and a shutdown future. On shutdown, drop listener
    - drop guard → file unlinked. In-flight handler tasks finish their
      current line and exit naturally when the client closes.
  - **Per-connection task.** `BufReader::with_capacity(8 * 1024, read_half)`
    - `read_line(&mut buf)` against a 64 KiB-capped `String`. **No
      `select!` inside this loop** — `read_line` is not cancel-safe in
      isolation, but with no concurrent future to race against in the
      same task, that doesn't bite us.
  - **Per-frame handling.** Acquire a semaphore permit (cap 8), parse
    JSON, dispatch via `choose_reaction`, then `tokio::task::spawn_blocking`
    for the actual `AudioSink::play` call (rodio is sync). Permit is
    held until the spawn_blocking returns, so cap is honored end-to-end.
  - **Concurrency note.** The semaphore caps audio-playback queue
    depth; synthesis throughput is already serialized inside
    `CloningEngine` by its own `TokioMutex`. So "8 permits" really
    means "≤8 audio plays queued, ≤1 synthesis at a time." Documented.
  - **Frame size cap.** 64 KiB enforced via a check on `read_line`'s
    grown-buffer length each iteration. Overflow → reply error, close
    connection. Prevents OOM from a hostile same-user process.
  - **Shutdown signal.** Explicit `select!` between `ctrl_c()` and
    `unix::signal(SignalKind::terminate())`:

    ```rust
    let mut sigterm = signal(SignalKind::terminate())?;
    let shutdown = async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => (),
            _ = sigterm.recv() => (),
        }
    };
    ```

    Sketched in the plan to prevent the implementer reaching for
    `ctrl_c().await` alone (silently breaks `kill <pid>`).

  - **SIGKILL / panic / power-loss leaves the socket file.** This is
    handled on the _next_ startup by the stale-socket detect. No
    pidfile, no flock — the connect-probe is sufficient. `SocketGuard`
    handles the panic-while-running case.
  - **Logging.** `eprintln!`-based for v1 (no `tracing` dep yet):
    accept, parse-error, oversized-frame, frame OK, shutdown. Add a
    TODO to migrate when there are 2+ daemons in the codebase.

- `apps/voiceforge-cli/src/audio_sink.rs` (small new module — keeps
  `audio.rs` as the existing free-function surface):

  ```rust
  pub trait AudioSink: Send + Sync {
      fn play(&self, wav_path: &Path) -> anyhow::Result<()>;
  }

  #[derive(Default)]
  pub struct RodioSink;
  impl AudioSink for RodioSink {
      fn play(&self, wav_path: &Path) -> anyhow::Result<()> {
          crate::audio::play(wav_path)
      }
  }
  ```

  Test-only `RecordingSink` lives in `#[cfg(test)] mod tests` of
  `daemon_server.rs`.

### Modified

- `apps/voiceforge-cli/src/daemon.rs` — placeholder body replaced with
  a thin wrapper that builds the four `Arc`s and calls
  `daemon_server::serve(...)`.
- `apps/voiceforge-cli/src/main.rs` — no signature change to
  `Commands::Daemon`. Adds `bootstrap::ensure_voiceforge_home` defensive
  re-call before `serve` in case the daemon is the first command run.
- `apps/voiceforge-cli/src/doctor.rs` — adds a "daemon socket" probe:
  reports `running` / `not running` / `stale file present` based on
  the same connect-test the daemon uses. One screen of code; ships in
  this PR so 3.x integration can be doctor-tested.
- `apps/voiceforge-cli/src/lib.rs` (or wherever modules are declared)
  — `mod audio_sink;` `mod daemon_server;`.

### Critical: Engine sharing

`Engine` (`tts.rs:30`) is **not** `Clone` and **does not need to be**.
`Engine::speak` is `&self` (`tts.rs:38`). The daemon constructs one
`Engine` at startup, wraps in `Arc<Engine>`, and clones the `Arc` into
each per-connection task. No `Clone` derive on `Engine` itself.

Plan v1 said "Arc is belt-and-suspenders" — that was wrong. `Arc` is
load-bearing, not optional. v2 corrects this.

### Critical: `audio::play` callers stay untouched

`audio::play` is called from:

- `apps/voiceforge-cli/src/main.rs:198` (Say)
- `apps/voiceforge-cli/src/runner.rs:62` (Run)
- (was) `apps/voiceforge-cli/src/daemon.rs:15` — placeholder, going away

The daemon will use `RodioSink` via the trait. `audio::play` stays as
a free function, used unchanged by Say/Run. Plan v1's "small refactor"
was bigger than the plan made it sound; v2 explicitly keeps the free
function so Say/Run are zero-touch.

## Tests

`apps/voiceforge-cli/src/daemon_server.rs` `#[cfg(test)] mod tests`:

1. **`serve_handles_event_frame`** — start daemon on tempdir socket,
   send `{"event":"build_failed"}\n`, assert reply `ok: true` + `spoken`
   matches a configured `build_failed` line. Stub via `RecordingSink`.
2. **`serve_handles_text_only_frame`** — `{"text":"hello","voice":"default"}`
   → `ok: true, spoken: "hello"`.
3. **`serve_rejects_empty_frame`** — `{}\n` → `ok: false`.
4. **`serve_unlinks_stale_socket`** — pre-create a regular file at the
   socket path, assert `serve` deletes it and binds successfully.
5. **`serve_refuses_when_live_daemon_present`** — start one daemon,
   start a second on the same path, assert second errors clearly.
6. **`serve_handles_multiple_clients_concurrently`** — fan out 8
   clients × 1 frame each; assert 8 acks + 8 sink invocations.
7. **`serve_handles_malformed_mid_stream`** — send
   `{"event":"x"}\n{garbage\n{"event":"y"}\n` on one connection;
   assert frame 1 OK, frame 2 `ok:false`, frame 3 OK, connection still
   open. Proves "one bad frame doesn't kill the connection."
8. **`serve_rejects_oversized_frame`** — send 1 MiB of `a` + `\n`;
   assert clean `ok:false` + connection closed, daemon survives.
9. **`serve_survives_client_disconnect_before_reply`** — send a frame,
   immediately drop the client socket; assert daemon doesn't crash,
   accepts the next connection.
10. **`serve_handles_partial_line_write`** — client sends `{"event":"x"`
    then sleeps 200 ms then `}\n`; assert daemon waits and processes
    correctly.
11. **`serve_drop_guard_unlinks_on_panic`** — best-effort. Spawn
    daemon in a task that panics after bind; assert socket file is
    gone after task awaits to error. (May skip if intractable; not
    worth blocking on.)

The audio-playback path is gated behind `AudioSink` so tests never
open the audio device.

## Verify

1. `cargo test daemon_server::` — all tests pass (10 must-haves +
   #11 best-effort).
2. `cargo test` (full) — no regressions in Say/Run paths
   (proves `audio::play` callers are untouched).
3. `cargo clippy --all-targets -- -D warnings` — clean.
4. `cargo fmt --check` — clean.
5. **Manual smoke**:
   ```sh
   voiceforge daemon &
   printf '{"event":"build_failed"}\n' | nc -U ~/.voiceforge/voiceforge.sock
   # → speaks; reply lands on stdout
   kill -TERM %1
   # → socket file unlinked
   ls ~/.voiceforge/voiceforge.sock  # → ENOENT
   voiceforge doctor --json | jq '.daemon'
   # → {"status":"not running"}
   ```
6. **Manual stale-recovery**:
   ```sh
   touch ~/.voiceforge/voiceforge.sock      # not a real socket
   voiceforge daemon                        # should bind successfully
   ```

## Risks (acknowledged, not blockers)

- **Stale-socket TOCTOU.** Two `voiceforge daemon` invocations racing
  is a developer-machine scenario. Loser of `bind` errors with
  `EADDRINUSE` and exits cleanly. `flock` would close the gap
  (`fs2` already in `Cargo.toml:25`) but the failure mode is a
  microsecond window on a single-user box. Documented non-goal.
- **rodio blocking.** `spawn_blocking` (per-handler) keeps the Tokio
  worker pool free. Tokio's default blocking pool is 512 — semaphore
  cap of 8 prevents pool exhaustion regardless.
- **`AudioSink` `Send + Sync`.** `RodioSink` is a unit struct, trivially
  both. Tests' `RecordingSink` uses `Mutex<Vec<PathBuf>>` for the
  recording state — also `Send + Sync`. Safe.

## Atomic commits

1. `refactor(audio): introduce AudioSink trait + RodioSink default impl`
   — adds `audio_sink.rs`, no caller changes.
2. `feat(daemon): Unix-socket NDJSON server with stale-socket detect`
   — adds `daemon_server.rs`, replaces `daemon.rs` placeholder body,
   wires `Arc<Engine>` + `Arc<Rules>` through.
3. `test(daemon): ten integration tests via tempdir socket + recording sink`
   — the test module + `RecordingSink`.
4. `feat(doctor): probe ~/.voiceforge/voiceforge.sock daemon socket`
   — adds the doctor check + a unit test.

Squash-merge will collapse them; staged for reviewer ergonomics.

## Coding-time implementation notes (rust-expert v2 APPROVE)

These are tactical, not blocking. Fold during implementation.

1. **64 KiB enforcement mechanics.** `BufRead::read_line` does not
   accept a cap and grows the `String` unbounded. Use
   `AsyncBufReadExt::read_until` against a `Vec<u8>` and reject if
   `buf.len() > 64 * 1024 + 1`. (Or `BufReader::take(64*1024+1)
.read_until(b'\n', ...)` — same effect.) Don't rely on post-hoc
   length checks — by then the allocator has done the work.
2. **`SocketGuard` ordering vs accept loop.** Construct the guard
   _immediately_ after `UnixListener::bind` succeeds and _before_
   the `chmod` call. If `chmod` fails the file still gets unlinked.
3. **Stale-detect timeout.** Wrap the connect probe in
   `tokio::time::timeout(Duration::from_millis(50), UnixStream::connect(&path))`.
   On `Err(Elapsed)` treat as live (conservative — refuse to clobber).
   Only `ConnectionRefused` / `NotFound` / `ENOENT`-class errors mean
   stale.
4. **Permit-hold across `spawn_blocking`.** Move the
   `OwnedSemaphorePermit` _into_ the `spawn_blocking` closure so it's
   dropped when the blocking task ends — survives task cancellation
   on the async side. Comment the choice.
5. **Per-connection task lifetime on shutdown.** Dropping the
   listener does NOT close existing accepted streams. Two options:
   (a) handlers run until the client disconnects (simpler, document);
   (b) `tokio_util::sync::CancellationToken` + `select!` only on the
   write side (writes are cancel-safe in a way `read_line` is not).
   Pick (a); document.
6. **Test #11 panic-guard mechanics.** Spawn daemon in `tokio::spawn`,
   await `JoinHandle` and assert `Err::is_panic()`. Then poll
   `metadata(&socket_path)` inside `tokio::time::timeout(2s, ...)`
   with bounded retry (drop runs after panic unwinds). No raw sleeps.
7. **Doctor probe shares the timeout.** Define
   `pub(crate) const STALE_PROBE_TIMEOUT: Duration = Duration::from_millis(50)`
   in `daemon_server` and use it from both the daemon's stale-detect
   and `doctor`'s probe. Otherwise doctor reports under load skew.
8. **`bootstrap::ensure_voiceforge_home` cheap-idempotent.** The
   defensive re-call before `serve` should early-return when the dir
   exists with the right mode. Don't re-create files.

## What plan v1 got wrong (audit log)

- v1 called `Arc<Engine>` "belt-and-suspenders." It's load-bearing —
  `Engine` isn't `Clone`. **v2: explicit Arc<Engine> design point.**
- v1 didn't analyze cancel-safety. \*\*v2: no `select!` inside per-
  connection read loop; only at accept-loop level. `read_line`
  - `accept` cancel-safety stated.\*\*
- v1 didn't connect SIGKILL / panic to recovery. **v2: stale-socket
  detect on next startup is the recovery; `SocketGuard` Drop covers
  panic. Stated explicitly.**
- v1 said "chmod after bind, microsecond race doesn't matter" without
  considering the alternative. **v2: rejects `umask` dance for stated
  reasons; keeps `bind`-then-`chmod`.**
- v1 underspecified the `audio::play` caller surface. **v2: enumerates
  all 3 callers; keeps `audio::play` free function so Say/Run are
  zero-touch.**
- v1 asked semaphore-vs-spawn_blocking and didn't answer. **v2: both,
  in that order, with reasoning.**
- v1 didn't cap frame size. **v2: 64 KiB cap.**
- v1 sketched signal handling vaguely. **v2: explicit `select!`.**
- v1 had `serve()` doing engine selection internally. **v2: `serve()`
  takes `Arc<Engine>` + `Arc<Rules>` + `Arc<dyn AudioSink>` for tests.**
- v1 had 6 tests. **v2: 10 must-haves + 1 best-effort.**
- v1 didn't touch `doctor`. **v2: ships a daemon-socket probe in the
  same PR.**
- v1 didn't mention logging. **v2: `eprintln!` on accept/reject/shutdown.**
