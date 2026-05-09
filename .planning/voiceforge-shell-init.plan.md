# Plan v2: voiceforge shell-init (ROADMAP 3.1)

> Plan v1 -> rust-expert REVISE with 8 items. v2 folds them in.

## Goal

Wire shell command exit-status into the daemon. Long commands speak;
fast commands and skip-listed commands stay silent.

```
$ eval "$(voiceforge shell-init zsh)"
$ npm test                                  # 12s, exits 1
[voiceforge speaks: "the build failed again"]

$ voiceforge shell-init --install zsh       # idempotent; refuses if a stale eval is detected
voiceforge: appended hook to ~/.zshrc

$ voiceforge shell-init --uninstall zsh
voiceforge: removed hook from ~/.zshrc
```

## CLI

```
voiceforge shell-init <shell>             # print to stdout (eval)
voiceforge shell-init --install <shell> [--force]
voiceforge shell-init --uninstall <shell>
voiceforge shell-init --status
```

`<shell>` is `zsh` or `bash`. Clap rejects others at parse time.
`--force` overrides the stale-eval guard (item 2).

## Hook semantics

1. Before each command: record start time + command line.
2. At prompt: capture `$?` AS THE VERY FIRST STATEMENT (item 6),
   compute elapsed, apply skip-list, apply threshold, fire
   `voiceforge send` in background.

Threshold: env `VOICEFORGE_SHELL_THRESHOLD_MS`, default 3000 ms.

Background-spawn so the hook returns instantly (zsh: `&!`, bash:
`</dev/null >/dev/null 2>&1 & disown`).

Skip-list: empty command, plus exact argv[0] match (item 4) against
`cd`, `ls`, `pwd`, `clear`, `history`, `voiceforge`. NOT prefix
match — `voiceforge-test` should fire. Configurable via
`VOICEFORGE_SHELL_SKIP` (colon-separated exact tokens).

Binary discovery (item 3): use `command -v voiceforge` shim, NOT a
hardcoded absolute path. Survives `voiceforge upgrade` transparently.
The `--install` path documents `--reinstall` (re-render + replace
block) for users who want the pinned path. (Note: `--reinstall` is
out-of-scope for this PR; document the future surface.)

## Daemon events

`configs/rules/events.json` audit:
- `command_succeeded` — add if missing.
- `command_failed` — add if missing.

Reuse existing voices (e.g. `default`, `angry_duck`) to avoid scope
creep. Lines should be terse: "Done." / "That failed."

## Files

### New

- `apps/voiceforge-cli/src/shell_init.rs` (~250 lines):
  - `pub fn render_hook(shell: Shell, binary_hint: BinaryHint) -> String`
    (item 8) — emits the hook text. `BinaryHint::DiscoverViaPath`
    (default for production) emits `command -v voiceforge`. `BinaryHint::Pinned(PathBuf)`
    embeds the absolute path (used by `--reinstall` later, also by
    tests so they can inject a known path).
  - `pub fn install(shell: Shell, rc_path: &Path, force: bool) -> Result<InstallReport>` —
    idempotent append wrapped in sentinel block. Detects stale `eval
    "$(voiceforge shell-init...)"` lines OUTSIDE the sentinels and
    refuses unless `force == true` (item 2).
  - `pub fn uninstall(shell: Shell, rc_path: &Path) -> Result<UninstallReport>`.
  - `pub fn status(home: &Path) -> Vec<ShellStatus>`.

### Modified

- `apps/voiceforge-cli/src/main.rs` — `Commands::ShellInit { shell, install, uninstall, status, force }` + dispatch.
- `configs/rules/events.json` — add `command_succeeded` / `command_failed` if missing.

## Hook templates

### zsh

```sh
# >>> voiceforge >>>
zmodload -i zsh/datetime
typeset -gi __voiceforge_threshold_ms=${VOICEFORGE_SHELL_THRESHOLD_MS:-3000}
typeset -g __voiceforge_skip="${VOICEFORGE_SHELL_SKIP:-cd:ls:pwd:clear:history:voiceforge}"
typeset -gF __voiceforge_start=0
typeset -g __voiceforge_cmd=""

__voiceforge_preexec() {
  __voiceforge_start=$EPOCHREALTIME
  __voiceforge_cmd=$1
}

__voiceforge_precmd() {
  local __vf_status=$?              # MUST be first statement (item 6)
  [[ -z "$__voiceforge_cmd" ]] && return
  local now=$EPOCHREALTIME
  local elapsed_ms=$(( (now - __voiceforge_start) * 1000 ))
  local first="${__voiceforge_cmd%% *}"
  local skip
  for skip in ${(s.:.)__voiceforge_skip}; do
    [[ "$first" == "$skip" ]] && { __voiceforge_cmd=""; return; }
  done
  if (( elapsed_ms < __voiceforge_threshold_ms )); then
    __voiceforge_cmd=""
    return
  fi
  local event
  if (( __vf_status == 0 )); then event="command_succeeded"; else event="command_failed"; fi
  command -v voiceforge >/dev/null 2>&1 && \
    voiceforge send "$event" \
      --message "${__voiceforge_cmd} [exit ${__vf_status}, $(( elapsed_ms / 1000 ))s]" \
      </dev/null >/dev/null 2>&1 &!
  __voiceforge_cmd=""
}

# Re-source idempotence (item 7): strip self before append.
typeset -ga preexec_functions precmd_functions
preexec_functions=(${preexec_functions:#__voiceforge_preexec})
precmd_functions=(${precmd_functions:#__voiceforge_precmd})
preexec_functions+=(__voiceforge_preexec)
precmd_functions+=(__voiceforge_precmd)
# <<< voiceforge <<<
```

### bash

```sh
# >>> voiceforge >>>
__voiceforge_threshold_ms="${VOICEFORGE_SHELL_THRESHOLD_MS:-3000}"
__voiceforge_skip="${VOICEFORGE_SHELL_SKIP:-cd:ls:pwd:clear:history:voiceforge}"
__voiceforge_start=0
__voiceforge_cmd=""
__voiceforge_inside_prompt=0

__voiceforge_now_ms() {
  local s=$(date +%s%3N 2>/dev/null)
  if [[ "$s" == *N ]]; then          # BSD date — %3N not supported
    printf '%s\n' "$(( $(date +%s) * 1000 ))"
  else
    printf '%s\n' "$s"
  fi
}

__voiceforge_preexec() {
  # Item 1: belt-and-braces guard. Skip when running under our
  # __voiceforge_precmd, completion, or PROMPT_COMMAND itself.
  (( __voiceforge_inside_prompt )) && return
  [[ -n "$COMP_LINE" ]] && return
  [[ "$BASH_COMMAND" == "$PROMPT_COMMAND" ]] && return
  __voiceforge_start=$(__voiceforge_now_ms)
  __voiceforge_cmd="$BASH_COMMAND"
}

__voiceforge_precmd() {
  local __vf_status=$?              # MUST be first statement (item 6)
  __voiceforge_inside_prompt=1
  if [[ -n "$__voiceforge_cmd" ]]; then
    local now=$(__voiceforge_now_ms)
    local elapsed_ms=$(( now - __voiceforge_start ))
    local first="${__voiceforge_cmd%% *}"
    local IFS=':'
    local skipped=0
    for skip in $__voiceforge_skip; do
      [[ "$first" == "$skip" ]] && { skipped=1; break; }
    done
    unset IFS
    if (( ! skipped )) && (( elapsed_ms >= __voiceforge_threshold_ms )); then
      local event
      if (( __vf_status == 0 )); then event="command_succeeded"; else event="command_failed"; fi
      command -v voiceforge >/dev/null 2>&1 && \
        voiceforge send "$event" \
          --message "${__voiceforge_cmd} [exit ${__vf_status}, $(( elapsed_ms / 1000 ))s]" \
          </dev/null >/dev/null 2>&1 &
      disown $! 2>/dev/null
    fi
    __voiceforge_cmd=""
  fi
  __voiceforge_inside_prompt=0
}

# Re-source idempotence (item 7): only register once.
case "$PROMPT_COMMAND" in
  *__voiceforge_precmd*) ;;
  *) PROMPT_COMMAND="__voiceforge_precmd${PROMPT_COMMAND:+; $PROMPT_COMMAND}" ;;
esac
trap '__voiceforge_preexec' DEBUG
# <<< voiceforge <<<
```

## Atomic commits

1. `feat(shell-init): render zsh + bash hook scripts`
2. `feat(shell-init): idempotent install/uninstall/status with stale-eval guard`
3. `feat(events): add command_succeeded + command_failed to rules` (only if audit shows missing)

## Tests

`apps/voiceforge-cli/src/shell_init.rs` `#[cfg(test)] mod tests`:

1. `render_hook_zsh_includes_threshold_env_var`
2. `render_hook_bash_uses_trap_debug` and the `__voiceforge_inside_prompt` flag
3. `render_hook_with_pinned_binary_embeds_absolute_path` — uses `BinaryHint::Pinned(...)`, asserts the path appears verbatim (shell-escaped if it contains spaces).
4. `render_hook_with_discover_emits_command_v_voiceforge` — default path; asserts `command -v voiceforge` appears, NOT an absolute path.
5. `install_appends_block_idempotently` — install twice, assert exactly one block (sentinel count == 1).
6. `install_preserves_surrounding_rc_content`.
7. `install_refuses_when_stale_eval_present_and_no_force` — rc contains `eval "$(voiceforge shell-init zsh)"` outside sentinels; install without `--force` returns Err mentioning the stale line.
8. `install_proceeds_with_force_even_when_stale_eval_present` — same setup, `force=true` succeeds.
9. `uninstall_strips_block_cleanly`.
10. `uninstall_is_noop_when_no_block_present`.
11. `status_reports_installed_and_missing` for zsh + bash via tempdir HOME.
12. `render_hook_shell_escapes_binary_path_with_spaces` — `BinaryHint::Pinned("/path with spaces/voiceforge")` produces a single-quoted token, no shell-injection.

## Manual smoke (post-merge)

1. `voiceforge daemon &`
2. `eval "$(voiceforge shell-init zsh)"`
3. `sleep 4` -> daemon speaks `command_succeeded`.
4. `sleep 4; false` -> daemon speaks `command_failed`.
5. `ls` -> silent.
6. `voiceforge shell-init --install zsh` -> block in `~/.zshrc`. Run again -> still one block.
7. `source ~/.zshrc; source ~/.zshrc` -> `preexec_functions` contains `__voiceforge_preexec` exactly once (verify via `print -l ${preexec_functions}`).
8. Add a stale `eval "$(voiceforge shell-init zsh)"` line manually -> `voiceforge shell-init --install zsh` refuses with hint -> `--force` succeeds.

## Risks (acknowledged)

- `trap DEBUG` perf — negligible for interactive use; the inside-prompt flag short-circuits most invocations.
- macOS `date` lacking `%3N` — second-resolution fallback is fine for a 3 s threshold.
- `command -v voiceforge` returning empty after upgrade-then-rename — silent no-op rather than crash; user notices via daemon-doctor.
- ZDOTDIR users — `--install zsh` writes to `$HOME/.zshrc`. ZDOTDIR-aware install is a future flag.

## Out of scope

- Fish (3.1.1).
- `--reinstall` flag for binary-path repinning (3.1.2).
- ZDOTDIR-aware install (3.1.3).

## What plan v1 got wrong (audit log)

1. **bash DEBUG guard.** v1: `[[ "$BASH_COMMAND" == "$PROMPT_COMMAND" ]]` only. v2: `__voiceforge_inside_prompt` flag set/unset around the precmd body, plus the v1 guard as belt-and-braces, plus `COMP_LINE` skip.
2. **Stale eval blocks.** v1: silent double-registration. v2: install scans rc for `voiceforge shell-init` outside sentinels and refuses without `--force`.
3. **Binary path drift on upgrade.** v1: hardcoded absolute path. v2: `command -v voiceforge` shim default; pinned path is opt-in (future `--reinstall`).
4. **Skip-list match.** v1: prefix match — would skip `voiceforge-test`. v2: exact argv[0] match.
5. **`current_exe()` resolution.** v1 implicit; v2 explicit non-issue once #3 adopted.
6. **`$?` capture ordering.** v1 didn't say. v2: `local __vf_status=$?` is the FIRST statement of `__voiceforge_precmd`, before any expansion can clobber it.
7. **Re-source idempotence.** v1 silent. v2: zsh strips self from `preexec_functions` / `precmd_functions` before appending; bash uses `case` on `PROMPT_COMMAND`.
8. **Test #9 signature.** v1 had no testable seam. v2: `render_hook(shell, binary_hint)` lets tests inject paths; production callsite passes `BinaryHint::DiscoverViaPath`.
