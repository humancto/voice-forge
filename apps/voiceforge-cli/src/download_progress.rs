//! Range-resumable HTTP downloads (ROADMAP v0.4 PR-AB step 5).
//!
//! `voiceforge install-cloning` will download ~15-18 GB on a fresh
//! install (fish-speech weights ~10 GB, Whisper medium ~1.5 GB, plus
//! pip wheels). On a slow connection that's 30+ minutes of wall time
//! per file — users WILL Ctrl-C, close their laptop lid, walk away,
//! and come back later. Resumability is mandatory, not a nice-to-have.
//!
//! ## Contract (per plan v3 §S5 + B-v3-5)
//!
//! 1. **Atomic rename pattern.** Body streams to `<dest>.partial`;
//!    sha256-verified; only then renamed to `<dest>`. No partial file
//!    at the canonical path is ever observable.
//! 2. **Resumable via `Range: bytes=N-`.** If `<dest>.partial` exists
//!    from a prior run, request the remainder and append.
//! 3. **Etag invalidation via `If-Range`.** Save the upstream etag
//!    next to `.partial` as `.partial.etag`. Send it back with
//!    `If-Range:` on resume. If server replies 200 (not 206), the
//!    upstream file changed mid-download and our partial bytes are
//!    garbage — restart from byte 0 with a loud warning.
//! 4. **Sha256 mismatch deletes the file** and bubbles up an error
//!    with the expected vs actual hashes. Caller decides whether to
//!    retry from scratch.
//! 5. **Progress callback** fires on each chunk read with
//!    `(downloaded_bytes, total_bytes_or_None, bytes_per_sec)`.
//!
//! Used by `install_cloning_v2` (PR-AB step 6) for fish-speech
//! weights, Whisper medium model, etc.

#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

/// Per-chunk progress observation. Callers wire this into a TUI
/// progress bar (`indicatif`), a log line, or a JSON stream.
#[derive(Debug, Clone, Copy)]
pub struct DownloadProgress {
    pub bytes_downloaded: u64,
    pub total_bytes: Option<u64>,
    pub bytes_per_sec: f64,
    pub eta_seconds: Option<u64>,
}

/// Boxed progress callback. Send + Sync so it can travel across
/// async task boundaries; Fn (not FnMut) so the callback type stays
/// simple — TUI progress bars expose interior-mutable state via
/// their own Arc<Mutex<...>>.
pub type ProgressCallback = Arc<dyn Fn(DownloadProgress) + Send + Sync>;

#[derive(Default)]
pub struct DownloadOpts {
    pub progress: Option<ProgressCallback>,
    /// Connect + read timeout per HTTP request. fish-speech weight
    /// shards are individually small; we don't need infinite patience.
    pub timeout: Option<Duration>,
    /// Sha256 hex of the expected complete file. Verified before
    /// atomic rename. None skips verification (NOT recommended for
    /// any file we trust enough to ship).
    pub expected_sha256: Option<String>,
}

/// Download `url` to `dest`. Resumable + sha256-verified per the
/// contract above.
///
/// Returns the path of the final file on success (always equal to
/// `dest`, but we return it so callers can chain).
pub async fn download_to(url: &str, dest: &Path, opts: DownloadOpts) -> Result<PathBuf> {
    let partial = partial_path(dest);
    let etag_file = etag_path(dest);

    let client = reqwest::Client::builder()
        .connect_timeout(opts.timeout.unwrap_or(Duration::from_secs(30)))
        // No top-level timeout — large downloads take >1 hour.
        .build()
        .context("building reqwest client")?;

    // Determine resume offset.
    let existing_partial_size = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);
    let prior_etag = std::fs::read_to_string(&etag_file).ok();

    let mut request = client.get(url);
    if existing_partial_size > 0 {
        let range = format!("bytes={existing_partial_size}-");
        request = request.header(reqwest::header::RANGE, range);
        if let Some(etag) = &prior_etag {
            request = request.header(reqwest::header::IF_RANGE, etag.trim());
        }
    }

    let resp = request.send().await.with_context(|| format!("GET {url}"))?;

    let status = resp.status();
    let server_etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Resolve resume vs restart.
    let (start_offset, content_length) = match (existing_partial_size, status.as_u16()) {
        (0, 200) => (0u64, content_length_from(&resp)),
        (_, 206) => {
            // Server honored our Range — append.
            let total = total_from_content_range(&resp).or_else(|| {
                // Some servers omit Content-Range; compute from current+remaining
                resp.headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(|remaining| existing_partial_size + remaining)
            });
            (existing_partial_size, total)
        }
        (_, 200) => {
            // We sent Range but got full body — etag changed.
            // Restart from byte 0 (loud warning to caller via stderr).
            eprintln!(
                "voiceforge: upstream etag changed for {url} — restarting from byte 0 ({existing_partial_size} bytes discarded)"
            );
            let _ = std::fs::remove_file(&partial);
            let _ = std::fs::remove_file(&etag_file);
            (0, content_length_from(&resp))
        }
        (_, 416) => {
            // Range Not Satisfiable: our partial is at-or-past the
            // end of the upstream file. Either it's already complete
            // and just needs verification, or upstream shrunk. Drop
            // partial + retry from 0.
            eprintln!(
                "voiceforge: server reported range not satisfiable for {url} (partial = {existing_partial_size} bytes); restarting"
            );
            let _ = std::fs::remove_file(&partial);
            let _ = std::fs::remove_file(&etag_file);
            return Box::pin(download_to(url, dest, opts)).await;
        }
        (_, code) if !status.is_success() => {
            bail!(
                "HTTP {code} from {url}: {}",
                status.canonical_reason().unwrap_or("")
            );
        }
        // Anything else (shouldn't reach) — bail with context.
        _ => bail!("unexpected response: {status} for {url}"),
    };

    // Save the etag for next time (only on a fresh successful start).
    if let Some(etag) = &server_etag {
        if start_offset == 0 {
            let _ = std::fs::write(&etag_file, etag);
        }
    }

    // Stream body to .partial. Append-mode so resume works.
    let partial_for_open = partial.clone();
    let parent = partial_for_open
        .parent()
        .with_context(|| format!("partial path has no parent: {}", partial_for_open.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(start_offset > 0)
        .write(true)
        .truncate(start_offset == 0)
        .open(&partial)
        .await
        .with_context(|| format!("opening {}", partial.display()))?;

    let start_time = Instant::now();
    let mut bytes_downloaded = start_offset;
    let mut last_progress_at = Instant::now();
    let mut last_progress_bytes = start_offset;

    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("reading response body chunk")?;
        file.write_all(&bytes)
            .await
            .with_context(|| format!("writing to {}", partial.display()))?;
        bytes_downloaded += bytes.len() as u64;

        // Throttle progress callback to ~10/sec.
        let elapsed = last_progress_at.elapsed();
        if elapsed >= Duration::from_millis(100) {
            if let Some(cb) = &opts.progress {
                let bps = (bytes_downloaded - last_progress_bytes) as f64
                    / elapsed.as_secs_f64().max(0.001);
                let eta = content_length.and_then(|total| {
                    if bps > 0.0 && total > bytes_downloaded {
                        Some(((total - bytes_downloaded) as f64 / bps) as u64)
                    } else {
                        None
                    }
                });
                cb(DownloadProgress {
                    bytes_downloaded,
                    total_bytes: content_length,
                    bytes_per_sec: bps,
                    eta_seconds: eta,
                });
            }
            last_progress_at = Instant::now();
            last_progress_bytes = bytes_downloaded;
        }
    }
    file.flush()
        .await
        .with_context(|| format!("flushing {}", partial.display()))?;
    drop(file);

    // Final progress beat at 100%.
    if let Some(cb) = &opts.progress {
        let total_elapsed = start_time.elapsed().as_secs_f64().max(0.001);
        let session_bytes = bytes_downloaded - start_offset;
        cb(DownloadProgress {
            bytes_downloaded,
            total_bytes: content_length,
            bytes_per_sec: session_bytes as f64 / total_elapsed,
            eta_seconds: Some(0),
        });
    }

    // Verify sha256 if requested.
    if let Some(expected) = &opts.expected_sha256 {
        let actual = sha256_file(&partial).await?;
        if !actual.eq_ignore_ascii_case(expected) {
            // Don't keep a corrupted partial around — sha mismatch
            // means resume is useless; the bytes don't match upstream.
            let _ = std::fs::remove_file(&partial);
            let _ = std::fs::remove_file(&etag_file);
            bail!("sha256 mismatch for {url}\n  expected: {expected}\n  actual:   {actual}");
        }
    }

    // Atomic rename — partial is fully verified now.
    std::fs::rename(&partial, dest)
        .with_context(|| format!("renaming {} -> {}", partial.display(), dest.display()))?;
    // etag file no longer useful (file is in its final location).
    let _ = std::fs::remove_file(&etag_file);

    Ok(dest.to_path_buf())
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut p = dest.to_path_buf();
    let mut name = dest
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".partial");
    p.set_file_name(name);
    p
}

fn etag_path(dest: &Path) -> PathBuf {
    let mut p = dest.to_path_buf();
    let mut name = dest
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".partial.etag");
    p.set_file_name(name);
    p
}

fn content_length_from(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

/// Parse `Content-Range: bytes A-B/C` → C (total file size).
fn total_from_content_range(resp: &reqwest::Response) -> Option<u64> {
    let h = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())?;
    // "bytes 1024-4095/8192"
    let after_slash = h.rsplit('/').next()?;
    after_slash.parse::<u64>().ok()
}

async fn sha256_file(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("opening {} for sha256", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    use tokio::io::AsyncReadExt;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

// ============================================================================
// Tests — use mockito for HTTP stubs (already in dev-deps)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn tmp_dest(tmp: &TempDir, name: &str) -> PathBuf {
        tmp.path().join(name)
    }

    /// Compute sha256 of in-memory bytes for fixture setup.
    fn sha256_bytes(b: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(b);
        hex::encode(h.finalize())
    }

    #[test]
    fn partial_path_is_sibling_with_partial_suffix() {
        let dest = Path::new("/a/b/c.bin");
        assert_eq!(partial_path(dest), Path::new("/a/b/c.bin.partial"));
    }

    #[test]
    fn etag_path_is_sibling_of_partial() {
        let dest = Path::new("/a/b/c.bin");
        assert_eq!(etag_path(dest), Path::new("/a/b/c.bin.partial.etag"));
    }

    // Note: `total_from_content_range` is covered indirectly by the
    // `resume_appends_to_existing_partial` integration test, which
    // exercises a real `Content-Range: bytes 100-999/1000` response
    // through mockito. No synthetic-response unit test needed.

    #[tokio::test]
    async fn fresh_download_writes_dest_atomically() {
        let mut server = mockito::Server::new_async().await;
        let body = b"hello voiceforge".repeat(1024); // ~16 KB
        let _m = server
            .mock("GET", "/file.bin")
            .with_status(200)
            .with_header("content-length", &body.len().to_string())
            .with_body(&body)
            .create_async()
            .await;

        let tmp = TempDir::new().unwrap();
        let dest = tmp_dest(&tmp, "file.bin");
        let url = format!("{}/file.bin", server.url());
        let result = download_to(
            &url,
            &dest,
            DownloadOpts {
                expected_sha256: Some(sha256_bytes(&body)),
                ..Default::default()
            },
        )
        .await
        .expect("download");
        assert_eq!(result, dest);
        assert!(dest.is_file());
        // .partial must NOT exist after success.
        assert!(!partial_path(&dest).exists());
        let read = std::fs::read(&dest).unwrap();
        assert_eq!(read, body);
    }

    #[tokio::test]
    async fn sha256_mismatch_deletes_partial_and_errors() {
        let mut server = mockito::Server::new_async().await;
        let body = b"the body".repeat(1000);
        let _m = server
            .mock("GET", "/file.bin")
            .with_status(200)
            .with_body(&body)
            .create_async()
            .await;

        let tmp = TempDir::new().unwrap();
        let dest = tmp_dest(&tmp, "file.bin");
        let url = format!("{}/file.bin", server.url());
        let err = download_to(
            &url,
            &dest,
            DownloadOpts {
                expected_sha256: Some(
                    "0000000000000000000000000000000000000000000000000000000000000000".into(),
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("sha256 mismatch"));
        assert!(!dest.exists(), "dest must not exist on sha mismatch");
        assert!(!partial_path(&dest).exists(), "partial must be cleaned up");
    }

    #[tokio::test]
    async fn http_error_status_bubbles_up() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/missing.bin")
            .with_status(404)
            .with_body("not found")
            .create_async()
            .await;

        let tmp = TempDir::new().unwrap();
        let dest = tmp_dest(&tmp, "missing.bin");
        let url = format!("{}/missing.bin", server.url());
        let err = download_to(&url, &dest, DownloadOpts::default())
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("404") || msg.contains("Not Found"),
            "got: {msg}"
        );
    }

    #[tokio::test]
    async fn progress_callback_fires_during_download() {
        let mut server = mockito::Server::new_async().await;
        // Make body large enough that progress fires at least once
        // even with the 100ms throttle.
        let body = b"x".repeat(512 * 1024); // 512 KB
        let _m = server
            .mock("GET", "/big.bin")
            .with_status(200)
            .with_header("content-length", &body.len().to_string())
            .with_body(&body)
            .create_async()
            .await;

        let tmp = TempDir::new().unwrap();
        let dest = tmp_dest(&tmp, "big.bin");
        let url = format!("{}/big.bin", server.url());
        let count = Arc::new(AtomicUsize::new(0));
        let count_for_cb = Arc::clone(&count);
        let cb: ProgressCallback = Arc::new(move |_p| {
            count_for_cb.fetch_add(1, Ordering::SeqCst);
        });
        download_to(
            &url,
            &dest,
            DownloadOpts {
                progress: Some(cb),
                ..Default::default()
            },
        )
        .await
        .expect("download");
        // At least the final-100% beat fires; for ~500KB on mockito
        // we typically see 1-3 callbacks.
        assert!(
            count.load(Ordering::SeqCst) >= 1,
            "expected at least one progress callback"
        );
    }

    #[tokio::test]
    async fn resume_appends_to_existing_partial() {
        // Pre-stage a partial file: first 100 bytes already on disk.
        let tmp = TempDir::new().unwrap();
        let dest = tmp_dest(&tmp, "resume.bin");
        let body_full = b"abcdefghij".repeat(100); // 1000 bytes
        std::fs::write(partial_path(&dest), &body_full[..100]).unwrap();

        // Server returns the second half (bytes 100-999) on Range request.
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/resume.bin")
            .match_header("range", "bytes=100-")
            .with_status(206)
            .with_header("content-range", "bytes 100-999/1000")
            .with_header("content-length", "900")
            .with_body(&body_full[100..])
            .create_async()
            .await;

        let url = format!("{}/resume.bin", server.url());
        download_to(
            &url,
            &dest,
            DownloadOpts {
                expected_sha256: Some(sha256_bytes(&body_full)),
                ..Default::default()
            },
        )
        .await
        .expect("resume download");
        let final_bytes = std::fs::read(&dest).unwrap();
        assert_eq!(final_bytes.len(), 1000);
        assert_eq!(final_bytes, body_full);
    }

    #[tokio::test]
    async fn etag_mismatch_restarts_from_zero() {
        // Pre-stage partial + stale etag.
        let tmp = TempDir::new().unwrap();
        let dest = tmp_dest(&tmp, "etag.bin");
        let new_body = b"NEW-BODY-".repeat(100); // 900 bytes
        std::fs::write(partial_path(&dest), b"OLD-BODY-bytes").unwrap();
        std::fs::write(etag_path(&dest), "\"old-etag\"").unwrap();

        let mut server = mockito::Server::new_async().await;
        // Server returns 200 (NOT 206) when If-Range etag doesn't
        // match — signals our partial bytes are now stale.
        let _m = server
            .mock("GET", "/etag.bin")
            .with_status(200)
            .with_header("etag", "\"new-etag\"")
            .with_header("content-length", &new_body.len().to_string())
            .with_body(&new_body)
            .create_async()
            .await;

        let url = format!("{}/etag.bin", server.url());
        download_to(
            &url,
            &dest,
            DownloadOpts {
                expected_sha256: Some(sha256_bytes(&new_body)),
                ..Default::default()
            },
        )
        .await
        .expect("etag-restart download");
        let final_bytes = std::fs::read(&dest).unwrap();
        // CRITICAL: we got the new body in full, not appended-to-stale.
        assert_eq!(final_bytes, new_body);
    }
}
