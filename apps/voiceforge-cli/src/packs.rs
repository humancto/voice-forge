//! Pre-rendered voice pack lookup at `~/.voiceforge/packs/<name>/`.
//!
//! Each installed pack is a directory containing:
//!
//! - `manifest.toml` — schema, name, license, attribution
//! - `reference.wav` — the source clip used to render (informational)
//! - `wav/<event>.wav` — one WAV per event id (build_failed, tests_passed, ...)
//! - `checksums.txt` — sha256 manifest
//!
//! `voiceforge play --pack <name> --event <id>` resolves to
//! `~/.voiceforge/packs/<name>/wav/<event>.wav` and plays it directly —
//! no TTS engine, no model load, sub-100ms warm path.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::paths;

/// Names that would collide with sibling dirs under `~/.voiceforge/`.
/// Mirrors `voices::RESERVED_NAMES`; pack names live in their own
/// `~/.voiceforge/packs/` namespace but the same hygiene applies in
/// case the layout changes.
const RESERVED_NAMES: &[&str] = &[
    "presets",
    "cache",
    "cloning",
    "voices",
    "embeddings",
    "logs",
    "packs",
];

/// Failures during pack lookup / playback.
///
/// Variants map 1:1 to process exit codes documented in `voiceforge play --help`.
/// `#[non_exhaustive]` so future variants don't break callers that match
/// exhaustively.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PlayError {
    /// Pack or event id failed `validate_pack_name` / `validate_event_id`.
    /// Process exits 2.
    #[error("invalid name: {0}")]
    InvalidName(String),

    /// `~/.voiceforge/packs/<pack>/` is not a directory (pack not installed).
    /// Process exits 2.
    #[error("pack not installed: {0:?}")]
    PackMissing(String),

    /// Pack exists but `wav/<event>.wav` is missing.
    /// Process exits 3 — callers (e.g. `voiceforge hook`) can fall back here.
    #[error("event {event:?} not in pack {pack:?}")]
    EventMissing { pack: String, event: String },

    /// WAV file present but rodio could not decode it.
    /// Process exits 4. Don't silently fall through — corruption is loud.
    #[error("wav file present but undecodable: {path}")]
    WavCorrupt {
        path: PathBuf,
        #[source]
        source: rodio::decoder::DecoderError,
    },

    /// rodio's `OutputStream::try_default` failed (no audio device, etc).
    /// Process exits 5.
    #[error("audio backend unavailable")]
    AudioBackend(#[source] rodio::StreamError),

    /// Catch-all for other I/O errors (EACCES on readdir, etc).
    /// Process exits 2 by default; dispatch may remap by context.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl PlayError {
    /// Process exit code per the documented contract.
    pub fn exit_code(&self) -> i32 {
        match self {
            PlayError::InvalidName(_) | PlayError::PackMissing(_) | PlayError::Io(_) => 2,
            PlayError::EventMissing { .. } => 3,
            PlayError::WavCorrupt { .. } => 4,
            PlayError::AudioBackend(_) => 5,
        }
    }
}

pub type Result<T> = std::result::Result<T, PlayError>;

/// Reject pack names that could path-traverse, contain shell-unsafe
/// characters, or collide with reserved sibling dirs under `~/.voiceforge/`.
///
/// Same rules as `voices::validate_name` plus an explicit `.` rejection
/// so `--pack foo.bar` and `--pack ..` both fail at parse, before any
/// FS access.
pub fn validate_pack_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(PlayError::InvalidName("pack name is empty".into()));
    }
    if name.len() > 64 {
        return Err(PlayError::InvalidName(format!(
            "pack name too long (max 64 chars): {name:?}"
        )));
    }
    if name == "." || name == ".." {
        return Err(PlayError::InvalidName(format!(
            "pack name cannot be {name:?}"
        )));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(PlayError::InvalidName(format!(
            "pack name {name:?} collides with a reserved subdir"
        )));
    }
    for c in name.chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
        if !ok {
            return Err(PlayError::InvalidName(format!(
                "pack name {name:?} has invalid char {c:?}; allowed: [a-z0-9_-], 1..=64"
            )));
        }
    }
    Ok(())
}

/// Reject event ids that could path-traverse or break the
/// `wav/<event>.wav` filename layout. Same rules as `validate_pack_name`.
///
/// In particular this rejects any `.` so `--event ../../etc/passwd` and
/// `--event foo.bar` fail at parse, before canonicalize.
pub fn validate_event_id(event: &str) -> Result<()> {
    if event.is_empty() {
        return Err(PlayError::InvalidName("event id is empty".into()));
    }
    if event.len() > 64 {
        return Err(PlayError::InvalidName(format!(
            "event id too long (max 64 chars): {event:?}"
        )));
    }
    for c in event.chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
        if !ok {
            return Err(PlayError::InvalidName(format!(
                "event id {event:?} has invalid char {c:?}; allowed: [a-z0-9_-], 1..=64"
            )));
        }
    }
    Ok(())
}

/// `~/.voiceforge/packs/` (or `$VOICEFORGE_HOME/packs/` if set).
pub fn packs_root() -> Option<PathBuf> {
    paths::user_home().map(|h| h.join("packs"))
}

/// Resolve `<packs_root>/<pack>/wav/<event>.wav` with the full
/// path-traversal posture documented in the plan:
///
/// 1. Validate names.
/// 2. Check `pack_dir.is_dir()` — distinguishes `PackMissing` (exit 2)
///    from `EventMissing` (exit 3).
/// 3. Canonicalize pack dir, assert prefix under canonical packs root.
/// 4. Check `wav/<event>.wav` exists.
/// 5. Canonicalize, assert prefix under **per-pack** `wav/` (not packs root).
///
/// TOCTOU between resolve and open is bounded by the per-pack `wav/`
/// prefix check; same trust boundary as direct write to `<pack>/wav/`.
pub fn resolve_event_wav(pack: &str, event: &str) -> Result<PathBuf> {
    validate_pack_name(pack)?;
    validate_event_id(event)?;

    let packs_root = packs_root()
        .ok_or_else(|| PlayError::InvalidName("could not resolve ~/.voiceforge/packs".into()))?;
    let pack_dir = packs_root.join(pack);
    if !pack_dir.is_dir() {
        return Err(PlayError::PackMissing(pack.to_owned()));
    }

    // Canonicalize pack dir + packs root, assert containment. Catches a
    // symlink-pivoted pack dir that points outside packs_root.
    let canonical_packs_root = packs_root.canonicalize().map_err(PlayError::from)?;
    let canonical_pack_dir = pack_dir.canonicalize().map_err(PlayError::from)?;
    if !canonical_pack_dir.starts_with(&canonical_packs_root) {
        // Don't reveal that the symlink existed; report as PackMissing.
        return Err(PlayError::PackMissing(pack.to_owned()));
    }

    let wav_path = pack_dir.join("wav").join(format!("{event}.wav"));
    if !wav_path.is_file() {
        return Err(PlayError::EventMissing {
            pack: pack.to_owned(),
            event: event.to_owned(),
        });
    }

    // Per-pack wav/ prefix check. Per rust-expert review: prefix must be
    // <canonical_pack_dir>/wav, NOT canonical_packs_root, otherwise an
    // attacker with write access to one pack can symlink into another's wav/.
    let canonical_wav = wav_path.canonicalize().map_err(PlayError::from)?;
    let canonical_pack_wav_dir = canonical_pack_dir.join("wav");
    if !canonical_wav.starts_with(&canonical_pack_wav_dir) {
        // Cross-pack symlink leak attempt; report as EventMissing.
        return Err(PlayError::EventMissing {
            pack: pack.to_owned(),
            event: event.to_owned(),
        });
    }

    Ok(canonical_wav)
}

/// List sorted event ids available in an installed pack.
///
/// Reads `<pack>/wav/`, strips `.wav`, sorts lexicographically. Same
/// canonicalize+prefix posture as `resolve_event_wav` so a symlinked
/// `<pack>/wav/` pointing elsewhere doesn't enumerate someone else's files.
pub fn list_events(pack: &str) -> Result<Vec<String>> {
    validate_pack_name(pack)?;

    let packs_root = packs_root()
        .ok_or_else(|| PlayError::InvalidName("could not resolve ~/.voiceforge/packs".into()))?;
    let pack_dir = packs_root.join(pack);
    if !pack_dir.is_dir() {
        return Err(PlayError::PackMissing(pack.to_owned()));
    }

    let canonical_packs_root = packs_root.canonicalize()?;
    let canonical_pack_dir = pack_dir.canonicalize()?;
    if !canonical_pack_dir.starts_with(&canonical_packs_root) {
        return Err(PlayError::PackMissing(pack.to_owned()));
    }

    let wav_dir = canonical_pack_dir.join("wav");
    if !wav_dir.is_dir() {
        // Pack exists but has no wav/ subdir — return empty list, not error.
        // Lets callers introspect partially-installed packs.
        return Ok(Vec::new());
    }
    let canonical_wav_dir = wav_dir.canonicalize()?;
    if !canonical_wav_dir.starts_with(&canonical_pack_dir) {
        return Err(PlayError::PackMissing(pack.to_owned()));
    }

    let mut events: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&canonical_wav_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if ext != "wav" {
            continue;
        }
        // Skip macOS resource-fork files (`._foo.wav` AppleDouble) that
        // sneak into tarballs built on Mac without COPYFILE_DISABLE=1.
        // Same as the pack-render side; surfaces as garbage in --list.
        if stem.starts_with("._") {
            continue;
        }
        events.push(stem.to_owned());
    }
    events.sort();
    Ok(events)
}

/// True iff the WAV path is under `<canonical_pack_dir>/wav`. Helper
/// for callers that have already done their own resolution and want a
/// final containment check.
#[allow(dead_code)]
pub(crate) fn assert_under_pack_wav(wav: &Path, canonical_pack_dir: &Path) -> bool {
    wav.starts_with(canonical_pack_dir.join("wav"))
}

/// Play the resolved WAV synchronously through rodio with **typed**
/// errors so the `voiceforge play` dispatch can map to the documented
/// exit codes (4 for corrupt WAV, 5 for audio backend).
///
/// Distinct from `audio::play` (which returns opaque `anyhow::Error`)
/// because callers of `voiceforge say` / `run` don't need to distinguish
/// these failure modes — they just propagate to the user. Pack playback
/// has the structured exit-code contract.
pub fn play_wav(path: &Path) -> Result<()> {
    use rodio::{Decoder, OutputStream, Sink};
    use std::fs::File;
    use std::io::BufReader;

    let (_stream, handle) = OutputStream::try_default().map_err(PlayError::AudioBackend)?;
    // Sink::try_new returns rodio::PlayError (different from our PlayError).
    // Failure here means the device was lost between try_default and try_new
    // — same exit-5 bucket as the primary backend failure.
    let sink = Sink::try_new(&handle)
        .map_err(|_| PlayError::AudioBackend(rodio::StreamError::NoDevice))?;
    let file = File::open(path)?;
    let source = Decoder::new(BufReader::new(file)).map_err(|e| PlayError::WavCorrupt {
        path: path.to_owned(),
        source: e,
    })?;
    sink.append(source);
    sink.sleep_until_end();
    Ok(())
}

// ============================================================================
// ROADMAP 6.2 — pack distribution: index fetch + install + remove + info.
//
// Distinct from PlayError because the failure modes overlap only in name
// (PackMissing here vs there). Per the rust-expert plan review, keep the
// exit-code contracts cleanly separated.
// ============================================================================

/// `voiceforge pack` install / remove / info / list_remote failure.
///
/// Exit codes are documented in `voiceforge pack {install,info,remove} --help`
/// and tested in `exit_code_mapping`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum InstallError {
    /// Failed to fetch the packs.json index over HTTPS. Network error,
    /// non-2xx response, etc. Process exits 3.
    #[error("failed to fetch pack index")]
    IndexFetch(#[source] reqwest::Error),

    /// Index URL refused at validation (e.g. http:// without
    /// VOICEFORGE_DEV=1). Process exits 3.
    #[error("invalid index URL: {0}")]
    IndexUrlRefused(String),

    /// HTTP timeout (connect or overall). Distinct from IndexFetch so
    /// the message can be specific. Process exits 3.
    #[error("HTTP timeout")]
    RequestTimeout(#[source] reqwest::Error),

    /// packs.json parse failed or schema_version != 1. Process exits 3.
    #[error("pack index parse error: {0}")]
    IndexParse(String),

    /// Pack name not present in the index. Process exits 2.
    #[error("unknown pack: {0:?}")]
    UnknownPack(String),

    /// Pack already installed and `--force` not passed. Process exits 6.
    #[error("pack {0:?} already installed; use --force to replace")]
    AlreadyInstalled(String),

    /// Concurrent install attempt — `<name>.lock.d` already exists.
    /// Process exits 6.
    #[error("another install of {0:?} is in progress")]
    LockHeld(String),

    /// Disk-space precheck failed (`available < tarball * 3`).
    /// Process exits 5.
    #[error("insufficient disk space: need ~{needed} bytes, have {available}")]
    DiskSpace { needed: u64, available: u64 },

    /// Tarball sha256 didn't match the index entry. Process exits 4.
    #[error("sha256 mismatch (expected {expected}, got {actual})")]
    Sha256Mismatch { expected: String, actual: String },

    /// Tarball entry rejected during extraction (symlink, hardlink,
    /// device, fifo, sparse, absolute path, `..` traversal).
    /// Process exits 5.
    #[error("rejected tarball entry {path}: {reason}")]
    BadEntry { path: PathBuf, reason: String },

    /// Extracted manifest.toml schema_version != 1. Process exits 5.
    /// Staging is `rm -rf`'d before this returns; no half-installed pack.
    #[error("manifest schema_version {0} unsupported (expected 1)")]
    ManifestSchema(u32),

    /// Per-file checksums.txt entry doesn't match. Process exits 5.
    #[error("checksum mismatch for {0}")]
    ChecksumMismatch(PathBuf),

    /// I/O during install (filesystem ops, etc). Process exits 5.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl InstallError {
    /// Process exit code per `voiceforge pack` documented contract.
    pub fn exit_code(&self) -> i32 {
        match self {
            InstallError::UnknownPack(_) => 2,
            InstallError::IndexFetch(_)
            | InstallError::IndexUrlRefused(_)
            | InstallError::RequestTimeout(_)
            | InstallError::IndexParse(_) => 3,
            InstallError::Sha256Mismatch { .. } => 4,
            InstallError::AlreadyInstalled(_) | InstallError::LockHeld(_) => 6,
            _ => 5,
        }
    }
}

pub type InstallResult<T> = std::result::Result<T, InstallError>;

/// pack index = packs.json (consumed by `voiceforge pack list`).
/// `#[serde(deny_unknown_fields)]` so unknown future fields fail fast on
/// old voiceforge binaries — we'd rather error than silently miss data.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackIndex {
    pub schema_version: u32,
    pub index_url: String,
    pub license_audio: String,
    pub takedown_url: String,
    pub packs: std::collections::BTreeMap<String, PackEntry>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackEntry {
    pub display_name: String,
    pub description: String,
    pub voice_source: String,
    pub source_clip_url: String,
    pub manifest_url: String,
    pub tarball_url: String,
    pub tarball_sha256: String,
    pub tarball_size_bytes: u64,
    pub version: String,
    pub tier: String,
    pub phrases: u32,
    pub sample_rate: u32,
    pub rendered_with: String,
    pub license: String,
    pub status: String,
}

/// Per-pack manifest.toml inside the tarball.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // fields decoded for `voiceforge pack info` future expansion
pub struct PackManifest {
    pub schema_version: u32,
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub rendered_with: String,
    pub rendered_at: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub phrases: u32,
    pub voice_source: String,
    pub source_clip_url: String,
    pub source_clip_episode: String,
    pub source_clip_duration: f64,
    pub reference_prompt_text: String,
    pub license: String,
    pub takedown_url: String,
    #[serde(default, rename = "phrase_text")]
    pub phrases_table: std::collections::BTreeMap<String, String>,
}

/// Streaming sha256: writes pass through to the inner writer AND update
/// the running hash. One pass over the bytes — no re-read of the file.
///
/// Implements `tokio::io::AsyncWrite` so it composes with
/// `tokio::io::copy_buf` and `tokio_util::io::StreamReader` over a
/// `reqwest::Response::bytes_stream()`.
pub struct HashingWriter<W> {
    inner: W,
    hasher: sha2::Sha256,
}

impl<W> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        use sha2::Digest;
        Self {
            inner,
            hasher: sha2::Sha256::new(),
        }
    }

    /// Consume the writer and return the lowercase hex digest.
    pub fn finalize(self) -> (W, String) {
        use sha2::Digest;
        let digest = self.hasher.finalize();
        (self.inner, hex::encode(digest))
    }
}

impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for HashingWriter<W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use sha2::Digest;
        // Update hash with whatever the inner writer accepts. If it
        // returns Ok(n < buf.len()), we hashed the right prefix; the
        // caller will re-call poll_write with the remainder.
        let inner = std::pin::Pin::new(&mut self.inner);
        match inner.poll_write(cx, buf) {
            std::task::Poll::Ready(Ok(n)) => {
                self.hasher.update(&buf[..n]);
                std::task::Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// RAII guard for the per-pack install lock dir (`<packs_root>/.<name>.lock.d`).
///
/// `mkdir` is atomic on POSIX and Windows. Acquire returns `LockHeld` if
/// the dir already exists. Drop logs-and-swallows on cleanup failure
/// (panicking in Drop during unwind aborts the process).
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    /// Acquire the lock. Returns `LockHeld(name)` if another install is
    /// already in progress.
    pub fn acquire(packs_root: &Path, name: &str) -> InstallResult<Self> {
        let path = packs_root.join(format!(".{name}.lock.d"));
        match std::fs::create_dir(&path) {
            Ok(()) => Ok(Self { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(InstallError::LockHeld(name.to_owned()))
            }
            Err(e) => Err(InstallError::Io(e)),
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            // We're in Drop — must not panic. Log to stderr and move on.
            // The next `install --force` will retry the cleanup as part
            // of step 1 (pre-cleanup of stale .lock.d / .partial / .old).
            eprintln!(
                "voiceforge: failed to release pack lock at {}: {} (next install will mop up)",
                self.path.display(),
                e
            );
        }
    }
}

/// Default index URL — points at the canonical voice-forge-packs repo's
/// `packs.json` on the main branch. Override via `VOICEFORGE_PACK_INDEX_URL`
/// env var (must be `https://` unless `VOICEFORGE_DEV=1`).
pub const DEFAULT_INDEX_URL: &str =
    "https://raw.githubusercontent.com/humancto/voice-forge-packs/main/packs.json";

/// Resolve the effective index URL with the env override + scheme guard.
fn resolve_index_url() -> InstallResult<String> {
    let url =
        std::env::var("VOICEFORGE_PACK_INDEX_URL").unwrap_or_else(|_| DEFAULT_INDEX_URL.to_owned());
    let dev = std::env::var("VOICEFORGE_DEV").is_ok();
    // Refuse plain HTTP and file:// unless dev mode. Per rust-expert
    // plan review: defense-in-depth — even if a tarball signature
    // were valid, an MITM on plain-HTTP could swap to a different
    // index and trick us into installing a pack we did not vet.
    if !dev && !url.starts_with("https://") {
        return Err(InstallError::IndexUrlRefused(format!(
            "non-https index URL refused (set VOICEFORGE_DEV=1 to override): {url}"
        )));
    }
    Ok(url)
}

/// Build the `reqwest::Client` we use for index + tarball fetches.
/// User-Agent for log forensics; explicit timeouts so a hung TCP
/// connection doesn't wedge the CLI.
fn http_client() -> InstallResult<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("voiceforge/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(InstallError::IndexFetch)
}

/// Fetch and parse `packs.json`. Async — caller is responsible for the
/// runtime context (always `tokio::main` in the CLI dispatch).
pub async fn fetch_index() -> InstallResult<PackIndex> {
    let url = resolve_index_url()?;
    let client = http_client()?;
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => return Err(InstallError::RequestTimeout(e)),
        Err(e) => return Err(InstallError::IndexFetch(e)),
    };
    let resp = resp.error_for_status().map_err(InstallError::IndexFetch)?;
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) if e.is_timeout() => return Err(InstallError::RequestTimeout(e)),
        Err(e) => return Err(InstallError::IndexFetch(e)),
    };
    parse_index(&body)
}

/// Parse the index body. Separated from `fetch_index` so unit tests can
/// exercise it without a network. `schema_version != 1` is rejected here.
pub fn parse_index(body: &str) -> InstallResult<PackIndex> {
    let idx: PackIndex = serde_json::from_str(body)
        .map_err(|e| InstallError::IndexParse(format!("packs.json parse: {e}")))?;
    if idx.schema_version != 1 {
        return Err(InstallError::IndexParse(format!(
            "packs.json schema_version {} unsupported (this voiceforge expects 1; upgrade voiceforge)",
            idx.schema_version
        )));
    }
    Ok(idx)
}

/// List installed packs by name. Skips internal/staging dirs:
/// `.<name>.partial`, `.<name>.partial.tar.gz`, `.<name>.lock.d`,
/// `<name>.old`. Same hygiene as `voices::list_cloned_voices`.
pub fn list_installed() -> std::io::Result<Vec<String>> {
    let Some(root) = packs_root() else {
        return Ok(Vec::new());
    };
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // Skip: leading dot (.partial, .lock.d, hidden), trailing .old.
        if name.starts_with('.') {
            continue;
        }
        if name.ends_with(".old") {
            continue;
        }
        // Skip anything that would fail validate_pack_name — these
        // are not pack dirs we own.
        if validate_pack_name(&name).is_err() {
            continue;
        }
        out.push(name);
    }
    out.sort();
    Ok(out)
}

/// Whether `<packs_root>/<name>/manifest.toml` exists. Cheap check
/// used by `voiceforge say --voice <name>` dispatch to decide whether
/// the named "voice" is actually an installed pack — distinct from a
/// cloned voice or a built-in preset.
pub fn pack_is_installed(name: &str) -> bool {
    if validate_pack_name(name).is_err() {
        return false;
    }
    let Some(root) = packs_root() else {
        return false;
    };
    root.join(name).join("manifest.toml").is_file()
}

/// Resolve a `voiceforge say --voice <pack> --text <text>` request to
/// a specific event id within the pack, if the text matches one of
/// the pack's pre-rendered phrases. Match precedence:
///
///   1. `text == event_id` exact (e.g. `--text "build_failed"`).
///   2. `text == phrase_text` case-insensitive trimmed.
///   3. `text.contains(phrase_text)` or `phrase_text.contains(text)`
///      case-insensitive — the "did you mean" tier.
///
/// Returns `Ok(Some(event_id))` on hit, `Ok(None)` when the pack is
/// installed but no phrase matches, `Err(...)` if the manifest
/// can't be loaded.
///
/// The third tier is intentionally generous: users will type "the
/// build failed" not "build_failed", and a fuzzy match feels right
/// when the pack lists exactly one phrase per event id.
pub fn resolve_text_to_event(pack: &str, text: &str) -> InstallResult<Option<String>> {
    let manifest = pack_info_local(pack)?;
    let needle = text.trim();
    let needle_lower = needle.to_lowercase();

    // 1. exact event_id match.
    if manifest.phrases_table.contains_key(needle) {
        return Ok(Some(needle.to_owned()));
    }

    // 2. exact phrase_text match (case-insensitive trimmed).
    for (event, phrase) in &manifest.phrases_table {
        if phrase.trim().to_lowercase() == needle_lower {
            return Ok(Some(event.clone()));
        }
    }

    // 3. fuzzy contains, but only if needle is non-trivial (>=4 chars
    //    and >=2 words, so "test" alone doesn't fuzzily match every
    //    phrase that contains "test").
    let word_count = needle.split_whitespace().count();
    if needle.len() >= 4 && word_count >= 2 {
        for (event, phrase) in &manifest.phrases_table {
            let p = phrase.trim().to_lowercase();
            if p.contains(&needle_lower) || needle_lower.contains(&p) {
                return Ok(Some(event.clone()));
            }
        }
    }

    Ok(None)
}

/// Read the manifest.toml of an installed pack. Errors if pack dir
/// missing or manifest unparseable.
pub fn pack_info_local(name: &str) -> InstallResult<PackManifest> {
    validate_pack_name(name).map_err(|e| match e {
        PlayError::InvalidName(msg) => InstallError::IndexParse(msg),
        // validate_pack_name only returns InvalidName at construction.
        _ => InstallError::IndexParse(format!("invalid pack name: {name}")),
    })?;
    let Some(root) = packs_root() else {
        return Err(InstallError::IndexParse(
            "could not resolve ~/.voiceforge/packs".into(),
        ));
    };
    let dir = root.join(name);
    if !dir.is_dir() {
        return Err(InstallError::UnknownPack(name.to_owned()));
    }
    let manifest_path = dir.join("manifest.toml");
    let body = std::fs::read_to_string(&manifest_path)?;
    let manifest: PackManifest = toml::from_str(&body)
        .map_err(|e| InstallError::IndexParse(format!("manifest.toml parse: {e}")))?;
    if manifest.schema_version != 1 {
        return Err(InstallError::ManifestSchema(manifest.schema_version));
    }
    Ok(manifest)
}

/// Remove an installed pack with the same canonicalize+prefix posture
/// as `voices::remove_cloned_voice`. Idempotent failure semantics:
/// returns `UnknownPack` if not installed.
pub fn remove_pack(name: &str) -> InstallResult<()> {
    validate_pack_name(name).map_err(|_| InstallError::UnknownPack(name.to_owned()))?;
    let Some(root) = packs_root() else {
        return Err(InstallError::UnknownPack(name.to_owned()));
    };
    let dir = root.join(name);
    if !dir.is_dir() {
        return Err(InstallError::UnknownPack(name.to_owned()));
    }
    // Defense: canonicalize + assert under packs root.
    let canonical_root = root.canonicalize()?;
    let canonical_dir = dir.canonicalize()?;
    if !canonical_dir.starts_with(&canonical_root) {
        // Symlink escape — refuse to remove anything.
        return Err(InstallError::UnknownPack(name.to_owned()));
    }
    std::fs::remove_dir_all(&canonical_dir)?;
    Ok(())
}

/// Install a pack: fetch index → match `name` → download tarball with
/// streaming sha256 → verify → extract with safe-extract validation →
/// validate manifest schema → atomic swap into `~/.voiceforge/packs/<name>/`.
///
/// See `.planning/voiceforge-pack-subcommand.plan.md` §"Atomic install
/// (v2)" for the full 14-step contract this implements. Each step has
/// a corresponding inline comment below.
pub async fn install_pack(name: &str, force: bool) -> InstallResult<PackEntry> {
    validate_pack_name(name).map_err(|_| InstallError::UnknownPack(name.to_owned()))?;

    // Step 0a: resolve index, find pack entry.
    let index = fetch_index().await?;
    let entry = index
        .packs
        .get(name)
        .cloned()
        .ok_or_else(|| InstallError::UnknownPack(name.to_owned()))?;

    let Some(packs_root) = packs_root() else {
        return Err(InstallError::IndexParse(
            "could not resolve ~/.voiceforge/packs".into(),
        ));
    };
    std::fs::create_dir_all(&packs_root)?;

    // Step 0b: per-pack lock.
    let _lock = LockGuard::acquire(&packs_root, name)?;

    // Step 1: pre-cleanup stale .partial / .old / .partial.tar.gz from
    // a prior crashed install. The lock dir guarantees no concurrent
    // process is mid-install of this pack right now.
    let partial_dir = packs_root.join(format!(".{name}.partial"));
    let partial_tarball = packs_root.join(format!(".{name}.partial.tar.gz"));
    let old_dir = packs_root.join(format!("{name}.old"));
    if partial_dir.exists() {
        std::fs::remove_dir_all(&partial_dir)?;
    }
    if partial_tarball.exists() {
        std::fs::remove_file(&partial_tarball)?;
    }
    if old_dir.exists() {
        std::fs::remove_dir_all(&old_dir)?;
    }

    // Step 2: AlreadyInstalled gate (before disk-space precheck so
    // we don't pessimistically check space when we'd refuse anyway).
    let final_dir = packs_root.join(name);
    if final_dir.exists() && !force {
        return Err(InstallError::AlreadyInstalled(name.to_owned()));
    }

    // Step 3: disk-space precheck. Need ~3x the tarball: download +
    // extracted + slack for filesystem overhead.
    let needed = entry.tarball_size_bytes.saturating_mul(3);
    let available = fs2::available_space(&packs_root).unwrap_or(u64::MAX);
    if available < needed {
        return Err(InstallError::DiskSpace { needed, available });
    }

    // Step 4: streaming download with single-pass sha256 via HashingWriter.
    download_to_file(&entry.tarball_url, &partial_tarball, &entry.tarball_sha256).await?;

    // Step 5: extract in spawn_blocking (tar + flate2 are sync).
    // Validation of every entry happens inside extract_tarball.
    let staging_clone = partial_dir.clone();
    let tarball_clone = partial_tarball.clone();
    tokio::task::spawn_blocking(move || extract_tarball(&tarball_clone, &staging_clone))
        .await
        .map_err(|join_err| {
            // JoinError from the blocking pool — stringify to Io.
            InstallError::Io(std::io::Error::other(format!(
                "extract task join error: {join_err}"
            )))
        })??;

    // Step 6: validate the extracted manifest BEFORE the atomic swap.
    // If it fails, rm -rf staging — the prior install (if any) survives.
    let manifest_path = partial_dir.join("manifest.toml");
    if !manifest_path.is_file() {
        let _ = std::fs::remove_dir_all(&partial_dir);
        return Err(InstallError::BadEntry {
            path: "manifest.toml".into(),
            reason: "missing from tarball root".into(),
        });
    }
    let manifest_body = std::fs::read_to_string(&manifest_path)?;
    let manifest: PackManifest = match toml::from_str(&manifest_body) {
        Ok(m) => m,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&partial_dir);
            return Err(InstallError::IndexParse(format!("manifest.toml: {e}")));
        }
    };
    if manifest.schema_version != 1 {
        let v = manifest.schema_version;
        let _ = std::fs::remove_dir_all(&partial_dir);
        return Err(InstallError::ManifestSchema(v));
    }

    // Step 7: per-file checksums.txt verification (parsed in Rust).
    let checksums_path = partial_dir.join("checksums.txt");
    if checksums_path.is_file() {
        if let Err(e) = verify_checksums_file(&partial_dir, &checksums_path) {
            let _ = std::fs::remove_dir_all(&partial_dir);
            return Err(e);
        }
    }
    // checksums.txt is optional in v1 — some early packs may ship without
    // it. The tarball-level sha256 (step 4) still gates correctness.

    // Step 8: rename existing install to .old (only reached if --force
    // because the AlreadyInstalled gate above would have errored).
    if final_dir.exists() {
        std::fs::rename(&final_dir, &old_dir)?;
    }

    // Step 9: atomic rename staging → final.
    if let Err(e) = std::fs::rename(&partial_dir, &final_dir) {
        // Roll back the .old swap if the rename fails.
        if old_dir.exists() {
            let _ = std::fs::rename(&old_dir, &final_dir);
        }
        return Err(InstallError::Io(e));
    }

    // Step 10: best-effort cleanup. Failures here are non-fatal —
    // next `install --force` will mop up via step 1.
    if old_dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&old_dir) {
            eprintln!(
                "voiceforge: failed to clean up {}: {} (will be retried on next install)",
                old_dir.display(),
                e
            );
        }
    }
    if let Err(e) = std::fs::remove_file(&partial_tarball) {
        eprintln!(
            "voiceforge: failed to clean up {}: {}",
            partial_tarball.display(),
            e
        );
    }

    Ok(entry)
}

/// Stream the tarball from `url` into `dest`, computing sha256 in a
/// single pass. Verify against `expected_sha256` (lowercase hex).
/// On mismatch, the partial file is deleted and `Sha256Mismatch` is
/// returned.
async fn download_to_file(url: &str, dest: &Path, expected_sha256: &str) -> InstallResult<()> {
    let client = http_client()?;
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => return Err(InstallError::RequestTimeout(e)),
        Err(e) => return Err(InstallError::IndexFetch(e)),
    };
    let resp = resp.error_for_status().map_err(InstallError::IndexFetch)?;

    // Bridge the reqwest body's `Stream<Item = Result<Bytes>>` to the
    // tokio AsyncRead world via tokio_util::io::StreamReader, then
    // copy_buf into our HashingWriter (which wraps a tokio File).
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let stream = resp.bytes_stream();
    let stream = stream.map(|chunk| chunk.map_err(std::io::Error::other));
    let mut reader = tokio_util::io::StreamReader::new(stream);

    let file = tokio::fs::File::create(dest).await?;
    let mut hashing = HashingWriter::new(file);
    let _bytes_copied = tokio::io::copy(&mut reader, &mut hashing).await?;
    hashing.flush().await?;
    let (file_back, actual_hex) = hashing.finalize();
    drop(file_back);

    if !actual_hex.eq_ignore_ascii_case(expected_sha256) {
        // Don't leave a corrupt tarball on disk.
        let _ = std::fs::remove_file(dest);
        return Err(InstallError::Sha256Mismatch {
            expected: expected_sha256.to_owned(),
            actual: actual_hex,
        });
    }
    Ok(())
}

/// Extract a gzipped tarball into `dest`. Per-entry validation:
///   * Reject any non-Regular / non-Directory entry type.
///   * Reject paths that are absolute, contain `..`, or have drive
///     prefixes / root anchors.
///   * Strip preserved permissions and mtime so we don't get surprise
///     +x or backdated files.
///
/// `unpack_in` provides a final containment check (since tar 0.4.40+);
/// we keep our own pre-check for clearer errors.
fn extract_tarball(tarball: &Path, dest: &Path) -> InstallResult<()> {
    use std::io::BufReader;
    std::fs::create_dir_all(dest)?;
    let f = std::fs::File::open(tarball)?;
    let gz = flate2::read::GzDecoder::new(BufReader::new(f));
    let mut ar = tar::Archive::new(gz);
    ar.set_preserve_permissions(false);
    ar.set_preserve_mtime(false);

    for entry in ar.entries()? {
        let mut entry = entry?;
        let entry_type = entry.header().entry_type();
        // Allow only Regular files and Directories. Reject everything
        // else — symlinks, hardlinks, devices, fifos, sparse, GNU
        // extensions are all known tar-extract attack vectors.
        let is_safe = matches!(
            entry_type,
            tar::EntryType::Regular
                | tar::EntryType::Directory
                | tar::EntryType::XHeader
                | tar::EntryType::XGlobalHeader
        );
        let path_for_err = entry.path().map(|p| p.into_owned()).unwrap_or_default();
        if !is_safe {
            return Err(InstallError::BadEntry {
                path: path_for_err,
                reason: format!("disallowed entry type: {entry_type:?}"),
            });
        }
        // Reject absolute paths, parent-dir traversal, drive prefixes,
        // root anchors. unpack_in catches escapes too but explicit is
        // better for the error message.
        let path = entry.path()?.into_owned();
        for component in path.components() {
            use std::path::Component;
            match component {
                Component::Normal(_) | Component::CurDir => {}
                _ => {
                    return Err(InstallError::BadEntry {
                        path: path.clone(),
                        reason: format!("disallowed path component: {component:?}"),
                    });
                }
            }
        }
        entry.set_preserve_permissions(false);
        entry.set_preserve_mtime(false);
        if let Err(e) = entry.unpack_in(dest) {
            // unpack_in's own containment check tripped — surface as
            // BadEntry rather than generic Io for a clearer error.
            return Err(InstallError::BadEntry {
                path,
                reason: format!("unpack_in: {e}"),
            });
        }
    }
    Ok(())
}

/// Parse `checksums.txt` (`<hex>  <relpath>` per line; sha256sum -c
/// format) and verify each file under `pack_dir` matches.
fn verify_checksums_file(pack_dir: &Path, checksums: &Path) -> InstallResult<()> {
    use sha2::Digest;
    let body = std::fs::read_to_string(checksums)?;
    for (lineno, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // shasum / sha256sum format: "<hex>  <path>" (two spaces) or
        // "<hex> *<path>" (binary mode). Accept both.
        let (hex, path) = match line.split_once("  ") {
            Some(t) => t,
            None => match line.split_once(" *") {
                Some(t) => t,
                None => {
                    return Err(InstallError::BadEntry {
                        path: format!("line {}", lineno + 1).into(),
                        reason: "checksums.txt format unrecognized".into(),
                    });
                }
            },
        };
        let hex = hex.trim();
        let relpath = std::path::PathBuf::from(path.trim());
        // Reject absolute / `..` paths in checksums.txt — same posture
        // as tar entries.
        if relpath.is_absolute() {
            return Err(InstallError::BadEntry {
                path: relpath,
                reason: "checksums.txt absolute path".into(),
            });
        }
        if relpath
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(InstallError::BadEntry {
                path: relpath,
                reason: "checksums.txt contains parent-dir traversal".into(),
            });
        }

        let target = pack_dir.join(&relpath);
        let bytes = std::fs::read(&target)?;
        let mut h = sha2::Sha256::new();
        h.update(&bytes);
        let actual = hex::encode(h.finalize());
        if !actual.eq_ignore_ascii_case(hex) {
            return Err(InstallError::ChecksumMismatch(relpath));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    fn setup_tmp_home() -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::env::set_var("VOICEFORGE_HOME", tmp.path());
        tmp
    }

    fn make_pack(home: &Path, pack: &str, events: &[&str]) {
        let pack_dir = home.join("packs").join(pack);
        let wav_dir = pack_dir.join("wav");
        fs::create_dir_all(&wav_dir).unwrap();
        for ev in events {
            // Tiny valid WAV: write 1 sample of silence at 32k mono 16-bit.
            let path = wav_dir.join(format!("{ev}.wav"));
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 32_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::create(&path, spec).unwrap();
            writer.write_sample(0i16).unwrap();
            writer.finalize().unwrap();
        }
    }

    // ---- validate_pack_name ----

    #[test]
    fn validate_pack_name_accepts_simple() {
        validate_pack_name("peter").unwrap();
        validate_pack_name("obama_2").unwrap();
        validate_pack_name("bob-ross").unwrap();
        validate_pack_name("a").unwrap();
        validate_pack_name(&"a".repeat(64)).unwrap();
    }

    #[test]
    fn validate_pack_name_rejects_bad() {
        assert!(matches!(
            validate_pack_name(""),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_pack_name(&"a".repeat(65)),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_pack_name("Foo"),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_pack_name("peter/x"),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_pack_name(".."),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_pack_name("."),
            Err(PlayError::InvalidName(_))
        ));
        // Reject `.` so `peter.wav` fails before any FS access.
        assert!(matches!(
            validate_pack_name("peter.wav"),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_pack_name("voices"),
            Err(PlayError::InvalidName(_))
        ));
    }

    // ---- validate_event_id ----

    #[test]
    fn validate_event_id_accepts_simple() {
        validate_event_id("build_success").unwrap();
        validate_event_id("tests_passed").unwrap();
        validate_event_id("a").unwrap();
    }

    #[test]
    fn validate_event_id_rejects_bad() {
        assert!(matches!(
            validate_event_id(""),
            Err(PlayError::InvalidName(_))
        ));
        // Reject `.` so `../../etc/passwd` and `foo.bar` fail at parse.
        assert!(matches!(
            validate_event_id("../../etc/passwd"),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_event_id("foo.bar"),
            Err(PlayError::InvalidName(_))
        ));
        assert!(matches!(
            validate_event_id("BUILD_SUCCESS"),
            Err(PlayError::InvalidName(_))
        ));
    }

    // ---- resolve_event_wav ----

    #[test]
    #[serial]
    fn resolve_event_wav_happy_path() {
        let tmp = setup_tmp_home();
        make_pack(tmp.path(), "peter", &["tests_passed"]);
        let resolved = resolve_event_wav("peter", "tests_passed").unwrap();
        assert!(resolved.is_file());
        assert!(resolved.ends_with("packs/peter/wav/tests_passed.wav"));
    }

    #[test]
    #[serial]
    fn resolve_event_wav_event_missing() {
        let tmp = setup_tmp_home();
        make_pack(tmp.path(), "peter", &["build_success"]);
        let err = resolve_event_wav("peter", "doesnt_exist").unwrap_err();
        assert!(matches!(err, PlayError::EventMissing { .. }));
        assert_eq!(err.exit_code(), 3);
    }

    #[test]
    #[serial]
    fn resolve_event_wav_pack_missing() {
        let _tmp = setup_tmp_home();
        let err = resolve_event_wav("nonexistent", "x").unwrap_err();
        assert!(matches!(err, PlayError::PackMissing(_)));
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    #[serial]
    fn resolve_event_wav_invalid_name_short_circuits() {
        let _tmp = setup_tmp_home();
        // No FS access happens — name validation fires first.
        let err = resolve_event_wav("peter", "../../etc/passwd").unwrap_err();
        assert!(matches!(err, PlayError::InvalidName(_)));
        assert_eq!(err.exit_code(), 2);
    }

    /// Cross-pack symlink leak: `peter/wav/escape.wav -> other/wav/build_success.wav`.
    /// resolve_event_wav("peter", "escape") MUST report EventMissing
    /// (per-pack wav/ prefix check), not silently leak the other pack's WAV.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn resolve_event_wav_blocks_cross_pack_symlink_leak() {
        let tmp = setup_tmp_home();
        make_pack(tmp.path(), "other", &["build_success"]);
        // Create peter pack with NO real wavs, just a symlink escape.
        let peter_wav_dir = tmp.path().join("packs/peter/wav");
        fs::create_dir_all(&peter_wav_dir).unwrap();
        let target = tmp
            .path()
            .canonicalize()
            .unwrap()
            .join("packs/other/wav/build_success.wav");
        std::os::unix::fs::symlink(&target, peter_wav_dir.join("escape.wav")).unwrap();
        let err = resolve_event_wav("peter", "escape").unwrap_err();
        // Per-pack wav/ prefix check rejects this.
        assert!(matches!(err, PlayError::EventMissing { .. }));
    }

    // ---- list_events ----

    #[test]
    #[serial]
    fn list_events_sorted() {
        let tmp = setup_tmp_home();
        make_pack(
            tmp.path(),
            "peter",
            &["tests_passed", "build_failed", "agent_done"],
        );
        let events = list_events("peter").unwrap();
        assert_eq!(events, vec!["agent_done", "build_failed", "tests_passed"]);
    }

    #[test]
    #[serial]
    fn list_events_empty_pack() {
        let tmp = setup_tmp_home();
        let pack_dir = tmp.path().join("packs/peter");
        fs::create_dir_all(&pack_dir).unwrap();
        // No wav/ subdir at all → empty list, no error.
        let events = list_events("peter").unwrap();
        assert!(events.is_empty());
    }

    #[test]
    #[serial]
    fn list_events_pack_missing() {
        let _tmp = setup_tmp_home();
        let err = list_events("nonexistent").unwrap_err();
        assert!(matches!(err, PlayError::PackMissing(_)));
    }

    // ---- exit_code ----

    #[test]
    fn exit_code_mapping() {
        assert_eq!(PlayError::InvalidName("x".into()).exit_code(), 2);
        assert_eq!(PlayError::PackMissing("x".into()).exit_code(), 2);
        assert_eq!(
            PlayError::EventMissing {
                pack: "x".into(),
                event: "y".into()
            }
            .exit_code(),
            3
        );
    }

    // ---- ROADMAP 6.2: install / index / lock guard / hashing writer ----

    #[test]
    fn install_error_exit_codes() {
        // 2: pack not in index
        assert_eq!(InstallError::UnknownPack("x".into()).exit_code(), 2);
        // 3: anything network/index-parse related
        assert_eq!(
            InstallError::IndexUrlRefused("http://x".into()).exit_code(),
            3
        );
        assert_eq!(InstallError::IndexParse("x".into()).exit_code(), 3);
        // 4: tarball hash mismatch
        assert_eq!(
            InstallError::Sha256Mismatch {
                expected: "a".into(),
                actual: "b".into()
            }
            .exit_code(),
            4
        );
        // 5: extraction-time failures
        assert_eq!(
            InstallError::BadEntry {
                path: "x".into(),
                reason: "y".into()
            }
            .exit_code(),
            5
        );
        assert_eq!(InstallError::ManifestSchema(99).exit_code(), 5);
        assert_eq!(InstallError::ChecksumMismatch("x".into()).exit_code(), 5);
        assert_eq!(
            InstallError::DiskSpace {
                needed: 100,
                available: 50
            }
            .exit_code(),
            5
        );
        // 6: replace-required
        assert_eq!(
            InstallError::AlreadyInstalled("peter".into()).exit_code(),
            6
        );
        assert_eq!(InstallError::LockHeld("peter".into()).exit_code(), 6);
    }

    #[test]
    fn pack_index_round_trip() {
        let json = r#"{
            "schema_version": 1,
            "index_url": "https://raw.githubusercontent.com/humancto/voice-forge-packs/main/packs.json",
            "license_audio": "https://github.com/humancto/voice-forge-packs/blob/main/LICENSE-AUDIO.md",
            "takedown_url": "https://github.com/humancto/voice-forge-packs/issues/new",
            "packs": {
                "peter": {
                    "display_name": "Peter Griffin",
                    "description": "Peter Griffin from Family Guy",
                    "voice_source": "Peter Griffin (Family Guy, Fox)",
                    "source_clip_url": "https://www.youtube.com/watch?v=T2w5SQ0L65I",
                    "manifest_url": "https://example.com/manifest.toml",
                    "tarball_url": "https://example.com/peter.tar.gz",
                    "tarball_sha256": "3ed1f85fde8b3621548c2bae918310748ba8e753718163e8cb2a177bd3a47301",
                    "tarball_size_bytes": 7864320,
                    "version": "0.1.0",
                    "tier": "character",
                    "phrases": 13,
                    "sample_rate": 44100,
                    "rendered_with": "fishaudio/fish-speech S2 Pro",
                    "license": "Educational / research / local-testing use only",
                    "status": "shipping"
                }
            }
        }"#;
        let idx: PackIndex = serde_json::from_str(json).unwrap();
        assert_eq!(idx.schema_version, 1);
        assert_eq!(idx.packs.len(), 1);
        let peter = &idx.packs["peter"];
        assert_eq!(peter.tarball_size_bytes, 7864320);
        assert_eq!(peter.phrases, 13);
        assert_eq!(peter.tier, "character");
    }

    #[test]
    fn pack_index_rejects_unknown_field() {
        // deny_unknown_fields means a future field added upstream would
        // fail fast on old voiceforge binaries — that's the contract.
        let json = r#"{
            "schema_version": 1,
            "index_url": "x",
            "license_audio": "x",
            "takedown_url": "x",
            "packs": {},
            "wat_is_this": "future field"
        }"#;
        // `Result` is shadowed by our `pub type Result<T> = ...<T, PlayError>;`
        // alias at module scope, so use the fully-qualified std type here.
        let result: std::result::Result<PackIndex, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // ---- HashingWriter ----

    #[tokio::test]
    async fn hashing_writer_matches_one_shot_sha256() {
        use sha2::Digest;
        use tokio::io::AsyncWriteExt;

        let payload = b"the quick brown fox jumps over the lazy dog";

        let mut sink: Vec<u8> = Vec::new();
        let mut hw = HashingWriter::new(&mut sink);
        hw.write_all(payload).await.unwrap();
        hw.flush().await.unwrap();
        let (_inner, hex_digest) = hw.finalize();

        let mut reference = sha2::Sha256::new();
        reference.update(payload);
        let want = hex::encode(reference.finalize());

        assert_eq!(hex_digest, want);
        assert_eq!(sink, payload);
    }

    #[tokio::test]
    async fn hashing_writer_chunked_writes_match_one_shot() {
        // Many small writes must hash the same as one big write.
        use sha2::Digest;
        use tokio::io::AsyncWriteExt;

        let payload: Vec<u8> = (0u8..=255).cycle().take(8192).collect();

        let mut sink: Vec<u8> = Vec::new();
        let mut hw = HashingWriter::new(&mut sink);
        for chunk in payload.chunks(7) {
            hw.write_all(chunk).await.unwrap();
        }
        hw.flush().await.unwrap();
        let (_inner, hex_digest) = hw.finalize();

        let mut reference = sha2::Sha256::new();
        reference.update(&payload);
        let want = hex::encode(reference.finalize());

        assert_eq!(hex_digest, want);
        assert_eq!(sink.len(), payload.len());
    }

    // ---- LockGuard ----

    #[test]
    #[serial]
    fn lock_guard_blocks_concurrent_acquire() {
        let tmp = TempDir::new().unwrap();
        let packs_root = tmp.path().to_owned();
        // First acquire succeeds.
        let _guard = LockGuard::acquire(&packs_root, "peter").unwrap();
        // Second acquire while the first is held → LockHeld.
        let err = LockGuard::acquire(&packs_root, "peter").unwrap_err();
        assert!(matches!(err, InstallError::LockHeld(_)));
        // Different pack → independent lock, succeeds.
        let _other = LockGuard::acquire(&packs_root, "obama").unwrap();
    }

    #[test]
    #[serial]
    fn lock_guard_drop_releases() {
        let tmp = TempDir::new().unwrap();
        let packs_root = tmp.path().to_owned();
        {
            let _guard = LockGuard::acquire(&packs_root, "peter").unwrap();
            assert!(packs_root.join(".peter.lock.d").is_dir());
        }
        // Lock dir is gone after the guard drops.
        assert!(!packs_root.join(".peter.lock.d").exists());
        // Re-acquire is fine.
        let _again = LockGuard::acquire(&packs_root, "peter").unwrap();
    }

    // ---- parse_index + index URL guards ----

    #[test]
    #[serial]
    fn parse_index_rejects_wrong_schema_version() {
        let json = r#"{
            "schema_version": 99,
            "index_url": "x",
            "license_audio": "x",
            "takedown_url": "x",
            "packs": {}
        }"#;
        let err = parse_index(json).unwrap_err();
        assert!(matches!(err, InstallError::IndexParse(_)));
    }

    #[test]
    #[serial]
    fn resolve_index_url_refuses_http_in_prod_mode() {
        std::env::set_var(
            "VOICEFORGE_PACK_INDEX_URL",
            "http://attacker.example/packs.json",
        );
        std::env::remove_var("VOICEFORGE_DEV");
        let err = resolve_index_url().unwrap_err();
        assert!(matches!(err, InstallError::IndexUrlRefused(_)));
        std::env::remove_var("VOICEFORGE_PACK_INDEX_URL");
    }

    #[test]
    #[serial]
    fn resolve_index_url_allows_http_in_dev_mode() {
        std::env::set_var(
            "VOICEFORGE_PACK_INDEX_URL",
            "http://localhost:8080/packs.json",
        );
        std::env::set_var("VOICEFORGE_DEV", "1");
        let url = resolve_index_url().unwrap();
        assert!(url.starts_with("http://localhost"));
        std::env::remove_var("VOICEFORGE_PACK_INDEX_URL");
        std::env::remove_var("VOICEFORGE_DEV");
    }

    // ---- list_installed: skip staging dirs ----

    #[test]
    #[serial]
    fn list_installed_skips_staging_dirs() {
        let tmp = TempDir::new().unwrap();
        std::env::set_var("VOICEFORGE_HOME", tmp.path());
        let root = tmp.path().join("packs");
        fs::create_dir_all(root.join("peter/wav")).unwrap();
        fs::create_dir_all(root.join("obama/wav")).unwrap();
        fs::create_dir_all(root.join(".trump.partial")).unwrap();
        fs::create_dir_all(root.join(".trump.lock.d")).unwrap();
        fs::create_dir_all(root.join("stewie.old")).unwrap();
        fs::write(root.join(".bender.partial.tar.gz"), b"junk").unwrap();
        fs::create_dir_all(root.join("INVALID-NAME-WITH-CAPS")).unwrap();

        let installed = list_installed().unwrap();
        assert_eq!(installed, vec!["obama".to_string(), "peter".to_string()]);
    }

    // ---- extract_tarball: per-attack-vector rejection ----

    fn build_tarball(entries: Vec<(tar::Header, Vec<u8>)>) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
            let mut tar = tar::Builder::new(gz);
            for (mut header, data) in entries {
                header.set_size(data.len() as u64);
                header.set_cksum();
                tar.append(&header, data.as_slice()).unwrap();
            }
            tar.finish().unwrap();
        }
        buf
    }

    fn header_for(path: &str, size: u64, ty: tar::EntryType) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        h.set_path(path).unwrap();
        h.set_size(size);
        h.set_entry_type(ty);
        h.set_mode(0o644);
        h.set_cksum();
        h
    }

    #[test]
    #[serial]
    fn extract_tarball_happy_path() {
        let tmp = TempDir::new().unwrap();
        let tarball = tmp.path().join("ok.tar.gz");
        let body = b"hello".to_vec();
        let bytes = build_tarball(vec![(
            header_for("manifest.toml", body.len() as u64, tar::EntryType::Regular),
            body.clone(),
        )]);
        std::fs::write(&tarball, bytes).unwrap();
        let dest = tmp.path().join("staging");
        extract_tarball(&tarball, &dest).unwrap();
        let actual = std::fs::read(dest.join("manifest.toml")).unwrap();
        assert_eq!(actual, body);
    }

    #[test]
    #[serial]
    fn extract_tarball_rejects_symlink_entries() {
        let tmp = TempDir::new().unwrap();
        let tarball = tmp.path().join("evil.tar.gz");
        let mut h = header_for("link", 0, tar::EntryType::Symlink);
        h.set_link_name("/etc/passwd").unwrap();
        h.set_cksum();
        let bytes = build_tarball(vec![(h, Vec::new())]);
        std::fs::write(&tarball, bytes).unwrap();
        let dest = tmp.path().join("staging");
        let err = extract_tarball(&tarball, &dest).unwrap_err();
        assert!(matches!(err, InstallError::BadEntry { .. }));
    }

    #[test]
    #[serial]
    fn extract_tarball_rejects_hardlink_entries() {
        let tmp = TempDir::new().unwrap();
        let tarball = tmp.path().join("evil.tar.gz");
        let mut h = header_for("link", 0, tar::EntryType::Link);
        h.set_link_name("manifest.toml").unwrap();
        h.set_cksum();
        let bytes = build_tarball(vec![(h, Vec::new())]);
        std::fs::write(&tarball, bytes).unwrap();
        let dest = tmp.path().join("staging");
        let err = extract_tarball(&tarball, &dest).unwrap_err();
        assert!(matches!(err, InstallError::BadEntry { .. }));
    }

    // NOTE: The `tar` crate's writer (`Header::set_path`) refuses to
    // accept paths containing `..` at write time, so we can't easily
    // construct a malicious tarball with parent-dir traversal in a
    // unit test without hand-rolling the bytes. The READ-side defenses
    // are still in place (per-component validation in extract_tarball
    // + tar::Entry::unpack_in's own containment check), and the
    // symlink + hardlink tests above exercise the most realistic
    // attack vectors. A real malicious tarball would have to come from
    // outside the tar crate's writer, in which case unpack_in catches
    // it. Tracked as future hardening: hand-craft byte-level malicious
    // tarballs as test fixtures (see GNU tar's `--transform` for ways
    // to produce these).

    // ---- verify_checksums_file ----

    #[test]
    #[serial]
    fn verify_checksums_happy_path() {
        let tmp = TempDir::new().unwrap();
        let pack_dir = tmp.path();
        std::fs::write(pack_dir.join("manifest.toml"), b"hello").unwrap();
        // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        let checksums =
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  manifest.toml\n";
        let cs_path = pack_dir.join("checksums.txt");
        std::fs::write(&cs_path, checksums).unwrap();
        verify_checksums_file(pack_dir, &cs_path).unwrap();
    }

    #[test]
    #[serial]
    fn verify_checksums_mismatch() {
        let tmp = TempDir::new().unwrap();
        let pack_dir = tmp.path();
        std::fs::write(pack_dir.join("manifest.toml"), b"DIFFERENT").unwrap();
        let checksums =
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  manifest.toml\n";
        let cs_path = pack_dir.join("checksums.txt");
        std::fs::write(&cs_path, checksums).unwrap();
        let err = verify_checksums_file(pack_dir, &cs_path).unwrap_err();
        assert!(matches!(err, InstallError::ChecksumMismatch(_)));
    }

    #[test]
    #[serial]
    fn verify_checksums_rejects_traversal_in_path() {
        let tmp = TempDir::new().unwrap();
        let pack_dir = tmp.path();
        let checksums = "deadbeef  ../../etc/passwd\n";
        let cs_path = pack_dir.join("checksums.txt");
        std::fs::write(&cs_path, checksums).unwrap();
        let err = verify_checksums_file(pack_dir, &cs_path).unwrap_err();
        assert!(matches!(err, InstallError::BadEntry { .. }));
    }

    // ---- remove_pack ----

    #[test]
    #[serial]
    fn remove_pack_happy_path() {
        let tmp = TempDir::new().unwrap();
        std::env::set_var("VOICEFORGE_HOME", tmp.path());
        fs::create_dir_all(tmp.path().join("packs/peter/wav")).unwrap();
        fs::write(tmp.path().join("packs/peter/manifest.toml"), b"x").unwrap();
        remove_pack("peter").unwrap();
        assert!(!tmp.path().join("packs/peter").exists());
    }

    #[test]
    #[serial]
    fn remove_pack_unknown() {
        let tmp = TempDir::new().unwrap();
        std::env::set_var("VOICEFORGE_HOME", tmp.path());
        let err = remove_pack("nonexistent").unwrap_err();
        assert!(matches!(err, InstallError::UnknownPack(_)));
    }

    // ---- pack_is_installed / resolve_text_to_event -------------------

    fn make_pack_with_manifest(home: &Path, pack: &str, phrases: &[(&str, &str)]) {
        make_pack(
            home,
            pack,
            &phrases.iter().map(|(e, _)| *e).collect::<Vec<_>>(),
        );
        let mut body = String::new();
        body.push_str("schema_version = 1\n");
        body.push_str(&format!("name           = \"{pack}\"\n"));
        body.push_str(&format!("display_name   = \"{pack}\"\n"));
        body.push_str("description    = \"x\"\n");
        body.push_str("rendered_with        = \"x\"\n");
        body.push_str("rendered_at          = \"2026-01-01\"\n");
        body.push_str("sample_rate          = 32000\n");
        body.push_str("channels             = 1\n");
        body.push_str(&format!("phrases              = {}\n", phrases.len()));
        body.push_str("voice_source         = \"x\"\n");
        body.push_str("source_clip_url      = \"x\"\n");
        body.push_str("source_clip_episode  = \"x\"\n");
        body.push_str("source_clip_duration = 10.0\n");
        body.push_str("reference_prompt_text = \"x\"\n");
        body.push_str("license       = \"x\"\n");
        body.push_str("takedown_url  = \"x\"\n");
        body.push_str("\n[phrase_text]\n");
        for (event, text) in phrases {
            body.push_str(&format!(
                "{event} = \"{}\"\n",
                text.replace('\\', "\\\\").replace('"', "\\\"")
            ));
        }
        std::fs::write(home.join("packs").join(pack).join("manifest.toml"), body).unwrap();
    }

    #[test]
    #[serial]
    fn pack_is_installed_true_when_manifest_present() {
        let tmp = setup_tmp_home();
        make_pack_with_manifest(tmp.path(), "trump", &[("build_failed", "Sad.")]);
        assert!(pack_is_installed("trump"));
        assert!(!pack_is_installed("nope"));
    }

    #[test]
    #[serial]
    fn pack_is_installed_false_for_invalid_name() {
        let _tmp = setup_tmp_home();
        assert!(!pack_is_installed("../etc/passwd"));
        assert!(!pack_is_installed("UPPER"));
    }

    #[test]
    #[serial]
    fn resolve_text_to_event_matches_event_id_exactly() {
        let tmp = setup_tmp_home();
        make_pack_with_manifest(
            tmp.path(),
            "trump",
            &[
                ("build_failed", "Sad. The build failed."),
                ("tests_passed", "Tremendous tests."),
            ],
        );
        let r = resolve_text_to_event("trump", "build_failed").unwrap();
        assert_eq!(r, Some("build_failed".to_string()));
    }

    #[test]
    #[serial]
    fn resolve_text_to_event_matches_phrase_text_case_insensitive() {
        let tmp = setup_tmp_home();
        make_pack_with_manifest(
            tmp.path(),
            "trump",
            &[(
                "build_success",
                "Tremendous build. Nobody builds like you. Nobody.",
            )],
        );
        let r = resolve_text_to_event(
            "trump",
            "TREMENDOUS BUILD. nobody builds like you. nobody.  ",
        )
        .unwrap();
        assert_eq!(r, Some("build_success".to_string()));
    }

    #[test]
    #[serial]
    fn resolve_text_to_event_fuzzy_contains_when_text_is_substantial() {
        let tmp = setup_tmp_home();
        make_pack_with_manifest(
            tmp.path(),
            "trump",
            &[(
                "build_failed",
                "Sad. The build failed. Many people are saying.",
            )],
        );
        let r = resolve_text_to_event("trump", "the build failed").unwrap();
        assert_eq!(r, Some("build_failed".to_string()));
    }

    #[test]
    #[serial]
    fn resolve_text_to_event_no_fuzzy_for_short_text() {
        let tmp = setup_tmp_home();
        make_pack_with_manifest(
            tmp.path(),
            "trump",
            &[(
                "build_failed",
                "Sad. The build failed. Many people are saying.",
            )],
        );
        // "sad" is 3 chars and 1 word — does NOT trigger fuzzy match
        // (would otherwise match every phrase containing "sad").
        let r = resolve_text_to_event("trump", "sad").unwrap();
        assert_eq!(r, None);
    }

    #[test]
    #[serial]
    fn resolve_text_to_event_returns_none_on_no_match() {
        let tmp = setup_tmp_home();
        make_pack_with_manifest(
            tmp.path(),
            "trump",
            &[("build_failed", "Sad. The build failed.")],
        );
        let r = resolve_text_to_event("trump", "completely random words here").unwrap();
        assert_eq!(r, None);
    }

    #[test]
    #[serial]
    fn resolve_text_to_event_errors_when_pack_missing() {
        let _tmp = setup_tmp_home();
        let err = resolve_text_to_event("nonexistent", "anything").unwrap_err();
        assert!(
            matches!(err, InstallError::UnknownPack(_)),
            "expected UnknownPack, got {err:?}"
        );
    }
}
