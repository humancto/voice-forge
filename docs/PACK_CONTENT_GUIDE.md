# Pack content style guide

This guide is for **anyone authoring a voice pack** for distribution through [voice-forge-packs](https://github.com/humancto/voice-forge-packs). It covers what makes a pack good, what makes a pack legally durable, and what gets a pack rejected at PR review.

If you're rendering a pack for personal use only, none of this is binding — just have fun. If you want it in the public index, this is the bar.

---

## The three tiers

Every pack declares a `tier` in its `manifest.toml`. The tier governs both the legal posture and the content rules.

### `tier = "character"`

**Fictional characters from animated shows, video games, films.** The clone is _of a character_, not of a person. Right-of-publicity exposure is on the voice actor, but they didn't say the synthetic line — the character did.

Examples: Peter Griffin, Stewie Griffin, Bender, Eric Cartman, Homer Simpson.

**Content rules**:

- Lean **into** the character's idioms, catchphrases, and tics. The point is recognizability.
- The phrase should sound like something the character _would_ say, in the show's tone.
- It's fine to break the fourth wall ("Holy crap Lois, the build is on fire") — that's the joke.

**Legal posture**: lowest risk. Default-shippable. Fan-fiction territory.

### `tier = "public-figure"`

**Real living public figures** — late-night hosts, scientists, actors, politicians — whose voices are widely broadcast and who have a recognizable public persona.

Examples: Jimmy Kimmel, Neil deGrasse Tyson, Bob Ross, Werner Herzog, Gordon Ramsay, Trump, Obama.

**Content rules — these are non-negotiable**:

1. **Phrases must read as obviously synthetic dev-feedback.** No one should be able to read the phrase out of context and think "the real person said this." A deploy-failed line about your code is fine. A political opinion is not.

2. **No fake political statements.** Even if the figure has a clear political identity, do not put policy positions in their mouth. "Trump on the build failed" is fine; "Trump on healthcare" is not.

3. **No fake endorsements.** No "I love using VoiceForge" lines. No product mentions outside of generic dev tools (npm, git, AI agents).

4. **No defamation, no slurs, no anything they could plausibly object to as "I would never say that."** When in doubt, ask: would the public figure plausibly write this same joke about themselves? If yes, ship it. If no, rewrite it.

5. **In-character idioms are still encouraged.** Bob Ross saying "happy little bugs" is _more_ obviously synthetic than Bob Ross delivering a flat status report.

**Legal posture**: medium risk. Ships under the educational-use disclaimer. Subject to 48-hour takedown on rights-holder complaint.

### `tier = "experimental"`

**Anything new or edge-case.** Hidden from default `voiceforge pack list`, surfaced via `--all`. Use this for:

- New voices we haven't validated for quality yet (the model might not handle them well).
- Voices where the rights posture is unusual.
- Joke / one-off packs that aren't ready for the public index.

**Content rules**: same as `public-figure` but everything is opt-in.

---

## Phrase design — the 13 events

Every pack must implement the v1 schema's 13 events (see `packs/peter/phrases.json` for the canonical example). The events are:

| Event             | When VoiceForge plays it                             |
| ----------------- | ---------------------------------------------------- |
| `build_success`   | Build pipeline succeeded                             |
| `build_failed`    | Build pipeline failed                                |
| `tests_passed`    | Test suite green                                     |
| `tests_failed`    | Test suite red                                       |
| `lint_error`      | Linter (eslint, clippy, ruff, …) found issues        |
| `type_error`      | Type checker (tsc, mypy, …) failed                   |
| `deploy_success`  | Deploy / push to production succeeded                |
| `deploy_failed`   | Deploy / push to production failed                   |
| `commit_success`  | git commit succeeded                                 |
| `push_rejected`   | git push rejected (conflict, lease, etc.)            |
| `secret_detected` | Secret-scanner found a leaked credential in the diff |
| `agent_done`      | An AI agent finished its task                        |
| `agent_stuck`     | An AI agent timed out / asked for help               |

**Phrase length**: aim for 6–18 words. Render time on Mac CPU scales with token count; longer phrases hit the ~10-min/phrase ceiling and start getting unreliable past 25 words.

**Phrase tone**: each pack should have a clear, consistent voice. Read all 13 phrases aloud in your own attempt at the voice. If two phrases feel like different people, rewrite the one that's off-tone.

**Punctuation matters**:

- One sentence per phrase, or two short sentences max.
- End with a period — never a question mark unless the line genuinely needs one (lots of question marks in a row sound weird in synthesis).
- Avoid `--` em-dashes; the model handles them inconsistently.
- Avoid all-caps screaming except in `tier="character"` packs where it's clearly part of the voice (Ramsay can scream; Obama cannot).

---

## Reference clip selection

(Detailed in [PACK_RENDERING.md](PACK_RENDERING.md). Summarized here.)

**The one rule that matters more than all the others**: **single speaker, no contamination**. Any audience laughter that overlaps the speaker's voice, any clip-cut to another person, any music bed mid-line, will degrade the clone audibly. We have caught this twice now (Peter compilation + Kimmel monologue) and each time the fix was to scan the whisper transcript for clean stretches before trimming.

**Recipe**:

1. Get ≥ 60s of source audio (ideally 60–90s).
2. Whisper-transcribe it (`--output_format srt` for timestamps).
3. Read the transcript. Identify segments where:
   - Only the target speaks (no interviewer, no clip).
   - No prolonged silence / applause / laughter.
   - The speaker is in a "natural" register, not yelling or whispering (model handles middle-range better).
4. Trim with `ffmpeg -ss <start> -to <end> -af "loudnorm=I=-16:TP=-1.5:LRA=11" -ar 32000 -ac 1`.
5. **32 kHz is required.** It matches fish-speech S2 Pro's native rate. Do not use 22050.

**Source provenance**: every pack manifest must include the original `source_clip_url`. PR reviewers will check this.

---

## What gets a PR rejected at review

1. **Reference clip has multiple speakers.** Single speaker, end of story.
2. **Reference clip has audible reverb / room acoustics that aren't representative.** A mic'd-up speech is fine; a Zoom call with echo is not.
3. **Phrase content violates the tier's rules.** See above — political statements in `public-figure` packs, fake endorsements, etc.
4. **Pack tries to impersonate a private individual without consent.** Public figures only.
5. **Pack misattributes the source.** The `source_clip_url` must match what the audio actually is.
6. **Schema violations.** `schema_version` missing, wrong field names, phrases as array instead of map. The renderer will reject these too.
7. **Quality is bad.** Even with a clean reference, the model sometimes produces audibly poor output. PR author must listen to all 13 WAVs and rerun any that sound off. We will spot-check at review.
8. **Missing `LICENSE-AUDIO.md` acknowledgement.** Every pack `manifest.toml` must reference the audio license.

---

## Recommended pack lineup

Voices we'd love community PRs for, with a one-line pitch each:

### Tier 1 — character

- **Peter Griffin** ✅ (shipped)
- **Stewie Griffin** — British baby evil genius
- **Quagmire** — sleazy pilot, "giggity" punctuation
- **Brian Griffin** — pretentious dog
- **Homer Simpson** — "D'oh", food-distracted
- **Eric Cartman** — spoiled-brat resentment
- **Bender** — bitter robot, "bite my shiny metal ass"
- **Patrick Star** — wholesome dumb starfish
- **Yoda** — syntax inverted has the build failed has

### Tier 2 — public-figure

- **Neil deGrasse Tyson** — astrophysics-as-metaphor
- **Bob Ross** — happy little bugs
- **Werner Herzog** — bleak existential narration
- **David Attenborough** — nature-doc commentary
- **Morgan Freeman** — narrator gravitas
- **Gordon Ramsay** — yelling chef yelling at code
- **Bernie Sanders** — passionate yelling
- **Jimmy Kimmel** — late-night-host commentary
- **Trump** — political bombast
- **Obama** — measured speech

If you want to claim one of these, open an issue in voice-forge-packs and we'll mark it "in progress" so two people don't render the same voice.

---

## TL;DR

- **Pick a clear voice with a recognizable persona.** The whole point is that the user installs the pack and immediately laughs at the joke.
- **Reference clip: single speaker, 60s+, 32 kHz, no reverb.**
- **Phrases: 13 events, 6–18 words each, in-character.**
- **Public figures: synthetic dev-feedback only. No politics, no endorsements, no impersonation-bait.**
- **Tier appropriately, attribute thoroughly, listen to every output before submitting.**

If a pack passes all of those, it ships.
