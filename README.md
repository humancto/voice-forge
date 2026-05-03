# VoiceForge Terminal

VoiceForge Terminal is a local-first voice runtime for your terminal.

It lets terminal events speak using consistent synthetic character voices. The product goal is simple:

> Turn terminal output, build failures, Git events, tests, and AI-agent activity into funny, repeatable voice reactions running locally on your machine.

This repo is designed to be given to Codex, Claude Code, Cursor, or another coding agent and evolved into a real product.

## What we are building

A local tool with four layers:

```text
Terminal event
  -> Rust CLI / daemon
  -> rule engine selects voice preset
  -> local TTS server generates speech
  -> cache + audio playback
```

The important part is consistency:

```text
Voice sample -> speaker embedding -> reusable voice identity -> cached generated audio
```

So the same voice can speak new text, and repeated text can play back identically from cache.

## What this is not

This is not a public celebrity or copyrighted character cloning product.

For educational and local experimentation, the technical system supports voice references and embeddings. For any real distribution, use your own voice, consented voices, synthetic voices, licensed voices, or actor-created voice packs.

## Repo structure

```text
voiceforge-terminal/
├── apps/voiceforge-cli/              # Rust CLI and daemon
├── services/tts-server/              # Python local TTS service
├── configs/presets/                  # Voice presets
├── configs/rules/                    # Event to voice mappings
├── datasets/example/                 # Example fine-tuning dataset format
├── scripts/                          # Setup and helper scripts
├── docs/                             # Full product docs
├── agent/                            # Codex / Claude instructions
└── .github/workflows/                # CI starter
```

## Quick start

### 1. Start the TTS server

```bash
cd services/tts-server
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
python server.py
```

By default this starts with a lightweight fallback engine using macOS `say` or Linux `espeak`. Replace it with XTTS when ready.

### 2. Run the Rust CLI

```bash
cd apps/voiceforge-cli
cargo run -- say --text "Build failed again" --voice angry_duck
```

### 3. Run as a daemon

```bash
cargo run -- daemon
```

### 4. Wrap a command

```bash
cargo run -- run -- npm test
```

## Voice consistency strategy

There are three levels:

| Goal | Technique |
|---|---|
| Same voice identity | saved speaker embedding |
| Same delivery style | fixed generation parameters |
| Same exact audio | cache by hash of text + voice + params |

The current repo implements the structure for all three.

## Roadmap

1. Working local CLI with fallback TTS
2. XTTS drop-in server
3. Speaker embedding extraction
4. Hash-based cache
5. Preset/rule system
6. Terminal command wrappers
7. Background daemon
8. Streaming playback
9. Fine-tuning pipeline
10. Packaged macOS local app / Homebrew install
