//! `voiceforge note` — long-form narration via fish-speech S2 Pro
//! with a resume cache and ffmpeg concat. The audiobook killer demo
//! for v0.4 (ROADMAP "studio quality, local-first, no cloud").
//!
//! This module is split across PR-D's atomic commits:
//!
//! - **D-1** (this commit): pure chunking (`chunk_text`,
//!   `chunk_sha256`), `ProgressJson` schema + atomic write helper,
//!   `pulldown-cmark` integration for markdown-strip.
//! - D-2: `NoteSynth` trait + `MockSynth` + macOS notify helper.
//! - D-3: `note::run_with` orchestrator (chunk loop, resume cache,
//!   cancel-safe Ctrl-C between chunks).
//! - D-4: per-chunk format verify + ffmpeg concat (pinned binary).
//! - D-5: `FishEngineNoteAdapter` + `Commands::Note` wiring.
//! - D-6: integration test exercising the partial-resume-of-
//!   partial-resume contract.
//!
//! Architecture decisions locked in `.planning/v0.4-pr-d-note-cli.plan.md`
//! v2 (rust-expert APPROVE-WITH-NITS). Notable:
//!   - 44.1 kHz mono PCM_16 throughout (fish-speech S2 Pro output).
//!   - `chunker_version` salt invalidates the cache on any chunker
//!     refactor.
//!   - Resume bails on `input_sha256` / `total_chunks` / `voice` /
//!     `voice_created_at` / `voice_recipe` mismatch with a `--force`
//!     hint.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Bump this on any change to `chunk_text` so existing progress.json
/// files invalidate cleanly (the loader bails with a `--force` hint).
pub const CHUNKER_VERSION: u32 = 1;

/// Fish-speech S2 Pro's quality sweet spot is sentence-to-paragraph
/// chunks. Empirically above ~600 chars quality drifts; below ~80
/// prosody flatlines. 500 is the middle ground. Override via
/// `VOICEFORGE_NOTE_CHUNK_MAX` (power-user knob).
pub const CHUNK_MAX_CHARS: usize = 500;

/// On-disk schema version for `<out>.progress.json`. Future bumps
/// trigger a bail with a `--force` hint.
pub const PROGRESS_JSON_VERSION: u32 = 1;

/// One unit of synth work. The `sha256` field is the resume cache
/// key — see `chunk_sha256` for the salt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub text: String,
    pub sha256: String,
    pub index: usize,
}

/// Compute the cache-key sha256 over the salt:
///   `chunker_version \n voice \n created_at \n recipe \n text`
/// Hex-encoded. Stable bytes-in, bytes-out.
pub fn chunk_sha256(
    chunker_version: u32,
    voice: &str,
    created_at: &str,
    recipe: &str,
    text: &str,
) -> String {
    let mut h = Sha256::new();
    h.update(format!("{chunker_version}\n{voice}\n{created_at}\n{recipe}\n{text}").as_bytes());
    hex::encode(h.finalize())
}

/// Split `raw` into one or more `Chunk`s.
///
/// Two paths share the post-strip pipeline:
/// - `is_markdown = true` runs `raw` through `pulldown-cmark`,
///   accumulating `Event::Text` content, skipping CodeBlock / Image,
///   and emitting `\n\n` between top-level blocks.
/// - `is_markdown = false` treats `raw` as plain text and only
///   normalizes whitespace + paragraph-splits.
///
/// Both paths then run normalize -> paragraph-split -> recursive
/// long-paragraph split with sentence-boundary preference. The
/// fallback hard-split at `chunk_max_chars` prevents the
/// no-punctuation infinite loop.
///
/// Empty / whitespace-only / all-code-block inputs return an empty
/// Vec — callers must bail loudly up-front (the orchestrator's job).
pub fn chunk_text(
    raw: &str,
    is_markdown: bool,
    voice: &str,
    created_at: &str,
    recipe: &str,
) -> Vec<Chunk> {
    let stripped = if is_markdown {
        strip_markdown(raw)
    } else {
        raw.to_string()
    };
    let normalized = normalize_whitespace(&stripped);
    if normalized.trim().is_empty() {
        return Vec::new();
    }

    let chunk_max = chunk_max_chars();

    let mut texts: Vec<String> = Vec::new();
    for para in split_paragraphs(&normalized) {
        if para.is_empty() {
            continue;
        }
        split_long_paragraph(&para, chunk_max, &mut texts);
    }

    texts
        .into_iter()
        .enumerate()
        .map(|(index, text)| {
            let sha256 = chunk_sha256(CHUNKER_VERSION, voice, created_at, recipe, &text);
            Chunk {
                text,
                sha256,
                index,
            }
        })
        .collect()
}

fn chunk_max_chars() -> usize {
    std::env::var("VOICEFORGE_NOTE_CHUNK_MAX")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n >= 50)
        .unwrap_or(CHUNK_MAX_CHARS)
}

/// CommonMark text extraction via `pulldown-cmark`. Strips bold /
/// italic / headers / links (keeps the link text) / images (keeps
/// alt) / fenced code blocks (dropped entirely) / inline code
/// (kept). Paragraph structure preserved via `\n\n` emission between
/// block-level closes.
fn strip_markdown(raw: &str) -> String {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};

    let mut out = String::with_capacity(raw.len());
    let mut in_codeblock_depth: u32 = 0;
    let mut in_image_depth: u32 = 0;
    let parser = Parser::new(raw);

    for event in parser {
        // True iff we're inside a span whose content should NOT be narrated
        // (fenced code blocks, image alt text). Recomputed per-event so the
        // depth-counter updates take effect immediately.
        let drop = in_codeblock_depth > 0 || in_image_depth > 0;
        match event {
            Event::Start(Tag::CodeBlock(_)) => in_codeblock_depth += 1,
            Event::End(TagEnd::CodeBlock) => {
                in_codeblock_depth = in_codeblock_depth.saturating_sub(1)
            }
            Event::Start(Tag::Image { .. }) => in_image_depth += 1,
            Event::End(TagEnd::Image) => {
                // pulldown-cmark emits `Event::Text(alt)` *inside* an Image span;
                // dropping it during the in_image_depth window is intentional so
                // we don't narrate a raw filename.
                in_image_depth = in_image_depth.saturating_sub(1);
            }
            Event::Text(t) | Event::Code(t) if !drop => out.push_str(&t),
            Event::SoftBreak if !drop => out.push(' '),
            Event::HardBreak if !drop => out.push('\n'),
            Event::End(TagEnd::Paragraph)
            | Event::End(TagEnd::Heading(_))
            | Event::End(TagEnd::BlockQuote)
            | Event::End(TagEnd::Item)
                if !drop =>
            {
                out.push_str("\n\n")
            }
            _ => {}
        }
    }

    out
}

fn normalize_whitespace(s: &str) -> String {
    // Normalize CRLF → LF first so paragraph splitting on `\n\n+`
    // works for Windows-authored input (rust-expert R4).
    let s = s.replace("\r\n", "\n").replace('\r', "\n");

    // Collapse runs of horizontal whitespace within a line, but
    // preserve `\n` (single) and `\n\n+` (paragraph) sequences.
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    let mut last_was_newline = false;
    let mut newline_run = 0usize;
    for c in s.chars() {
        if c == '\n' {
            newline_run += 1;
            // Emit `\n` for the first two newlines in a run (preserves the
            // paragraph break); collapse the rest.
            if newline_run <= 2 {
                out.push('\n');
            }
            last_was_space = false;
            last_was_newline = true;
        } else if c.is_whitespace() {
            if !last_was_newline && !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(c);
            last_was_space = false;
            last_was_newline = false;
            newline_run = 0;
        }
    }
    out.trim().to_string()
}

fn split_paragraphs(s: &str) -> Vec<String> {
    s.split("\n\n")
        .map(|p| p.replace('\n', " ").trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn split_long_paragraph(p: &str, chunk_max: usize, out: &mut Vec<String>) {
    if p.chars().count() <= chunk_max {
        out.push(p.to_string());
        return;
    }
    // Find the last sentence boundary (. ! ?) followed by whitespace
    // that fits within the budget. Char-indexed.
    let chars: Vec<char> = p.chars().collect();
    let upper = chars.len().min(chunk_max);
    let mut split_at: Option<usize> = None;
    for i in (0..upper).rev() {
        let c = chars[i];
        if (c == '.' || c == '!' || c == '?') && i + 1 < chars.len() && chars[i + 1].is_whitespace()
        {
            split_at = Some(i + 1);
            break;
        }
    }
    let split_at = match split_at {
        Some(n) => n,
        None => {
            // Hard split at chunk_max — no sentence boundary; better
            // than infinite recursion. Tries word boundary first.
            let mut hard = upper;
            for i in (0..upper).rev() {
                if chars[i].is_whitespace() {
                    hard = i + 1;
                    break;
                }
            }
            hard.max(1)
        }
    };
    let head: String = chars[..split_at].iter().collect();
    let tail: String = chars[split_at..].iter().collect();
    out.push(head.trim().to_string());
    split_long_paragraph(tail.trim(), chunk_max, out);
}

/// On-disk schema for `<out>.progress.json`. `version` +
/// `chunker_version` give two independent invalidation axes.
/// `deny_unknown_fields` so forward-compat fields trip an explicit
/// error rather than silently dropping (rust-expert A2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProgressJson {
    pub version: u32,
    pub chunker_version: u32,
    pub voice: String,
    pub voice_created_at: String,
    pub voice_recipe: String,
    pub input_sha256: String,
    pub total_chunks: usize,
    pub completed: Vec<CompletedChunk>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompletedChunk {
    pub index: usize,
    pub sha256: String,
    pub wav_path: PathBuf,
}

impl ProgressJson {
    pub fn fresh(
        voice: &str,
        voice_created_at: &str,
        voice_recipe: &str,
        input_sha256: &str,
        total_chunks: usize,
    ) -> Self {
        Self {
            version: PROGRESS_JSON_VERSION,
            chunker_version: CHUNKER_VERSION,
            voice: voice.to_string(),
            voice_created_at: voice_created_at.to_string(),
            voice_recipe: voice_recipe.to_string(),
            input_sha256: input_sha256.to_string(),
            total_chunks,
            completed: Vec::new(),
        }
    }

    /// Replace any prior entry at `index` with the new tuple.
    pub fn add(&mut self, index: usize, sha256: String, wav_path: PathBuf) {
        self.completed.retain(|e| e.index != index);
        self.completed.push(CompletedChunk {
            index,
            sha256,
            wav_path,
        });
        self.completed.sort_by_key(|e| e.index);
    }
}

/// Atomic `.tmp + rename` write. Caller is responsible for fsyncing
/// the parent dir if durability across power loss is required
/// (rust-expert Hole-A nit, folded in D-3).
pub fn write_progress_atomically(progress: &ProgressJson, path: &Path) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(progress).context("serializing progress.json")?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("progress path has no parent"))?;
    std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Load and validate. Returns `Err` (with a `--force` hint in the
/// chain) on any schema/invariant mismatch. Returns `Ok(None)` only
/// if the file is missing.
pub fn load_progress_if_valid(
    path: &Path,
    expected_voice: &str,
    expected_created_at: &str,
    expected_recipe: &str,
    expected_input_sha256: &str,
    expected_total_chunks: usize,
) -> Result<Option<ProgressJson>> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: ProgressJson = serde_json::from_str(&raw).with_context(|| {
        format!(
            "parsing {} (unknown or malformed field; re-run with --force to start fresh)",
            path.display()
        )
    })?;
    if parsed.version != PROGRESS_JSON_VERSION {
        bail!(
            "progress.json version {} (this build expects {}). \
             Re-run with --force to start fresh.",
            parsed.version,
            PROGRESS_JSON_VERSION
        );
    }
    if parsed.chunker_version != CHUNKER_VERSION {
        bail!(
            "progress.json chunker_version {} (this build expects {}). \
             Re-run with --force to start fresh.",
            parsed.chunker_version,
            CHUNKER_VERSION
        );
    }
    if parsed.voice != expected_voice {
        bail!(
            "progress.json is for voice {:?}, you passed {:?}. \
             Re-run with --force or a fresh --out path.",
            parsed.voice,
            expected_voice
        );
    }
    if parsed.voice_created_at != expected_created_at {
        bail!(
            "progress.json's voice_created_at {:?} doesn't match the current voice {:?}. \
             Re-run with --force or a fresh --out path.",
            parsed.voice_created_at,
            expected_created_at
        );
    }
    if parsed.voice_recipe != expected_recipe {
        bail!(
            "progress.json's voice_recipe {:?} doesn't match {:?}. \
             Re-run with --force or a fresh --out path.",
            parsed.voice_recipe,
            expected_recipe
        );
    }
    if parsed.input_sha256 != expected_input_sha256 {
        bail!(
            "input file changed since the prior run (input_sha256 mismatch). \
             Re-run with --force or a fresh --out path."
        );
    }
    if parsed.total_chunks != expected_total_chunks {
        bail!(
            "chunk count changed since the prior run (was {}, now {}). \
             Re-run with --force or a fresh --out path.",
            parsed.total_chunks,
            expected_total_chunks
        );
    }
    Ok(Some(parsed))
}

/// sha256-hex of a `&[u8]`. Used to compute `input_sha256` and to
/// hash arbitrary text for tests.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

// ============================================================================
// PR-D D-2: NoteSynth trait + MockSynth
// ============================================================================
//
// `NoteSynth` is the injection point that lets us test the chunk loop +
// resume cache without bringing fish-speech online. Mirrors the
// `install_smoke::SmokeSynth` parametric pattern (`run_with<S: Synth>`),
// but with NO per-call ref args because note's ref clip + transcript
// are constant across an entire `note::run` invocation (rust-expert A1
// deliberate divergence — documented here so a future "merge these two
// traits" PR doesn't quietly break either).

/// Synth abstraction for `note::run_with`. Production impl is
/// `FishEngineNoteAdapter` (D-5); tests inject `MockSynth`.
#[async_trait::async_trait]
pub trait NoteSynth: Send + Sync {
    /// Write a 44.1 kHz mono PCM_16 WAV containing the synth of
    /// `text` to `out`. Caller fsyncs after this returns.
    async fn synth(&self, text: &str, out: &Path) -> Result<()>;
}

/// Test-only synth: writes a deterministic silent 44.1 kHz mono PCM_16
/// WAV per call so the orchestrator + concat pipeline can run without
/// fish-speech. Optional `fail_after` triggers an error on the Nth
/// call (1-indexed) — used by the partial-resume-of-partial-resume
/// integration test (rust-expert R3 / nit-8).
#[allow(dead_code)]
pub struct MockSynth {
    pub fail_after: Option<usize>,
    pub samples_per_chunk: u32,
    pub calls: std::sync::atomic::AtomicUsize,
}

#[allow(dead_code)]
impl MockSynth {
    /// Default: 0.25-second silent chunks; never fails.
    pub fn new() -> Self {
        Self {
            fail_after: None,
            samples_per_chunk: 11025, // 0.25s @ 44.1 kHz
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    pub fn with_fail_after(n: usize) -> Self {
        Self {
            fail_after: Some(n),
            samples_per_chunk: 11025,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    pub fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for MockSynth {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl NoteSynth for MockSynth {
    async fn synth(&self, _text: &str, out: &Path) -> Result<()> {
        use std::sync::atomic::Ordering;
        let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(fail_at) = self.fail_after {
            if n == fail_at {
                bail!("MockSynth: simulated synth failure on call {n}");
            }
        }
        // Write a real 44.1 kHz mono PCM_16 silent WAV so the concat
        // demuxer + downstream `hound` spec-check pass.
        write_silent_wav_44100_mono(out, self.samples_per_chunk)?;
        Ok(())
    }
}

// ============================================================================
// PR-D D-3: orchestrator (synth loop + resume cache)
// ============================================================================

/// Arguments for `voiceforge note`. Field naming intentionally avoids
/// the Rust keyword `in` (rust-expert A3); the CLI exposes it as
/// `--in` via `#[arg(long = "in")]` in `main.rs` (D-5).
#[derive(Debug, Clone)]
pub struct NoteArgs {
    pub voice: String,
    /// `None` = stdin. If `Some(path)`, the orchestrator caller reads
    /// it; `run_with` itself receives the raw bytes already.
    pub input: Option<PathBuf>,
    pub output: PathBuf,
    pub force: bool,
    pub cleanup: bool,
}

/// Snapshot of the v2 voice fields the orchestrator needs. Avoids
/// the `run_with` test path needing to construct a full
/// `VoiceProfileV2` with serde-skipped `dir` / `ref_wav` / `ref_txt`
/// fields populated.
#[derive(Debug, Clone)]
pub struct VoiceMeta {
    pub name: String,
    pub created_at: String,
    pub recipe: String,
}

/// Locate `<out>.chunks/`.
pub fn chunks_dir_for(output: &Path) -> PathBuf {
    let stem = output
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "note".to_string());
    output.with_file_name(format!("{stem}.chunks"))
}

/// Locate `<out>.progress.json`.
pub fn progress_path_for(output: &Path) -> PathBuf {
    let stem = output
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "note".to_string());
    output.with_file_name(format!("{stem}.progress.json"))
}

pub fn chunk_wav_path(chunks_dir: &Path, index: usize) -> PathBuf {
    chunks_dir.join(format!("chunk_{index:04}.wav"))
}

fn chunk_wav_tmp_path(chunks_dir: &Path, index: usize) -> PathBuf {
    chunks_dir.join(format!("chunk_{index:04}.wav.tmp"))
}

/// Best-effort fsync of a file. On any error, we propagate — chunk
/// durability is a load-bearing claim per the plan's B3 nit.
fn fsync_file(path: &Path) -> Result<()> {
    let f = std::fs::File::open(path)
        .with_context(|| format!("opening {} for fsync", path.display()))?;
    f.sync_all()
        .with_context(|| format!("fsyncing {}", path.display()))?;
    Ok(())
}

/// Best-effort fsync of a directory. On platforms / FSes where this
/// isn't supported (Windows, some filesystems), errors are logged
/// but not propagated — the worst case is one chunk re-synth on
/// power loss, which the resume cache already handles.
fn fsync_dir_best_effort(path: &Path) {
    match std::fs::File::open(path) {
        Ok(f) => {
            if let Err(e) = f.sync_all() {
                eprintln!("voiceforge: fsync of {} failed: {e}", path.display());
            }
        }
        Err(e) => {
            eprintln!(
                "voiceforge: open-for-fsync of {} failed: {e}",
                path.display()
            );
        }
    }
}

/// Synthesize every chunk required to produce `args.output`. Does NOT
/// concat — D-4 adds that on top. Tests inject `synth` directly.
///
/// `cancel` is set by an external `tokio::signal::ctrl_c` task
/// (installed only in `run`, never in `run_with`) — see rust-expert
/// nit-3.
pub async fn synth_all_chunks<S: NoteSynth + ?Sized>(
    synth: &S,
    voice: &VoiceMeta,
    raw_input: &str,
    is_markdown: bool,
    args: &NoteArgs,
    cancel: Arc<AtomicBool>,
) -> Result<SynthReport> {
    // Step 1: compute input_sha256 over the raw bytes (BEFORE markdown
    // strip). This is the "did the user edit the input file?" detector
    // rust-expert S3 asked for.
    let input_sha256 = sha256_hex(raw_input.as_bytes());

    // Step 2: chunk.
    let chunks = chunk_text(
        raw_input,
        is_markdown,
        &voice.name,
        &voice.created_at,
        &voice.recipe,
    );
    if chunks.is_empty() {
        bail!(
            "input has no narrate-able text after markdown stripping. \
             Check that the file isn't empty or all code fences."
        );
    }
    let total = chunks.len();

    // Step 3: paths.
    let chunks_dir = chunks_dir_for(&args.output);
    std::fs::create_dir_all(&chunks_dir)
        .with_context(|| format!("mkdir {}", chunks_dir.display()))?;
    let progress_path = progress_path_for(&args.output);

    // Step 4: load + validate prior progress (skipped on --force).
    let mut progress = if args.force {
        ProgressJson::fresh(
            &voice.name,
            &voice.created_at,
            &voice.recipe,
            &input_sha256,
            total,
        )
    } else {
        match load_progress_if_valid(
            &progress_path,
            &voice.name,
            &voice.created_at,
            &voice.recipe,
            &input_sha256,
            total,
        )? {
            Some(p) => p,
            None => ProgressJson::fresh(
                &voice.name,
                &voice.created_at,
                &voice.recipe,
                &input_sha256,
                total,
            ),
        }
    };

    // Stale-from-prior-rendering chunks may have been deleted under us;
    // prune the `completed` list to entries whose WAV is still present
    // AND whose sha matches the (now recomputed) chunk_sha (rust-expert R2).
    progress.completed.retain(|e| {
        if !e.wav_path.is_file() {
            return false;
        }
        let chunk = chunks.iter().find(|c| c.index == e.index);
        matches!(chunk, Some(c) if c.sha256 == e.sha256)
    });
    write_progress_atomically(&progress, &progress_path)?;

    // Step 5: synth loop. Cancel checked at top of iteration, NEVER
    // select! over the synth await (rust-expert B4). A mid-synth
    // process termination is safe because the next run spawns a
    // fresh python child — the stale-pipe poisoning only matters
    // within one process.
    let mut synthesized: Vec<usize> = Vec::new();
    let mut skipped: Vec<usize> = Vec::new();
    for chunk in &chunks {
        if cancel.load(Ordering::Relaxed) {
            bail!(
                "cancelled by Ctrl-C. Resume with the same args: \
                 voiceforge note --voice {} --in <same> --out {}",
                voice.name,
                args.output.display()
            );
        }
        let already_done = progress
            .completed
            .iter()
            .any(|e| e.index == chunk.index && e.sha256 == chunk.sha256);
        if already_done {
            skipped.push(chunk.index);
            continue;
        }
        let chunk_tmp = chunk_wav_tmp_path(&chunks_dir, chunk.index);
        let chunk_final = chunk_wav_path(&chunks_dir, chunk.index);
        synth
            .synth(&chunk.text, &chunk_tmp)
            .await
            .with_context(|| format!("synthesizing chunk {}", chunk.index))?;
        fsync_file(&chunk_tmp)?;
        std::fs::rename(&chunk_tmp, &chunk_final).with_context(|| {
            format!(
                "rename {} -> {}",
                chunk_tmp.display(),
                chunk_final.display()
            )
        })?;
        fsync_file(&chunk_final)?;
        fsync_dir_best_effort(&chunks_dir);
        progress.add(chunk.index, chunk.sha256.clone(), chunk_final.clone());
        write_progress_atomically(&progress, &progress_path)?;
        if let Some(parent) = progress_path.parent() {
            fsync_dir_best_effort(parent);
        }
        synthesized.push(chunk.index);
    }

    Ok(SynthReport {
        total_chunks: total,
        synthesized,
        skipped,
        chunks_dir,
        progress_path,
        input_sha256,
    })
}

/// Outcome of `synth_all_chunks`. The CLI shim consumes this for the
/// human summary; D-4 reads `chunks_dir` to drive the concat.
#[derive(Debug, Clone)]
pub struct SynthReport {
    pub total_chunks: usize,
    #[allow(dead_code)]
    pub synthesized: Vec<usize>,
    #[allow(dead_code)]
    pub skipped: Vec<usize>,
    pub chunks_dir: PathBuf,
    pub progress_path: PathBuf,
    #[allow(dead_code)]
    pub input_sha256: String,
}

/// Read `args.input` (or stdin) to a String + detect markdown via
/// extension. Used by `run` (D-5); test callers can short-circuit by
/// calling `synth_all_chunks` directly with a known-good string.
pub fn read_input_with_markdown_detection(args: &NoteArgs) -> Result<(String, bool)> {
    match &args.input {
        Some(path) => {
            let is_markdown = matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("md") | Some("markdown")
            );
            // If a `<out>.progress.json` exists AND stdin was used,
            // bail per rust-expert B7 — but this branch is path-only,
            // so stdin checks belong in the None arm below.
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            Ok((raw, is_markdown))
        }
        None => {
            // Stdin + resume incompatibility check (rust-expert B7).
            let progress = progress_path_for(&args.output);
            if progress.is_file() && !args.force {
                bail!(
                    "--in from stdin doesn't support resume. \
                     Re-run with --in <file>, or delete {} and retry, \
                     or pass --force.",
                    progress.display()
                );
            }
            let mut buf = String::new();
            use std::io::Read;
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading stdin")?;
            Ok((buf, false))
        }
    }
}

/// Bail with exit code 2 + helpful migrate hint when the voice is v1.
/// `run` (D-5) maps this to `process::exit(2)`. For now we just bail
/// with a clearly-marked message that callers can match on.
pub fn v1_voice_bail_message(voice_name: &str) -> String {
    format!(
        "voice {voice_name:?} is on schema 1 (gpt-sovits).\n\
         Run: voiceforge voices migrate {voice_name}\n\
         Then retry this command."
    )
}

/// Write a silent 44.1 kHz mono PCM_16 WAV. Shared between MockSynth
/// and any test fixture that needs a placeholder chunk WAV.
#[allow(dead_code)]
pub fn write_silent_wav_44100_mono(path: &Path, samples: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("creating WAV at {}", path.display()))?;
    for _ in 0..samples {
        writer.write_sample(0i16).context("writing silent sample")?;
    }
    writer.finalize().context("finalizing WAV")?;
    Ok(())
}

// ============================================================================
// PR-D D-4: per-chunk format verify + ffmpeg concat
// ============================================================================
//
// `-c copy` concat-demuxer is sample-accurate only when every input has
// identical codec/SR/channels. Per rust-expert S1 we verify each chunk's
// hound spec BEFORE writing the concat list, bailing on mismatch with
// a clear pointer at the bad chunk.

pub const REQUIRED_SAMPLE_RATE: u32 = 44_100;
pub const REQUIRED_CHANNELS: u16 = 1;
pub const REQUIRED_BITS_PER_SAMPLE: u16 = 16;

/// Open each `chunk_NNNN.wav` in `chunks_dir` (for indices 0..total)
/// and assert the spec is `{1, 44100, 16, Int}`. Bails on first
/// mismatch with the offending path + actual spec in the message.
pub fn verify_chunk_formats(chunks_dir: &Path, total: usize) -> Result<()> {
    for i in 0..total {
        let path = chunk_wav_path(chunks_dir, i);
        let reader = hound::WavReader::open(&path)
            .with_context(|| format!("opening {} for spec check", path.display()))?;
        let spec = reader.spec();
        let ok = spec.channels == REQUIRED_CHANNELS
            && spec.sample_rate == REQUIRED_SAMPLE_RATE
            && spec.bits_per_sample == REQUIRED_BITS_PER_SAMPLE
            && spec.sample_format == hound::SampleFormat::Int;
        if !ok {
            bail!(
                "chunk {} at {} has spec {:?} (channels={}, sample_rate={}, bits_per_sample={}, sample_format={:?}); \
                 expected channels=1, sample_rate=44100, bits_per_sample=16, sample_format=Int. \
                 Re-run with --force to re-synth the chunk.",
                i,
                path.display(),
                spec,
                spec.channels,
                spec.sample_rate,
                spec.bits_per_sample,
                spec.sample_format,
            );
        }
    }
    Ok(())
}

/// Write `<chunks_dir>/concat_list.txt` with relative entries:
///   file 'chunk_0000.wav'
///   file 'chunk_0001.wav'
/// All filenames are hard-coded `chunk_{:04}.wav` literals — no
/// user-controlled string enters the file (rust-expert B5 safe-by-
/// construction).
pub fn write_concat_list(chunks_dir: &Path, total: usize) -> Result<PathBuf> {
    let mut body = String::with_capacity(total * 24);
    for i in 0..total {
        body.push_str(&format!("file 'chunk_{i:04}.wav'\n"));
    }
    let path = chunks_dir.join("concat_list.txt");
    std::fs::write(&path, &body).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Run the pinned ffmpeg from `InstallStateV2::ffmpeg6_prefix` to
/// concat the chunk WAVs into `out`. Uses the demuxer with `-c copy`
/// — no re-encode. Caller is responsible for having already verified
/// per-chunk formats via `verify_chunk_formats`.
pub async fn run_ffmpeg_concat(ffmpeg_bin: &Path, concat_list: &Path, out: &Path) -> Result<()> {
    let status = tokio::process::Command::new(ffmpeg_bin)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-f")
        .arg("concat")
        .arg("-safe")
        .arg("0")
        .arg("-i")
        .arg(concat_list)
        .arg("-c")
        .arg("copy")
        .arg(out)
        .status()
        .await
        .with_context(|| format!("spawning ffmpeg at {}", ffmpeg_bin.display()))?;
    if !status.success() {
        bail!(
            "ffmpeg concat failed (exit {:?}). Try --force to re-synth all chunks, \
             or inspect {} for diagnostics.",
            status.code(),
            concat_list.parent().unwrap_or(Path::new(".")).display()
        );
    }
    Ok(())
}

/// Final wrap: verify every chunk's format, write the concat list,
/// run ffmpeg. Used by `run` (D-5) after `synth_all_chunks` returns.
pub async fn concat_chunks(
    chunks_dir: &Path,
    total_chunks: usize,
    out: &Path,
    ffmpeg_bin: &Path,
) -> Result<()> {
    verify_chunk_formats(chunks_dir, total_chunks)?;
    let list = write_concat_list(chunks_dir, total_chunks)?;
    run_ffmpeg_concat(ffmpeg_bin, &list, out).await
}

/// Locate the pinned ffmpeg binary from `InstallStateV2`. Returns a
/// path that may or may not exist; callers should verify before use.
/// On absent install marker, returns an error pointing at
/// `voiceforge install-cloning`.
pub fn pinned_ffmpeg_path() -> Result<PathBuf> {
    let state = crate::install_cloning::read_install_state_v2()
        .context("could not read v2 install state. Run `voiceforge install-cloning` first.")?;
    Ok(PathBuf::from(state.ffmpeg6_prefix).join("bin/ffmpeg"))
}

// ============================================================================
// PR-D D-5: FishEngineNoteAdapter + production run() wrapper
// ============================================================================

/// Production `NoteSynth` impl. Wraps a `FishEngine` and the resolved
/// (ref_wav, ref_txt) per-voice constants. One instance is constructed
/// per `note::run` call.
pub struct FishEngineNoteAdapter {
    engine: crate::tts::FishEngine,
    ref_wav: PathBuf,
    ref_txt: String,
}

impl FishEngineNoteAdapter {
    pub fn from_v2(profile: &crate::voices::VoiceProfileV2) -> Result<Self> {
        let engine =
            crate::tts::FishEngine::new().context("constructing FishEngine for voiceforge note")?;
        let ref_wav = profile.ref_wav.clone();
        let ref_txt = std::fs::read_to_string(&profile.ref_txt)
            .with_context(|| format!("reading ref.txt at {}", profile.ref_txt.display()))?;
        Ok(Self {
            engine,
            ref_wav,
            ref_txt,
        })
    }
}

#[async_trait::async_trait]
impl NoteSynth for FishEngineNoteAdapter {
    async fn synth(&self, text: &str, out: &Path) -> Result<()> {
        self.engine
            .speak_with_explicit_ref(text, &self.ref_wav, &self.ref_txt, out)
            .await
    }
}

/// Compute the total audio duration in seconds for a finalized WAV.
pub fn wav_duration_secs(path: &Path) -> Result<f64> {
    let reader = hound::WavReader::open(path)
        .with_context(|| format!("opening {} to compute duration", path.display()))?;
    let spec = reader.spec();
    let samples = reader.duration() as f64;
    Ok(samples / spec.sample_rate as f64)
}

/// Production entry point: load + validate voice, read input, install
/// Ctrl-C handler, run `synth_all_chunks`, run `concat_chunks`,
/// optionally cleanup, fire macOS notification.
///
/// The Ctrl-C signal handler is installed HERE (not in
/// `synth_all_chunks`), so library callers can drive the orchestrator
/// with their own cancel flag without colliding with the process-global
/// SIGINT handler (rust-expert nit-3).
pub async fn run(args: NoteArgs) -> Result<()> {
    let profile = crate::voices::load_voice(&args.voice)?;
    let v2 = match profile {
        crate::voices::VoiceProfile::V2(v) => v,
        crate::voices::VoiceProfile::V1(_) => bail!(v1_voice_bail_message(&args.voice)),
    };

    let (raw, is_markdown) = read_input_with_markdown_detection(&args)?;

    let cancel = Arc::new(AtomicBool::new(false));
    let c = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            c.store(true, Ordering::Relaxed);
            eprintln!("\nvoiceforge: Ctrl-C received; finishing current chunk then exiting.");
        }
    });

    let voice_meta = VoiceMeta {
        name: v2.name.clone(),
        created_at: v2.created_at.clone(),
        recipe: v2.recipe.clone(),
    };
    let adapter = FishEngineNoteAdapter::from_v2(&v2)?;
    let report = synth_all_chunks(
        &adapter,
        &voice_meta,
        &raw,
        is_markdown,
        &args,
        cancel.clone(),
    )
    .await?;

    let ffmpeg = pinned_ffmpeg_path()?;
    concat_chunks(
        &report.chunks_dir,
        report.total_chunks,
        &args.output,
        &ffmpeg,
    )
    .await?;

    if args.cleanup {
        let _ = std::fs::remove_dir_all(&report.chunks_dir);
        let _ = std::fs::remove_file(&report.progress_path);
    }

    let secs = wav_duration_secs(&args.output).unwrap_or(0.0);
    println!(
        "voiceforge: wrote {} ({} chunks, {:.1}s audio)",
        args.output.display(),
        report.total_chunks,
        secs,
    );
    crate::notify_macos::notify_note_complete(&voice_meta.name, report.total_chunks, secs);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: &str = "tyson";
    const C: &str = "2026-05-04T00:00:00Z";
    const R: &str = "fish-speech-s2-pro";

    // ----- chunking ----------------------------------------------------

    /// Plan test #1: plain-text paragraph split.
    #[test]
    fn chunk_text_splits_paragraphs_on_blank_lines() {
        let chunks = chunk_text("a\n\nb\n\nc", false, V, C, R);
        let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "b", "c"]);
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.index, i);
        }
    }

    /// Plan test #2: long paragraph splits on sentence boundary.
    #[test]
    fn chunk_text_long_paragraph_splits_on_sentence_boundary() {
        let sentence = "The quick brown fox jumps over the lazy dog. ";
        // Build a paragraph of ~1200 chars.
        let para: String = sentence.repeat(28);
        let chunks = chunk_text(&para, false, V, C, R);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(
                c.text.chars().count() <= CHUNK_MAX_CHARS,
                "chunk too long: {} chars",
                c.text.chars().count()
            );
            assert!(c.text.contains('.'));
        }
    }

    /// Plan test #3 (rust-expert R4 + nit-4): no-punctuation 10k-char
    /// paragraph must terminate AND yield bounded chunk count, NOT
    /// infinite-loop. Wrapped in a tokio timeout via the test
    /// harness's default timeout (cargo test sets this).
    #[test]
    fn chunk_text_hard_splits_when_no_sentence_boundary() {
        let huge: String = "x".repeat(10_000);
        let start = std::time::Instant::now();
        let chunks = chunk_text(&huge, false, V, C, R);
        let elapsed = start.elapsed();
        // Termination assertion: must complete in <100ms (1000x
        // larger than expected; catches accidental quadratic blow-up).
        assert!(
            elapsed.as_millis() < 100,
            "chunk_text on 10k chars took {elapsed:?}; suspect infinite recursion"
        );
        // Bounded count: 10000 / CHUNK_MAX_CHARS = 20, plus a few
        // for boundary-rounding. Cap at 25 to catch runaway splits.
        assert!(
            chunks.len() <= 25,
            "expected <= 25 chunks, got {}",
            chunks.len()
        );
        // Every chunk respects the cap.
        for c in &chunks {
            assert!(c.text.chars().count() <= CHUNK_MAX_CHARS);
        }
    }

    /// Plan test #4 (rust-expert B1): bold + italic strip via
    /// pulldown-cmark, NOT regex.
    #[test]
    fn chunk_text_markdown_strips_bold_italic_via_pulldown_cmark() {
        let chunks = chunk_text("**hello** *world*", true, V, C, R);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello world");
    }

    /// Plan test #5 (rust-expert R4): fenced code blocks are dropped
    /// entirely, content never appears in chunks.
    #[test]
    fn chunk_text_markdown_strips_fenced_code_blocks() {
        let raw = "para one\n\n```rust\nfn main() { secret_keyword }\n```\n\npara two";
        let chunks = chunk_text(raw, true, V, C, R);
        let joined: String = chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" || ");
        assert!(
            !joined.contains("secret_keyword"),
            "code block content leaked: {joined}"
        );
        assert!(joined.contains("para one"));
        assert!(joined.contains("para two"));
    }

    /// Plan test #6: links keep the text, images keep alt-text-only,
    /// URLs are dropped.
    #[test]
    fn chunk_text_markdown_strips_links_and_images() {
        let chunks = chunk_text("[click me](https://x.example)", true, V, C, R);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "click me");
        // Images: the alt text is intentionally dropped per
        // strip_markdown's in_image_depth gate.
        let chunks = chunk_text("![alt](https://x.example/i.png)", true, V, C, R);
        assert!(
            chunks.is_empty(),
            "image-only input should produce 0 chunks"
        );
    }

    /// Plan test #7: punctuation preserved for prosody.
    #[test]
    fn chunk_text_preserves_punctuation_for_prosody() {
        let chunks = chunk_text("Wait, what?! Really; yes: now.", false, V, C, R);
        assert_eq!(chunks.len(), 1);
        let t = &chunks[0].text;
        assert!(t.contains(","), "comma dropped");
        assert!(t.contains("?"), "? dropped");
        assert!(t.contains("!"), "! dropped");
        assert!(t.contains(";"), "; dropped");
        assert!(t.contains(":"), ": dropped");
        assert!(t.contains("."), ". dropped");
    }

    /// Plan test #8 (rust-expert R4): CRLF paragraph breaks normalize.
    #[test]
    fn chunk_text_handles_crlf_paragraph_breaks() {
        let chunks = chunk_text("alpha\r\n\r\nbeta", false, V, C, R);
        let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["alpha", "beta"]);
    }

    /// Plan test #9 (rust-expert B6): empty/whitespace/all-code input
    /// yields an empty Vec; caller bails up front.
    #[test]
    fn chunk_text_bails_safely_on_empty_or_all_code_input() {
        assert!(chunk_text("", false, V, C, R).is_empty());
        assert!(chunk_text("   \n\n\t  ", false, V, C, R).is_empty());
        assert!(chunk_text("```\nfn main(){}\n```", true, V, C, R).is_empty());
    }

    /// Plan test #10 (rust-expert B1): pathological bold-in-prose
    /// does NOT eat surrounding content.
    #[test]
    fn chunk_text_pathological_bold_in_prose() {
        let chunks = chunk_text(
            "This is **really, really** important — and I mean **really**.",
            true,
            V,
            C,
            R,
        );
        assert_eq!(chunks.len(), 1);
        let t = &chunks[0].text;
        assert!(t.contains("This is"));
        assert!(t.contains("really, really"));
        assert!(t.contains("important"));
        assert!(t.contains("I mean"));
        assert!(!t.contains("**"));
    }

    // ----- chunk_sha256 ------------------------------------------------

    /// Plan test #11: same inputs → same hex.
    #[test]
    fn chunk_sha256_stable_for_same_inputs() {
        let a = chunk_sha256(1, "tyson", "2026-01-01", "fish-speech-s2-pro", "hello");
        let b = chunk_sha256(1, "tyson", "2026-01-01", "fish-speech-s2-pro", "hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    /// Plan test #12 (rust-expert S2): bumping CHUNKER_VERSION
    /// invalidates every cached chunk.
    #[test]
    fn chunk_sha256_changes_on_chunker_version_bump() {
        let a = chunk_sha256(1, "tyson", "2026-01-01", "fish-speech-s2-pro", "hello");
        let b = chunk_sha256(2, "tyson", "2026-01-01", "fish-speech-s2-pro", "hello");
        assert_ne!(a, b);
    }

    #[test]
    fn chunk_sha256_changes_on_one_char_text_edit() {
        let a = chunk_sha256(1, "tyson", "2026-01-01", "fish-speech-s2-pro", "hello");
        let b = chunk_sha256(1, "tyson", "2026-01-01", "fish-speech-s2-pro", "Hello");
        assert_ne!(a, b);
    }

    // ----- ProgressJson ------------------------------------------------

    /// Plan test #13 (rust-expert A2 + nit-9): round-trip + unknown-
    /// fields rejection + error chain mentions `--force`.
    #[test]
    fn progress_json_round_trip_with_deny_unknown_fields() {
        let p = ProgressJson::fresh("tyson", "2026-01-01", "fish-speech-s2-pro", "abcd", 3);
        let json = serde_json::to_string(&p).unwrap();
        let p2: ProgressJson = serde_json::from_str(&json).unwrap();
        assert_eq!(p, p2);

        // Add a frobnicator field → deserialization must fail.
        let json_with_extra = serde_json::to_string(&serde_json::json!({
            "version": 1, "chunker_version": 1, "voice": "tyson",
            "voice_created_at": "2026-01-01", "voice_recipe": "fish-speech-s2-pro",
            "input_sha256": "abcd", "total_chunks": 3, "completed": [],
            "frobnicator": "evil",
        }))
        .unwrap();
        let err = serde_json::from_str::<ProgressJson>(&json_with_extra).unwrap_err();
        // serde's "unknown field" error message has the field name.
        assert!(
            err.to_string().contains("frobnicator") || err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );

        // The load_progress_if_valid wrapper adds the --force hint.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("progress.json");
        std::fs::write(&path, &json_with_extra).unwrap();
        let err = load_progress_if_valid(
            &path,
            "tyson",
            "2026-01-01",
            "fish-speech-s2-pro",
            "abcd",
            3,
        )
        .unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("--force"),
            "expected --force hint in error chain, got: {chain}"
        );
    }

    #[test]
    fn progress_json_add_replaces_prior_index() {
        let mut p = ProgressJson::fresh("tyson", "2026-01-01", "fish-speech-s2-pro", "abcd", 3);
        p.add(0, "sha0".to_string(), PathBuf::from("a.wav"));
        p.add(1, "sha1".to_string(), PathBuf::from("b.wav"));
        p.add(0, "sha0new".to_string(), PathBuf::from("a-new.wav"));
        assert_eq!(p.completed.len(), 2);
        assert_eq!(p.completed[0].sha256, "sha0new");
        assert_eq!(p.completed[1].sha256, "sha1");
    }

    #[test]
    fn write_progress_atomically_leaves_no_tmp_file_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let p = ProgressJson::fresh("tyson", "2026-01-01", "fish-speech-s2-pro", "abcd", 1);
        let path = tmp.path().join("progress.json");
        write_progress_atomically(&p, &path).unwrap();
        assert!(path.is_file());
        assert!(!tmp.path().join("progress.json.tmp").exists());
    }

    #[test]
    fn load_progress_if_valid_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let r = load_progress_if_valid(
            &tmp.path().join("missing.json"),
            "tyson",
            "2026-01-01",
            "fish-speech-s2-pro",
            "abcd",
            1,
        )
        .unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn load_progress_if_valid_bails_on_voice_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let p = ProgressJson::fresh("tyson", "2026-01-01", "fish-speech-s2-pro", "abcd", 1);
        let path = tmp.path().join("progress.json");
        write_progress_atomically(&p, &path).unwrap();
        let err =
            load_progress_if_valid(&path, "neil", "2026-01-01", "fish-speech-s2-pro", "abcd", 1)
                .unwrap_err();
        assert!(format!("{err:#}").contains("--force"));
    }

    #[test]
    fn load_progress_if_valid_bails_on_input_sha_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let p = ProgressJson::fresh("tyson", "2026-01-01", "fish-speech-s2-pro", "abcd", 1);
        let path = tmp.path().join("progress.json");
        write_progress_atomically(&p, &path).unwrap();
        let err = load_progress_if_valid(
            &path,
            "tyson",
            "2026-01-01",
            "fish-speech-s2-pro",
            "WXYZ",
            1,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("input"), "expected input mention, got: {msg}");
        assert!(msg.contains("--force"));
    }

    #[test]
    fn load_progress_if_valid_bails_on_total_chunks_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let p = ProgressJson::fresh("tyson", "2026-01-01", "fish-speech-s2-pro", "abcd", 5);
        let path = tmp.path().join("progress.json");
        write_progress_atomically(&p, &path).unwrap();
        let err = load_progress_if_valid(
            &path,
            "tyson",
            "2026-01-01",
            "fish-speech-s2-pro",
            "abcd",
            3,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("chunk count"));
        assert!(msg.contains("--force"));
    }

    // ----- D-2: MockSynth + notify -----------------------------------

    #[tokio::test]
    async fn mock_synth_writes_44100_mono_pcm16_wav() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("chunk.wav");
        let synth = MockSynth::new();
        synth.synth("hello world", &out).await.unwrap();
        let reader = hound::WavReader::open(&out).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 44_100);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, hound::SampleFormat::Int);
        assert_eq!(synth.call_count(), 1);
    }

    #[tokio::test]
    async fn mock_synth_fail_after_returns_err_on_nth_call() {
        let tmp = tempfile::tempdir().unwrap();
        let synth = MockSynth::with_fail_after(3);
        let out1 = tmp.path().join("c1.wav");
        let out2 = tmp.path().join("c2.wav");
        let out3 = tmp.path().join("c3.wav");
        synth.synth("a", &out1).await.unwrap();
        synth.synth("b", &out2).await.unwrap();
        let err = synth.synth("c", &out3).await.unwrap_err();
        assert!(format!("{err:#}").contains("simulated synth failure"));
        assert_eq!(synth.call_count(), 3);
        // The WAV at out3 was never written.
        assert!(!out3.exists());
    }

    /// Plan test #25 (rust-expert R6): notify_note_complete_with calls
    /// the injected Mirror with the right (voice, body) tuple.
    #[test]
    fn notify_note_complete_calls_mirror_with_chunks_and_secs() {
        use crate::notify_macos::{notify_note_complete_with, test_support::RecordingMirror};

        // The notify helper consults VOICEFORGE_MIRROR_NOTIFICATIONS
        // via the cached `enabled()` OnceLock. We can't toggle it
        // mid-test (cache is process-global), so this test only
        // asserts the no-op path is correct: when disabled, mirror
        // is NOT called.
        let mirror = RecordingMirror::default();
        notify_note_complete_with(&mirror, "tyson", 7, 12.3);
        // In the default test process, the env var is unset → enabled
        // returns false → mirror.mirror is NOT called.
        if !crate::notify_macos::enabled() {
            assert!(
                mirror.calls().is_empty(),
                "mirror should not fire when disabled"
            );
        } else {
            // If enabled (someone exported the env var before invoking
            // cargo test), assert the call shape.
            let calls = mirror.calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, "tyson");
            assert!(calls[0].1.contains("7 chunks"));
            assert!(calls[0].1.contains("12.3"));
        }
    }

    /// Companion to the above: prove the formatting logic directly
    /// via the `enabled_with` test-only override. We construct the
    /// notify call's body string the same way `notify_note_complete_with`
    /// does and assert against expected substrings.
    #[test]
    fn notify_body_format_includes_chunks_and_secs() {
        let body = format!("note rendered ({} chunks, {:.1}s)", 7, 12.345);
        assert!(body.contains("7 chunks"));
        assert!(body.contains("12.3s"));
    }

    // ----- D-3: orchestrator (synth_all_chunks) ----------------------

    fn voice_meta() -> VoiceMeta {
        VoiceMeta {
            name: "tyson".into(),
            created_at: "2026-01-01".into(),
            recipe: "fish-speech-s2-pro".into(),
        }
    }

    fn args_with_out(out: &Path) -> NoteArgs {
        NoteArgs {
            voice: "tyson".into(),
            input: Some(PathBuf::from("ignored-for-direct-call")),
            output: out.to_path_buf(),
            force: false,
            cleanup: false,
        }
    }

    /// Plan test #14 (rust-expert nit-5 sharpened): after each chunk
    /// completes, progress.json has the full list with byte-matching
    /// shas and each wav_path file exists + is non-empty.
    #[tokio::test]
    async fn note_run_writes_progress_json_after_each_chunk() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("note.wav");
        let synth = MockSynth::new();
        let cancel = Arc::new(AtomicBool::new(false));
        let raw = "para one\n\npara two\n\npara three";
        let report = synth_all_chunks(
            &synth,
            &voice_meta(),
            raw,
            false,
            &args_with_out(&out),
            cancel,
        )
        .await
        .expect("synth all");
        assert_eq!(report.total_chunks, 3);
        assert_eq!(report.synthesized, vec![0, 1, 2]);
        assert!(report.skipped.is_empty());

        // progress.json has all three with matching shas and existing wavs.
        let raw_json = std::fs::read_to_string(&report.progress_path).unwrap();
        let progress: ProgressJson = serde_json::from_str(&raw_json).unwrap();
        assert_eq!(progress.completed.len(), 3);
        let expected_shas: Vec<String> = chunk_text(
            raw,
            false,
            &voice_meta().name,
            &voice_meta().created_at,
            &voice_meta().recipe,
        )
        .into_iter()
        .map(|c| c.sha256)
        .collect();
        for (i, e) in progress.completed.iter().enumerate() {
            assert_eq!(e.sha256, expected_shas[i], "sha mismatch at index {i}");
            assert!(e.wav_path.is_file(), "wav missing at {:?}", e.wav_path);
            let bytes = std::fs::metadata(&e.wav_path).unwrap().len();
            assert!(bytes > 44, "wav at {:?} is empty/too-short", e.wav_path);
        }

        // No stray .tmp files.
        for entry in std::fs::read_dir(&report.chunks_dir).unwrap() {
            let p = entry.unwrap().path();
            assert!(
                !p.to_string_lossy().ends_with(".tmp"),
                "leftover tmp file: {p:?}"
            );
        }
    }

    /// Plan test #15: resume skips already-done chunks.
    #[tokio::test]
    async fn note_run_skips_completed_chunks_on_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("note.wav");
        let cancel = Arc::new(AtomicBool::new(false));
        let raw = "a\n\nb\n\nc";
        let args = args_with_out(&out);
        let synth1 = MockSynth::new();
        let r1 = synth_all_chunks(&synth1, &voice_meta(), raw, false, &args, cancel.clone())
            .await
            .unwrap();
        assert_eq!(synth1.call_count(), 3);

        // Second run: same input, no force → all chunks skipped.
        let synth2 = MockSynth::new();
        let r2 = synth_all_chunks(&synth2, &voice_meta(), raw, false, &args, cancel)
            .await
            .unwrap();
        assert_eq!(synth2.call_count(), 0);
        assert_eq!(r2.synthesized.len(), 0);
        assert_eq!(r2.skipped.len(), 3);
        // Progress path is stable.
        assert_eq!(r1.progress_path, r2.progress_path);
    }

    /// Plan test #16 (rust-expert R2 negative): resync when WAV is
    /// missing.
    #[tokio::test]
    async fn note_run_resynths_when_chunk_wav_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("note.wav");
        let cancel = Arc::new(AtomicBool::new(false));
        let raw = "a\n\nb\n\nc";
        let args = args_with_out(&out);
        let synth1 = MockSynth::new();
        let r1 = synth_all_chunks(&synth1, &voice_meta(), raw, false, &args, cancel.clone())
            .await
            .unwrap();
        // Delete chunk 1's WAV.
        std::fs::remove_file(chunk_wav_path(&r1.chunks_dir, 1)).unwrap();
        let synth2 = MockSynth::new();
        let r2 = synth_all_chunks(&synth2, &voice_meta(), raw, false, &args, cancel)
            .await
            .unwrap();
        // Only chunk 1 re-synthed.
        assert_eq!(synth2.call_count(), 1);
        assert_eq!(r2.synthesized, vec![1]);
        assert_eq!(r2.skipped, vec![0, 2]);
    }

    /// Plan test #17: force re-synths every chunk.
    #[tokio::test]
    async fn note_run_force_overwrites_chunks_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("note.wav");
        let cancel = Arc::new(AtomicBool::new(false));
        let raw = "a\n\nb\n\nc";

        // First run baseline.
        let args1 = args_with_out(&out);
        let synth1 = MockSynth::new();
        let r1 = synth_all_chunks(&synth1, &voice_meta(), raw, false, &args1, cancel.clone())
            .await
            .unwrap();

        // Pre-stage a sentinel file in chunks_dir so we can assert
        // --force does NOT rm -rf the dir (rust-expert nit-6).
        let sentinel = r1.chunks_dir.join("sentinel.txt");
        std::fs::write(&sentinel, b"survive-me").unwrap();

        // Second run with --force.
        let mut args2 = args_with_out(&out);
        args2.force = true;
        let synth2 = MockSynth::new();
        let r2 = synth_all_chunks(&synth2, &voice_meta(), raw, false, &args2, cancel)
            .await
            .unwrap();
        assert_eq!(synth2.call_count(), 3);
        assert_eq!(r2.synthesized, vec![0, 1, 2]);
        assert!(r2.skipped.is_empty());
        // Sentinel survives — force didn't rm the dir.
        assert!(sentinel.is_file(), "force must not rm chunks dir");
    }

    /// Plan test #19: progress.json voice mismatch bails (covered by
    /// load_progress_if_valid unit tests above — this one exercises
    /// the orchestrator path).
    #[tokio::test]
    async fn note_run_bails_on_progress_json_voice_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("note.wav");
        let cancel = Arc::new(AtomicBool::new(false));
        let raw = "a\n\nb\n\nc";
        let args = args_with_out(&out);
        synth_all_chunks(
            &MockSynth::new(),
            &voice_meta(),
            raw,
            false,
            &args,
            cancel.clone(),
        )
        .await
        .unwrap();
        // Run with a different voice.
        let other = VoiceMeta {
            name: "neil".into(),
            created_at: "2026-01-01".into(),
            recipe: "fish-speech-s2-pro".into(),
        };
        let err = synth_all_chunks(&MockSynth::new(), &other, raw, false, &args, cancel)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("--force"));
    }

    /// Plan test #20 (rust-expert S3): progress.json input_sha256
    /// mismatch bails.
    #[tokio::test]
    async fn note_run_bails_on_progress_json_input_sha_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("note.wav");
        let cancel = Arc::new(AtomicBool::new(false));
        let args = args_with_out(&out);
        synth_all_chunks(
            &MockSynth::new(),
            &voice_meta(),
            "a\n\nb",
            false,
            &args,
            cancel.clone(),
        )
        .await
        .unwrap();
        // Same chunk count (still 2 paragraphs after the swap) but
        // different content → different input_sha256 AND different
        // per-chunk shas.
        let err = synth_all_chunks(
            &MockSynth::new(),
            &voice_meta(),
            "x\n\ny",
            false,
            &args,
            cancel,
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("input file changed"),
            "expected input-changed msg, got: {msg}"
        );
        assert!(msg.contains("--force"));
    }

    /// Plan test #21 (rust-expert S2): progress.json total_chunks
    /// mismatch bails.
    #[tokio::test]
    async fn note_run_bails_on_progress_json_total_chunks_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("note.wav");
        let cancel = Arc::new(AtomicBool::new(false));
        let args = args_with_out(&out);
        synth_all_chunks(
            &MockSynth::new(),
            &voice_meta(),
            "a\n\nb",
            false,
            &args,
            cancel.clone(),
        )
        .await
        .unwrap();
        // Add a paragraph → total_chunks changes 2 → 3, but also
        // input_sha256 changes. Both bail conditions fire; either
        // message is acceptable.
        let err = synth_all_chunks(
            &MockSynth::new(),
            &voice_meta(),
            "a\n\nb\n\nc",
            false,
            &args,
            cancel,
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--force"), "expected --force hint, got: {msg}");
    }

    /// Empty input bails up front with no chunks written.
    #[tokio::test]
    async fn note_run_bails_on_empty_input() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("note.wav");
        let cancel = Arc::new(AtomicBool::new(false));
        let args = args_with_out(&out);
        let err = synth_all_chunks(
            &MockSynth::new(),
            &voice_meta(),
            "   ",
            false,
            &args,
            cancel,
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("no narrate-able text"));
    }

    /// Cancel flag set before the first chunk: bails cleanly without
    /// any synth call.
    #[tokio::test]
    async fn note_run_bails_when_cancel_flag_pre_set() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("note.wav");
        let cancel = Arc::new(AtomicBool::new(true));
        let args = args_with_out(&out);
        let synth = MockSynth::new();
        let err = synth_all_chunks(&synth, &voice_meta(), "a\n\nb", false, &args, cancel)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("cancelled"));
        assert_eq!(synth.call_count(), 0);
    }

    /// V1-voice helper renders the expected migrate hint.
    #[test]
    fn v1_voice_bail_message_includes_migrate_hint() {
        let msg = v1_voice_bail_message("peter");
        assert!(msg.contains("voiceforge voices migrate peter"));
        assert!(msg.contains("schema 1"));
    }

    // ----- D-4: per-chunk format verify + concat list ----------------

    // Plan test #23 (rust-expert nit-7 parametrized): four sibling tests
    // each vary one axis of the WAV spec. All four must bail with a
    // message naming the offending chunk.
    fn write_wav_with_spec(path: &Path, spec: hound::WavSpec, samples: u32) {
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for _ in 0..samples {
            // Sample type depends on the spec; for bps=16 Int we
            // write i16. For other variants we still write i16 (hound
            // will write the right encoding for the spec).
            if spec.sample_format == hound::SampleFormat::Int && spec.bits_per_sample == 16 {
                w.write_sample(0i16).unwrap();
            } else if spec.sample_format == hound::SampleFormat::Int && spec.bits_per_sample == 24 {
                w.write_sample(0i32).unwrap();
            } else if spec.sample_format == hound::SampleFormat::Float {
                w.write_sample(0.0f32).unwrap();
            } else {
                w.write_sample(0i16).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    #[test]
    fn verify_chunk_formats_bails_on_channels_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        write_wav_with_spec(
            &chunk_wav_path(tmp.path(), 0),
            hound::WavSpec {
                channels: 2,
                sample_rate: 44_100,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
            100,
        );
        let err = verify_chunk_formats(tmp.path(), 1).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("channels=2"), "got: {msg}");
        assert!(msg.contains("chunk 0"));
    }

    #[test]
    fn verify_chunk_formats_bails_on_sample_rate_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        write_wav_with_spec(
            &chunk_wav_path(tmp.path(), 0),
            hound::WavSpec {
                channels: 1,
                sample_rate: 32_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
            100,
        );
        let err = verify_chunk_formats(tmp.path(), 1).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("sample_rate=32000"), "got: {msg}");
    }

    #[test]
    fn verify_chunk_formats_bails_on_bits_per_sample_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        write_wav_with_spec(
            &chunk_wav_path(tmp.path(), 0),
            hound::WavSpec {
                channels: 1,
                sample_rate: 44_100,
                bits_per_sample: 24,
                sample_format: hound::SampleFormat::Int,
            },
            100,
        );
        let err = verify_chunk_formats(tmp.path(), 1).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("bits_per_sample=24"), "got: {msg}");
    }

    #[test]
    fn verify_chunk_formats_bails_on_sample_format_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        write_wav_with_spec(
            &chunk_wav_path(tmp.path(), 0),
            hound::WavSpec {
                channels: 1,
                sample_rate: 44_100,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
            100,
        );
        let err = verify_chunk_formats(tmp.path(), 1).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Float"), "got: {msg}");
    }

    #[test]
    fn verify_chunk_formats_accepts_canonical_44100_mono_pcm16() {
        let tmp = tempfile::tempdir().unwrap();
        write_silent_wav_44100_mono(&chunk_wav_path(tmp.path(), 0), 100).unwrap();
        write_silent_wav_44100_mono(&chunk_wav_path(tmp.path(), 1), 100).unwrap();
        verify_chunk_formats(tmp.path(), 2).unwrap();
    }

    #[test]
    fn write_concat_list_uses_relative_filenames() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_concat_list(tmp.path(), 3).unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        // No absolute paths — only relative `chunk_NNNN.wav` literals.
        assert!(!body.contains(tmp.path().to_string_lossy().as_ref()));
        assert!(body.contains("file 'chunk_0000.wav'"));
        assert!(body.contains("file 'chunk_0001.wav'"));
        assert!(body.contains("file 'chunk_0002.wav'"));
    }

    // ----- D-5: adapter + wav_duration_secs -------------------------

    #[test]
    fn wav_duration_secs_computes_from_samples_over_rate() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.wav");
        write_silent_wav_44100_mono(&path, 44_100).unwrap(); // 1.0 sec
        let secs = wav_duration_secs(&path).unwrap();
        assert!((secs - 1.0).abs() < 1e-6, "got {secs}");
    }

    #[test]
    fn chunks_dir_for_appends_dot_chunks_suffix() {
        let p = chunks_dir_for(Path::new("/tmp/note.wav"));
        assert_eq!(p, Path::new("/tmp/note.wav.chunks"));
    }

    #[test]
    fn progress_path_for_appends_dot_progress_json() {
        let p = progress_path_for(Path::new("/tmp/note.wav"));
        assert_eq!(p, Path::new("/tmp/note.wav.progress.json"));
    }

    // ----- D-6 + rust-expert R3: partial-resume-of-partial-resume ----

    /// The load-bearing soundness claim: a render that's killed
    /// mid-flight resumes correctly, even if that resume is itself
    /// killed mid-flight. We simulate "kill" via MockSynth's
    /// `fail_after` field, which returns `Err` on the Nth call.
    ///
    /// Pipeline: 5 paragraphs -> 5 chunks.
    /// Run 1: MockSynth fails on call 3 -> chunks 0,1 written.
    /// Run 2: MockSynth fails on call 3 (= chunk 4 overall) ->
    ///         chunks 2,3 written; chunk 4 errors.
    /// Run 3: clean MockSynth -> chunk 4 written.
    /// Final: progress.json has 5 completed entries; all WAVs exist.
    #[tokio::test]
    async fn note_partial_resume_of_partial_resume_three_run_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("note.wav");
        let raw = "a\n\nb\n\nc\n\nd\n\ne";
        let args = args_with_out(&out);
        let cancel = Arc::new(AtomicBool::new(false));

        // Run 1: fails on call 3 (after writing chunks 0 and 1).
        let s1 = MockSynth::with_fail_after(3);
        let err1 = synth_all_chunks(&s1, &voice_meta(), raw, false, &args, cancel.clone())
            .await
            .unwrap_err();
        assert!(format!("{err1:#}").contains("simulated synth failure"));
        let chunks_dir = chunks_dir_for(&out);
        assert!(chunks_dir.join("chunk_0000.wav").is_file());
        assert!(chunks_dir.join("chunk_0001.wav").is_file());
        assert!(!chunks_dir.join("chunk_0002.wav").exists());
        let raw_json = std::fs::read_to_string(progress_path_for(&out)).unwrap();
        let p1: ProgressJson = serde_json::from_str(&raw_json).unwrap();
        assert_eq!(p1.completed.len(), 2);

        // Run 2: fails on call 3 (= chunk 4 — counter is per-instance).
        // Should skip 0,1 (done) and write 2,3 before failing.
        let s2 = MockSynth::with_fail_after(3);
        let err2 = synth_all_chunks(&s2, &voice_meta(), raw, false, &args, cancel.clone())
            .await
            .unwrap_err();
        assert!(format!("{err2:#}").contains("simulated synth failure"));
        assert!(chunks_dir.join("chunk_0002.wav").is_file());
        assert!(chunks_dir.join("chunk_0003.wav").is_file());
        assert!(!chunks_dir.join("chunk_0004.wav").exists());
        let raw_json = std::fs::read_to_string(progress_path_for(&out)).unwrap();
        let p2: ProgressJson = serde_json::from_str(&raw_json).unwrap();
        assert_eq!(p2.completed.len(), 4);

        // Run 3: clean synth finishes the last chunk.
        let s3 = MockSynth::new();
        let r3 = synth_all_chunks(&s3, &voice_meta(), raw, false, &args, cancel)
            .await
            .expect("third run should complete");
        assert_eq!(s3.call_count(), 1, "should only re-synth chunk 4");
        assert_eq!(r3.synthesized, vec![4]);
        assert_eq!(r3.skipped, vec![0, 1, 2, 3]);
        assert!(chunks_dir.join("chunk_0004.wav").is_file());
        let raw_json = std::fs::read_to_string(progress_path_for(&out)).unwrap();
        let pfinal: ProgressJson = serde_json::from_str(&raw_json).unwrap();
        assert_eq!(pfinal.completed.len(), 5);
        for i in 0..5 {
            assert_eq!(pfinal.completed[i].index, i);
            assert!(pfinal.completed[i].wav_path.is_file());
        }
    }
}
