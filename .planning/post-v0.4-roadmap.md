---
status: forward-looking; depends on v0.4 shipping first
horizon: v0.4.1 → v1.0
---

# Post-v0.4 build roadmap — "voice as a platform"

Once v0.4 ships ("Studio Quality clones on your local machine"), the
core engine + clone + note loop unlocks a long tail of "X reads Y" use
cases. This document is the parking lot for that work.

Each milestone here is sized as one focused release cycle. Items can be
pulled forward or deferred based on user signal post-launch.

---

## v0.4.1 (immediate post-launch polish, ~1-2 weeks)

The v0.4 deferrals + the obvious "users will ask for this on day 2."

| ID | Item | Notes |
|---|---|---|
| 411 | **PDF intake** | `voiceforge note --in book.pdf` via `pdftotext` (poppler-utils, already in v0.4 install). Plain-text extraction; chapter detection in 412. |
| 412 | **EPUB intake + chapter detection** | `epub` crate or `pandoc` shell-out. `--per-chapter` emits one WAV per detected chapter. Markdown headings + EPUB nav.xhtml + PDF layout heuristics as the three signals. |
| 413 | **m4b output format** | Proper audiobook container with chapter markers. Plays in iOS Books, Audible-compatible apps. ffmpeg-based. |
| 414 | **mp3 output** | Smaller files for distribution. ffmpeg `-codec:a libmp3lame -q:a 4`. |
| 415 | **`voiceforge pack render <my-cloned-voice>`** | Render the 13 standard event WAVs from any cloned voice → personal pack with sub-100ms terminal-reaction playback. Same fish-speech pipeline that built the 5 official packs, exposed as a CLI command. Bridges "I cloned a voice for narration" with "I want it to also yell at me when builds fail." |
| 416 | **`voiceforge cache prune`** | Manual + auto on engine startup. Drops orphan chunk WAVs whose recipe doesn't match any current voice. Soft cap 5 GB; LRU evict. Already in v0.4 plan but ship visible UX in 0.4.1. |

---

## v0.5 (the "voice as a platform" beat, ~3-4 weeks)

This is where voiceforge stops being a tool and starts being infrastructure.

| ID | Item | Notes |
|---|---|---|
| 51 | **Multi-voice cast in `note`** | `[griffin] hi there lois [brian] sigh` → multi-voice narration via single command. Existing 4.3 cast feature already works on event reactions; extend to `voiceforge note` so you can author scripted dialogue. Killer for "podcast intro" use case. |
| 52 | **`voiceforge stream`** | `tail -f your-log \| voiceforge stream --voice tyson` → live narration of streaming text. ChatOps with audio. Backpressure-safe (skip lines if synth can't keep up; warn loudly). |
| 53 | **Browser extension (Chrome/Safari/Firefox)** | Right-click any text on a webpage → "Narrate this in [voice]" → WAV saved to ~/Downloads. Talks to local `voiceforge daemon` over the existing Unix socket. 100% local; no website knows about voiceforge. |
| 54 | **Community voice marketplace** | `humancto/voice-forge-packs` index already exists for the 5 official packs; expand with user-submitted packs from cloned voices. Strict opt-in, license-tier enforcement (CC0 / educational / character / public-figure). PR-based contribution. |
| 55 | **Delete GPT-SoVITS legacy code** | One release after v0.4's deprecation window. Removes `CloningEngine`, `clone_voice.sh` legacy, `services/tts-server/server.py`, schema-1 reader. ~1-2K LoC delete. |
| 56 | **Linux x86_64 first-class** | Promotes Linux from "experimental" to "supported." Auto-detect CUDA, full smoke test gate on Ubuntu 22.04 + 24.04 in CI. |

---

## v0.6 (post-launch polish + integrations, ~4-6 weeks)

Quality-of-life and ecosystem integrations.

| ID | Item | Notes |
|---|---|---|
| 61 | **Web UI** | Local-first. Queue management, sample browsing, drag-drop "render this PDF." `voiceforge ui` opens a browser tab to `localhost:5557`. Built with HTMX + the existing daemon socket — no JS framework. |
| 62 | **Voice mixing + editing** | Fade-in/fade-out, optional background music track, audiobook-style intros/outros, per-paragraph re-synth without re-running whole narration. Useful for serious audiobook production. |
| 63 | **iOS Shortcuts integration** | "Hey Siri, narrate this in Peter's voice" → Shortcut talks to your Mac via Tailscale or local network → `voiceforge note` runs → result returned to phone. Requires a small `voiceforge http` bridge (separate from daemon's Unix socket). |
| 64 | **Voice fine-tuning UI** | Adjust per-voice parameters (temperature, speaking rate, pitch shift). Saved to voice profile. Today fish-speech defaults are baked in. |
| 65 | **Pronunciation overrides** | `voiceforge note --pronounce "voiceforge=voice forge"` for proper noun handling. Per-voice + global dictionaries. |
| 66 | **`voiceforge transcribe`** | Reverse direction: feed an audio file, get transcript via Whisper-medium (already installed). Useful precursor to "edit narration text → re-synth modified passages." |

---

## The use-case catalog (what people will actually do)

These motivate the build. Each is one CLI command + the existing pipeline:

1. **Personal audiobooks.** Narrate paid PDFs you own, in any voice you'd actually want to listen to. Fully private.
2. **Daily AI agent summaries.** `claude -p "summarize my work today" \| voiceforge note --voice tyson` → wake up to a spoken summary instead of unread Slack threads.
3. **Code review commentary.** `gh pr view 123 --json body \| voiceforge note --voice griffin` → roast your colleague's PR in Peter's voice.
4. **Recipe narration.** Morgan Freeman reads tonight's pasta instructions while you cook.
5. **Meeting summaries.** Tyson narrates the 1-page brief your AI produced from the Zoom transcript.
6. **Custom podcast intros.** Different voice each episode; render once, reuse forever.
7. **Audio diary playback.** Write your journal entry → hear it back in any voice. Therapeutic + funny.
8. **Walking-around narration.** Save articles during the day → batch-narrate overnight → listen on commute.
9. **Multi-voice scripted dialogue.** `[griffin] hi lois [brian] *sigh* [stewie] WHAT IS THIS, PETER` → narrated as a Family Guy cold-open. v0.5 territory.
10. **Live ChatOps with audio.** Slack alerts pipe through `voiceforge stream` so your team's incident channel actually speaks. v0.5 territory.

---

## What this does NOT promise

Out-of-scope items the user community will inevitably ask for; flagged so we don't accidentally roadmap them:

- **Real-time conversational TTS** (sub-100ms first-byte). Fish-speech S2 Pro is studio-quality, not low-latency. Stay focused.
- **Singing voice synthesis.** Different model class entirely.
- **Voice anti-spoofing / watermarking.** Important and we should consider it; not in scope until there's evidence of misuse.
- **Cloud sync of voices/cache.** Local-first is a feature, not a limitation.
- **Voice training from < 60s of audio.** Fish-speech's quality degrades; we enforce 60s minimum at clone time.
- **Real-time voice changing** ("speak into mic, hear your voice as Peter"). Different latency budget; future research project.
