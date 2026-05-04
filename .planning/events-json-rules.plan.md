# Plan: wire events.json into runner.rs (ROADMAP 1.4)

## Goal

`voiceforge run -- <cmd>` picks the spoken line randomly from
`configs/rules/events.json` instead of hardcoding two strings. Falls
back to the hardcoded defaults when the file is missing or malformed.

## Out of scope

- Custom user rules in `~/.voiceforge/rules.json` (that's a follow-up).
- LLM-generated reactions (4.1).
- New event types (`git_commit`, `daemon_alive` already in events.json
  but not yet wired — leave that for the daemon work in 1.8).

## Files

### New

- `apps/voiceforge-cli/src/rules.rs` — pure module:
  - `pub struct EventRule { pub voice: String, pub lines: Vec<String> }`
  - `pub struct Rules(HashMap<String, EventRule>)` newtype with:
    - `Rules::load(path: &Path) -> anyhow::Result<Rules>` — reads + parses.
    - `Rules::default_builtin() -> Rules` — embedded fallback (the same
      two reactions that `runner.rs` hardcodes today, plus the rest of
      events.json baked in via `include_str!`).
    - `Rules::pick<R: Rng>(&self, event: &str, rng: &mut R)
-> Option<(&str, &str)>` — returns `(voice, line)` with the line
      chosen uniformly. `None` when the event class isn't in the map.
  - `pub fn resolve_rules_path() -> Option<PathBuf>` — searches
    `$VOICEFORGE_HOME/rules/events.json` → `~/.voiceforge/rules/events.json`
    → repo-relative `configs/rules/events.json` (walks up looking for
    the file). Returns `None` if nothing found, signalling "use the
    embedded default."

### Modified

- `apps/voiceforge-cli/src/main.rs` — `mod rules;`.
- `apps/voiceforge-cli/src/runner.rs`:
  - Load rules at the top of `run_command` via `Rules::load(...)
.unwrap_or_else(|_| Rules::default_builtin())`.
  - Map exit success/failure to `"build_success"` / `"build_failed"`
    event keys.
  - Pick line via `rules.pick(event, &mut rand::thread_rng())`. On
    `None` (event not configured), fall back to a tiny inline string
    pair so the runner never silently goes mute.
- `apps/voiceforge-cli/Cargo.toml` — `rand` is already a dep, no
  additions.

## Tests

- `apps/voiceforge-cli/tests/rules.rs` — integration:
  - `loads_repo_rules` — uses `resolve_rules_path()` against the repo
    checkout, asserts at least `build_failed` and `build_success` exist.
  - `picks_line_deterministically_with_seeded_rng` — seeds a
    `StdRng::seed_from_u64(42)`, calls `Rules::pick("build_failed",
&mut rng)` three times, asserts the result is one of the configured
    lines and is reproducible across runs (snapshot the indices).
  - `falls_back_to_builtin_when_file_missing` — points at a tempdir
    with no rules file, asserts `Rules::default_builtin()` returns a
    map with at least `build_failed`/`build_success`.
  - `rejects_malformed_json` — writes garbage JSON in tempdir,
    asserts `Rules::load` returns `Err` whose context mentions the
    path.

## Verify

1. `cargo test --test rules` — all four pass.
2. `cargo test` (full) — no regressions, including audio_ingest.
3. Manual: `voiceforge run -- false` runs `false`, exits non-zero,
   speaks one of the three `build_failed` lines (run 5 times, expect
   variation).

## Risks

- **No daemon yet.** `git_commit` / `daemon_alive` lines are loaded but
  unreached from runner. That's intentional — the daemon (1.8) and git
  hook (3.2) consume them later.
- **`rand::thread_rng()` is non-deterministic.** Test uses `StdRng`
  with a fixed seed for reproducibility; production stays on
  thread_rng so users get variety.

## Atomic commits

1. `feat(rules): add rules module with embedded defaults + path resolver`
2. `feat(runner): pick reaction line from events.json with builtin fallback`
3. `test(rules): integration tests + snapshot for seeded picks`
