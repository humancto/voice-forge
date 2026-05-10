---
roadmap_item: 3.5 macOS notification bridge
status: plan v2 — rust-expert REVISE folded
---

# 3.5 macOS notification bridge

## Goal (from ROADMAP)

> macOS notification bridge: `osascript` notification mirrors every spoken line.

The daemon already speaks every line. Mirror that line as a macOS Notification
Center banner so the user gets a visual record + a chime even when audio is
muted, AirPods are off, or they're heads-down on another desktop. Opt-in via
env var so headless / Linux / non-mac users are unaffected.

## Plan v2 deltas (rust-expert review fold)

| #         | Item                                                                                   | Resolution                                                                                                                                                                                        |
| --------- | -------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Bug 1     | Wrong symbol name                                                                      | Call site is `process_frame`, not `handle_speak_internal`. Same line (~317). Fixed below.                                                                                                         |
| Bug 2     | `std::process::Command::spawn` leaks zombies in long-lived daemon                      | Use **`tokio::process::Command::spawn`**, then `tokio::spawn` an awaiter that `.wait().await`s the `Child` so it's reaped. Tokio's runtime owns the SIGCHLD handler.                              |
| Bug 3     | AppleScript escape rule incomplete                                                     | Strip C0 controls (NUL etc.) → space first, THEN escape `\` (must come before `"`), THEN `"`. Order matters.                                                                                      |
| Bug 4     | `std::env::var` on hot path is wasteful + becomes UB if anything calls `set_var` later | Cache via `OnceLock<bool>` at first call. Test path uses an injected env-reader to avoid the cache + serial_test contention.                                                                      |
| Risk 1    | Cached `enabled()` breaks env-flip tests                                               | Split: `pub fn enabled() -> bool` (caches via OnceLock for prod) + `pub(crate) fn enabled_with(env: impl Fn(&str) -> Option<String>) -> bool` (used by tests, no cache).                          |
| Risk 2    | TLS recorder breaks under async (worker thread ≠ test thread)                          | Use the **`Arc<dyn Mirror>` trait-injection** pattern that mirrors how `AudioSink` already works. `OsascriptMirror` is the prod impl; `RecordingMirror` is for tests. Wired through daemon state. |
| Risk 3    | Mirror-before-speak timing                                                             | Documented + accepted. Banner-without-audio-on-synth-failure is preferable to silent failure.                                                                                                     |
| Risk 4    | Char-boundary truncation pattern                                                       | **Reuse `watch.rs::truncate_to_bytes`** (promote to `pub(crate)`). 240 **bytes** (matches Notification Center's actual byte limit + `watch.rs` precedent).                                        |
| Risk 5    | `sound name "Pop"` contradiction                                                       | Drop sound name. Silent. Voiceforge IS the sound.                                                                                                                                                 |
| Risk 6    | osascript notifications are coalesced under "Script Editor", not "voiceforge"          | Documented as known limitation in caveats + README. Fixing requires a real bundle ID — out of scope for v1.                                                                                       |
| Missing 1 | Test harness wiring                                                                    | Trait injection (above). Daemon `state.mirror: Arc<dyn Mirror>` field, default = `Arc::new(OsascriptMirror)`.                                                                                     |
| Missing 2 | clippy + fmt gates                                                                     | Added explicitly to test plan.                                                                                                                                                                    |
| Missing 3 | osascript spawn time variability                                                       | Confirmed: never `.await` the spawn. `spawn()` returns `io::Result<Child>` immediately; `tokio::spawn` the wait.                                                                                  |
| Missing 4 | macOS Notification Center permission prompt                                            | Documented in edge cases + README ("first run, macOS will prompt to grant Script Editor notification permission").                                                                                |
| Missing 5 | tracing vs eprintln                                                                    | Codebase uses `eprintln!` everywhere (verified `grep`). Match.                                                                                                                                    |
| Missing 6 | Env vs future config precedence                                                        | One-liner in README: env always wins.                                                                                                                                                             |
| Missing 7 | Doctor row                                                                             | Added: `voiceforge doctor` reports osascript on PATH + env var state.                                                                                                                             |
| Nit       | "240 chars" vs "240 bytes"                                                             | **Bytes**. Match `watch.rs`.                                                                                                                                                                      |
| Nit       | mirror() arg order ambiguity                                                           | Use named locals at call site: `mirror.mirror(/* voice */ &voice, /* text */ &text)`.                                                                                                             |

## Surface (final)

- **NEW** `apps/voiceforge-cli/src/notify_macos.rs` (~180 lines + tests):

  ```rust
  use std::sync::{Arc, OnceLock};

  /// Trait so the daemon can swap the real osascript path for a recording
  /// mirror in tests. Object-safe; one method.
  pub(crate) trait Mirror: Send + Sync {
      /// Best-effort fire-and-forget. Implementors MUST NOT block.
      /// `voice` populates the banner title; `text` populates the body.
      fn mirror(&self, voice: &str, text: &str);
  }

  /// Production impl. Spawns `osascript -e <script>` via `tokio::process::Command`
  /// and detaches an awaiter task that reaps the child.
  pub(crate) struct OsascriptMirror;

  impl Mirror for OsascriptMirror {
      fn mirror(&self, voice: &str, text: &str) {
          // Cheap exits first.
          if !enabled() { return; }
          // (macOS-only impl behind cfg)
          spawn_osascript(voice, text);
      }
  }

  /// Wires up the right Mirror for the platform and current env.
  pub(crate) fn default_mirror() -> Arc<dyn Mirror> {
      Arc::new(OsascriptMirror)
  }

  /// Cached at first call (OnceLock). After this returns, the daemon
  /// will never read the env var again — avoids the post-1.85 unsafe
  /// `set_var` UB risk and the per-event lookup cost.
  pub fn enabled() -> bool {
      static E: OnceLock<bool> = OnceLock::new();
      *E.get_or_init(|| enabled_with(|k| std::env::var(k).ok()))
  }

  /// Test-friendly: pure function over an injected env reader.
  pub(crate) fn enabled_with<F: Fn(&str) -> Option<String>>(env: F) -> bool {
      matches!(
          env("VOICEFORGE_MIRROR_NOTIFICATIONS").as_deref(),
          Some("1" | "true" | "yes" | "on")
      )
  }

  /// Strip C0 control chars → space (AppleScript-safe + NUL would otherwise
  /// blow up `Command::arg` on Unix). Then escape `\` (FIRST), then `"`.
  /// Order: `\` before `"` is mandatory — reverse order escapes the
  /// backslash you just inserted.
  pub(crate) fn escape_for_applescript(s: &str) -> String { ... }

  /// macOS-only spawn. cfg-gated. tokio::process::Command + tokio::spawn
  /// the wait so the Child is reaped and never zombies.
  #[cfg(target_os = "macos")]
  fn spawn_osascript(voice: &str, text: &str) { ... }

  #[cfg(not(target_os = "macos"))]
  fn spawn_osascript(_voice: &str, _text: &str) { /* no-op */ }
  ```

- **MODIFIED** `apps/voiceforge-cli/src/watch.rs`: promote
  `fn truncate_to_bytes` → `pub(crate)`. Reuse from `notify_macos`.

- **MODIFIED** `apps/voiceforge-cli/src/daemon_server.rs`:
  - Add a `mirror: Arc<dyn notify_macos::Mirror>` field on whichever struct
    holds the daemon's per-server state. (If the existing handlers take
    `engine`, `rules`, `sink` as separate `Arc`s, add `mirror` as a fourth
    sibling — match the existing shape.)
  - In `process_frame`, right BEFORE `engine.speak(&text, &voice).await?`
    (line ~317):

    ```rust
    mirror.mirror(/* voice */ &voice, /* text */ &text);
    ```

    The `mirror.mirror(...)` call self-checks `enabled()` and is a no-op
    when off, so no `if` wrapper at the call site. Cleaner.

  - Default constructor wires `notify_macos::default_mirror()`. Test
    constructor accepts an injected `Arc<dyn Mirror>` — same shape as the
    existing `RecordingSink` injection in `test_support`.

- **MODIFIED** `apps/voiceforge-cli/src/main.rs`: add `mod notify_macos;`.

- **MODIFIED** `apps/voiceforge-cli/src/doctor.rs`: add a row that reports:
  - `osascript` on PATH (macOS only — skipped on Linux)
  - `VOICEFORGE_MIRROR_NOTIFICATIONS` value (or "unset")

- **MODIFIED** `README.md`: one paragraph under "Capabilities":

  > **Visual mirror (macOS).** Set `VOICEFORGE_MIRROR_NOTIFICATIONS=1` and the
  > daemon mirrors every spoken line as a macOS notification banner. First run
  > triggers a permission prompt for "Script Editor" (osascript runs under
  > that bundle ID). If you say no, mirroring silently no-ops. Env always wins
  > if a future config field is added.

## Design choices (folded)

### Why env var, not config file?

Three reasons:

1. **Per-shell scoping**. Users want it on for `tmux` / Claude Code sessions
   but off for headless cron daemons. Env beats config for this.
2. **Daemon picks it up at process start**. Simpler than wiring a config
   reload through the daemon protocol. We cache via `OnceLock` so the env
   read happens exactly once, at first event.
3. **One-line muscle memory** —
   `VOICEFORGE_MIRROR_NOTIFICATIONS=1 voiceforge daemon` — reads better than
   editing a TOML.

### Why fire-and-forget via `tokio::process::Command`?

`std::process::Command::spawn` + drop-Child leaks zombies in a long-lived
daemon (the parent never `wait`s and never exits to be reparented to launchd).
`tokio::process::Command::spawn` returns immediately with an `io::Result<Child>`
and Tokio's runtime registers a SIGCHLD handler that reaps the child when we
`.wait().await`. We **detach the awaiter** via `tokio::spawn` so synthesis is
never blocked on osascript:

```rust
let child = tokio::process::Command::new("osascript")
    .arg("-e").arg(script)
    .kill_on_drop(false)  // we want the script to finish
    .spawn();
match child {
    Ok(mut c) => { tokio::spawn(async move { let _ = c.wait().await; }); }
    Err(e) => eprintln!("voiceforge: osascript spawn failed: {e}"),
}
```

This is `cfg(target_os = "macos")` only. `mirror()` MUST be called from a
Tokio runtime context (our daemon always is). Documented in the doc comment.

### Why only macOS?

The ROADMAP item is explicit: "macOS notification bridge". Linux notifications
are a future `notify-send` bridge (out of scope). Use `cfg(target_os = "macos")`
guards so the module compiles on Linux as a no-op (`mirror()` returns without
spawning).

### Title format

Title = `"voiceforge · <voice>"`, body = the spoken line. Keeps the voice name
visible (so the user can tell `peter` from `tiny_robot` at a glance) without
making the body redundant.

### Mirror-before-speak timing (accepted)

Banner fires BEFORE `engine.speak(...)` so the user sees it when audio
_starts_, not 200-800 ms later. Trade-off: if synthesis errors (voice not
found), the banner appears for a line that was never spoken aloud. This is
preferable to silent failure and matches the spec ("mirrors every spoken
line" — the line was attempted).

### AppleScript escaping (corrected)

The osascript invocation is exactly:

```
osascript -e 'display notification "<body>" with title "<title>"'
```

No `sound name` (silent). We pass `-e` and `<script>` as **separate argv** to
`Command::new("osascript")`, so shell metachars (`;`, `$()`, backticks) are
inert. Only AppleScript string-literal escaping applies.

`escape_for_applescript` order:

1. Replace any `c` where `c.is_control()` with a single space (catches NUL,
   newline, tab, CR, etc.). NUL must die or `Command::arg` errors.
2. Replace `\` → `\\`.
3. Replace `"` → `\"`.

Order 2 → 3 is mandatory. Reverse it and step 3 inserts a `\` that step 2
would have escaped.

The escape neutralizes the obvious injection vector
`" with title "evil` because `"` becomes `\"`.

### 240 BYTES, not chars

Notification Center truncates around 256 bytes. We cap at 240 bytes
(headroom for the title + AppleScript wrapper). **Reuse**
`watch::truncate_to_bytes` (promote to `pub(crate)`). One canonical truncator
in the binary. UTF-8-safe via `is_char_boundary` walkback.

## Test plan (final)

**Pure unit tests (10, no spawn, no sleep, no env contention):**

- [ ] `escape_for_applescript_passes_plain_text_unchanged`
- [ ] `escape_for_applescript_escapes_double_quote`
- [ ] `escape_for_applescript_escapes_backslash`
- [ ] `escape_for_applescript_strips_newlines_and_tabs_to_space`
- [ ] `escape_for_applescript_strips_nul`
- [ ] `escape_for_applescript_neutralizes_injection_attempt` — input
      `\" with title \"evil` → ends up double-escaped, no early string close
- [ ] `enabled_with_returns_true_for_1_true_yes_on`
- [ ] `enabled_with_returns_false_for_0_empty_other`
- [ ] `enabled_with_returns_false_when_unset`
- [ ] `truncate_reuses_watch_truncate_to_bytes` — sanity that the function
      the module imports is the one we expect (compile-time guarantee in code,
      but a runtime test against a 250-byte multi-byte string proves no-panic).

**Integration tests (3, behind `#[cfg(target_os = "macos")]`, use trait injection — no env contention, no `serial_test` needed):**

- [ ] `osascript_mirror_spawn_happy_path` — sets env via the injected
      `enabled_with` reader, calls `OsascriptMirror::mirror`, asserts no panic.
      (Real spawn against real `osascript`; if the test runner doesn't have
      notification permission, osascript still exits 0.)
- [ ] `recording_mirror_captures_voice_and_text` — local
      `RecordingMirror(Mutex<Vec<(String, String)>>)`, drive it through
      `Mirror::mirror`, assert capture.
- [ ] `daemon_process_frame_calls_mirror` — wire `RecordingMirror` into the
      test daemon via `daemon_server::test_support`, send a frame through, assert
      the recorder captured the resolved (voice, text). This is the test that
      proves the daemon plumbing actually wires the mirror through.

**Gates (CI):**

- [ ] `cargo test --all` green on macOS + Linux
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] `cargo fmt --check` clean

**Manual smoke (post-merge):**

- [ ] `VOICEFORGE_MIRROR_NOTIFICATIONS=1 voiceforge daemon &; disown`
- [ ] `voiceforge say "hello world"` → banner appears with title
      `voiceforge · default`
- [ ] `voiceforge say --voice peter "test"` → banner says `voiceforge · peter`
- [ ] Unset env, restart daemon, say something → no banner
- [ ] `voiceforge doctor` → reports osascript present + env-var state

## Edge cases (folded)

1. **osascript missing**. Practically impossible on macOS, but: spawn fails
   → `eprintln!` + carry on. Daemon does not stall.
2. **Notification Center permission denied**. First osascript notification
   triggers a system prompt for "Script Editor". If the user denies, all
   subsequent osascript notifications silently no-op (osascript still exits
   0). Documented in README — common support question.
3. **Notification Center disabled entirely**. Same as #2 but quieter.
4. **Body contains shell metacharacters**. Inert — we use argv, not a shell.
5. **AppleScript injection** (`\" with title \"evil`). Neutralized by
   `escape_for_applescript`. Test covers it.
6. **NUL bytes in body**. Stripped to space by step 1 of escape.
7. **Body is large** (4 KiB cap from daemon). Truncated to 240 bytes via
   `watch::truncate_to_bytes`.
8. **Notification spam**. macOS coalesces under "Script Editor". Annoying but
   not catastrophic. v2 could add per-event debouncing if it bites.
9. **Cosmetic: title shows "Script Editor", icon is the AppleScript icon**.
   Known limitation — fixing requires a real bundle ID via `terminal-notifier`
   or `objc2 + UserNotifications`. Out of scope.
10. **CI on Linux**. Module compiles to a no-op `mirror()`. Integration
    tests are `#[cfg(target_os = "macos")]`. Linux CI is unaffected.
11. **Hot env-var set after first call**. Won't reflect in `enabled()` (cached).
    Fine — daemons restart cheaply; this is the explicit trade-off for the
    post-1.85 `set_var` UB safety story.

## Rollback

Set `VOICEFORGE_MIRROR_NOTIFICATIONS=0` (or unset entirely) and restart the
daemon. PR is one new module + one daemon-state field + one call line; trivial
revert if needed.

## Out of scope

- Linux notification bridge via `notify-send` / D-Bus (future ROADMAP item if
  asked for).
- Windows notification bridge (no current Windows users).
- Per-event-kind notification opt-in. The whole point of the bridge is
  "mirrors every spoken line" — filtering goes against that contract. If we
  add it, it should be a proper rules-level filter, not a notification-only
  knob.
- Custom notification sound. Voiceforge IS the sound.
- Click-to-focus deep links. Not without a real bundle ID.
- `terminal-notifier` integration to get the voiceforge name + icon on the
  banner. Worth doing eventually; tracked as 3.5.1 follow-up.
