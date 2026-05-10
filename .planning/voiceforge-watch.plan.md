# Plan v2: voiceforge watch <path> (ROADMAP 3.4)

> Plan v1 -> rust-expert REVISE with 8 items including outdated crate
> versions. v2 verified against crates.io.

## Goal

Watch one or more filesystem paths. On any change (write/create/
remove/rename), debounce, then fire a daemon event. Lets users wire
voiceforge to "this file/dir matters" — log files, build outputs,
status sentinels, hand-edited triggers.

```
$ voiceforge daemon &; disown
$ voiceforge watch ./build/                          # default: file_changed event, 500ms debounce
$ voiceforge watch ./out.log --event log_changed --debounce 2000
$ voiceforge watch ./status.txt --voice peter --quiet
```

## CLI shape

```
voiceforge watch <PATH>... [--event <name>] [--message <fmt>]
                           [--voice <name>] [--debounce <ms>]
                           [--include <glob>] [--exclude <glob>]
                           [--no-recursive]
                           [--strict]
                           [--quiet]
```

- `<PATH>...`: one or more paths (file or directory). Each watched
  independently. Existence required at start; canonicalized.
- `--event` (default `file_changed`): daemon event to fire.
- `--message` (default `{path} changed`): message format. Placeholders
  `{path}`, `{count}`, `{kinds}` per item 4 below.
- `--voice`: per-frame voice override.
- `--debounce` (default 500): per-burst window in ms.
- `--include` / `--exclude`: glob filters matched against paths
  RELATIVE to the watched root (item 5).
- `--no-recursive`: shallow watch on dirs (default: recursive).
- `--strict`: exit 2 on first-frame `NotReachable` (item 3).
- `--quiet`: suppress per-event stderr.

Exit codes: 0 normal SIGINT, 2 first-frame NotReachable AND `--strict`,
3 bad CLI args (clap), 4 watch backend init failure (incl.
`fs.inotify.max_user_watches` exhaustion).

## Crate selection (verified against crates.io 2026-05-10)

```
notify = "8"                    # 8.2.0 latest stable
notify-debouncer-mini = "0.7"   # 0.7.0 latest stable; pairs with notify 8
globset = "0.4"                 # 0.4.18 latest
```

`notify-debouncer-mini` (not `-full`) — we only need debounced
"something happened" semantics, not coalesced rename pairs. mini's
`new_debouncer(timeout, callback)` invokes the callback from notify's
worker thread with `Result<Vec<DebouncedEvent>, _>` per debounce
window.

## Sync→async bridge (item 2 from v1 review)

The notify worker thread invokes the callback synchronously. We can't
`tokio::sync::mpsc::send().await` from there (no runtime). Pattern:

```rust
let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<DebouncedEvent>>(64);
let mut debouncer = new_debouncer(
    Duration::from_millis(cfg.debounce_ms),
    move |res: DebounceEventResult| {
        if let Ok(events) = res {
            // blocking_send: notify's worker thread is OUTSIDE the
            // tokio runtime, so blocking is safe; channel is bounded
            // (64) so backpressure on a runaway watch is handled.
            let _ = tx.blocking_send(events);
        }
    },
)?;
```

Then a single tokio task does:

```rust
while let Some(burst) = rx.recv().await {
    let filtered = filter_with_globs(&burst, &cfg);
    if filtered.is_empty() { continue; }
    let req = build_send_request(&cfg, &filtered);
    let outcome = daemon_client::send(&socket, &req, ...).await;
    handle_outcome(outcome, first_frame, cfg.strict, cfg.quiet);
}
```

## First-frame semantics (item 3)

`voiceforge hook` exits 2 immediately on first-frame NotReachable
because hooks are short-lived. `voiceforge watch` is long-running and
the daemon may legitimately not be up yet. Default behavior: log a
warning, retry the next burst. `--strict` opts into the hook semantic.

## Message format (item 4 spec)

`{path}` placeholder for N>1 paths in a debounced burst:
- N == 1: literal path
- 2 <= N <= 3: comma-joined
- N > 3: first 3 comma-joined + ` (+N more)`

`{count}`: literal N.

`{kinds}`: comma-joined kind tags from notify (`modify`, `create`,
`remove`, `rename`, `access`, `other`), de-duplicated, sorted.

Final rendered message capped at 4 KiB (matches daemon_client's
MAX_MESSAGE_BYTES).

## Globs match RELATIVE paths (item 5)

notify gives us absolute paths. Users write globs relative to the
watched root (`*.rs` not `/Users/.../foo.rs`). Strip the watched root
prefix before glob matching:

```rust
let rel = path.strip_prefix(&watched_root).unwrap_or(&path);
let matches = include_set.is_match(rel) && !exclude_set.is_match(rel);
```

If multiple `<PATH>...` watches share globs, each event carries its
matched root.

## inotify watch limit (item 6)

On Linux, `notify::Watcher::new` returns
`Error::PathNotFound` or `Error::Generic` containing "max user watches"
when `fs.inotify.max_user_watches` is exhausted. Catch + remediate:

```
voiceforge watch: failed to create watcher — kernel watch limit reached.
hint: raise the limit (Linux):
  sudo sysctl fs.inotify.max_user_watches=524288
to make it persist across reboots, add to /etc/sysctl.conf:
  fs.inotify.max_user_watches=524288
```

Exit 4.

## Symlink + cycle protection (item 7)

Canonicalize input paths via `std::fs::canonicalize`. If a recursive
watch starts at `$HOME` or `/`, refuse with a `--allow-broad-watch`
opt-out (sane-default: explicit-only for these). Cap recursion depth
hint via the watcher's recursive_mode flag — notify itself doesn't
expose a depth knob, so the rejection is the protection.

## Tests (item 8: timeout, not sleep)

`apps/voiceforge-cli/src/watch.rs` `#[cfg(test)] mod tests`:

1. `format_message_renders_path_one_path` — pure unit
2. `format_message_renders_path_three_paths_comma_joined`
3. `format_message_renders_path_many_paths_caps_at_three_plus_more`
4. `format_message_renders_count_placeholder`
5. `format_message_renders_kinds_placeholder_dedup_and_sorted`
6. `format_message_truncates_to_4kib_cap`
7. `format_message_no_placeholders_returns_template_verbatim`
8. `relative_glob_matches_after_root_strip`
9. (real watcher, gated) `watch_fires_event_on_file_modify` —
   `tokio::time::timeout(Duration::from_secs(2), rx.recv())`
10. `watch_debounces_burst_into_single_frame` — write file 5x,
    assert `recv()` returns one burst; second `recv()` with 200 ms
    timeout returns `Elapsed`
11. `watch_respects_exclude_globs` — touch a `*.tmp`, assert no frame
    inside 1.5 s window
12. `watch_first_frame_unreachable_warns_in_default_mode` — no daemon,
    assert process keeps running (does NOT exit 2)
13. `watch_first_frame_unreachable_exits_2_in_strict_mode`

## Files

### New

- `apps/voiceforge-cli/src/watch.rs` (~280 lines + tests):
  - `pub struct WatchConfig { paths, event, message_template, voice, debounce_ms, includes, excludes, recursive, strict, quiet }`
  - `pub async fn run(cfg, socket_path) -> i32`
  - `pub(crate) fn format_message(template, paths, kinds) -> String`
  - `fn filter_with_globs(events, includes, excludes, watched_root) -> Vec<...>`

### Modified

- `apps/voiceforge-cli/Cargo.toml` — add `notify`, `notify-debouncer-mini`, `globset`
- `apps/voiceforge-cli/src/main.rs` — `Commands::Watch { paths, event, message, voice, debounce_ms, include, exclude, no_recursive, strict, quiet }` + `run_watch` dispatcher
- `configs/rules/events.json` — add `file_changed` (verified missing). Reuse `tiny_robot` voice for low-key notifications.

## Manual smoke (post-merge)

1. `voiceforge daemon &; disown`
2. `voiceforge watch ./test/`
3. `touch test/x.txt` → daemon speaks `file_changed`
4. `for i in 1 2 3 4 5; do touch test/x.txt; done` → ONE event (debounced)
5. `voiceforge watch . --exclude '**/target/**' --exclude '**/.git/**'`
6. `voiceforge watch nonexistent.txt` → clear error
7. `voiceforge watch ./test/ --strict` (no daemon) → exit 2 immediately

## Out of scope

- 3.4.1: per-event-kind routing (different event for create vs delete)
- 3.4.2: pattern-driven event names
- 3.4.3: `--exec` to run a command on change

## Atomic commits

1. `feat(watch): voiceforge watch <path> via notify + debouncer-mini`
2. `feat(events): add file_changed to rules`

## What plan v1 got wrong (audit log)

1. Pinned `notify = "6"` + `notify-debouncer-mini = "0.4"`. v2:
   verified crates.io stable as `notify = "8"` + `notify-debouncer-mini = "0.7"`.
2. Sync→async bridge unspecified. v2: explicit `tokio::sync::mpsc` +
   `tx.blocking_send` from notify's worker thread; explanation
   inline in plan.
3. First-frame NotReachable -> exit 2 inherited from hook. v2:
   default = log+retry, `--strict` opts into the hook semantic.
4. `{path}` undefined for N>1. v2: spec'd 1 / 2-3 comma-joined /
   >3 first-3-plus-more.
5. Glob root-relative ambiguity. v2: strip watched-root prefix
   before matching.
6. inotify limit handling. v2: catch + crisp remediation hint with
   sysctl command.
7. Symlink cycle protection. v2: canonicalize + refuse $HOME/`/`
   without `--allow-broad-watch`.
8. Tests used sleep. v2: `tokio::time::timeout` deadlines.
