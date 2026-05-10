//! `voiceforge install git-hooks` (ROADMAP 3.2) — installs sentinel-
//! bounded blocks into `.git/hooks/` (or `core.hooksPath`) for the
//! four hooks that fire daemon events:
//!
//!   post-commit    -> git_commit
//!   post-merge     -> git_merge
//!   post-rewrite   -> git_rewrite
//!   pre-push       -> git_push
//!
//! Mirror of `shell_init` (zsh/bash) for ROADMAP 3.1 — same sentinel-
//! bounded block, idempotent install, stale-invocation guard,
//! per-hook status.
//!
//! See `.planning/git-hooks-install.plan.md` for the design audit
//! (rust-expert plan v2 APPROVE).

use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::sentinel::{self, BlockAction};
use crate::shell_init::BinaryHint;

const SIDECAR_FILENAME: &str = ".voiceforge-install.json";

/// Sidecar JSON written into the hooks dir per install. Records which
/// hook files we created (`Created`) vs which we appended to
/// (`AppendedToExisting`) — needed at uninstall time so we don't
/// delete a user's husk hook just because it now matches our auto-
/// shebang after we strip our block.
#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct InstallSidecar {
    actions: BTreeMap<String, String>, // hook-name -> "Created" | "AppendedToExisting"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHook {
    PostCommit,
    PostMerge,
    PostRewrite,
    PrePush,
}

impl GitHook {
    pub const ALL: [GitHook; 4] = [
        GitHook::PostCommit,
        GitHook::PostMerge,
        GitHook::PostRewrite,
        GitHook::PrePush,
    ];

    pub fn name(self) -> &'static str {
        match self {
            GitHook::PostCommit => "post-commit",
            GitHook::PostMerge => "post-merge",
            GitHook::PostRewrite => "post-rewrite",
            GitHook::PrePush => "pre-push",
        }
    }

    fn render_body(self, invocation: &str) -> String {
        match self {
            GitHook::PostCommit => format!(
                "{open}\n\
{{\n\
  short=$(git rev-parse --short HEAD 2>/dev/null || echo \"?\")\n\
  subject=$(git log -1 --pretty=%s 2>/dev/null | cut -c1-80)\n\
  {invocation} send git_commit \\\n\
    --message \"$short $subject\" \\\n\
    </dev/null >/dev/null 2>&1 &\n\
}} 2>/dev/null\n\
{close}\n",
                open = sentinel::SENTINEL_OPEN,
                close = sentinel::SENTINEL_CLOSE,
                invocation = invocation,
            ),
            GitHook::PostMerge => format!(
                "{open}\n\
{{\n\
  squash=\"${{1:-?}}\"\n\
  {invocation} send git_merge \\\n\
    --message \"squash=$squash\" \\\n\
    </dev/null >/dev/null 2>&1 &\n\
}} 2>/dev/null\n\
{close}\n",
                open = sentinel::SENTINEL_OPEN,
                close = sentinel::SENTINEL_CLOSE,
                invocation = invocation,
            ),
            GitHook::PostRewrite => format!(
                "{open}\n\
{{\n\
  cmd=\"${{1:-?}}\"\n\
  {invocation} send git_rewrite \\\n\
    --message \"$cmd\" \\\n\
    </dev/null >/dev/null 2>&1 &\n\
}} 2>/dev/null\n\
{close}\n",
                open = sentinel::SENTINEL_OPEN,
                close = sentinel::SENTINEL_CLOSE,
                invocation = invocation,
            ),
            GitHook::PrePush => format!(
                "{open}\n\
cat >/dev/null  # consume ref-updates stdin so git doesn't half-drain\n\
{{\n\
  remote=\"${{1:-?}}\"\n\
  url=\"${{2:-?}}\"\n\
  {invocation} send git_push \\\n\
    --message \"$remote $url\" \\\n\
    </dev/null >/dev/null 2>&1 &\n\
}} 2>/dev/null\n\
exit 0\n\
{close}\n",
                open = sentinel::SENTINEL_OPEN,
                close = sentinel::SENTINEL_CLOSE,
                invocation = invocation,
            ),
        }
    }
}

const AUTO_SHEBANG: &str = "#!/usr/bin/env sh\nset -eu\n";

/// Render the invocation prefix for a hook body. Mirrors
/// `shell_init::render_hook`'s pattern.
fn render_invocation(hint: &BinaryHint) -> String {
    match hint {
        BinaryHint::DiscoverViaPath => {
            "command -v voiceforge >/dev/null 2>&1 && voiceforge".to_string()
        }
        BinaryHint::Pinned(path) => {
            // Single-quote shell-escape; survives spaces / dollars / quotes.
            let escaped = single_quote(&path.display().to_string());
            format!("[ -x {escaped} ] && {escaped}")
        }
    }
}

fn single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

// -- repo + hooks-dir discovery --------------------------------------

/// Walk up from `start` looking for a `.git` directory or file. The
/// `.git` file form (`gitdir: <path>`) is what `git worktree add` and
/// submodules write; we resolve that indirection.
pub fn discover_repo(start: &Path) -> Result<PathBuf> {
    let start = if start.is_relative() {
        std::env::current_dir()?.join(start)
    } else {
        start.to_path_buf()
    };
    let mut cursor: &Path = &start;
    loop {
        let dot_git = cursor.join(".git");
        if dot_git.is_dir() {
            return Ok(cursor.to_path_buf());
        }
        if dot_git.is_file() {
            // Worktree / submodule .git file — we still treat THIS
            // dir as the repo root for hooks-dir discovery; the
            // `.git` file's `gitdir:` indirection only matters when
            // running git commands, which we shell out to (those
            // honor `.git` natively).
            return Ok(cursor.to_path_buf());
        }
        match cursor.parent() {
            Some(p) if p != cursor => cursor = p,
            _ => bail!(
                "not a git repo: walked up from {} and found no .git",
                start.display()
            ),
        }
    }
}

/// Resolve the hooks dir for `repo`. Honors `core.hooksPath` (per-repo
/// or global) via `git -C <repo> config --get core.hooksPath`. Empty
/// stdout / non-zero exit -> default `<repo>/.git/hooks`. Relative
/// `core.hooksPath` resolves against `<repo>`.
pub fn resolve_hooks_dir(repo: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args([
            "-C",
            repo.to_str().ok_or_else(|| anyhow!("non-utf8 repo path"))?,
        ])
        .args(["config", "--get", "core.hooksPath"])
        .output();
    let configured = match output {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).trim().to_string()),
        _ => None,
    };
    if let Some(path) = configured.filter(|p| !p.is_empty()) {
        let p = PathBuf::from(&path);
        if p.is_absolute() {
            return Ok(p);
        }
        return Ok(repo.join(p));
    }
    Ok(repo.join(".git").join("hooks"))
}

// -- framework collision detection -----------------------------------

/// Markers we look for in an existing hook file to detect that
/// another framework owns it. Detection is a soft warning, not a
/// refusal — users can wire `voiceforge install git-hooks` into
/// their tool's post-install if they want symbiosis.
fn detect_framework(content: &str) -> Option<&'static str> {
    let lower = content.to_lowercase();
    if lower.contains("husky.sh") || lower.contains("# husky") {
        Some("husky")
    } else if lower.contains("lefthook") {
        Some("lefthook")
    } else if lower.contains("pre-commit") && lower.contains("framework") {
        Some("pre-commit framework")
    } else {
        None
    }
}

// -- install / uninstall / status ------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub struct HookInstallReport {
    pub hook: GitHook,
    pub action: BlockAction,
    pub framework_warning: Option<&'static str>,
}

#[derive(Debug)]
pub struct InstallReport {
    pub hooks_dir: PathBuf,
    pub per_hook: Vec<HookInstallReport>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HookUninstallAction {
    Removed,
    NotPresent,
    HookFileDeleted,
}

#[derive(Debug)]
pub struct HookUninstallReport {
    /// Which hook this report describes. Currently informational —
    /// the dispatcher in `main.rs` aggregates by `action` only — but
    /// kept on the public report so a future per-hook stderr line
    /// or test consumer can use it.
    #[allow(dead_code)]
    pub hook: GitHook,
    pub action: HookUninstallAction,
}

#[derive(Debug)]
pub struct UninstallReport {
    pub hooks_dir: PathBuf,
    pub per_hook: Vec<HookUninstallReport>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct HookStatus {
    pub hook: GitHook,
    pub path: PathBuf,
    pub installed: bool,
}

fn is_stale_git_send(line: &str) -> bool {
    line.contains("voiceforge") && line.contains("send git_")
}

/// Install (or replace) the voiceforge block in all 4 hooks under
/// `repo`'s hooks dir. Idempotent. Refuses (without `--force`) if any
/// hook contains a stale `voiceforge send git_*` invocation outside
/// the sentinels.
pub fn install(repo: &Path, hint: &BinaryHint, force: bool, quiet: bool) -> Result<InstallReport> {
    let hooks_dir = resolve_hooks_dir(repo)?;
    std::fs::create_dir_all(&hooks_dir)
        .with_context(|| format!("creating hooks dir {}", hooks_dir.display()))?;

    let invocation = render_invocation(hint);
    let mut sidecar = read_sidecar(&hooks_dir).unwrap_or_default();
    let mut reports = Vec::with_capacity(GitHook::ALL.len());

    // Pre-flight: stale-invocation guard across all 4 hooks.
    if !force {
        for hook in GitHook::ALL {
            let path = hooks_dir.join(hook.name());
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            if let Some((lineno, line)) =
                sentinel::find_stale_invocation(&content, is_stale_git_send)
            {
                bail!(
                    "{}:{lineno}: found a stale `voiceforge send git_*` invocation outside the managed block:\n  {}\n\n\
                    Re-running install would result in the hook firing twice. Either:\n  \
                    1. Remove the stale line by hand, or\n  \
                    2. Re-run with --force to install anyway (the stale line will keep firing)",
                    path.display(),
                    line.trim()
                );
            }
        }
    }

    // Install per-hook.
    for hook in GitHook::ALL {
        let path = hooks_dir.join(hook.name());
        let prior_content = std::fs::read_to_string(&path).ok();
        let pre_existing = prior_content.is_some();
        let mut content = prior_content.unwrap_or_default();

        // Framework warning (only on existing files).
        let framework = if pre_existing {
            detect_framework(&content)
        } else {
            None
        };
        if let Some(fw) = framework {
            if !quiet {
                eprintln!(
                    "voiceforge: warning: {} hook contains markers for {fw}; \
                     our block may be clobbered when {fw} regenerates this hook. \
                     Consider wiring `voiceforge install git-hooks` into {fw}'s post-install.",
                    hook.name()
                );
            }
        }

        // For brand-new hook files, prepend the auto-shebang so the
        // file is a valid shell script even before our block fires.
        if !pre_existing {
            content.push_str(AUTO_SHEBANG);
        }

        let block = hook.render_body(&invocation);
        let (new_content, action) = sentinel::replace_or_append_block(&content, &block);

        std::fs::write(&path, new_content)
            .with_context(|| format!("writing {}", path.display()))?;
        set_executable(&path, pre_existing)?;

        // Track install action in sidecar (only when we Created; if
        // we appended to an existing hook, uninstall must NOT delete
        // it).
        let sidecar_value = if pre_existing {
            "AppendedToExisting"
        } else {
            "Created"
        };
        sidecar
            .actions
            .insert(hook.name().to_string(), sidecar_value.to_string());

        reports.push(HookInstallReport {
            hook,
            action,
            framework_warning: framework,
        });
    }

    write_sidecar(&hooks_dir, &sidecar)?;

    Ok(InstallReport {
        hooks_dir,
        per_hook: reports,
    })
}

/// Strip our sentinel block from every hook. If we created the hook
/// AND the post-strip content equals the auto-shebang exactly, delete
/// the hook file too. Otherwise preserve the husk so user content
/// (or our own shebang stub if they may have customized) survives.
pub fn uninstall(repo: &Path) -> Result<UninstallReport> {
    let hooks_dir = resolve_hooks_dir(repo)?;
    let sidecar = read_sidecar(&hooks_dir).unwrap_or_default();
    let mut reports = Vec::with_capacity(GitHook::ALL.len());

    for hook in GitHook::ALL {
        let path = hooks_dir.join(hook.name());
        let content = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                reports.push(HookUninstallReport {
                    hook,
                    action: HookUninstallAction::NotPresent,
                });
                continue;
            }
            Err(e) => return Err(anyhow!("reading {}: {e}", path.display())),
        };
        let (new_content, was_present) = sentinel::strip_block(&content);
        if !was_present {
            reports.push(HookUninstallReport {
                hook,
                action: HookUninstallAction::NotPresent,
            });
            continue;
        }

        let we_created = sidecar
            .actions
            .get(hook.name())
            .map(|s| s == "Created")
            .unwrap_or(false);

        let post_strip_trimmed = new_content.trim();
        let auto_shebang_trimmed = AUTO_SHEBANG.trim();

        if we_created && post_strip_trimmed == auto_shebang_trimmed {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            reports.push(HookUninstallReport {
                hook,
                action: HookUninstallAction::HookFileDeleted,
            });
        } else {
            std::fs::write(&path, &new_content)
                .with_context(|| format!("writing {}", path.display()))?;
            reports.push(HookUninstallReport {
                hook,
                action: HookUninstallAction::Removed,
            });
        }
    }

    // Best-effort: remove sidecar if every hook is now NotPresent or
    // HookFileDeleted. Leaves it on AppendedToExisting so a future
    // install knows we appended.
    let any_left = reports
        .iter()
        .any(|r| matches!(r.action, HookUninstallAction::Removed));
    if !any_left {
        let _ = std::fs::remove_file(hooks_dir.join(SIDECAR_FILENAME));
    }

    Ok(UninstallReport {
        hooks_dir,
        per_hook: reports,
    })
}

pub fn status(repo: &Path) -> Result<Vec<HookStatus>> {
    let hooks_dir = resolve_hooks_dir(repo)?;
    Ok(GitHook::ALL
        .iter()
        .map(|&hook| {
            let path = hooks_dir.join(hook.name());
            let installed = std::fs::read_to_string(&path)
                .map(|c| {
                    c.contains(sentinel::SENTINEL_OPEN) && c.contains(sentinel::SENTINEL_CLOSE)
                })
                .unwrap_or(false);
            HookStatus {
                hook,
                path,
                installed,
            }
        })
        .collect())
}

// -- mode + sidecar I/O ----------------------------------------------

#[cfg(unix)]
fn set_executable(path: &Path, pre_existing: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let target_mode = if pre_existing {
        let cur = std::fs::metadata(path)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o644);
        cur | 0o111
    } else {
        0o755
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(target_mode))
        .with_context(|| format!("chmod {target_mode:o} on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _pre_existing: bool) -> Result<()> {
    // Windows has no chmod; git on Windows uses a separate
    // executable-bit semantic. Out of scope for 3.2.
    Ok(())
}

fn read_sidecar(hooks_dir: &Path) -> Result<InstallSidecar> {
    let path = hooks_dir.join(SIDECAR_FILENAME);
    let body = std::fs::read_to_string(&path).context("read sidecar")?;
    let parsed =
        serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    Ok(parsed)
}

fn write_sidecar(hooks_dir: &Path, sidecar: &InstallSidecar) -> Result<()> {
    let path = hooks_dir.join(SIDECAR_FILENAME);
    let body = serde_json::to_string_pretty(sidecar)?;
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn init_git(repo: &Path) {
        Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(repo)
            .status()
            .expect("git init");
    }

    fn discover() -> BinaryHint {
        BinaryHint::DiscoverViaPath
    }

    #[test]
    fn discover_repo_finds_git_dir_in_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        init_git(tmp.path());
        let nested = tmp.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        let r = discover_repo(&nested).expect("discover");
        assert_eq!(
            r.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn discover_repo_handles_git_file_for_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        // Don't init git; just write a `.git` FILE (worktree pattern).
        fs::write(tmp.path().join(".git"), b"gitdir: /not/real\n").unwrap();
        let r = discover_repo(tmp.path()).expect("discover");
        assert_eq!(
            r.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn discover_repo_errors_outside_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let err = discover_repo(tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("not a git repo"));
    }

    #[test]
    fn install_writes_all_four_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        init_git(tmp.path());
        let report = install(tmp.path(), &discover(), false, true).expect("install");
        assert_eq!(report.per_hook.len(), 4);
        for h in GitHook::ALL {
            let p = report.hooks_dir.join(h.name());
            assert!(p.is_file(), "missing hook file: {}", p.display());
            let body = fs::read_to_string(&p).unwrap();
            assert!(body.contains(sentinel::SENTINEL_OPEN));
            assert!(body.contains(sentinel::SENTINEL_CLOSE));
        }
    }

    #[test]
    fn install_preserves_existing_hook_content() {
        let tmp = tempfile::tempdir().unwrap();
        init_git(tmp.path());
        let pre = "#!/usr/bin/env sh\necho user-pre-commit\n";
        let post_commit = tmp.path().join(".git/hooks/post-commit");
        fs::write(&post_commit, pre).unwrap();
        install(tmp.path(), &discover(), false, true).expect("install");
        let body = fs::read_to_string(&post_commit).unwrap();
        assert!(body.contains("echo user-pre-commit"));
        assert!(body.contains(sentinel::SENTINEL_OPEN));
    }

    #[test]
    #[cfg(unix)]
    fn install_makes_hooks_executable_0o755() {
        let tmp = tempfile::tempdir().unwrap();
        init_git(tmp.path());
        install(tmp.path(), &discover(), false, true).expect("install");
        for h in GitHook::ALL {
            let p = tmp.path().join(".git/hooks").join(h.name());
            let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755, "{} has mode {:o}", p.display(), mode);
        }
    }

    #[test]
    #[cfg(unix)]
    fn install_preserves_executable_bits_on_existing_hook() {
        let tmp = tempfile::tempdir().unwrap();
        init_git(tmp.path());
        let post_commit = tmp.path().join(".git/hooks/post-commit");
        fs::create_dir_all(post_commit.parent().unwrap()).unwrap();
        fs::write(&post_commit, "#!/usr/bin/env sh\n").unwrap();
        // Set 0o750 (group-readable but not other-readable).
        fs::set_permissions(&post_commit, fs::Permissions::from_mode(0o750)).unwrap();

        install(tmp.path(), &discover(), false, true).expect("install");
        let mode = fs::metadata(&post_commit).unwrap().permissions().mode() & 0o777;
        // Should be 0o750 | 0o111 = 0o751 (we OR-in execute, don't clobber).
        assert_eq!(mode, 0o751);
    }

    #[test]
    fn install_is_idempotent_sentinel_count_stays_one() {
        let tmp = tempfile::tempdir().unwrap();
        init_git(tmp.path());
        install(tmp.path(), &discover(), false, true).expect("first install");
        install(tmp.path(), &discover(), false, true).expect("second install");
        for h in GitHook::ALL {
            let p = tmp.path().join(".git/hooks").join(h.name());
            let body = fs::read_to_string(&p).unwrap();
            assert_eq!(
                body.matches(sentinel::SENTINEL_OPEN).count(),
                1,
                "hook {} has more than one sentinel",
                h.name()
            );
        }
    }

    #[test]
    fn install_refuses_when_stale_invocation_present_no_force() {
        let tmp = tempfile::tempdir().unwrap();
        init_git(tmp.path());
        let post_commit = tmp.path().join(".git/hooks/post-commit");
        fs::create_dir_all(post_commit.parent().unwrap()).unwrap();
        fs::write(
            &post_commit,
            "#!/usr/bin/env sh\nvoiceforge send git_commit --message manual\n",
        )
        .unwrap();
        let err = install(tmp.path(), &discover(), false, true).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("stale"), "got: {msg}");
    }

    #[test]
    fn install_proceeds_with_force_when_stale_invocation_present() {
        let tmp = tempfile::tempdir().unwrap();
        init_git(tmp.path());
        let post_commit = tmp.path().join(".git/hooks/post-commit");
        fs::create_dir_all(post_commit.parent().unwrap()).unwrap();
        fs::write(
            &post_commit,
            "#!/usr/bin/env sh\nvoiceforge send git_commit --message manual\n",
        )
        .unwrap();
        install(tmp.path(), &discover(), true, true).expect("force install");
    }

    #[test]
    fn install_honors_core_hookspath() {
        let tmp = tempfile::tempdir().unwrap();
        init_git(tmp.path());
        // Set core.hooksPath to a relative `.githooks` dir.
        Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["config", "core.hooksPath", ".githooks"])
            .status()
            .expect("git config");
        install(tmp.path(), &discover(), false, true).expect("install");
        for h in GitHook::ALL {
            let p = tmp.path().join(".githooks").join(h.name());
            assert!(p.is_file(), "missing {}", p.display());
        }
    }

    #[test]
    fn install_warns_when_husky_detected() {
        // Functional check: the hook's framework_warning field is
        // populated. (Testing stderr capture is harder.)
        let tmp = tempfile::tempdir().unwrap();
        init_git(tmp.path());
        let post_commit = tmp.path().join(".git/hooks/post-commit");
        fs::create_dir_all(post_commit.parent().unwrap()).unwrap();
        fs::write(
            &post_commit,
            "#!/usr/bin/env sh\n. ~/.husky/husky.sh\necho hi\n",
        )
        .unwrap();
        let report = install(tmp.path(), &discover(), false, true).expect("install");
        let pc = report
            .per_hook
            .iter()
            .find(|r| r.hook == GitHook::PostCommit)
            .unwrap();
        assert_eq!(pc.framework_warning, Some("husky"));
    }

    #[test]
    fn pre_push_exit_zero_is_terminal() {
        // Render the hook body and assert `exit 0` is present
        // immediately before the close sentinel (the last non-sentinel
        // statement).
        let body = GitHook::PrePush.render_body("voiceforge");
        let lines: Vec<&str> = body.lines().collect();
        let close_idx = lines
            .iter()
            .position(|l| *l == sentinel::SENTINEL_CLOSE)
            .unwrap();
        assert_eq!(lines[close_idx - 1], "exit 0");
    }

    #[test]
    fn pre_push_consumes_stdin() {
        let body = GitHook::PrePush.render_body("voiceforge");
        assert!(body.contains("cat >/dev/null"));
        // The cat must come BEFORE the spawn so stdin is drained
        // first.
        let cat_idx = body.find("cat >/dev/null").unwrap();
        let spawn_idx = body.find("voiceforge send git_push").unwrap();
        assert!(cat_idx < spawn_idx);
    }

    #[test]
    fn post_commit_has_no_terminal_exit_zero() {
        // The other 3 hooks must NOT have `exit 0` so any user-
        // appended content can still run.
        let body = GitHook::PostCommit.render_body("voiceforge");
        assert!(!body.contains("\nexit 0\n"));
    }

    #[test]
    fn uninstall_strips_block_cleanly_preserves_user_content() {
        let tmp = tempfile::tempdir().unwrap();
        init_git(tmp.path());
        let post_commit = tmp.path().join(".git/hooks/post-commit");
        fs::create_dir_all(post_commit.parent().unwrap()).unwrap();
        fs::write(&post_commit, "#!/usr/bin/env sh\necho user-content\n").unwrap();
        install(tmp.path(), &discover(), false, true).unwrap();
        uninstall(tmp.path()).unwrap();
        let body = fs::read_to_string(&post_commit).unwrap();
        assert!(body.contains("echo user-content"));
        assert!(!body.contains(sentinel::SENTINEL_OPEN));
    }

    #[test]
    fn uninstall_deletes_hook_file_only_when_we_created() {
        let tmp = tempfile::tempdir().unwrap();
        init_git(tmp.path());
        // No pre-existing hooks. Install creates all 4.
        install(tmp.path(), &discover(), false, true).unwrap();
        // Now uninstall — files should be deleted because we
        // Created them and post-strip == auto-shebang.
        uninstall(tmp.path()).unwrap();
        for h in GitHook::ALL {
            let p = tmp.path().join(".git/hooks").join(h.name());
            assert!(
                !p.exists(),
                "{} should be deleted after uninstall",
                p.display()
            );
        }
    }

    #[test]
    fn uninstall_keeps_husk_when_user_added_content() {
        let tmp = tempfile::tempdir().unwrap();
        init_git(tmp.path());
        let post_commit = tmp.path().join(".git/hooks/post-commit");
        fs::create_dir_all(post_commit.parent().unwrap()).unwrap();
        fs::write(&post_commit, "#!/usr/bin/env sh\necho user\n").unwrap();
        install(tmp.path(), &discover(), false, true).unwrap();
        uninstall(tmp.path()).unwrap();
        // post-commit existed pre-install with user content; uninstall
        // must NOT delete it.
        assert!(post_commit.is_file());
        let body = fs::read_to_string(&post_commit).unwrap();
        assert!(body.contains("echo user"));
    }

    #[test]
    fn uninstall_is_noop_when_no_block_present() {
        let tmp = tempfile::tempdir().unwrap();
        init_git(tmp.path());
        let report = uninstall(tmp.path()).unwrap();
        for r in &report.per_hook {
            assert_eq!(r.action, HookUninstallAction::NotPresent);
        }
    }

    #[test]
    fn status_reports_per_hook_installed_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        init_git(tmp.path());
        install(tmp.path(), &discover(), false, true).unwrap();
        // Delete one hook to simulate partial state.
        fs::remove_file(tmp.path().join(".git/hooks/post-merge")).unwrap();
        let report = status(tmp.path()).unwrap();
        let pc = report
            .iter()
            .find(|s| s.hook == GitHook::PostCommit)
            .unwrap();
        let pm = report
            .iter()
            .find(|s| s.hook == GitHook::PostMerge)
            .unwrap();
        assert!(pc.installed);
        assert!(!pm.installed);
    }
}
