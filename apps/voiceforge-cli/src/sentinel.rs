//! Sentinel-bounded shell-script-block helpers — shared between
//! `shell_init` (zsh/bash rc files, ROADMAP 3.1) and `git_hooks`
//! (per-hook scripts, ROADMAP 3.2).
//!
//! All managed blocks are wrapped in:
//!
//! ```text
//! # >>> voiceforge >>>
//! ...managed content...
//! # <<< voiceforge <<<
//! ```
//!
//! These helpers parse, replace, or strip such blocks while leaving
//! every other line of the host file untouched.

pub const SENTINEL_OPEN: &str = "# >>> voiceforge >>>";
pub const SENTINEL_CLOSE: &str = "# <<< voiceforge <<<";

/// Whether `replace_or_append_block` newly created the block or
/// replaced an existing one.
#[derive(Debug, PartialEq, Eq)]
pub enum BlockAction {
    Created,
    Replaced,
}

/// Look for a stale "voiceforge ..." invocation OUTSIDE the
/// sentinel-delimited block. The caller supplies a predicate that
/// inspects the leading-whitespace-trimmed line and returns true if
/// it looks like a stale invocation of the caller's specific entry
/// point (e.g. `"voiceforge shell-init"` for shell_init,
/// `"voiceforge send git_"` for git_hooks).
///
/// Returns `Some((1-based-lineno, raw-line))` on first hit, `None`
/// otherwise. Comment lines (`#` or whitespace+`#`) are skipped.
pub fn find_stale_invocation<F>(content: &str, is_stale: F) -> Option<(usize, String)>
where
    F: Fn(&str) -> bool,
{
    let mut in_sentinel = false;
    for (lineno, line) in content.lines().enumerate() {
        if line.trim() == SENTINEL_OPEN {
            in_sentinel = true;
            continue;
        }
        if line.trim() == SENTINEL_CLOSE {
            in_sentinel = false;
            continue;
        }
        if in_sentinel {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        if is_stale(trimmed) {
            return Some((lineno + 1, line.to_string()));
        }
    }
    None
}

/// If `content` already contains a sentinel-bounded block, replace
/// its body with `block`. Otherwise append `block` after a leading
/// blank line (only if `content` is non-empty and doesn't already end
/// in a blank line). `block` should already include the sentinels.
pub fn replace_or_append_block(content: &str, block: &str) -> (String, BlockAction) {
    let lines: Vec<&str> = content.lines().collect();
    let open_idx = lines.iter().position(|l| l.trim() == SENTINEL_OPEN);
    let close_idx = lines.iter().position(|l| l.trim() == SENTINEL_CLOSE);

    match (open_idx, close_idx) {
        (Some(open), Some(close)) if close >= open => {
            let mut out = String::new();
            for (i, line) in lines.iter().enumerate() {
                if i < open || i > close {
                    out.push_str(line);
                    out.push('\n');
                }
                if i == open {
                    out.push_str(block);
                    if !block.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
            if !content.ends_with('\n') && out.ends_with('\n') {
                out.pop();
            }
            (out, BlockAction::Replaced)
        }
        _ => {
            let mut out = content.to_string();
            if !out.is_empty() && !out.ends_with("\n\n") {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            out.push_str(block);
            if !block.ends_with('\n') {
                out.push('\n');
            }
            (out, BlockAction::Created)
        }
    }
}

/// Strip the sentinel-bounded block from `content`. Also drops a
/// single trailing blank line immediately following the close
/// sentinel (cosmetic — keeps the rc tidy).
///
/// Returns `(new_content, was_present)`.
pub fn strip_block(content: &str) -> (String, bool) {
    let lines: Vec<&str> = content.lines().collect();
    let open_idx = lines.iter().position(|l| l.trim() == SENTINEL_OPEN);
    let close_idx = lines.iter().position(|l| l.trim() == SENTINEL_CLOSE);

    match (open_idx, close_idx) {
        (Some(open), Some(close)) if close >= open => {
            let mut out: Vec<&str> = Vec::with_capacity(lines.len());
            let drop_trailing_blank = lines.get(close + 1).is_some_and(|l| l.trim().is_empty());
            for (i, line) in lines.iter().enumerate() {
                if i >= open && i <= close {
                    continue;
                }
                if drop_trailing_blank && i == close + 1 {
                    continue;
                }
                out.push(line);
            }
            let mut joined = out.join("\n");
            if content.ends_with('\n') {
                joined.push('\n');
            }
            (joined, true)
        }
        _ => (content.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(s: &str, needle: &str) -> usize {
        s.matches(needle).count()
    }

    #[test]
    fn append_into_empty_content() {
        let (out, action) =
            replace_or_append_block("", "# >>> voiceforge >>>\nbody\n# <<< voiceforge <<<\n");
        assert_eq!(action, BlockAction::Created);
        assert!(out.contains("body"));
    }

    #[test]
    fn append_after_existing_content() {
        let original = "alias gs='git status'\n";
        let (out, action) = replace_or_append_block(
            original,
            "# >>> voiceforge >>>\nbody\n# <<< voiceforge <<<\n",
        );
        assert_eq!(action, BlockAction::Created);
        assert!(out.starts_with("alias gs='git status'"));
        assert_eq!(count(&out, SENTINEL_OPEN), 1);
    }

    #[test]
    fn replace_inside_existing_block_is_idempotent() {
        let mut content = "alias gs='git status'\n".to_string();
        let block = "# >>> voiceforge >>>\nbody-v1\n# <<< voiceforge <<<\n";
        let (after_first, _) = replace_or_append_block(&content, block);
        content = after_first;
        let block_v2 = "# >>> voiceforge >>>\nbody-v2\n# <<< voiceforge <<<\n";
        let (after_second, action) = replace_or_append_block(&content, block_v2);
        assert_eq!(action, BlockAction::Replaced);
        assert_eq!(count(&after_second, SENTINEL_OPEN), 1);
        assert!(after_second.contains("body-v2"));
        assert!(!after_second.contains("body-v1"));
    }

    #[test]
    fn strip_block_clean_preserves_other_content() {
        let original = "alias gs='git status'\n\n# >>> voiceforge >>>\nbody\n# <<< voiceforge <<<\n\nexport FOO=bar\n";
        let (out, was) = strip_block(original);
        assert!(was);
        assert!(out.contains("alias gs='git status'"));
        assert!(out.contains("export FOO=bar"));
        assert!(!out.contains("body"));
        assert!(!out.contains(SENTINEL_OPEN));
    }

    #[test]
    fn strip_block_noop_when_absent() {
        let original = "alias gs='git status'\n";
        let (out, was) = strip_block(original);
        assert!(!was);
        assert_eq!(out, original);
    }

    #[test]
    fn find_stale_invocation_uses_predicate() {
        let content = "alias x='y'\n# voiceforge shell-init zsh -- this is a comment, skip\n  voiceforge shell-init zsh\n# >>> voiceforge >>>\nvoiceforge shell-init zsh\n# <<< voiceforge <<<\n";
        let hit = find_stale_invocation(content, |l| {
            l.contains("voiceforge") && l.contains("shell-init")
        });
        // Should hit the third line (1-indexed); skip the comment AND
        // the line inside the sentinel block.
        assert_eq!(hit.unwrap().0, 3);
    }
}
