//! `voiceforge watch <path>` — fires daemon events on filesystem
//! changes (ROADMAP 3.4). Uses `notify` (cross-platform) +
//! `notify-debouncer-mini` (collapses bursts) + `globset` (filters).
//!
//! Long-running. Default behavior: log warnings on transient daemon
//! unreachability and keep going. `--strict` opts into hook-style
//! exit-2 on first-frame NotReachable.
//!
//! See `.planning/voiceforge-watch.plan.md` for the design audit
//! (rust-expert plan v2 APPROVE).

use anyhow::{anyhow, bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::daemon_client::{self, SendError, SendOutcome, SendRequest};

/// Hard cap on rendered message length. Matches daemon_client's
/// MAX_MESSAGE_BYTES so the daemon never has to truncate.
const MAX_MESSAGE_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone)]
pub struct WatchConfig {
    pub paths: Vec<PathBuf>,
    pub event: String,
    pub message_template: String,
    pub voice: Option<String>,
    pub debounce_ms: u64,
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
    pub recursive: bool,
    pub strict: bool,
    pub quiet: bool,
    pub allow_broad_watch: bool,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            event: "file_changed".into(),
            message_template: "{path} changed".into(),
            voice: None,
            debounce_ms: 500,
            includes: Vec::new(),
            excludes: Vec::new(),
            recursive: true,
            strict: false,
            quiet: false,
            allow_broad_watch: false,
            connect_timeout: Duration::from_millis(1000),
            read_timeout: Duration::from_secs(5),
        }
    }
}

/// Render the message template with `{path}`, `{count}`, `{kinds}`
/// placeholders filled in.
///
/// `{path}`:
///   - 1 path → literal
///   - 2 or 3 → comma-joined
///   - >3 → first 3 comma-joined + ` (+N more)`
///
/// `{count}` → literal N
/// `{kinds}` → deduplicated, lexicographically sorted, comma-joined
///
/// Final string is truncated to `MAX_MESSAGE_BYTES` at a UTF-8
/// char boundary.
pub(crate) fn format_message(
    template: &str,
    paths: &[PathBuf],
    kinds: &BTreeSet<String>,
) -> String {
    let count = paths.len();
    let path_str = match count {
        0 => "(none)".to_string(),
        1 => paths[0].display().to_string(),
        2 | 3 => paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        n => {
            let head = paths
                .iter()
                .take(3)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!("{head} (+{} more)", n - 3)
        }
    };
    let kinds_str = kinds.iter().cloned().collect::<Vec<_>>().join(", ");
    let rendered = template
        .replace("{path}", &path_str)
        .replace("{count}", &count.to_string())
        .replace("{kinds}", &kinds_str);
    truncate_to_bytes(&rendered, MAX_MESSAGE_BYTES)
}

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

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let g = Glob::new(p).with_context(|| format!("bad glob pattern: {p:?}"))?;
        b.add(g);
    }
    Ok(Some(b.build()?))
}

/// Strip the watched-root prefix from `path` if present, else return
/// the path unchanged. Lets globs match user-natural relative
/// patterns (`*.tmp`) instead of absolute (`/Users/.../*.tmp`).
pub(crate) fn relative_to_root<'a>(path: &'a Path, roots: &[PathBuf]) -> &'a Path {
    for root in roots {
        if let Ok(stripped) = path.strip_prefix(root) {
            return stripped;
        }
    }
    path
}

/// Refuse a recursive watch on `$HOME` or `/` unless the user passed
/// `--allow-broad-watch`. These canonicalize to commonly-symlinked
/// paths; recursive notify on them blows up the inotify watch count
/// AND surprises the user.
fn check_broad_watch(canonical: &Path, recursive: bool, allow: bool) -> Result<()> {
    if !recursive || allow {
        return Ok(());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if canonical == Path::new("/") {
        bail!("refusing recursive watch on /. Pass --allow-broad-watch if you really mean it.");
    }
    if let Some(h) = home {
        if let Ok(canon_home) = std::fs::canonicalize(&h) {
            if canonical == canon_home {
                bail!(
                    "refusing recursive watch on $HOME ({}). Pass --allow-broad-watch if you really mean it.",
                    h.display()
                );
            }
        }
    }
    Ok(())
}

/// Map a notify error into a clearer end-user message — most commonly
/// the inotify watch-count exhaustion on Linux. Returns exit code 4.
fn watcher_init_error(e: notify::Error) -> anyhow::Error {
    let msg = format!("{e}");
    if msg.to_lowercase().contains("max")
        && (msg.contains("watch") || msg.contains("user instances"))
    {
        anyhow!(
            "voiceforge watch: failed to create watcher — kernel watch limit reached.\n\
             hint: raise the limit (Linux):\n  \
             sudo sysctl fs.inotify.max_user_watches=524288\n\
             to make it persist across reboots, add to /etc/sysctl.conf:\n  \
             fs.inotify.max_user_watches=524288\n\
             (underlying error: {e})"
        )
    } else {
        anyhow!("voiceforge watch: failed to create watcher: {e}")
    }
}

/// Run the watcher to completion. Returns the process exit code.
pub async fn run(cfg: WatchConfig, socket_path: &Path) -> i32 {
    if cfg.paths.is_empty() {
        eprintln!("voiceforge watch: no paths supplied");
        return 3;
    }

    // Canonicalize each input path; refuse $HOME/'/' recursive watches
    // unless explicitly allowed.
    let mut canonical_roots: Vec<PathBuf> = Vec::with_capacity(cfg.paths.len());
    for p in &cfg.paths {
        let canon = match std::fs::canonicalize(p) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("voiceforge watch: cannot canonicalize {}: {e}", p.display());
                return 4;
            }
        };
        if let Err(e) = check_broad_watch(&canon, cfg.recursive, cfg.allow_broad_watch) {
            eprintln!("voiceforge watch: {e:#}");
            return 3;
        }
        canonical_roots.push(canon);
    }

    let includes = match build_globset(&cfg.includes) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("voiceforge watch: --include {e:#}");
            return 3;
        }
    };
    let excludes = match build_globset(&cfg.excludes) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("voiceforge watch: --exclude {e:#}");
            return 3;
        }
    };

    // Bounded channel — backpressure on a runaway watch.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<notify_debouncer_mini::DebounceEventResult>(64);

    let mut debouncer = match new_debouncer(
        Duration::from_millis(cfg.debounce_ms),
        move |res: notify_debouncer_mini::DebounceEventResult| {
            // notify's worker thread runs OUTSIDE the tokio runtime,
            // so blocking_send is safe. Bounded channel handles a
            // runaway producer.
            let _ = tx.blocking_send(res);
        },
    ) {
        Ok(d) => d,
        Err(e) => {
            let err = watcher_init_error(e);
            eprintln!("{err:#}");
            return 4;
        }
    };

    let mode = if cfg.recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    for root in &canonical_roots {
        if let Err(e) = debouncer.watcher().watch(root, mode) {
            eprintln!(
                "voiceforge watch: failed to watch {}: {e:#}",
                root.display()
            );
            return 4;
        }
    }
    if !cfg.quiet {
        eprintln!(
            "voiceforge watch: watching {} path(s) (recursive={}, debounce={}ms, event={:?})",
            canonical_roots.len(),
            cfg.recursive,
            cfg.debounce_ms,
            cfg.event,
        );
    }

    // SIGINT handling: ctrl_c() returns a future that resolves on
    // the FIRST Ctrl-C; box-pin so the select loop can race it
    // against rx.recv() across iterations.
    let mut sigint = Box::pin(tokio::signal::ctrl_c());

    let mut first_frame = true;
    let mut consecutive_failures: u32 = 0;

    loop {
        tokio::select! {
            biased;
            _ = &mut sigint => {
                if !cfg.quiet {
                    eprintln!("voiceforge watch: SIGINT received, shutting down");
                }
                return 0;
            }
            maybe = rx.recv() => {
                let Some(result) = maybe else {
                    return 0;
                };
                let events = match result {
                    Ok(v) => v,
                    Err(e) => {
                        if !cfg.quiet {
                            eprintln!("voiceforge watch: notify error: {e}");
                        }
                        continue;
                    }
                };
                if events.is_empty() {
                    continue;
                }

                // Filter: include/exclude globs, matched against the
                // path RELATIVE to the watched root (per plan).
                let mut keep_paths: Vec<PathBuf> = Vec::with_capacity(events.len());
                let mut kinds: BTreeSet<String> = BTreeSet::new();
                for ev in &events {
                    let abs = &ev.path;
                    let rel = relative_to_root(abs, &canonical_roots);
                    if let Some(inc) = &includes {
                        if !inc.is_match(rel) { continue; }
                    }
                    if let Some(exc) = &excludes {
                        if exc.is_match(rel) { continue; }
                    }
                    keep_paths.push(abs.clone());
                    kinds.insert(format!("{:?}", ev.kind).to_lowercase());
                }
                if keep_paths.is_empty() {
                    continue;
                }

                // Dedup paths (a burst can mention the same file
                // multiple times for create+modify+remove). Sort for
                // deterministic message output.
                keep_paths.sort();
                keep_paths.dedup();

                let message = format_message(&cfg.message_template, &keep_paths, &kinds);
                let req = SendRequest {
                    event: Some(cfg.event.clone()),
                    text: None,
                    voice: cfg.voice.clone(),
                    message: Some(message.clone()),
                };

                match daemon_client::send(socket_path, &req, cfg.connect_timeout, cfg.read_timeout).await {
                    Ok(SendOutcome::Ok { .. }) => {
                        consecutive_failures = 0;
                        first_frame = false;
                    }
                    Ok(SendOutcome::Rejected { error }) => {
                        if !cfg.quiet {
                            eprintln!("voiceforge watch: daemon rejected frame: {error}");
                        }
                        first_frame = false;
                    }
                    Err(SendError::NotReachable(msg)) => {
                        if first_frame && cfg.strict {
                            eprintln!("voiceforge watch: {msg}");
                            return 2;
                        }
                        consecutive_failures += 1;
                        if !cfg.quiet {
                            eprintln!(
                                "voiceforge watch: daemon not reachable ({} failures): {msg}",
                                consecutive_failures
                            );
                        }
                    }
                    Err(SendError::Protocol(msg)) => {
                        if !cfg.quiet {
                            eprintln!("voiceforge watch: daemon protocol error: {msg}");
                        }
                        first_frame = false;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_sink::AudioSink;
    use crate::daemon_server::test_support::{fixture, spawn_serve};
    use std::sync::Arc;

    fn paths_from(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    fn kinds_from(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // 1. {path} for one path → literal
    #[test]
    fn format_message_renders_path_one_path() {
        let m = format_message(
            "{path} changed",
            &paths_from(&["/x/y.txt"]),
            &BTreeSet::new(),
        );
        assert_eq!(m, "/x/y.txt changed");
    }

    // 2. {path} for 2-3 paths → comma-joined
    #[test]
    fn format_message_renders_path_three_paths_comma_joined() {
        let m = format_message(
            "changes: {path}",
            &paths_from(&["/a", "/b", "/c"]),
            &BTreeSet::new(),
        );
        assert_eq!(m, "changes: /a, /b, /c");
    }

    // 3. {path} for >3 → first 3 + (+N more)
    #[test]
    fn format_message_renders_path_many_paths_caps_at_three_plus_more() {
        let m = format_message(
            "{path}",
            &paths_from(&["/a", "/b", "/c", "/d", "/e"]),
            &BTreeSet::new(),
        );
        assert_eq!(m, "/a, /b, /c (+2 more)");
    }

    // 4. {count}
    #[test]
    fn format_message_renders_count_placeholder() {
        let m = format_message(
            "{count} files",
            &paths_from(&["/a", "/b"]),
            &BTreeSet::new(),
        );
        assert_eq!(m, "2 files");
    }

    // 5. {kinds} dedup + sorted
    #[test]
    fn format_message_renders_kinds_placeholder_dedup_and_sorted() {
        let kinds = kinds_from(&["modify", "create", "modify", "any"]);
        let m = format_message("{kinds}", &paths_from(&["/x"]), &kinds);
        assert_eq!(m, "any, create, modify");
    }

    // 6. message truncated to 4 KiB
    #[test]
    fn format_message_truncates_to_4kib_cap() {
        let big = "x".repeat(8 * 1024);
        let m = format_message(&big, &paths_from(&["/x"]), &BTreeSet::new());
        assert!(m.len() <= MAX_MESSAGE_BYTES);
    }

    // 7. no placeholders → template verbatim
    #[test]
    fn format_message_no_placeholders_returns_template_verbatim() {
        let m = format_message("hello world", &paths_from(&["/x"]), &BTreeSet::new());
        assert_eq!(m, "hello world");
    }

    // 8. globs match against the relative-to-root path
    #[test]
    fn relative_glob_matches_after_root_strip() {
        let root = PathBuf::from("/tmp/watchroot");
        let roots = vec![root.clone()];
        let abs = root.join("subdir/foo.tmp");
        let rel = relative_to_root(&abs, &roots);
        assert_eq!(rel, Path::new("subdir/foo.tmp"));

        // Build a globset that should match the relative form
        let mut b = GlobSetBuilder::new();
        b.add(Glob::new("**/*.tmp").unwrap());
        let gs = b.build().unwrap();
        assert!(gs.is_match(rel));
    }

    // 9. Real watcher fires on file modify (gated on tempdir +
    // tokio runtime). Uses test_support fixture for a fake daemon.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn watch_fires_event_on_file_modify() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let watch_dir = tmp.path().join("watched");
        std::fs::create_dir_all(&watch_dir).unwrap();

        let (cfg_d, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg_d.socket_path.clone();
        let h = spawn_serve(cfg_d, engine, rules, sink.clone() as Arc<dyn AudioSink>).await;

        let cfg = WatchConfig {
            paths: vec![watch_dir.clone()],
            debounce_ms: 100,
            quiet: true,
            ..WatchConfig::default()
        };
        let socket_for_run = socket.clone();
        let run_handle = tokio::spawn(async move { run(cfg, &socket_for_run).await });

        // Write a file inside the watched dir. macOS FSEvents needs
        // ~200-500ms to actually start observing after watch() — give
        // the watcher generous head-start.
        tokio::time::sleep(Duration::from_millis(800)).await;
        std::fs::write(watch_dir.join("x.txt"), b"hello").unwrap();

        // Wait for the daemon to record at least one play.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if sink.count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(sink.count() >= 1, "watcher did not fire any frames");
        run_handle.abort();
        h.abort();
    }

    // 10. Burst writes within debounce → ONE frame
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn watch_debounces_burst_into_single_frame() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let watch_dir = tmp.path().join("watched");
        std::fs::create_dir_all(&watch_dir).unwrap();

        let (cfg_d, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg_d.socket_path.clone();
        let h = spawn_serve(cfg_d, engine, rules, sink.clone() as Arc<dyn AudioSink>).await;

        let cfg = WatchConfig {
            paths: vec![watch_dir.clone()],
            debounce_ms: 500,
            quiet: true,
            ..WatchConfig::default()
        };
        let socket_for_run = socket.clone();
        let run_handle = tokio::spawn(async move { run(cfg, &socket_for_run).await });
        // FSEvents bootstrap delay (see test 9 comment).
        tokio::time::sleep(Duration::from_millis(800)).await;

        // 5 writes inside the 500ms debounce window.
        for i in 0..5 {
            std::fs::write(watch_dir.join("x.txt"), format!("write-{i}")).unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Wait for the burst to flush + the spawn_blocking play call.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let count = sink.count();
        // Some debouncing IS happening even if macOS FSEvents emits
        // in more than one chunk: 5 writes should produce strictly
        // fewer than 5 frames. (In practice we typically see 1-2 on
        // macOS depending on FSEvents' own coalescing latency.)
        assert!(
            (1..=4).contains(&count),
            "5 writes should debounce to 1-4 frames (proving debouncing); got {count}"
        );
        run_handle.abort();
        h.abort();
    }

    // 11. exclude glob suppresses the event
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn watch_respects_exclude_globs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let watch_dir = tmp.path().join("watched");
        std::fs::create_dir_all(&watch_dir).unwrap();

        let (cfg_d, engine, rules, sink) = fixture(tmp.path());
        let socket = cfg_d.socket_path.clone();
        let h = spawn_serve(cfg_d, engine, rules, sink.clone() as Arc<dyn AudioSink>).await;

        let cfg = WatchConfig {
            paths: vec![watch_dir.clone()],
            debounce_ms: 100,
            excludes: vec!["**/*.tmp".to_string()],
            quiet: true,
            ..WatchConfig::default()
        };
        let socket_for_run = socket.clone();
        let run_handle = tokio::spawn(async move { run(cfg, &socket_for_run).await });
        // FSEvents bootstrap delay.
        tokio::time::sleep(Duration::from_millis(800)).await;

        std::fs::write(watch_dir.join("x.tmp"), b"excluded").unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(sink.count(), 0, "*.tmp write should be excluded");
        run_handle.abort();
        h.abort();
    }

    // 12. first-frame NotReachable in default mode → keep running
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watch_first_frame_unreachable_warns_in_default_mode() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let watch_dir = tmp.path().join("watched");
        std::fs::create_dir_all(&watch_dir).unwrap();

        let cfg = WatchConfig {
            paths: vec![watch_dir.clone()],
            debounce_ms: 100,
            connect_timeout: Duration::from_millis(150),
            quiet: true,
            ..WatchConfig::default()
        };
        let socket_path = tmp.path().join("nope.sock");
        let run_handle = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(3), run(cfg, &socket_path)).await
        });
        // FSEvents bootstrap delay.
        tokio::time::sleep(Duration::from_millis(800)).await;
        std::fs::write(watch_dir.join("x.txt"), b"hi").unwrap();

        // Process should NOT exit — assert the timeout fires.
        let outcome = run_handle.await.expect("join");
        assert!(
            outcome.is_err(),
            "default mode should keep watching past first NotReachable; instead exited with {outcome:?}"
        );
    }

    // 13. --strict + first-frame NotReachable → exit 2
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watch_first_frame_unreachable_exits_2_in_strict_mode() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let watch_dir = tmp.path().join("watched");
        std::fs::create_dir_all(&watch_dir).unwrap();

        let cfg = WatchConfig {
            paths: vec![watch_dir.clone()],
            debounce_ms: 100,
            connect_timeout: Duration::from_millis(150),
            strict: true,
            quiet: true,
            ..WatchConfig::default()
        };
        let socket_path = tmp.path().join("nope.sock");
        let run_handle = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), run(cfg, &socket_path)).await
        });
        // FSEvents bootstrap delay.
        tokio::time::sleep(Duration::from_millis(800)).await;
        std::fs::write(watch_dir.join("x.txt"), b"hi").unwrap();
        let exit = run_handle.await.expect("join").expect("ran to completion");
        assert_eq!(exit, 2, "strict mode should exit 2 on first NotReachable");
    }
}
