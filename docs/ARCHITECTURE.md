# Architecture

## Runtime

```text
voiceforge CLI
  -> config loader
  -> cache key
  -> local TTS server
  -> audio playback
```

## Voice identity resolution

Voice identity should be resolved in this order:

1. Cached audio exists
2. Saved embedding exists
3. Reference wav exists
4. Default fallback voice

## Why this ordering

Cache gives exact repeatability.

Embedding gives stable voice identity.

Reference wav gives convenient experimentation.

Fallback lets the system work immediately.

## Local paths

```text
services/tts-server/voices/
services/tts-server/embeddings/
services/tts-server/audio_cache/
configs/presets/
configs/rules/
```

## Future daemon design

The daemon should expose a local Unix socket:

```text
~/.voiceforge/voiceforge.sock
```

Other tools can send:

```json
{
  "event": "build_failed",
  "message": "cargo build failed",
  "voice": "angry_duck"
}
```
