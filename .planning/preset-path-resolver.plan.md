# Plan: fix preset path resolution (ROADMAP 1.3)

## Goal

Replace the `Path::new("../../configs/presets")` hardcode in `config.rs`
— it only works when CWD is `apps/voiceforge-cli/`, breaks on
`cargo run` from the workspace root and breaks for any installed
binary. Use the `paths.rs` helper that landed in 1.4.

## Approach

Mirror the 1.4 rules pattern exactly:

1. Resolution order, first hit wins:
   - `$VOICEFORGE_HOME/presets/`
   - `<repo>/configs/presets/` (via `paths::repo_config_dir()`)
   - Embedded fallback (compile-time `include_str!` of every JSON in
     `configs/presets/`)
2. `load_presets()` returns `Result<Vec<VoicePreset>>` — same signature.
   New behavior: when no on-disk dir exists, returns the embedded
   defaults (Vec of 5 bundled presets). Old behavior was an empty Vec —
   `voiceforge voices` is now usable on a freshly-installed binary.

## Files

### Modified

- `apps/voiceforge-cli/src/config.rs`:
  - Add `resolve_presets_dir() -> Option<PathBuf>` mirroring
    `rules::resolve_rules_path()`.
  - Add `embedded_presets() -> Vec<VoicePreset>` baking in each preset
    JSON via `include_str!` and parsing once.
  - `load_presets()` rewires to: try resolved dir → on error or empty,
    return embedded.
  - Push down: skip non-JSON files (already does), but also reject
    files whose parsed `id` is empty.

### Tests

- `apps/voiceforge-cli/src/config.rs` — unit tests at the bottom of the
  file:
  - `embedded_presets_have_default` — at least the `default` and
    `angry_duck` presets are present.
  - `loads_from_voiceforge_home` — write a custom preset to a
    tempdir's `presets/` subdir, set `VOICEFORGE_HOME` to that
    tempdir, call `load_presets()`, assert the custom preset is
    returned and embedded ones aren't (when on-disk dir wins, embedded
    is bypassed).
  - `falls_back_to_embedded_when_dir_missing` — set
    `VOICEFORGE_HOME` to an empty tempdir, assert the returned vec
    matches embedded.
  - `skips_non_json_files` — drop a `notes.txt` next to a real preset
    in the tempdir; assert it's skipped without erroring.
  - `rejects_malformed_preset_json` — drop garbage `bad.json`; assert
    `load_presets()` returns `Err` whose context mentions the file.

## Verify

1. `cargo test --bin voiceforge -- config::` — all five pass.
2. `cargo test` (full) — no regressions, including the rules
   integration tests.
3. Manual: `cd /tmp && /path/to/voiceforge voices` — outputs the five
   embedded presets even though CWD has no `configs/`.

## Atomic commits

1. `feat(config): preset resolver with embedded defaults via include_str!`
2. `test(config): unit tests for resolver precedence + embedded fallback`
