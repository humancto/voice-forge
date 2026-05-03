# VoiceForge Terminal Product Writeup

## One-line description

VoiceForge Terminal is a local voice runtime that gives your terminal consistent synthetic character voices for builds, tests, Git events, AI-agent progress, and custom commands.

## Why this exists

LLM agents are making terminals more conversational. Developers now watch tools like Codex, Claude Code, Cursor, and build systems run for long stretches. Most terminal feedback is still silent, boring, and easy to miss.

VoiceForge makes local developer tooling more expressive.

Examples:

```text
Tests passed -> hype voice
Build failed -> angry duck voice
Git commit created -> smug narrator voice
AI agent stuck -> sarcastic goblin voice
Long task completed -> celebration voice
```

## Core product idea

The terminal emits events. VoiceForge turns those events into short voice reactions.

```text
event + message + voice preset -> local audio
```

## Main users

1. Developers using AI coding agents
2. Power users who want audible terminal alerts
3. Streamers and creators
4. Accessibility users who benefit from spoken terminal summaries

## MVP

The MVP should do five things well:

1. Speak any text locally.
2. Let the user choose a voice preset.
3. Cache repeated generations.
4. Wrap terminal commands.
5. Trigger different voices based on success or failure.

Example:

```bash
voiceforge run -- npm test
```

If `npm test` fails, VoiceForge says a failure line in the selected voice.

## V1 product

V1 should add:

1. Background daemon
2. Preset manager
3. Speaker embedding extraction
4. Configurable rules
5. Command wrappers
6. Installation script
7. macOS audio support
8. Optional local XTTS setup

## V2 product

V2 can add:

1. Streaming TTS
2. LLM-generated reaction text
3. Multiple agent personalities
4. Fine-tuned voice packs
5. GUI preset manager
6. Homebrew distribution
7. Local-only voice library
8. System notification integration

## What “consistent voice” means

There are three meanings people confuse:

### 1. Same identity

The voice sounds like the same synthetic character each time.

Achieved with:

```text
speaker embedding
```

### 2. Same style

The voice has similar pitch, speed, emotion, and cadence.

Achieved with:

```text
fixed generation parameters + preset config
```

### 3. Same exact waveform

The exact same text produces the exact same audio file.

Achieved with:

```text
cache key = hash(text + voice + params)
```

A TTS model alone usually will not guarantee bit-identical audio on every run. Caching does.

## Preset vs embedding vs fine-tuning

### Preset

A JSON config that points to a voice identity and style settings.

### Embedding

A fixed vector extracted from a reference voice. Reusing it improves voice consistency.

### Fine-tuning

A heavier process where a pretrained TTS model is adapted to a specific voice dataset.

Use fine-tuning only after embeddings are not good enough.

## Safety and rights

For private education, you can experiment locally. For any shared product, package, video, demo, or commercial use:

Use:

- your own voice
- consented voices
- hired voice actors
- licensed voice packs
- synthetic voices that are not impersonations

Avoid:

- exact celebrity cloning
- recognizable copyrighted characters
- political impersonations presented as real
- distributing voice models trained on unauthorized audio
