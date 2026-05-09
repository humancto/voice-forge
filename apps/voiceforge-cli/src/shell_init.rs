//! `voiceforge shell-init` — render + install zsh / bash hooks that
//! fire `command_succeeded` / `command_failed` daemon events for
//! commands over a configurable threshold (default 3 s). Companion
//! to ROADMAP 1.8 (daemon) + 1.9 (`voiceforge send` client).
//!
//! See `.planning/voiceforge-shell-init.plan.md` for the design audit
//! (rust-expert plan v2 APPROVE + 8 implementation notes).

#![allow(dead_code)] // install/uninstall/status + main.rs wiring land in commit 2

use anyhow::{anyhow, Result};
use std::path::PathBuf;
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
/// (deterministic output) and by the future `--reinstall` flag for
/// users who want a stable pin.
#[derive(Debug, Clone)]
pub enum BinaryHint {
    DiscoverViaPath,
    Pinned(PathBuf),
}

/// Single-quote shell-escape: wrap in `'...'`, replace inner `'`
/// with `'\''`. Survives any path content including spaces, `$`,
/// backticks, and `'` itself. We don't pull in a crate for this —
/// it's nine lines.
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

/// Render the hook block for the given shell. The output includes the
/// sentinels — callers either eval it directly (`voiceforge shell-init zsh`)
/// or write it into an rc file (via `install`).
pub fn render_hook(shell: Shell, hint: &BinaryHint) -> String {
    // The "voiceforge invocation" line differs only by whether we
    // shim through `command -v` or call a pinned absolute path. The
    // surrounding hook body is otherwise identical per shell.
    let invocation = match hint {
        BinaryHint::DiscoverViaPath => {
            // `command -v` is a builtin (no fork). Cheap to call every
            // prompt; do NOT try to cache, the upgrade path depends on
            // re-resolving each time.
            "command -v voiceforge >/dev/null 2>&1 && voiceforge".to_string()
        }
        BinaryHint::Pinned(path) => {
            let q = shell_quote(&path.display().to_string());
            // Test for existence first so a stale pin (binary moved/
            // removed) doesn't spam errors.
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
        // Single-quoted token must contain the literal path including spaces.
        assert!(h.contains("'/path with spaces/voiceforge'"));
    }

    #[test]
    fn render_hook_shell_escapes_inner_single_quote() {
        let pinned = BinaryHint::Pinned(PathBuf::from("/odd'place/vf"));
        let h = render_hook(Shell::Bash, &pinned);
        // The inner ' must be escaped as '\'' in single-quoted shell context.
        assert!(h.contains(r"'/odd'\''place/vf'"));
    }

    #[test]
    fn vf_status_capture_is_first_statement_in_zsh_precmd() {
        // Assert the first body line of __voiceforge_precmd captures
        // $? before any other expansion can clobber it.
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
}
