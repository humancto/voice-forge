# Technical Specification

## Components

### voiceforge-cli

Rust binary responsible for:

- command parsing
- daemon mode
- command wrapping
- event classification
- cache key generation
- HTTP requests to TTS server
- audio playback

### tts-server

Python service responsible for:

- loading TTS model
- resolving voice presets
- loading embeddings
- generating audio
- returning file paths

## Cache

Cache key:

```text
sha256(text + voice_id + temperature + speed + model_version)
```

Cache path:

```text
audio_cache/<cache_key>.wav
```

## Determinism

The system should never promise that live TTS generation is bit-identical.

It should promise:

```text
same text + same preset + same cache = identical playback
```

## CLI interface

```bash
voiceforge say --text "Build failed" --voice angry_duck
voiceforge run -- npm test
voiceforge daemon
voiceforge voices
```

## Server API

### POST /tts

Request:

```json
{
  "text": "Build failed",
  "voice": "angry_duck"
}
```

Response:

```json
{
  "audio_path": "audio_cache/abc123.wav",
  "cache_hit": false
}
```

### GET /voices

Returns available presets.

### POST /embed

Future endpoint to create an embedding from a voice reference wav.

## Future streaming API

### POST /tts/stream

Returns chunked audio or streams over WebSocket.

Defer until the basic cache-first flow works.
