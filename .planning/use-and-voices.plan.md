# Plan: voiceforge use + voices list/remove (ROADMAP 2.6 + 2.7)

## Goal

```bash
voiceforge use peter                    # set active voice in ~/.voiceforge/config.toml
voiceforge voices                       # list all voices (built-in + cloned)
voiceforge voices remove peter          # remove a cloned voice
```

After this lands, the demo pitch is:

```bash
voiceforge clone peter ./peter.wav
voiceforge use peter
voiceforge run -- npm test              # speaks in peter's voice (no --voice flag needed)
```

## Out of scope

- `voiceforge voices install <pack>` — bundled-catalog downloads, ROADMAP 2.5+ follow-up.
- `voiceforge use --temporary` — single-session override; defer.
- Listing remote/registry voices.

## Files

### Modified

- `apps/voiceforge-cli/src/main.rs`:
  - `Commands::Use { name: String }` — writes `active_voice = "<name>"` to `~/.voiceforge/config.toml`. Validates voice exists (built-in preset or cloned profile) before writing.
  - Replace existing `Commands::Voices` (no args) with `Commands::Voices { #[command(subcommand)] action: Option<VoicesAction> }`.
  - `enum VoicesAction { Remove { name: String, #[arg(long)] force: bool } }` — `--force` skips the confirmation prompt. None action = list (existing behavior).
  - `Commands::Run` and `Commands::Say` already take `--voice` with `default` as fallback. When the user runs them without `--voice`, fall back to `config.toml`'s `active_voice`. (Today they hardcode `"default"`.)
  - Same for `Commands::Run` (currently doesn't take `--voice` at all — add `--voice` flag with same fallback chain).
- `apps/voiceforge-cli/src/config.rs`:
  - Add `pub fn read_active_voice() -> Result<String>` — reads `~/.voiceforge/config.toml`'s `active_voice`, falls back to `"default"` if missing or unparseable. Doesn't error on missing file (first-run case).
  - Add `pub fn write_active_voice(name: &str) -> Result<()>` — atomic write (tmp+rename). Preserves any other config keys via parse-edit-write rather than overwriting the whole file.
- `apps/voiceforge-cli/src/voices.rs`:
  - Add `pub fn list_cloned_voices() -> Result<Vec<VoiceProfile>>` — walks `voices_dir/`, parses every `<name>/profile.toml`, skips broken ones with a warning to stderr.
  - Add `pub fn remove_cloned_voice(name: &str) -> Result<()>` — validates name, asserts voice exists, removes the dir + invalidates cache entries (cache key includes `created_at` so this is just a `rm -rf` of the voice dir; cache files orphan and will be cleaned by cache-eviction in a future PR).

### New behavior

- `voiceforge voices` (no subcommand) prints two sections — built-in presets + cloned voices, with a `*` next to the active voice.
- `voiceforge voices remove <name>` errors clearly on built-in presets ("can't remove built-in voice X; use voiceforge voices --reset to restore defaults" — defer the reset).
- `voiceforge use <name>` errors if the name isn't found in either set.

## Tests

- `config.rs` `#[cfg(test)] mod tests` (#[serial]):
  - `read_active_voice_returns_default_when_missing`
  - `read_active_voice_round_trips_known_value`
  - `write_active_voice_preserves_other_keys` — pre-write a config with `other_key = "x"`, write `active_voice`, assert both keys present after.
- `voices.rs` adds:
  - `list_cloned_voices_skips_broken_profiles` — stage one valid + one missing-aux dir, assert list returns only the valid one.
  - `remove_cloned_voice_deletes_dir`
  - `remove_cloned_voice_rejects_invalid_name`
  - `remove_cloned_voice_errors_when_absent`
- `main.rs` integration smoke (via shelling to the binary):
  - `voiceforge use nonexistent` → exit 1 with clear error.
  - `voiceforge voices remove preset_name` (built-in) → exit 1.
  - Round trip: `voiceforge use peter` then `voiceforge say` (no --voice) speaks via `peter` (via env-set VOICEFORGE_HOME and stub voice).

## Verify

1. `cargo test` — all green.
2. `voiceforge use default` writes `active_voice = "default"` to `~/.voiceforge/config.toml`.
3. `voiceforge voices` shows the built-in 5 + any cloned voices, marks active with `*`.
4. After `voiceforge use peter`, `voiceforge say --text "hi"` (no --voice) speaks in peter.
5. `voiceforge voices remove peter` deletes `~/.voiceforge/voices/peter/`; `voiceforge voices` no longer lists peter.
6. Built-in voices reject removal with a clear error.

## Atomic commits

1. `feat(config): read_active_voice + write_active_voice with key preservation`
2. `feat(voices): list_cloned_voices + remove_cloned_voice`
3. `feat(cli): voiceforge use <name> + voices [remove] subcommands`
4. `feat(cli): say/run fall back to active_voice when --voice unset`
5. `test(integration): use + voices round-trip smoke`
