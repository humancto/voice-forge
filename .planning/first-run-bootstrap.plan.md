# Plan: first-run bootstrap (ROADMAP 1.2)

## Goal

On first invocation of any `voiceforge` subcommand, ensure
`~/.voiceforge/` exists with the canonical layout and a default
`config.toml`:

```text
~/.voiceforge/
├── presets/        (populated from embedded defaults)
├── cache/          (already created lazily by EmbeddedEngine in 1.1)
├── voices/         (empty, ready for `voiceforge clone` in 2.2+)
├── embeddings/     (empty, populated by `voiceforge embed` in 2.3)
├── logs/           (empty, for daemon + audit logs in 1.8+)
└── config.toml     (default: `active_voice = "default"`)
```

Idempotent: re-running never overwrites a customized preset or
config. First-run prints a one-time banner so the user sees what
happened.

## Out of scope

- `voiceforge doctor` (item 1.6 — adds a richer health summary).
- Validating the contents of preset files beyond what `config::load_presets`
  already does.
- Logging framework — `eprintln!` for now; tracing is a follow-up.

## Files

### New

- `apps/voiceforge-cli/src/bootstrap.rs`:
  - `pub fn ensure_voiceforge_home() -> anyhow::Result<BootstrapReport>`
    — idempotent. Creates each subdir if missing, writes
    `config.toml` if missing, copies any embedded preset that doesn't
    already exist on disk. Returns a report describing what was done
    so callers can decide whether to print the first-run banner.
  - `pub struct BootstrapReport { pub created_home: bool,
pub created_dirs: Vec<&'static str>,
pub copied_presets: Vec<String>, pub wrote_default_config: bool }`
  - `BootstrapReport::is_first_run(&self) -> bool` — true when
    `created_home` (the parent `~/.voiceforge/` itself didn't exist
    before).
  - `pub fn print_first_run_banner(report: &BootstrapReport)` — only
    prints if `is_first_run`. Lists the created path and the
    next-step command.

- Default config at `apps/voiceforge-cli/embedded/config.toml`
  (compiled in via `include_str!`):
  ```toml
  # ~/.voiceforge/config.toml
  # Edit this file to change the default voice that `voiceforge say`
  # and `voiceforge run` use when no --voice flag is passed.
  active_voice = "default"
  ```

### Modified

- `apps/voiceforge-cli/src/main.rs` — call `bootstrap::ensure_voiceforge_home()?`
  at the top of `main()`, before subcommand dispatch. Print the
  first-run banner once.
- `apps/voiceforge-cli/Cargo.toml` — add `toml = "0.8"` (read-only
  for now; bootstrap only writes the file via `fs::write` of a
  static template).

## Tests

`apps/voiceforge-cli/src/bootstrap.rs` `#[cfg(test)] mod tests`:

- `creates_layout_in_empty_home` — pass a tempdir as `VOICEFORGE_HOME`,
  call `ensure_voiceforge_home()`, assert all 5 dirs + config.toml
  exist; assert `is_first_run() == true`; assert presets directory
  has the 5 embedded JSONs.
- `is_idempotent` — call twice, second call's report has
  `is_first_run == false` and `copied_presets.is_empty()`.
- `preserves_user_customized_preset` — pre-populate
  `home/presets/default.json` with hand-edited content, call
  `ensure_voiceforge_home()`, assert the file's content is unchanged
  (compare bytes).
- `preserves_user_config` — pre-populate `home/config.toml` with
  custom content, call bootstrap, assert content unchanged.
- `creates_only_missing_dirs` — pre-create `home/cache/`, call
  bootstrap, assert `created_dirs` doesn't include "cache".

All tests use `#[serial]` because they touch `$VOICEFORGE_HOME`.

## Verify

1. `cargo test bootstrap::` — all five pass.
2. `cargo test` (full) — no regressions, 39+ tests.
3. Manual: `rm -rf ~/.voiceforge && voiceforge voices`. First run
   should print the banner; layout should exist; second run should
   be silent.

## Risks

- **First-run banner spam** if the user deletes `~/.voiceforge` then
  runs many subcommands fast. Acceptable — `is_first_run` is
  per-process, not persistent state.
- **config.toml format drift.** This PR ships only `active_voice`.
  Future fields land additively in the embedded template; bootstrap
  never rewrites an existing file, so old users get the new fields
  by deleting the file (loud) or hand-editing (silent).

## Atomic commits

1. `feat(bootstrap): add ensure_voiceforge_home with embedded config.toml`
2. `feat(cli): call bootstrap at startup; print first-run banner once`
