---
roadmap_item: 4.3 Multi-voice personality presets (cast)
status: plan v2 — rust-expert REVISE folded
---

# 4.3 Multi-voice personality presets

## Goal (from ROADMAP)

> Multi-voice personality presets: a preset can declare a cast, LLM returns
> `(voice, line)` tuples played in sequence.

A "cast" is a list of voices that together react to an event with a _short
exchange_ — e.g. for `build_failed`, cast = `[peter, brian]` produces
"Peter: oh no the build broke. Brian: this is what tests are for." Played
in audible sequence with character change. Demo win for PH.

## Plan v2 deltas (rust-expert REVISE fold)

| #       | Item                                                                | Resolution                                                                                                                                                                                                                                                                                                                            |
| ------- | ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **SS1** | Concurrent casts interleave audio (also affects single-voice today) | **Single-consumer playback queue.** New `PlaybackQueue` with an mpsc channel + a dedicated tokio task that pops `(audio_path, semaphore_permit)` items and plays them via `spawn_blocking`. Both single-voice AND cast paths post to the queue. Per-event ordering preserved; no overlap ever. Fixes a latent bug AND unblocks casts. |
| **B2**  | Vec-trait-break has 15-20 touch points                              | **Default-method extension.** Keep `react(&str) -> (String, String)` unchanged. Add `react_cast(&str) -> Vec<(String, String)>` with default impl `vec![self.react(event).await]`. Daemon calls `react_cast` exclusively. Only `LlmProvider` overrides. All 272 existing tests stay green.                                            |
| **B3**  | "play_blocking" rename was confused                                 | **No trait change.** `RodioSink::play` already blocks until the clip ends. The daemon's `spawn_blocking` already wraps it; we just need to `await` the join handle for casts (or post to the queue). Document the existing contract: `AudioSink::play` SHALL block until playback finishes.                                           |
| **B6**  | Sequencing (a) leaves clients waiting 9s+ → hook timeouts           | **Pattern (b): reply immediately, play in background.** Daemon resolves the cast turns synchronously (LLM call), replies with `{ok:true, spoken:[{voice,line},...]}` representing what _will_ be spoken, then posts the turns to the playback queue. Pairs naturally with SS1: queue consumer plays in order.                         |
| **B4**  | Untagged enum dispatch order                                        | Put `Cast` variant **first** in the enum; serde tries in declaration order. Defensive ordering against future loosening of `Single::spoken`.                                                                                                                                                                                          |
| **B5**  | StaticProvider silently ignores configured casts                    | At daemon startup, if `casts.is_empty() == false` AND provider is `StaticProvider`, eprintln once: `voiceforge: N casts configured but no LLM provider — set VOICEFORGE_LLM_URL to enable, or remove ~/.voiceforge/casts/`. Doctor row also surfaces the mismatch.                                                                    |
| **B7**  | `max_turns = 0` is a silent footgun                                 | `NonZeroU8` for `max_turns`. Push validation to deserialize time; can't enter the cast loop with a zero cap.                                                                                                                                                                                                                          |
| **B9**  | Cast voice allowlist must be cast-specific                          | Cast validation uses `HashSet::from_iter(cast.voices.iter().map(normalize_voice))` — NOT the global `voices_normalized`. LLM returning `kimmel` for a `[peter, brian]` cast → fall back.                                                                                                                                              |
| **B10** | "No voice speaks twice in a row" enforcement                        | **Accept + dedupe**: collapse consecutive same-voice turns into one (concatenate lines with a space). Avoids LLM-violation fallback churn for a cosmetic rule.                                                                                                                                                                        |
| **R11** | Two prompt templates will drift                                     | **Unified `build_prompt(event, turns: NonZeroU8, allowed: &HashSet<String>)`**. Single-voice = `build_prompt(event, NonZeroU8::new(1).unwrap(), &full_voices)`. Cast = `build_prompt(event, cast.max_turns, &cast.voices)`. `try_llm` always parses `Vec<LlmDirect>`; length-1 is the no-cast case.                                   |
| **R12** | Per-file casts/ doesn't scale                                       | **Single `casts.toml`** at `~/.voiceforge/casts.toml` or `$VOICEFORGE_HOME/casts.toml`, with top-level `[casts.<event>]` table. Repo ships `configs/casts.toml` as default. Per-file form NOT supported in v1; revisit if requested.                                                                                                  |
| **R13** | Per-turn synth failure → uncanny silence                            | **Abort + fall back**: if any turn fails synthesis, scrap the cast, fire one single-voice line for the event. "Total silence for one turn is worse than scrapping the bit and saying something."                                                                                                                                      |
| **R14** | `spawn_blocking` JoinError on shutdown                              | Cast loop handles `JoinError::cancelled` cleanly (stops the cast; doesn't propagate).                                                                                                                                                                                                                                                 |
| **R15** | Cancel-safety doc                                                   | Pattern (b) means client disconnect doesn't matter — playback task is detached. Document on the cast loop.                                                                                                                                                                                                                            |
| **M16** | No test for cast-prompt voice-list inclusion                        | Add `cast_prompt_includes_only_cast_voices`.                                                                                                                                                                                                                                                                                          |
| **M17** | No test for cast-without-LLM warning                                | Add `daemon_warns_when_casts_configured_without_llm`.                                                                                                                                                                                                                                                                                 |
| **M18** | Sequencing test with instant sink can't prove ordering              | New `BlockingRecordingSink` that holds for N ms per call; sequencing test asserts turn-2's `play()` start-time > turn-1's `play()` end-time + ε.                                                                                                                                                                                      |
| **M20** | Wire-compat note for reply schema bump                              | Add to README + commit message. Existing 3.3 hooks check `ok` only — unaffected.                                                                                                                                                                                                                                                      |
| **M21** | Doctor row should preview cast contents                             | Doctor reports `casts: 2 (build_failed=[peter,brian], deploy_failed=[trump,musk])` — helps users sanity-check without running the daemon.                                                                                                                                                                                             |
| Nit     | `CastConfig::voices: Vec<String>` allows dupes                      | Validator at deserialize: dedup with warning.                                                                                                                                                                                                                                                                                         |
| Nit     | `embedded_default()` misleading                                     | Rename → `Casts::empty()`.                                                                                                                                                                                                                                                                                                            |
| Nit     | `Casts(HashMap)` tuple-struct                                       | Use `Casts { by_event: HashMap, source: Option<PathBuf> }` for diagnostics.                                                                                                                                                                                                                                                           |
| Nit     | "Voices alternate" rule is cosmetic                                 | Drop the constraint from the prompt; rely on the dedupe pass at parse time.                                                                                                                                                                                                                                                           |

## Surface (final)

- **NEW** `apps/voiceforge-cli/src/cast.rs` (~200 lines + tests):

  ```rust
  use std::num::NonZeroU8;

  #[derive(Debug, Clone, Deserialize)]
  pub struct CastConfig {
      pub voices: Vec<String>,        // 2-4 recommended; cap 6
      #[serde(default = "default_max_turns")]
      pub max_turns: NonZeroU8,
  }

  pub struct Casts {
      by_event: HashMap<String, CastConfig>,
      source: Option<PathBuf>,
  }

  impl Casts {
      pub fn load_from_path(path: &Path) -> Result<Self>;
      pub fn empty() -> Self;
      pub fn for_event(&self, event: &str) -> Option<&CastConfig>;
      pub fn is_empty(&self) -> bool;
      pub fn iter(&self) -> impl Iterator<Item = (&str, &CastConfig)>;
  }

  fn default_max_turns() -> NonZeroU8 { NonZeroU8::new(3).unwrap() }
  ```

- **NEW** `apps/voiceforge-cli/src/playback.rs` (~120 lines + tests):

  ```rust
  /// Single-consumer playback queue. All audio (single-voice + cast)
  /// posts here. Consumer task pops in FIFO order and plays
  /// sequentially via spawn_blocking. Guarantees: no overlap, ever.
  /// FIFO across event sources, ordered within a cast.
  pub struct PlaybackQueue {
      tx: mpsc::Sender<PlaybackItem>,
  }

  pub struct PlaybackItem {
      pub path: PathBuf,
      pub permit: tokio::sync::OwnedSemaphorePermit,
  }

  impl PlaybackQueue {
      /// Spawn the consumer task; returns the handle (sender side).
      pub fn spawn(sink: Arc<dyn AudioSink>) -> Self;
      /// Post an item. Backpressure if the queue is full (bounded).
      pub async fn push(&self, item: PlaybackItem);
  }
  ```

- **MODIFIED** `apps/voiceforge-cli/src/reaction.rs`:
  - **No breaking change to `react`.** Add default `react_cast`:
    ```rust
    #[async_trait]
    pub trait ReactionProvider: Send + Sync {
        async fn react(&self, event: &str) -> (String, String);
        /// Returns one or more turns. Default: single-element vec
        /// from `react`. Cast-aware providers override.
        async fn react_cast(&self, event: &str) -> Vec<(String, String)> {
            vec![self.react(event).await]
        }
        fn name(&self) -> &'static str;
    }
    ```
  - `LlmProvider`: gets a `casts: Arc<Casts>` field. Override
    `react_cast` to check `casts.for_event`; if present, build cast
    prompt + parse `Vec<LlmDirect>` + validate against cast.voices
    - dedupe consecutive same-voice + return. On any failure, fall
      back to single-voice via `vec![self.react(event).await]`.
  - Unified `build_prompt(event, turns: NonZeroU8, allowed: &HashSet<String>)`.
    Single-voice path uses `turns=1`. Cast path uses cast's max_turns.
  - `try_llm` always parses `Vec<LlmDirect>`; single-voice path takes
    `.into_iter().next()`.
  - `RecordingProvider` keeps `react()` returning the fixed single
    pair. New `RecordingCastProvider` returns a fixed N-element vec.

- **MODIFIED** `apps/voiceforge-cli/src/daemon_server.rs`:
  - `process_frame` for events: `let turns = provider.react_cast(event).await;`
  - For each turn, synthesize sequentially (TTS engine is already
    serialized internally by its own mutex), then push to playback
    queue. Reply BEFORE turn-2 starts synth (option (b)).
  - Single-voice path: same as today, just goes through the queue
    instead of `spawn_blocking` directly.
  - Reply schema:
    ```rust
    #[derive(Serialize)]
    #[serde(untagged)]
    enum Reply {
        Cast { ok: bool, spoken: Vec<TurnReply> },  // first!
        Single { ok: bool, spoken: String, voice: String },
        Err { ok: bool, error: String },
    }
    ```

- **MODIFIED** `apps/voiceforge-cli/src/audio_sink.rs`:
  - Document existing contract: `play` blocks until playback finishes.
  - No method change. Add `BlockingRecordingSink` for sequencing tests.

- **MODIFIED** `apps/voiceforge-cli/src/daemon.rs`:
  - Loads casts from `~/.voiceforge/casts.toml` (falling back to
    embedded `configs/casts.toml`); passes to `select_provider`.
  - At startup, warn if casts loaded but provider is StaticProvider.

- **NEW** `configs/casts.toml` (initial demo):

  ```toml
  [casts.build_failed]
  voices = ["peter", "brian"]
  max_turns = 3

  [casts.deploy_failed]
  voices = ["trump", "musk"]
  max_turns = 3
  ```

- **MODIFIED** `apps/voiceforge-cli/src/doctor.rs`:
  - Row: `casts: 2 (build_failed=[peter,brian], deploy_failed=[trump,musk])` or `casts: 0 (no casts.toml)`.
  - Mismatch warning: if casts > 0 AND provider is "static", row is
    `Warn` not `Ok`.

- **MODIFIED** `apps/voiceforge-cli/src/main.rs`: `mod cast;`, `mod playback;`.

- **MODIFIED** `README.md`: capability paragraph + cast example +
  wire-compat note (3.3 hooks unaffected; new clients can switch on
  `spoken` shape).

## Design choices (folded)

### Why a playback queue (not a mutex)?

Under SS1's pressure-test, three architectures were on the table:

1. `Mutex<()>` around `play()` calls. Works but creates lock contention
   on the audio sink and complicates the cast loop.
2. Single-slot `Semaphore` for cast turns only. Doesn't help the existing
   single-voice overlap bug.
3. Single-consumer mpsc playback queue. Fixes both. Cast loop posts N
   items; single-voice posts 1; consumer plays in FIFO order.

Option 3 wins. It's also the natural fit for option-b sequencing (reply
immediately, queue consumes async).

The queue is bounded (cap 16 — generous for a sane event stream). On
backpressure, the daemon awaits, which translates to client-visible
slowdown rather than dropped audio. Acceptable.

### Why default-method `react_cast`?

All 272 existing tests pass `react()` and destructure a tuple. Bumping
the trait return type breaks every one. `react_cast` with a default impl
that wraps `react()` is zero-touch for everyone except `LlmProvider`.

Trade-off: a hypothetical future provider that's cast-only would need
a vestigial `react()` impl. Acceptable.

### Why option-b sequencing (reply immediately)?

Hooks have timeouts. A 3-turn cast at ~3s/turn = 9-12s wall clock. Git's
pre-commit hook is forgiving (~30s in practice) but Claude Code's hook
budget is shorter, and the user will perceive the daemon as "stuck" if
the hook hangs while audio plays.

Option-b: daemon resolves the cast (one LLM call, ~500-3000ms), replies
with the planned turns, posts to playback queue, returns. Audio plays
async. Same UX shape as today's single-voice path.

Trade-off: clients can't tell when the cast finished. They never could
for single-voice either.

### Why one `casts.toml` not per-file?

Casts are a registry, not user profiles. Per-file made sense for
profiles/manifests where each file represents one entity. A registry
of event→cast mappings is one file. Easier to grok at a glance, easier
to grep, easier to ship a default with the binary.

### Why abort cast on synth failure?

"Peter: oh no the build broke. _[silence]_ Brian: this is what tests are
for." is uncanny — like a phone call dropping mid-sentence. Better to
scrap the cast and ship a single-voice line: "the build failed."

The fallback uses StaticProvider semantics — a deterministic line for
the event from rules.json (or the embedded fallback if not configured).

### Why dedupe consecutive same-voice (not reject)?

Small LLMs routinely violate "alternate voices" no matter how the prompt
is worded. Rejecting → fallback churn → silent-cast UX bug. Deduping
(concatenate consecutive same-voice lines with a space) gives the user a
working cast even when the LLM doesn't perfectly comply. Cosmetic.

### Cast prompt template (unified)

```
You're a reactions engine for `voiceforge`. The user just hit
event="<event>". Respond with <turns> short turn(s) of dialogue.

Each turn picks ONE voice from this list:
<comma-separated allowed voices>

Per turn:
- 8-12 words, max 200 characters
- present tense, in character, no markdown, no emoji

Respond with raw JSON only:
[
  {"voice": "<one of the listed voices>", "line": "<line>"},
  ...
]
```

Single-voice path: `turns=1`, allowed = full voice menu.
Cast path: `turns=cast.max_turns.get()`, allowed = `cast.voices`.

## Test plan (final)

**Cast config (8):**

- [ ] `cast_config_parses_minimal_toml`
- [ ] `cast_config_parses_with_max_turns`
- [ ] `cast_config_rejects_empty_voices`
- [ ] `cast_config_rejects_voices_over_six`
- [ ] `cast_config_rejects_zero_max_turns_via_NonZeroU8` (compile-time enforced; deserialize-time error)
- [ ] `cast_config_dedupes_duplicate_voices_with_warning`
- [ ] `casts_load_from_path_parses_multiple_events`
- [ ] `casts_for_event_returns_none_for_unconfigured`

**LLM cast (10):**

- [ ] `cast_prompt_includes_only_cast_voices` (NOT global menu)
- [ ] `parse_cast_response_returns_list_of_turns`
- [ ] `parse_cast_response_rejects_empty_list`
- [ ] `parse_cast_response_truncates_over_max_turns`
- [ ] `validate_cast_rejects_voice_not_in_cast`
- [ ] `dedupe_collapses_consecutive_same_voice` (turn 1 = peter, turn 2 = peter, turn 3 = brian → 2 turns)
- [ ] `llm_cast_falls_back_on_connection_refused`
- [ ] `llm_cast_falls_back_on_synth_failure_per_turn` (mid-cast)
- [ ] `llm_cast_returns_single_turn_when_no_cast_configured`
- [ ] `llm_react_cast_default_impl_returns_single_turn` (ensures non-LLM providers work)

**Playback queue (5):**

- [ ] `playback_queue_plays_items_in_fifo_order`
- [ ] `playback_queue_serializes_concurrent_pushes` (BlockingRecordingSink: assert turn-2 start > turn-1 end)
- [ ] `playback_queue_drops_permit_after_play`
- [ ] `playback_queue_handles_consumer_panic`
- [ ] `playback_queue_backpressure_when_full`

**Daemon integration (3):**

- [ ] `serve_plays_cast_in_sequence` (BlockingRecordingSink + cast: assert ordering AND no overlap)
- [ ] `serve_replies_before_cast_audio_finishes` (assert reply latency < first turn's playback duration)
- [ ] `serve_warns_when_casts_configured_without_llm` (capture stderr)

**Gates:**

- [ ] `cargo test --all` green
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] `cargo fmt --check` clean

## Edge cases (folded)

1. Cast references unknown voice → log warning at load; cast still loads;
   LLM-side cast voice validation rejects → fall back.
2. LLM returns more turns than `max_turns` → truncate.
3. LLM returns 0 turns → fallback to single-voice.
4. Cast file has typo in event name → that cast never fires; doctor row
   lists configured event names so user spots it.
5. Concurrent same-event events → each `process_frame` is its own task;
   playback queue serializes ALL audio across all tasks. No overlap.
6. Cast playback in progress, new event arrives → both go through the
   queue; user hears them sequentially. Could feel laggy if many events
   pile up; playback queue cap of 16 + backpressure prevents unbounded
   growth.
7. One turn fails synthesis → abort cast, fall back to single-voice line
   for the event.
8. `JoinError::cancelled` from `spawn_blocking` on shutdown → cast loop
   exits cleanly.
9. Client disconnects mid-cast → no impact; playback task is detached
   under option-b.

## Out of scope

- Per-file casts (single `casts.toml` only)
- LLM picking the cast (manual config only)
- Cross-voice timing / overlap effects (strictly sequential)
- Cast-specific system prompt overrides
- Wildcard config (`casts.* = [a, b]`)
- StaticProvider serving pre-canned cast lines (would feel scripted)
- TTS cancellation mid-turn (synth completes; we just don't play it)

## Rollback

Remove `~/.voiceforge/casts.toml` → no casts → behavior identical to 4.1.
Or revert the PR: single-voice path unchanged.

## Reply schema wire-compat note

3.3 hooks (`voiceforge hook`) and the daemon-client smoke tests check
`reply.ok` only. The `spoken` field shape change (string vs array) is
invisible to them. New clients can switch on the JSON type:

```python
spoken = reply["spoken"]
if isinstance(spoken, str):
    print(f"{reply['voice']}: {spoken}")
else:
    for turn in spoken:
        print(f"{turn['voice']}: {turn['line']}")
```

CHANGELOG entry: "Daemon reply `spoken` field is now `String | Array<{voice, line}>`. Existing clients that only check `ok` are unaffected."
