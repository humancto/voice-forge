# VoiceForge

<p align="center">
  <strong>Your terminal, in any voice you clone.</strong><br/>
  <em>Build fails? Peter Griffin yells at you. Tests pass? Trump says they're tremendous. Local-first. No cloud. No accounts.</em>
</p>

<p align="center">
  <a href="https://humancto.github.io/voice-forge/"><strong>website</strong></a> ·
  <a href="#quick-start">quick start</a> ·
  <a href="#how-it-works">how it works</a> ·
  <a href="ROADMAP.md">roadmap</a> ·
  <a href="https://github.com/humancto/voice-forge/issues">issues</a>
</p>

<p align="center">
  <a href="https://github.com/humancto/voice-forge/actions"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/humancto/voice-forge/ci.yml?branch=main&label=CI&logo=githubactions&logoColor=white"></a>
  <a href="https://github.com/humancto/voice-forge/blob/main/LICENSE"><img alt="License" src="https://img.shields.io/github/license/humancto/voice-forge?color=blue"></a>
  <a href="https://github.com/humancto/voice-forge/stargazers"><img alt="Stars" src="https://img.shields.io/github/stars/humancto/voice-forge?style=flat&logo=github"></a>
  <img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-orange?logo=rust&logoColor=white">
  <img alt="Cloning backend" src="https://img.shields.io/badge/cloning-GPT--SoVITS%20v2-3776AB?logo=python&logoColor=white">
  <img alt="Platforms" src="https://img.shields.io/badge/platforms-macOS%20%7C%20Linux-lightgrey">
</p>

---

VoiceForge is a **local voice runtime for your terminal.** It turns build failures, test runs, git commits, and AI-agent activity into spoken reactions — using any voice you point it at.

```text
voiceforge install-cloning
voiceforge clone peter ./peter_griffin_60s.wav
voiceforge run -- npm test
  # ...20 minutes pass...
  # 🔊  "Holy crap Lois, the build is on fire!"
```

Bring **any clean ≥60-second audio file** of the voice you want — a Family Guy clip you transcoded, a podcast segment, a recording of yourself. VoiceForge runs the proven multi-aux-ref recipe (1 main + 5 aux × 10 s, Whisper-transcribed) through **GPT-SoVITS v2** locally and the next time your build dies, that voice says so.

### Two quality tiers

|                    | **Live cloning** (default) | **Pre-rendered packs**                                       |
| ------------------ | -------------------------- | ------------------------------------------------------------ |
| Backend            | GPT-SoVITS v2              | fish-speech S2 Pro                                           |
| Speaks             | arbitrary text             | curated phrases (build_failed, tests_passed, …)              |
| Latency            | ~2 sec / phrase            | **~50 ms** (just plays a WAV)                                |
| Quality on cartoon | OK                         | **genuinely recognizable** (Peter Griffin, Stewie, Quagmire) |
| Render cost        | per-phrase at runtime      | one-time, ~10 min/phrase on a Mac CPU                        |
| GPU needed?        | no                         | no — CPU works, GPU is faster                                |

**For arbitrary text you write yourself, use live cloning.** For terminal feedback (a fixed set of events) where you want best-in-class character voice quality, **render a pack once, ship the WAVs**. Anyone can render their own packs locally — see [`docs/PACK_RENDERING.md`](docs/PACK_RENDERING.md).

### Plug into any AI coding agent

VoiceForge is designed to be invoked from agent hooks (Claude Code, Cursor, Codex, Continue, Aider, etc.). Any agent that can run a shell command on a tool-use event can speak through VoiceForge:

```bash
# Claude Code post-tool-use hook (example)
voiceforge say --voice peter --text "Tests passed."

# Or, with a pre-rendered pack — sub-100ms playback:
voiceforge say --pack peter --event tests_passed       # roadmap 6.5

# Or pipe structured JSON events from any source:
echo '{"event":"tests_passed"}' | voiceforge ingest    # roadmap 3.3
```

When your agent is doing 20 minutes of background work and finally finishes a deploy, you hear Peter announce it from the kitchen. That's the whole pitch.

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

**Step 1 — install the binary** (no Python required, ~2 min on a clean Rust cache):

```bash
curl -fsSL https://raw.githubusercontent.com/humancto/voice-forge/main/install.sh | bash
voiceforge say --text "VoiceForge is ready"
```

That alone gets you `voiceforge run -- <cmd>` reactions in the OS default voice (`say` on macOS, `espeak-ng` on Linux). For real voice cloning, continue:

**Step 2 — install the cloning runtime** (~10 min, ~1.7 GB; macOS arm64 only for now):

```bash
voiceforge install-cloning
```

This installs Python 3.11 (arm64), `ffmpeg@6`, the GPT-SoVITS v2 weights, and a venv at `~/.voiceforge/cloning/`. Idempotent — re-runs are seconds. `voiceforge install-cloning --check` verifies the install. `--force` rebuilds, `--uninstall` removes it.

**Step 3 — clone a voice from any local audio file** (≥60 s of clean single-speaker audio):

```bash
voiceforge clone peter ./peter_griffin_60s.wav
```

**Step 4 — speak in that voice:**

```bash
voiceforge say --voice peter --text "Holy crap, the build is on fire."
voiceforge run --voice peter -- npm test    # speaks on success/failure
```

That's the full flow. Each `voiceforge run` invocation pays one ~15 s model-load cold-start; subsequent reactions in the same process are warm (~3 s synth on CPU).

### Sourcing audio

Bring your own. We don't bundle yt-dlp — download with whatever tool you like, then point `voiceforge clone` at the local file. The recipe wants:

- ≥ 60 seconds duration
- Single speaker, no music / sound effects / other voices
- Decent broadcast or podcast-quality audio

We also pre-tested the recipe on a real C-SPAN Trump speech and got **100% Whisper-verified output** on full-sentence reactions. Cleaner the source, closer the clone. Stylized cartoon voices (Peter Griffin, Stewie) hit ~70% timbre fidelity zero-shot — fine-tuning on roadmap.

### Wary of `curl | bash`?

```bash
curl -fsSL https://raw.githubusercontent.com/humancto/voice-forge/main/install.sh -o install.sh
less install.sh
bash install.sh
```

Override defaults:

```bash
VOICEFORGE_REF=v0.2.0 \
VOICEFORGE_INSTALL_DIR=$HOME/.local/bin \
bash install.sh
```

### Build from source manually

```bash
git clone https://github.com/humancto/voice-forge
cd voice-forge
cargo build --release --manifest-path apps/voiceforge-cli/Cargo.toml
./apps/voiceforge-cli/target/release/voiceforge say --text "Built from source"
```

## How it works

Two layers, all local:

```text
         terminal event                           voiceforge process
         (build, test, git, agent)
                │
                ▼
       ┌─────────────────────┐         ┌──────────────────────────┐
       │ apps/voiceforge-cli │         │ scripts/cloning_synth.py │
       │ (Rust, async tokio) │  ──►    │ (Python, GPT-SoVITS v2)  │
       │                     │  NDJSON │                          │
       │ rule engine         │  stdin  │ lazy load                │
       │ engine facade       │  stdout │ 1 main + 5 aux refs      │
       │ ~/.voiceforge/cache │         │ atomic write             │
       └─────────────────────┘         └──────────────────────────┘
                │                                │
                │  embedded fallback             │  cached .wav under
                │  (macOS say / espeak-ng)       │  ~/.voiceforge/cache/
                ▼                                ▼
       ┌─────────────────────────────────────────────────────────┐
       │                   rodio (CoreAudio / ALSA)              │
       └─────────────────────────────────────────────────────────┘
                │
                ▼
                🔊  speakers go brrrr
```

## What's shipped

CLI subcommands (run any with `--help`):

|                                              |                                                                                                  |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| `voiceforge install-cloning`                 | One-shot install of Python 3.11 + ffmpeg@6 + GPT-SoVITS v2 (`--check`, `--force`, `--uninstall`) |
| `voiceforge clone <name> <source>`           | Clone a voice from a local audio file ≥ 60 s                                                     |
| `voiceforge say --voice <name> --text "..."` | One-shot synthesis through embedded / server / cloning engines (auto-routed)                     |
| `voiceforge run -- <cmd>`                    | Run a command, react on success/failure with a random line from `events.json`                    |
| `voiceforge ingest <input> <output>`         | Transcode any audio source to canonical 22050 Hz mono 16-bit PCM                                 |
| `voiceforge doctor`                          | 9-check system health, JSON via `--json`                                                         |
| `voiceforge voices`                          | List built-in presets                                                                            |
| `voiceforge daemon`                          | (heartbeat placeholder; real Unix-socket daemon is roadmap 1.8)                                  |

Other shipped infrastructure:

- ✅ Three-engine facade (`Embedded` / `Server` / `Cloning`) with per-call dispatch by voice name
- ✅ Long-lived NDJSON synth child — model loads once per process, warm for the rest
- ✅ Atomic-rename cache at `~/.voiceforge/cache/<sha>.wav`
- ✅ Path-traversal-proof voice profile loader (canonicalize + reserved-name list)
- ✅ CI: rustfmt, clippy `-D warnings`, doc-link check, integration tests on macOS + Linux
- ✅ Verified end-to-end: 100% Whisper round-trip on real Trump speech via the cloning pipeline

The full backlog and per-item status lives in [`ROADMAP.md`](ROADMAP.md). Currently 12 items shipped, headline ones: 1.1 (embedded fallback), 1.2 (first-run bootstrap), 1.6 (install.sh), 1.7 (doctor), 2.1 (install-cloning), **2.5 (clone)**.

## Project docs

- [Website](https://humancto.github.io/voice-forge/) — install, clone, use, pipeline overview
- [`ROADMAP.md`](ROADMAP.md) — the build plan, one PR per item
- [`docs/MAC_INSTALL.md`](docs/MAC_INSTALL.md) — what works on which Mac (arm64 vs Intel, OS versions, gotchas)
- [`docs/FINE_TUNING_GUIDE.md`](docs/FINE_TUNING_GUIDE.md) — the path to ~99% on stylized character voices

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

VoiceForge ships the cloning pipeline. Whatever WAV you point it at, that's what it speaks in. **What you clone for personal/local use is your call.** What we ship in the bundled voice catalog stays original synthetic characters — `angry_duck`, `sarcastic_goblin`, `tiny_robot`, `hype_narrator`, `default` — to keep the distribution clean.

### Quality expectations (honest)

- **Real human voices** (Trump, Obama, your colleague) → ~95% timbre fidelity zero-shot. Sounds like them in a quiet scene.
- **Stylized cartoon/character voices** (Peter Griffin, Stewie, Quagmire) → ~70% zero-shot. Recognizable but not Seth-MacFarlane-grade. Open-source zero-shot has a model-imposed ceiling for hyper-stylized timbres.
- **Fine-tuning** (20–30 min of clean dialogue + transcripts, 30–60 min training on a free Colab GPU) is the path to ~99% on character voices. On the roadmap, not shipped.

### Why GPT-SoVITS v2 (vs XTTS, F5-TTS, OpenVoice, Tortoise)?

We A/B-tested them all. GPT-SoVITS v2 with the multi-aux-ref recipe (1 main + 5 aux × 10 s, Whisper-transcribed) was the clear winner: faster than Tortoise, sharper than XTTS, more stable than F5-TTS for English, ~250 MB models vs Tortoise's gigabytes. v2Pro and v4 didn't earn their extra cost on short reaction lines.

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
