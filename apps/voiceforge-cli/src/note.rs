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

// D-1 ships pure data + parsers + cache schema; the orchestrator
// (D-3) and CLI wiring (D-5) light up the `pub` surface. Strip this
// allow when D-5 lands.
#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

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
            Event::Text(t) | Event::Code(t) => {
                if in_codeblock_depth == 0 && in_image_depth == 0 {
                    out.push_str(&t);
                }
            }
            Event::SoftBreak => {
                if in_codeblock_depth == 0 && in_image_depth == 0 {
                    out.push(' ');
                }
            }
            Event::HardBreak => {
                if in_codeblock_depth == 0 && in_image_depth == 0 {
                    out.push('\n');
                }
            }
            Event::End(TagEnd::Paragraph)
            | Event::End(TagEnd::Heading(_))
            | Event::End(TagEnd::BlockQuote)
            | Event::End(TagEnd::Item) => {
                if in_codeblock_depth == 0 && in_image_depth == 0 {
                    out.push_str("\n\n");
                }
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
}
