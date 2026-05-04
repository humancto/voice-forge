//! Path discovery helpers used by config + rules + future modules.
//!
//! Walks up from the running binary (or the workspace at compile time)
//! looking for a sibling `configs/` dir, so the CLI works whether it
//! was invoked from the workspace root, the manifest dir, or installed
//! to `/usr/local/bin`.

use std::env;
use std::path::{Path, PathBuf};

/// Find the repo's `configs/` directory by walking up from the
/// directory containing the current executable, then from
/// `CARGO_MANIFEST_DIR` (compile-time fallback for `cargo run` /
/// `cargo test` from arbitrary CWDs). Returns `None` when neither
/// search finds a `configs/` sibling — callers should treat that as
/// "use embedded defaults."
pub fn repo_config_dir() -> Option<PathBuf> {
    if let Ok(exe) = env::current_exe() {
        if let Some(found) = walk_up_for(&exe, "configs") {
            return Some(found);
        }
    }

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    walk_up_for(&manifest, "configs")
}

/// User-config root, in order of precedence:
/// 1. `$VOICEFORGE_HOME` if set
/// 2. `~/.voiceforge`
///
/// Returns `None` when `$VOICEFORGE_HOME` is unset *and* the home
/// directory can't be discovered.
pub fn user_home() -> Option<PathBuf> {
    if let Ok(custom) = env::var("VOICEFORGE_HOME") {
        if !custom.is_empty() {
            return Some(PathBuf::from(custom));
        }
    }
    env::var_os("HOME").map(|h| PathBuf::from(h).join(".voiceforge"))
}

fn walk_up_for(start: &Path, sibling: &str) -> Option<PathBuf> {
    let mut cursor = start;
    loop {
        let candidate = cursor.join(sibling);
        if candidate.is_dir() {
            return Some(candidate);
        }
        match cursor.parent() {
            Some(parent) if parent != cursor => cursor = parent,
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn walk_up_finds_sibling_in_ancestor() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let nested = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(tmp.path().join("configs")).unwrap();

        let found = walk_up_for(&nested, "configs").expect("found");
        assert_eq!(found, tmp.path().join("configs"));
    }

    #[test]
    fn walk_up_returns_none_when_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(walk_up_for(tmp.path(), "configs").is_none());
    }

    #[test]
    #[serial]
    fn user_home_prefers_env_override() {
        // Use a tempdir so we don't depend on the host env.
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("VOICEFORGE_HOME").ok();
        std::env::set_var("VOICEFORGE_HOME", tmp.path());

        let got = user_home().expect("home");
        assert_eq!(got, tmp.path());

        // Restore.
        match prev {
            Some(v) => std::env::set_var("VOICEFORGE_HOME", v),
            None => std::env::remove_var("VOICEFORGE_HOME"),
        }
    }
}
