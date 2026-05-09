//! `voiceforge shell-init` — render + install zsh / bash hooks that
//! fire `command_succeeded` / `command_failed` daemon events for
//! commands over a configurable threshold (default 3 s). Companion
//! to ROADMAP 1.8 (daemon) + 1.9 (`voiceforge send` client).
//!
//! See `.planning/voiceforge-shell-init.plan.md` for the design audit
//! (rust-expert plan v2 APPROVE + 8 implementation notes).

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Sentinels delimit the voiceforge-managed block in the rc file.
/// Exact strings — no regex parsing. Re-running install replaces the
/// block contents while preserving everything outside.
pub const SENTINEL_OPEN: &str = "# >>> voiceforge >>>";
pub const SENTINEL_CLOSE: &str = "# <<< voiceforge <<<";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    Bash,
}

impl Shell {
    pub fn rc_filename(self) -> &'static str {
        match self {
            Shell::Zsh => ".zshrc",
            Shell::Bash => ".bashrc",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Shell::Zsh => "zsh",
            Shell::Bash => "bash",
        }
    }
}

impl FromStr for Shell {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "zsh" => Ok(Shell::Zsh),
            "bash" => Ok(Shell::Bash),
            other => Err(anyhow!("unknown shell {other:?}; supported: zsh, bash")),
        }
    }
}

/// How the rendered hook should locate the `voiceforge` binary.
///
/// Default for production is `DiscoverViaPath` — the hook calls
/// `command -v voiceforge` and skips firing if not on PATH. Survives
/// `voiceforge upgrade` transparently.
///
/// `Pinned(path)` embeds the absolute path verbatim. Used by tests
/// (deterministic output) and by the future `--reinstall` flag.
#[derive(Debug, Clone)]
pub enum BinaryHint {
    DiscoverViaPath,
    /// Reserved for the future `--reinstall` flag (3.1.2). Used today
    /// by tests to verify shell-escape and embedding behavior.
    #[allow(dead_code)]
    Pinned(PathBuf),
}

/// Single-quote shell-escape: wrap in `'...'`, replace inner `'`
/// with `'\''`. Survives any path content including spaces, `$`,
/// backticks, and `'` itself.
fn shell_quote(s: &str) -> String {
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

pub fn render_hook(shell: Shell, hint: &BinaryHint) -> String {
    let invocation = match hint {
        BinaryHint::DiscoverViaPath => {
            "command -v voiceforge >/dev/null 2>&1 && voiceforge".to_string()
        }
        BinaryHint::Pinned(path) => {
            let q = shell_quote(&path.display().to_string());
            format!("[ -x {q} ] && {q}")
        }
    };

    match shell {
        Shell::Zsh => render_zsh(&invocation),
        Shell::Bash => render_bash(&invocation),
    }
}

fn render_zsh(invocation: &str) -> String {
    format!(
        "{open}\n\
zmodload -i zsh/datetime\n\
typeset -gi __voiceforge_threshold_ms=${{VOICEFORGE_SHELL_THRESHOLD_MS:-3000}}\n\
typeset -g __voiceforge_skip=\"${{VOICEFORGE_SHELL_SKIP:-cd:ls:pwd:clear:history:voiceforge}}\"\n\
typeset -gF __voiceforge_start=0\n\
typeset -g __voiceforge_cmd=\"\"\n\
\n\
__voiceforge_preexec() {{\n\
  __voiceforge_start=$EPOCHREALTIME\n\
  __voiceforge_cmd=$1\n\
}}\n\
\n\
__voiceforge_precmd() {{\n\
  local __vf_status=$?              # MUST stay first — captures user-command exit\n\
  [[ -z \"$__voiceforge_cmd\" ]] && return\n\
  local now=$EPOCHREALTIME\n\
  local elapsed_ms=$(( (now - __voiceforge_start) * 1000 ))\n\
  local first=\"${{__voiceforge_cmd%% *}}\"\n\
  local skip\n\
  for skip in ${{(s.:.)__voiceforge_skip}}; do\n\
    [[ \"$first\" == \"$skip\" ]] && {{ __voiceforge_cmd=\"\"; return; }}\n\
  done\n\
  if (( elapsed_ms < __voiceforge_threshold_ms )); then\n\
    __voiceforge_cmd=\"\"\n\
    return\n\
  fi\n\
  local event\n\
  if (( __vf_status == 0 )); then event=\"command_succeeded\"; else event=\"command_failed\"; fi\n\
  {{ {invocation} send \"$event\" \\\n\
      --message \"${{__voiceforge_cmd}} [exit ${{__vf_status}}, $(( elapsed_ms / 1000 ))s]\" \\\n\
      </dev/null >/dev/null 2>&1 &!; }} 2>/dev/null\n\
  __voiceforge_cmd=\"\"\n\
}}\n\
\n\
typeset -ga preexec_functions precmd_functions\n\
preexec_functions=(${{preexec_functions:#__voiceforge_preexec}})\n\
precmd_functions=(${{precmd_functions:#__voiceforge_precmd}})\n\
preexec_functions+=(__voiceforge_preexec)\n\
precmd_functions+=(__voiceforge_precmd)\n\
{close}\n",
        open = SENTINEL_OPEN,
        close = SENTINEL_CLOSE,
        invocation = invocation,
    )
}

fn render_bash(invocation: &str) -> String {
    format!(
        "{open}\n\
__voiceforge_threshold_ms=\"${{VOICEFORGE_SHELL_THRESHOLD_MS:-3000}}\"\n\
__voiceforge_skip=\"${{VOICEFORGE_SHELL_SKIP:-cd:ls:pwd:clear:history:voiceforge}}\"\n\
__voiceforge_start=0\n\
__voiceforge_cmd=\"\"\n\
__voiceforge_inside_prompt=0\n\
\n\
__voiceforge_now_ms() {{\n\
  local s\n\
  s=$(date +%s%3N 2>/dev/null)\n\
  if [[ \"$s\" == *N ]]; then\n\
    printf '%s\\n' \"$(( $(date +%s) * 1000 ))\"\n\
  else\n\
    printf '%s\\n' \"$s\"\n\
  fi\n\
}}\n\
\n\
__voiceforge_preexec() {{\n\
  (( __voiceforge_inside_prompt )) && return\n\
  [[ -n \"$COMP_LINE\" ]] && return\n\
  __voiceforge_start=$(__voiceforge_now_ms)\n\
  __voiceforge_cmd=\"$BASH_COMMAND\"\n\
}}\n\
\n\
__voiceforge_precmd() {{\n\
  local __vf_status=$?              # MUST stay first — captures user-command exit\n\
  __voiceforge_inside_prompt=1\n\
  if [[ -n \"$__voiceforge_cmd\" ]]; then\n\
    local now elapsed_ms first skip skipped=0\n\
    now=$(__voiceforge_now_ms)\n\
    elapsed_ms=$(( now - __voiceforge_start ))\n\
    first=\"${{__voiceforge_cmd%% *}}\"\n\
    local IFS=':'\n\
    for skip in $__voiceforge_skip; do\n\
      [[ \"$first\" == \"$skip\" ]] && {{ skipped=1; break; }}\n\
    done\n\
    unset IFS\n\
    if (( ! skipped )) && (( elapsed_ms >= __voiceforge_threshold_ms )); then\n\
      local event\n\
      if (( __vf_status == 0 )); then event=\"command_succeeded\"; else event=\"command_failed\"; fi\n\
      {{ {invocation} send \"$event\" \\\n\
          --message \"${{__voiceforge_cmd}} [exit ${{__vf_status}}, $(( elapsed_ms / 1000 ))s]\" \\\n\
          </dev/null >/dev/null 2>&1 & disown $!; }} 2>/dev/null\n\
    fi\n\
    __voiceforge_cmd=\"\"\n\
  fi\n\
  __voiceforge_inside_prompt=0\n\
}}\n\
\n\
case \"$PROMPT_COMMAND\" in\n\
  *__voiceforge_precmd*) ;;\n\
  *) PROMPT_COMMAND=\"__voiceforge_precmd${{PROMPT_COMMAND:+; $PROMPT_COMMAND}}\" ;;\n\
esac\n\
trap '__voiceforge_preexec' DEBUG\n\
{close}\n",
        open = SENTINEL_OPEN,
        close = SENTINEL_CLOSE,
        invocation = invocation,
    )
}

// -- install / uninstall / status -------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum InstallAction {
    Created,
    Replaced,
}

#[derive(Debug)]
pub struct InstallReport {
    pub rc_path: PathBuf,
    pub action: InstallAction,
}

#[derive(Debug, PartialEq, Eq)]
pub enum UninstallAction {
    Removed,
    NotPresent,
}

#[derive(Debug)]
pub struct UninstallReport {
    pub rc_path: PathBuf,
    pub action: UninstallAction,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ShellStatus {
    pub shell: Shell,
    pub rc_path: PathBuf,
    pub installed: bool,
}

/// Look for any `voiceforge shell-init` invocation OUTSIDE the
/// sentinel-delimited block. A user with a stale invocation in their
/// rc would otherwise end up with two hook installations — silent
/// double-firing. We refuse without `--force` and print the offending
/// line so they can audit.
fn find_stale_invocation(content: &str) -> Option<(usize, String)> {
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
        if trimmed.contains("voiceforge") && trimmed.contains("shell-init") {
            return Some((lineno + 1, line.to_string()));
        }
    }
    None
}

fn replace_or_append_block(content: &str, block: &str) -> (String, InstallAction) {
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
            (out, InstallAction::Replaced)
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
            (out, InstallAction::Created)
        }
    }
}

fn strip_block(content: &str) -> (String, bool) {
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

pub fn install(
    shell: Shell,
    rc_path: &Path,
    hint: &BinaryHint,
    force: bool,
) -> Result<InstallReport> {
    let content = std::fs::read_to_string(rc_path).unwrap_or_default();

    if let Some((lineno, line)) = find_stale_invocation(&content) {
        if !force {
            bail!(
                "{}:{lineno}: found a stale `voiceforge shell-init` invocation outside the managed block:\n  {}\n\nRe-running install would result in the hook firing twice. Either:\n  1. Remove the stale line by hand, or\n  2. Re-run with --force to install anyway (the stale line will keep firing)",
                rc_path.display(),
                line.trim()
            );
        }
    }

    let block = render_hook(shell, hint);
    let (new_content, action) = replace_or_append_block(&content, &block);

    if let Some(parent) = rc_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir {}", parent.display()))?;
        }
    }
    std::fs::write(rc_path, new_content)
        .with_context(|| format!("writing {}", rc_path.display()))?;

    Ok(InstallReport {
        rc_path: rc_path.to_path_buf(),
        action,
    })
}

pub fn uninstall(rc_path: &Path) -> Result<UninstallReport> {
    let content = match std::fs::read_to_string(rc_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(UninstallReport {
                rc_path: rc_path.to_path_buf(),
                action: UninstallAction::NotPresent,
            });
        }
        Err(e) => return Err(anyhow!("reading {}: {e}", rc_path.display())),
    };

    let (new_content, was_present) = strip_block(&content);
    if was_present {
        std::fs::write(rc_path, new_content)
            .with_context(|| format!("writing {}", rc_path.display()))?;
        Ok(UninstallReport {
            rc_path: rc_path.to_path_buf(),
            action: UninstallAction::Removed,
        })
    } else {
        Ok(UninstallReport {
            rc_path: rc_path.to_path_buf(),
            action: UninstallAction::NotPresent,
        })
    }
}

pub fn status(home: &Path) -> Vec<ShellStatus> {
    [Shell::Zsh, Shell::Bash]
        .iter()
        .map(|&shell| {
            let rc_path = home.join(shell.rc_filename());
            let installed = std::fs::read_to_string(&rc_path)
                .map(|c| c.contains(SENTINEL_OPEN) && c.contains(SENTINEL_CLOSE))
                .unwrap_or(false);
            ShellStatus {
                shell,
                rc_path,
                installed,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discover() -> BinaryHint {
        BinaryHint::DiscoverViaPath
    }

    #[test]
    fn shell_from_str_accepts_zsh_and_bash() {
        assert_eq!(Shell::from_str("zsh").unwrap(), Shell::Zsh);
        assert_eq!(Shell::from_str("bash").unwrap(), Shell::Bash);
        assert!(Shell::from_str("fish").is_err());
    }

    #[test]
    fn render_hook_zsh_includes_threshold_env_var() {
        let h = render_hook(Shell::Zsh, &discover());
        assert!(h.contains("VOICEFORGE_SHELL_THRESHOLD_MS"));
        assert!(h.contains("preexec_functions+=(__voiceforge_preexec)"));
    }

    #[test]
    fn render_hook_bash_uses_trap_debug_and_inside_prompt_flag() {
        let h = render_hook(Shell::Bash, &discover());
        assert!(h.contains("trap '__voiceforge_preexec' DEBUG"));
        assert!(h.contains("__voiceforge_inside_prompt=1"));
        assert!(h.contains("__voiceforge_inside_prompt=0"));
    }

    #[test]
    fn render_hook_with_pinned_binary_embeds_absolute_path() {
        let pinned = BinaryHint::Pinned(PathBuf::from("/usr/local/bin/voiceforge"));
        let h = render_hook(Shell::Zsh, &pinned);
        assert!(h.contains("'/usr/local/bin/voiceforge'"));
        assert!(!h.contains("command -v voiceforge"));
    }

    #[test]
    fn render_hook_with_discover_emits_command_v_voiceforge() {
        let h = render_hook(Shell::Zsh, &discover());
        assert!(h.contains("command -v voiceforge"));
    }

    #[test]
    fn render_hook_shell_escapes_binary_path_with_spaces() {
        let pinned = BinaryHint::Pinned(PathBuf::from("/path with spaces/voiceforge"));
        let h = render_hook(Shell::Bash, &pinned);
        assert!(h.contains("'/path with spaces/voiceforge'"));
    }

    #[test]
    fn render_hook_shell_escapes_inner_single_quote() {
        let pinned = BinaryHint::Pinned(PathBuf::from("/odd'place/vf"));
        let h = render_hook(Shell::Bash, &pinned);
        assert!(h.contains(r"'/odd'\''place/vf'"));
    }

    #[test]
    fn vf_status_capture_is_first_statement_in_zsh_precmd() {
        let h = render_hook(Shell::Zsh, &discover());
        let needle = "__voiceforge_precmd() {\nlocal __vf_status=$?";
        assert!(
            h.contains(needle),
            "first line of __voiceforge_precmd must capture $? (got hook:\n{h})"
        );
    }

    #[test]
    fn vf_status_capture_is_first_statement_in_bash_precmd() {
        let h = render_hook(Shell::Bash, &discover());
        let needle = "__voiceforge_precmd() {\nlocal __vf_status=$?";
        assert!(h.contains(needle));
    }

    // -- install / uninstall / status tests ----------------------------

    fn count_substring(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    #[test]
    fn install_appends_block_idempotently() {
        let tmp = tempfile::tempdir().unwrap();
        let rc = tmp.path().join(".zshrc");
        std::fs::write(&rc, "# user's existing rc\nalias ll='ls -la'\n").unwrap();

        let r1 = install(Shell::Zsh, &rc, &discover(), false).unwrap();
        assert_eq!(r1.action, InstallAction::Created);
        let r2 = install(Shell::Zsh, &rc, &discover(), false).unwrap();
        assert_eq!(r2.action, InstallAction::Replaced);

        let final_content = std::fs::read_to_string(&rc).unwrap();
        assert_eq!(count_substring(&final_content, SENTINEL_OPEN), 1);
        assert_eq!(count_substring(&final_content, SENTINEL_CLOSE), 1);
    }

    #[test]
    fn install_preserves_surrounding_rc_content() {
        let tmp = tempfile::tempdir().unwrap();
        let rc = tmp.path().join(".zshrc");
        let original = "# top of rc\nalias gs='git status'\nexport FOO=bar\n";
        std::fs::write(&rc, original).unwrap();

        install(Shell::Zsh, &rc, &discover(), false).unwrap();
        let after = std::fs::read_to_string(&rc).unwrap();

        assert!(after.contains("alias gs='git status'"));
        assert!(after.contains("export FOO=bar"));
        assert!(after.contains(SENTINEL_OPEN));
    }

    #[test]
    fn install_refuses_when_stale_invocation_present_and_no_force() {
        let tmp = tempfile::tempdir().unwrap();
        let rc = tmp.path().join(".zshrc");
        std::fs::write(
            &rc,
            "alias ll='ls -la'\neval \"$(voiceforge shell-init zsh)\"\n",
        )
        .unwrap();

        let err = install(Shell::Zsh, &rc, &discover(), false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("stale"),
            "expected stale-line error, got: {msg}"
        );
        assert!(msg.contains("--force"), "expected --force hint, got: {msg}");
    }

    #[test]
    fn install_proceeds_with_force_when_stale_invocation_present() {
        let tmp = tempfile::tempdir().unwrap();
        let rc = tmp.path().join(".zshrc");
        std::fs::write(
            &rc,
            "alias ll='ls -la'\neval \"$(voiceforge shell-init zsh)\"\n",
        )
        .unwrap();

        let r = install(Shell::Zsh, &rc, &discover(), true).unwrap();
        assert_eq!(r.action, InstallAction::Created);
        let after = std::fs::read_to_string(&rc).unwrap();
        assert!(after.contains(SENTINEL_OPEN));
    }

    #[test]
    fn uninstall_strips_block_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let rc = tmp.path().join(".zshrc");
        let original = "# top\nalias gs='git status'\n";
        std::fs::write(&rc, original).unwrap();

        install(Shell::Zsh, &rc, &discover(), false).unwrap();
        let report = uninstall(&rc).unwrap();
        assert_eq!(report.action, UninstallAction::Removed);

        let after = std::fs::read_to_string(&rc).unwrap();
        assert!(!after.contains(SENTINEL_OPEN));
        assert!(after.contains("alias gs='git status'"));
        assert!(after.contains("# top"));
    }

    #[test]
    fn uninstall_is_noop_when_no_block_present() {
        let tmp = tempfile::tempdir().unwrap();
        let rc = tmp.path().join(".zshrc");
        std::fs::write(&rc, "alias ll='ls -la'\n").unwrap();

        let report = uninstall(&rc).unwrap();
        assert_eq!(report.action, UninstallAction::NotPresent);
        let after = std::fs::read_to_string(&rc).unwrap();
        assert_eq!(after, "alias ll='ls -la'\n");
    }

    #[test]
    fn uninstall_returns_not_present_when_rc_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let rc = tmp.path().join(".zshrc-missing");
        let report = uninstall(&rc).unwrap();
        assert_eq!(report.action, UninstallAction::NotPresent);
    }

    #[test]
    fn status_reports_installed_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let zsh_rc = tmp.path().join(".zshrc");
        std::fs::write(&zsh_rc, "").unwrap();
        install(Shell::Zsh, &zsh_rc, &discover(), false).unwrap();

        let statuses = status(tmp.path());
        let zsh = statuses.iter().find(|s| s.shell == Shell::Zsh).unwrap();
        let bash = statuses.iter().find(|s| s.shell == Shell::Bash).unwrap();
        assert!(zsh.installed);
        assert!(!bash.installed);
    }
}
