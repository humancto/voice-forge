# VoiceForge

<p align="center">
  <strong>Make your terminal talk back. In any voice.</strong>
</p>

<p align="center">
  <a href="https://github.com/humancto/voice-forge/actions"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/humancto/voice-forge/ci.yml?branch=main&label=CI&logo=githubactions&logoColor=white"></a>
  <a href="https://github.com/humancto/voice-forge/blob/main/LICENSE"><img alt="License" src="https://img.shields.io/github/license/humancto/voice-forge?color=blue"></a>
  <a href="https://github.com/humancto/voice-forge/releases"><img alt="Latest release" src="https://img.shields.io/github/v/release/humancto/voice-forge?include_prereleases&sort=semver"></a>
  <a href="https://github.com/humancto/voice-forge/stargazers"><img alt="Stars" src="https://img.shields.io/github/stars/humancto/voice-forge?style=flat&logo=github"></a>
  <img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-orange?logo=rust&logoColor=white">
  <img alt="Built with Python" src="https://img.shields.io/badge/cloning-Python%20%2B%20XTTS-3776AB?logo=python&logoColor=white">
  <img alt="Platforms" src="https://img.shields.io/badge/platforms-macOS%20%7C%20Linux-lightgrey">
</p>

---

VoiceForge is a **local voice runtime for your terminal**. It turns build
failures, test runs, git commits, and AI-agent activity into spoken
reactions — using any voice you point it at.

```text
voiceforge clone ./peter.wav as peter
voiceforge use peter
voiceforge run -- npm test
  # ...20 minutes pass...
  # 🔊  "Holy crap Lois, the build is on fire!"
```

Bring your own voice, a YouTube clip, an mp3 you ripped — VoiceForge
ingests it, embeds it locally with [XTTS v2](https://huggingface.co/coqui/XTTS-v2),
and the next time your build dies, that voice says so.

## Why

LLM coding agents are turning terminals into long-running, conversational
workflows. You start `claude code`, walk away, come back fifteen minutes
later not knowing whether tests passed or the agent got stuck. VoiceForge
gives that workflow a voice — literally.

The product is **local-first by design**. No cloud TTS, no accounts, no
network calls beyond the model download. Your audio stays on your machine.

## Demo

```text
$ voiceforge run -- cargo test
   Compiling voiceforge v0.1.0
    Finished test [unoptimized + debuginfo] target(s) in 12.4s
     Running unittests src/main.rs
test result: FAILED. 3 passed; 1 failed
🔊  "Roads? Where we're going we don't need... oh wait, the test failed."
```

## Quick start

### Prerequisites

```bash
brew install rust ffmpeg yt-dlp
```

### Run the demo

```bash
git clone https://github.com/humancto/voice-forge
cd voice-forge

# 1. Build the CLI
cd apps/voiceforge-cli && cargo build --release && cd ../..

# 2. Start the local TTS server (fallback engine — macOS `say` / Linux `espeak`)
cd services/tts-server
python3 -m venv .venv && source .venv/bin/activate
pip install flask
python server.py &
cd ../..

# 3. Speak something
./apps/voiceforge-cli/target/release/voiceforge \
  say --text "Build failed again." --voice angry_duck

# 4. Wrap a command
./apps/voiceforge-cli/target/release/voiceforge \
  run -- cargo test
```

That's the **fallback** path — works without any model download, no GPU,
no Python ML stack. For real voice cloning, install the heavy stack:

```bash
# Adds ~3 GB of torch + coqui-tts + the XTTS v2 model
cd services/tts-server
pip install coqui-tts
VOICEFORGE_TTS_ENGINE=xtts python server.py
```

Then drop any wav into `services/tts-server/voices/<name>.wav` and the
server uses it as a speaker reference.

## Architecture

```text
                                   ┌──────────────────────────┐
  terminal event                   │  apps/voiceforge-cli     │
  (build, test, git, agent)  ────▶ │     (Rust)               │
                                   │  ─ command wrapping      │
                                   │  ─ rule engine           │
                                   │  ─ audio playback (rodio)│
                                   └──────────┬───────────────┘
                                              │ POST /tts
                                              ▼
                                   ┌──────────────────────────┐
                                   │  services/tts-server     │
                                   │     (Python, Flask)      │
                                   │  ─ XTTS v2 (Coqui)       │
                                   │  ─ macOS say fallback    │
                                   │  ─ sha256 audio cache    │
                                   └──────────┬───────────────┘
                                              │ wav path
                                              ▼
                                       speakers go brrrr
```

## What's shipped

- ✅ Rust CLI with `say`, `run`, `daemon`, `voices`, **`ingest`** subcommands
- ✅ Python TTS server with macOS / Linux fallback engines
- ✅ Audio ingest pipeline — accepts wav/mp3/m4a/ogg/flac/aiff/webm via `ffmpeg`
- ✅ Sha256 audio cache (server-side)
- ✅ Voice presets + event rules
- ✅ End-to-end smoke tests against a real Peter Griffin clip
- 🚧 `voiceforge clone <source> as <name>` — see [ROADMAP](ROADMAP.md) item 2.5
- 🚧 One-curl install (item 1.5)
- 🚧 Real Unix-socket daemon (item 1.8)

The full backlog and per-item status lives in [`ROADMAP.md`](ROADMAP.md).

## Project docs

- [`ROADMAP.md`](ROADMAP.md) — the build plan, one PR per item
- [`docs/PRODUCT_WRITEUP.md`](docs/PRODUCT_WRITEUP.md) — what we're building and why
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — runtime + voice resolution
- [`docs/TECHNICAL_SPEC.md`](docs/TECHNICAL_SPEC.md) — CLI + server API
- [`docs/FINE_TUNING_GUIDE.md`](docs/FINE_TUNING_GUIDE.md) — when embeddings aren't enough
- [`docs/ADOPTION_AND_PRODUCT_DIRECTION.md`](docs/ADOPTION_AND_PRODUCT_DIRECTION.md) — strategic frame

## Test fixtures

The integration tests exercise a real audio clip. To fetch it:

```bash
bash scripts/fetch_fixtures.sh
```

This pulls a 20-second sample with `yt-dlp`, transcodes it via `ffmpeg`,
and drops it into `tests/fixtures/`. Audio binaries are gitignored —
every contributor runs the script once. Tests SKIP cleanly when the
fixture is absent; CI sets `VOICEFORGE_REQUIRE_FIXTURES=1` to keep
that skip path honest.

## A word on voice cloning

VoiceForge ships the cloning pipeline. Whatever WAV you point it at,
that's what it speaks in. **What you clone for personal/local use is
your call.** What we ship in the bundled voice catalog (Phase 2.5 in
the roadmap) stays original synthetic characters — `angry_duck`,
`sarcastic_goblin`, `tiny_robot`, etc. — to keep the distribution
clean.

## Contributing

```bash
# Run the full test suite
cargo test --manifest-path apps/voiceforge-cli/Cargo.toml
pytest services/tts-server/tests/   # once item 0.4 ships

# Format + lint (required for CI)
cargo fmt --manifest-path apps/voiceforge-cli/Cargo.toml
cargo clippy --manifest-path apps/voiceforge-cli/Cargo.toml -- -D warnings
ruff check services/tts-server/
```

PRs are welcome on any unchecked `- [ ]` item in [ROADMAP.md](ROADMAP.md).

## License

MIT — see [`LICENSE`](LICENSE).
