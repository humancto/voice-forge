# Codex / Claude Code Instructions

You are working on VoiceForge Terminal.

## Goal

Build a local-first Rust + Python system that lets terminal events speak using consistent synthetic voices.

Do not turn this into a cloud app. Do not add authentication. Do not add unnecessary frontend UI unless requested. Focus on local developer experience.

## Product principles

1. Local-first
2. Fast feedback
3. Voice consistency
4. Simple configuration
5. Safe defaults
6. Easy to extend

## Architecture

Use Rust for:

- CLI
- daemon
- command wrapping
- cache
- audio playback
- config loading

Use Python for:

- TTS model loading
- XTTS integration
- speaker embedding extraction
- audio generation

## Required Rust crates

Prefer:

- clap for CLI
- tokio for async
- reqwest for HTTP
- serde / serde_json for config
- sha2 for cache keys
- rodio for audio playback
- anyhow for error handling

## Required Python libs

Prefer:

- flask or fastapi
- torch
- TTS
- soundfile if needed

## Implementation order

### Milestone 1

Make this work:

```bash
voiceforge say --text "Hello" --voice default
```

### Milestone 2

Make command wrapper work:

```bash
voiceforge run -- npm test
```

It should detect exit status and speak success or failure.

### Milestone 3

Add cache.

Same text + same voice should reuse existing audio.

### Milestone 4

Add preset config.

Load voices from:

```text
configs/presets/*.json
```

### Milestone 5

Add embedding extraction.

Voice sample:

```text
services/tts-server/voices/angry_duck.wav
```

Embedding:

```text
services/tts-server/embeddings/angry_duck.pt
```

### Milestone 6

Add daemon mode.

The daemon can initially simulate events, then later read from a local socket or file queue.

## Safety constraints

Do not include celebrity names, copyrighted character names, or political impersonation presets in committed sample config.

Use generic names:

- angry_duck
- sarcastic_goblin
- dramatic_narrator
- tiny_robot
- sleepy_wizard

## Acceptance criteria

The repo is successful when:

1. Rust CLI compiles.
2. Python server starts.
3. `voiceforge say` sends a request to server.
4. Audio file is generated.
5. Rust plays the audio.
6. Cache prevents regeneration.
7. Presets are loaded from JSON.
8. Docs explain how to evolve into XTTS embeddings and fine-tuning.
