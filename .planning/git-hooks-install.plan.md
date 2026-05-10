# Plan v2: voiceforge install git-hooks (ROADMAP 3.2)

> Plan v1 -> rust-expert REVISE with 6 items + 2 nits. v2 folds them in.

## Goal

Install per-repo git hooks (`post-commit`, `post-merge`, `post-rewrite`,
`pre-push`) into `.git/hooks/` (or `core.hooksPath` if set) that fire
daemon events. Mirror of 3.1 (shell-init for zsh/bash) — same
sentinel-bounded block, idempotent install, stale-invocation guard.

```
$ cd ~/projects/myapp
$ voiceforge install git-hooks
voiceforge: installed 4 hooks (post-commit, post-merge, post-rewrite, pre-push)
hint: existing hook content was preserved -- our block is sentinel-bounded
```

## CLI shape

```
voiceforge install git-hooks [--repo <path>] [--force] [--uninstall] [--status]
```

`Commands::Install` is a NEW parent subcommand (mirrors `Commands::Pack`'s
pattern). Initial action: `git-hooks`. Future: `voiceforge install
cloning` (mirror, kept alongside top-level `InstallCloning` for muscle-
memory; see item 7).

## Hook semantics

Per-hook event mapping:

| git hook       | event              | message                                       |
| -------------- | ------------------ | --------------------------------------------- |
| `post-commit`  | `git_commit`       | `<short-sha> <subject>` (truncated 80 chars)  |
| `post-merge`   | `git_merge`        | `squash=<flag>`                               |
| `post-rewrite` | `git_rewrite`      | `rebase` or `amend`                           |
| `pre-push`     | `git_push`         | `<remote> <url>`                              |

`git_commit` already exists in events.json (verified). Need to ADD
`git_merge`, `git_rewrite`, `git_push` (verified missing). Reuse
existing voices.

## Three concrete fixes from rust-expert v1 review (now folded)

### Item 1: factor sentinel.rs NOW

Three helpers (`find_stale_invocation`, `replace_or_append_block`,
`strip_block`) are character-identical between shell_init and what
git_hooks needs, save the stale-marker substring (`"shell-init"` vs
`"send git_"`). Extract first.

```rust
// apps/voiceforge-cli/src/sentinel.rs
pub const SENTINEL_OPEN: &str = "# >>> voiceforge >>>";
pub const SENTINEL_CLOSE: &str = "# <<< voiceforge <<<";

pub fn find_stale_invocation<F>(content: &str, is_stale: F) -> Option<(usize, String)>
where F: Fn(&str) -> bool { ... }

pub fn replace_or_append_block(content: &str, block: &str) -> (String, BlockAction) { ... }

pub fn strip_block(content: &str) -> (String, bool) { ... }

pub enum BlockAction { Created, Replaced }
```

shell_init.rs migrates to consume `sentinel::*` (4-line refactor).
git_hooks.rs uses the same.

### Item 2: `core.hooksPath`

Resolution: `git -C <repo> config --get core.hooksPath`. Empty stdout
+ exit 1 -> default to `<repo>/.git/hooks`. Relative paths resolve
against repo root (e.g. `.githooks`).

### Item 3: file mode = `0o755` always

`OpenOptions::new().mode(0o755).create(true).write(true).truncate(true)`.
When PRESERVING an existing user hook, OR-in `0o111` rather than
clobbering their mode (they may have `0o750`):

```rust
let mode = std::fs::metadata(&path)?.permissions().mode();
std::fs::set_permissions(&path, Permissions::from_mode(mode | 0o111))?;
```

### Item 4: hook templates

`pre-push` — MUST exit 0 + consume stdin (per `githooks(5)`, pre-push
receives ref updates on stdin):

```sh
# >>> voiceforge >>>
cat >/dev/null  # consume ref-updates stdin so we don't half-drain it
{
  remote="${1:-?}"
  url="${2:-?}"
  command -v voiceforge >/dev/null 2>&1 && \
    voiceforge send git_push \
      --message "$remote $url" \
      </dev/null >/dev/null 2>&1 &
} 2>/dev/null
exit 0
# <<< voiceforge <<<
```

post-commit / post-merge / post-rewrite — NO `exit 0` (so any user-
appended content below our block can still run). post-merge `$1` =
`is_squash`; post-rewrite `$1` = `rebase` or `amend`.

### Item 5: tighten `uninstall_deletes_hook_file_when_empty`

Track install action as `Created` vs `AppendedToExisting` per hook.
On uninstall, ONLY delete the file when:
- install action was `Created` (we made the file)
- AND post-strip content equals the auto-shebang exactly:
  `"#!/usr/bin/env sh\nset -eu\n\n"` (with surrounding whitespace stripped)

Otherwise leave the husk shebang. Persist install action via a
sidecar `.voiceforge-install` JSON in the hooks dir (one file per
install; tells uninstall what to do per hook).

### Item 6: husky/lefthook/pre-commit collision warning

At install time, scan each hook file for known framework markers:
- Husky: `husky.sh` source line OR `# husky` comment
- Lefthook: `lefthook` magic comment
- pre-commit framework: `pre-commit` header

Found -> stderr warning: "detected <tool>; our block may be clobbered
when <tool> regenerates this hook." Don't refuse. `--quiet` suppresses.

### Item 7: `Commands::Install` parent

```rust
#[derive(Subcommand)]
enum Commands {
    // ... existing variants
    Install {
        #[command(subcommand)]
        action: InstallAction,
    },
    // Old InstallCloning kept at top level for backwards compat.
}

#[derive(Subcommand)]
enum InstallAction {
    GitHooks { repo: Option<PathBuf>, force: bool, uninstall: bool, status: bool, quiet: bool },
}
```

Future `install cloning` mirror lands when 3.2.x ships. Don't break
muscle memory.

### Item 8: `discover_repo` handles `.git` as FILE

`git worktree` and submodules write `.git` as a FILE containing
`gitdir: <path>`. Detect both:
- `.git` is dir -> repo root is parent
- `.git` is file -> read first line, parse `gitdir: <path>` (resolve
  relative to repo dir)

## Files

### New

- `apps/voiceforge-cli/src/sentinel.rs` (~80 lines + tests):
  Shared sentinel-block helpers. shell_init.rs migrates to use them.
- `apps/voiceforge-cli/src/git_hooks.rs` (~350 lines + tests):
  Discover repo, `core.hooksPath` resolution, `GitHook` enum + body
  templates, install/uninstall/status, framework collision detection,
  `Created`/`AppendedToExisting` tracking via sidecar JSON.

### Modified

- `apps/voiceforge-cli/src/shell_init.rs` -- use `sentinel::*` (4-line
  refactor; preserves all 17 tests).
- `apps/voiceforge-cli/src/main.rs` -- add `Commands::Install { action }`
  with `InstallAction::GitHooks` initial variant. Dispatch to
  `git_hooks::run_install_dispatcher(...)`.
- `configs/rules/events.json` -- ADD `git_merge`, `git_rewrite`,
  `git_push` (verified missing; `git_commit` already present).

## Tests

`apps/voiceforge-cli/src/sentinel.rs` `#[cfg(test)] mod tests`:

- 5 small tests porting the existing shell_init helper tests to the
  shared module (idempotent append, replace inside block, strip clean,
  strip-noop, find-stale-with-predicate).

`apps/voiceforge-cli/src/git_hooks.rs` `#[cfg(test)] mod tests`:

1. `discover_repo_finds_git_dir_in_ancestor`
2. `discover_repo_handles_git_file_for_worktree` (write `.git` as a
   text file with `gitdir: ...`)
3. `discover_repo_errors_outside_git_repo`
4. `install_writes_all_four_hooks`
5. `install_preserves_existing_hook_content`
6. `install_makes_hooks_executable_0o755`
7. `install_preserves_executable_bits_on_existing_hook` (don't
   downgrade `0o750`)
8. `install_is_idempotent_sentinel_count_stays_one`
9. `install_refuses_when_stale_invocation_present_no_force`
10. `install_proceeds_with_force_when_stale_invocation_present`
11. `install_honors_core_hookspath` (set `core.hooksPath = .githooks`,
    assert hooks land in `.githooks/`)
12. `install_warns_when_husky_detected` (pre-create hook with
    `husky.sh` source line; assert stderr captures warning)
13. `pre_push_exit_zero_is_terminal` (parse rendered hook, assert
    `exit 0` is the last non-sentinel statement)
14. `pre_push_consumes_stdin` (parse rendered hook, assert
    `cat >/dev/null` precedes the spawn)
15. `uninstall_strips_block_cleanly_preserves_user_content`
16. `uninstall_deletes_hook_file_only_when_we_created_and_no_other_content`
17. `uninstall_keeps_husk_when_user_added_content`
18. `uninstall_is_noop_when_no_block_present`
19. `status_reports_per_hook_installed_and_missing`

## Manual smoke (post-merge)

1. `cd ~/projects/test-repo`
2. `voiceforge daemon &; disown`
3. `voiceforge install git-hooks`
4. `git commit --allow-empty -m "test"` -> speaks `git_commit` line
5. `voiceforge install git-hooks` again -> still one block per hook
6. `voiceforge install git-hooks --uninstall` -> block gone
7. Worktree test: `git worktree add /tmp/wt-test`, `cd /tmp/wt-test`,
   `voiceforge install git-hooks` -> writes to the parent's `.git/hooks`
   (or `core.hooksPath` if set)

## Risks (acknowledged)

- **Hook framework regeneration** (husky/lefthook/pre-commit). We
  warn at install + document. User wires `voiceforge install
  git-hooks` into their tool's post-install if needed.
- **`pre-push` exit 0 short-circuits user content below**. Since we
  append at bottom, no user content is below — but if a user later
  rearranges, our `exit 0` blocks their additions. Documented.
- **Non-bare repos only**. `git init --bare` writes hooks at
  `<repo>/hooks/` not `<repo>/.git/hooks/`. Out of scope.

## Out of scope

- 3.2.1: per-user hooks via `git config --global core.hooksPath`.
- 3.2.2: `commit-msg` hook for emoji-prefix check.
- 3.2.3: GitHub-side webhook variant for push events you don't make.

## Atomic commits

1. `refactor(sentinel): extract shared shell-script-block helpers from shell_init`
2. `feat(git-hooks): add git_hooks module + Install subcommand`
3. `feat(events): add git_merge / git_rewrite / git_push to rules`

## What plan v1 got wrong (audit log)

1. Deferred `sentinel.rs` extraction. v2: do it first, in commit 1.
2. `core.hooksPath` was hand-waved. v2: shell out to `git config --get`.
3. File mode was "chmod after write." v2: `from_mode(0o755)` always;
   OR-in `0o111` when preserving existing.
4. `pre-push` template missed `cat >/dev/null` for stdin. v2: included.
5. `uninstall_deletes_hook_file_when_empty` was loose. v2: only delete
   when we Created AND post-strip == auto-shebang exactly. Track via
   sidecar JSON.
6. Hook framework collision was undocumented. v2: detect-and-warn
   at install with `--quiet` opt-out.
7. `Commands::Install` parent shape was vague. v2: explicit subcommand
   nesting mirroring `Commands::Pack`. Old `InstallCloning` stays.
8. `discover_repo` only handled `.git/` as dir. v2: also handles
   `.git` file (`gitdir: ...`) for worktrees + submodules.
9. Missing tests for hookspath / framework / pre-push terminal /
   executable-bit-preserve. v2: 5 added.
10. events.json — verified `git_commit` exists; `git_merge`,
    `git_rewrite`, `git_push` missing. Add only the 3 missing.
