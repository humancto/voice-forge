//! URL ingest via `yt-dlp` (ROADMAP 2.3).
//!
//! `voiceforge clone <name> <URL>` and `voiceforge ingest <URL> <out>`
//! both route through `resolve_source` here. Local file paths fall
//! through to `ResolvedSource::Local`. URLs (http/https/ytsearch/file)
//! are downloaded into a tempdir via `yt-dlp` and returned as
//! `ResolvedSource::Downloaded { local_path, _tempdir }`.
//!
//! See `.planning/url-ingest.plan.md` for the design audit
//! (rust-expert plan v2 APPROVE).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

/// Cap on captured yt-dlp stderr in error messages. yt-dlp can spew
/// kilobytes on format probes; truncate to the last 4 KiB.
const STDERR_TAIL_BYTES: usize = 4 * 1024;

/// Default per-download max size. yt-dlp accepts `K`, `M`, `G` suffix.
/// 250 MiB covers ~3 hours of 192 kbps audio, which is far more than
/// the 60 s the cloning pipeline actually wants. Capping prevents a
/// playlist URL from filling /tmp.
const MAX_FILESIZE: &str = "250M";

/// `--socket-timeout` in seconds. yt-dlp default is unlimited which
/// means a stalled connection hangs the whole CLI forever.
const SOCKET_TIMEOUT_SECS: u32 = 30;

/// `--retries` for transient HTTP failures. yt-dlp's own retry logic
/// is preferred over wrapping the whole spawn in a retry loop.
const YT_DLP_RETRIES: u32 = 3;

/// URL detection. Schemeless hostnames (`youtube.com/...`) deliberately
/// do NOT count — they fall through to the local-path branch which
/// errors crisply and teaches the user to add `https://`. Rejected
/// alternative was a hostname regex; the crisp error is simpler.
pub fn is_url(s: &str) -> bool {
    s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("ytsearch") // covers ytsearch:, ytsearch5:, etc.
        || s.starts_with("file://")
}

/// A source the cloning / ingest pipeline can read from a local path.
///
/// `Local` is for paths the caller already had on disk. `Downloaded`
/// holds the tempdir alive — the caller MUST keep this value in scope
/// for the entire duration of any consumer of `local_path()`. If a
/// future caller wraps this in async, the `ResolvedSource` value MUST
/// outlive the `.await` of any consumer.
#[must_use]
#[derive(Debug)]
pub struct ResolvedSource {
    local_path: PathBuf,
    // Renamed from `_guard` to `_tempdir` per rust-expert review —
    // `_guard` reads like a lock guard, but this is a Drop-on-scope
    // tempdir cleaner. None for `Local`, Some for `Downloaded`.
    _tempdir: Option<TempDir>,
}

impl ResolvedSource {
    pub fn local_path(&self) -> &Path {
        &self.local_path
    }

    /// Test-only: was this resolved from a URL (vs a local path)?
    #[cfg(test)]
    pub(crate) fn was_downloaded(&self) -> bool {
        self._tempdir.is_some()
    }
}

/// Resolve `source` to a local file path the pipeline can read.
///
/// - `file:///abs/path.wav` -> Local (scheme stripped).
/// - URL via `is_url` -> Downloaded via `yt-dlp` into a tempdir.
/// - Else -> Local with `canonicalize()` (errors if the path is
///   missing, which is the right teaching moment for users who paste
///   a schemeless `youtube.com/...`).
pub fn resolve_source(source: &str) -> Result<ResolvedSource> {
    // file:// short-circuit -- never hit yt-dlp for local URLs.
    if let Some(rest) = source.strip_prefix("file://") {
        let path = PathBuf::from(rest);
        if !path.exists() {
            bail!(
                "file:// URL points to a path that does not exist: {}",
                path.display()
            );
        }
        return Ok(ResolvedSource {
            local_path: path,
            _tempdir: None,
        });
    }

    if is_url(source) {
        return download(source);
    }

    let path = PathBuf::from(source);
    if !path.exists() {
        bail!(
            "source path does not exist: {}\n\
             hint: if you meant a URL, prefix with `https://` (schemeless hostnames are not auto-detected)",
            path.display()
        );
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("could not canonicalize source path {}", path.display()))?;
    Ok(ResolvedSource {
        local_path: canonical,
        _tempdir: None,
    })
}

/// Spawn yt-dlp to download `url` into a fresh tempdir. Returns the
/// path to the resulting WAV (yt-dlp picks the extension via the
/// `--audio-format` flag; we glob for `source.*`).
pub fn download(url: &str) -> Result<ResolvedSource> {
    download_with_yt_dlp_path("yt-dlp", url)
}

/// Internal: lets tests inject a binary path to verify the
/// "yt-dlp not on PATH" error path hermetically.
fn download_with_yt_dlp_path(yt_dlp_bin: &str, url: &str) -> Result<ResolvedSource> {
    if which::which(yt_dlp_bin).is_err() && !Path::new(yt_dlp_bin).exists() {
        bail!(
            "yt-dlp is not on PATH (needed for URL ingest)\n\
             install with one of:\n  \
             brew install yt-dlp\n  \
             pipx install yt-dlp\n  \
             pip install --user yt-dlp\n\
             then re-run. (Or download the audio yourself and pass a local file path.)"
        );
    }

    let tempdir = tempfile::tempdir().context("creating tempdir for yt-dlp download")?;
    let template = tempdir.path().join("source.%(ext)s");

    let socket_timeout = SOCKET_TIMEOUT_SECS.to_string();
    let retries = YT_DLP_RETRIES.to_string();

    eprintln!("==> downloading via yt-dlp (max {MAX_FILESIZE}, {SOCKET_TIMEOUT_SECS}s socket timeout) ...");

    let output = Command::new(yt_dlp_bin)
        .args([
            "--quiet",
            "--no-warnings",
            "--no-playlist",
            "--max-filesize",
            MAX_FILESIZE,
            "--socket-timeout",
            &socket_timeout,
            "--retries",
            &retries,
            "-x",
            "--audio-format",
            "wav",
            "--audio-quality",
            "0",
            "-o",
        ])
        .arg(&template)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawning {yt_dlp_bin}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail = if stderr.len() > STDERR_TAIL_BYTES {
            // Snap to char boundary so we don't slice mid-codepoint.
            let start = stderr.len().saturating_sub(STDERR_TAIL_BYTES);
            let mut s = start;
            while !stderr.is_char_boundary(s) && s < stderr.len() {
                s += 1;
            }
            format!("...(truncated {} bytes)...\n{}", start, &stderr[s..])
        } else {
            stderr.to_string()
        };
        bail!(
            "yt-dlp failed (exit {}) for URL: {url}\n--- stderr (last {} bytes) ---\n{}",
            output.status,
            STDERR_TAIL_BYTES,
            tail.trim_end()
        );
    }

    // Glob for source.*. yt-dlp picks the extension after extraction.
    let mut matches: Vec<PathBuf> = std::fs::read_dir(tempdir.path())
        .context("reading yt-dlp output dir")?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("source."))
        })
        .collect();

    if matches.is_empty() {
        bail!(
            "yt-dlp succeeded but produced no source.* file in {}",
            tempdir.path().display()
        );
    }
    if matches.len() > 1 {
        bail!(
            "yt-dlp produced multiple source.* files in {}: {:?}",
            tempdir.path().display(),
            matches
        );
    }

    let local_path = matches.pop().unwrap();
    eprintln!("==> downloaded -> {}", local_path.display());

    Ok(ResolvedSource {
        local_path,
        _tempdir: Some(tempdir),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // 1. http(s) recognized.
    #[test]
    fn is_url_recognizes_http_https() {
        assert!(is_url("http://example.com/x.mp4"));
        assert!(is_url("https://www.youtube.com/watch?v=abc"));
    }

    // 2. ytsearch variants.
    #[test]
    fn is_url_recognizes_ytsearch_variants() {
        assert!(is_url("ytsearch:peter griffin"));
        assert!(is_url("ytsearch5:trump speech"));
    }

    // 3. file://.
    #[test]
    fn is_url_recognizes_file_scheme() {
        assert!(is_url("file:///tmp/x.wav"));
    }

    // 4. local paths rejected (incl. Windows-style + schemey-but-not).
    #[test]
    fn is_url_rejects_local_paths() {
        assert!(!is_url("/tmp/x.wav"));
        assert!(!is_url("./relative.mp3"));
        assert!(!is_url("~/audio"));
        assert!(!is_url("peter:foo"));
        assert!(!is_url("C:/foo.wav"));
        assert!(!is_url("C:\\foo.wav"));
    }

    // 5. Schemeless hostname is NOT a URL by our rules (design choice).
    #[test]
    fn is_url_rejects_schemeless_hostname() {
        assert!(!is_url("youtube.com/watch?v=x"));
        assert!(!is_url("www.youtube.com"));
    }

    // 6. yt-dlp missing -> clear error (HERMETIC: env-cleared PATH).
    #[test]
    fn download_errors_clearly_when_yt_dlp_missing() {
        // Use an absolute path that doesn't exist; bypasses PATH lookup
        // ambiguity. The is-on-PATH check is the first thing download
        // does, so we don't actually invoke yt-dlp.
        let err = download_with_yt_dlp_path(
            "/nonexistent/yt-dlp-DEFINITELY-NOT-HERE",
            "https://example.com/x",
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("yt-dlp"), "msg should mention yt-dlp: {msg}");
        assert!(
            msg.contains("install") || msg.contains("not on PATH"),
            "msg should hint install: {msg}",
        );
    }

    // 7. file:// strips scheme, returns Local.
    #[test]
    fn resolve_source_strips_file_scheme() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let url = format!("file://{}", tmp.path().display());
        let r = resolve_source(&url).expect("resolve");
        assert!(!r.was_downloaded());
        assert_eq!(r.local_path(), tmp.path());
    }

    // 8. Local path returns Local.
    #[test]
    fn resolve_source_returns_local_for_path() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let r = resolve_source(tmp.path().to_str().unwrap()).expect("resolve");
        assert!(!r.was_downloaded());
    }

    // 9. Missing local path -> crisp error mentioning the path + hint.
    #[test]
    fn resolve_source_errors_on_missing_local_path() {
        let err = resolve_source("/this/does/not/exist/peter.wav").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("/this/does/not/exist/peter.wav"));
        assert!(
            msg.contains("https://") || msg.contains("schemeless"),
            "missing-path error should hint about URL prefix: {msg}",
        );
    }

    // 10. Schemeless youtube.com hostname goes through the local-path
    //     error (as designed) — tests the "crisp error is the teach".
    #[test]
    fn resolve_source_schemeless_hostname_errors_with_url_hint() {
        let err = resolve_source("youtube.com/watch?v=abc").unwrap_err();
        let msg = format!("{err:#}");
        // The error text mentions the path AND the URL hint.
        assert!(msg.contains("youtube.com"));
        assert!(msg.contains("https://"));
    }

    // 11. ResolvedSource is #[must_use]: this is a compile-time
    //     attribute, not a runtime test; we just confirm the struct
    //     has the attribute via doc-test placement above. Smoke:
    //     Local vs Downloaded are distinguishable.
    #[test]
    fn resolved_source_local_is_not_downloaded() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let r = resolve_source(tmp.path().to_str().unwrap()).expect("resolve");
        assert!(!r.was_downloaded());
    }

    // Marker test: download() against a clearly-unreachable URL with
    // a real yt-dlp installation should fail fast (not hang). Skipped
    // when yt-dlp isn't on PATH so CI without it stays green.
    #[test]
    fn download_against_unreachable_url_fails_fast_when_yt_dlp_present() {
        if which::which("yt-dlp").is_err() {
            eprintln!("[skip] yt-dlp not on PATH");
            return;
        }
        let start = Instant::now();
        let err = download("https://nonexistent.invalid.tld.does.not.exist/x.mp4").unwrap_err();
        let elapsed = start.elapsed();
        // Should fail well within the socket-timeout * retries budget
        // (30s * 3 + connect attempts). Cap test wait at 90s.
        assert!(
            elapsed < Duration::from_secs(90),
            "download took {elapsed:?} on unreachable URL"
        );
        let msg = format!("{err:#}");
        assert!(msg.contains("yt-dlp"), "msg should mention yt-dlp: {msg}");
    }
}
