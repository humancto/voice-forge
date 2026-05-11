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
voiceforge pack install peter
voiceforge run -- npm test
  # ...20 minutes pass...
  # 🔊  "Holy crap Lois, the build is on fire!"
```

Bring **any clean ≥60-second audio file** of the voice you want (or a YouTube URL — yt-dlp resolves it) — a Family Guy clip, a podcast segment, a recording of yourself. VoiceForge runs the proven multi-aux-ref recipe (1 main + 5 aux × 10 s, Whisper-transcribed) through **GPT-SoVITS v2** locally and the next time your build dies, that voice says so. **Or skip the cloning entirely** — `voiceforge pack install <name>` ships 5 pre-rendered celebrity voice packs (Peter Griffin, Trump, Musk, Kimmel, Neil deGrasse Tyson) that play in ~50 ms with no model load.

## What you can do with voiceforge today

Six real flows, all live on `main`. Each one ships, has tests, and works on `voiceforge --version` 0.2.0+.

### 1. Hear celebrity voices react to your terminal — zero model install

```bash
brew tap humancto/voiceforge && brew install voiceforge          # or: curl install.sh | bash
voiceforge daemon &; disown
voiceforge pack install trump
voiceforge say --voice trump --text "build_failed"               # plays pre-rendered WAV in ~50 ms
voiceforge run --voice trump -- npm test                          # speaks on success/failure
```

5 packs × 13 events = 65 ready-to-play reactions. No Python, no GPU, no 1.7 GB download. The pack tarballs are 7-10 MB each and the binary is ~3 MB.

### 2. Wire it into Claude Code so your AI agent talks back

Edit `~/.claude/settings.json`:

```json
{
  "hooks": {
    "Notification": [{ "command": "voiceforge hook --profile claude-code" }],
    "Stop": [{ "command": "voiceforge hook --profile claude-code" }]
  }
}
```

Now every Claude Code Notification / Stop event fires through the daemon. Walk away from a 20-minute deploy task; come back to Peter Griffin yelling that it succeeded — or the build burning down.

### 3. Auto-react to long terminal commands (zsh / bash)

```bash
voiceforge shell-init --install zsh         # idempotent; one-time
# new shell — anything taking > 3 s now fires command_succeeded / command_failed
npm test                                     # 12 s, exits 0 → "Done."
cargo build --release                        # 2 min, exits 1 → "The build failed again."
```

Catches everything you (or your AI agent) runs in the shell — `npm`, `cargo`, `pytest`, `pulumi up`, `terraform apply`. Skip-list defaults to silent on `cd`, `ls`, `pwd`, `clear`, `history`, `voiceforge`. Configurable.

### 4. Audible git workflow

```bash
cd ~/projects/myapp
voiceforge install git-hooks                 # writes 4 hooks, idempotent, honors core.hooksPath
git commit -m "feat: x"                      # 🔊 "Commit saved."
git push                                     # 🔊 "Pushed. Now everyone knows."
git rebase -i HEAD~3                         # 🔊 "History has been rewritten."
```

Husky / lefthook / pre-commit users get a stderr warning (we never silently clobber). Worktrees and submodules work via `git rev-parse --git-common-dir`.

### 5. Clone any voice from any audio source — including YouTube URLs

```bash
voiceforge install-cloning                   # one-time, ~10 min, ~1.7 GB; macOS arm64 today
voiceforge clone peter https://www.youtube.com/watch?v=T2w5SQ0L65I    # yt-dlp under the hood
voiceforge say --voice peter --text "the staging deploy is on fire"   # ~2-3 s synth on CPU
```

URL ingest is hardened: `--no-playlist`, `--max-filesize 250M`, `--socket-timeout 30`, `--retries 3`. Schemeless `youtube.com/...` is intentionally NOT auto-detected — the error teaches you to add `https://`. EBU R128 loudnorm is applied during ingest so quiet recordings get bumped to broadcast level before the model ever sees them. Silent inputs are rejected loudly.

### 6. Pipe NDJSON events from any tool into the daemon

```bash
tail -f ~/.my-agent/events.log | voiceforge hook --event-from event_type
echo '{"hook":{"event_name":"deploy_failed"}}' | voiceforge hook --event-from hook.event_name
my-script --stream | voiceforge hook --voice peter --passthrough | jq
```

Generic streaming forwarder. Backpressure-safe (per-frame fail threshold), passthrough preserves stdout pipelines, exit codes distinguish "daemon down" (2) from "daemon rejected the frame" (1) from "post-connect garbage" (4).

---

### 7b. Use an LLM to write reactions in any voice's character (opt-in)

```bash
export VOICEFORGE_LLM_URL=https://api.openai.com/v1/chat/completions
export OPENAI_API_KEY=sk-...
voiceforge daemon &
# Now `build_failed` events get a fresh in-character line every time,
# not a random pull from the static rules.json.
```

| Env var                     | Default              | Purpose                                           |
| --------------------------- | -------------------- | ------------------------------------------------- |
| `VOICEFORGE_LLM_URL`        | unset (static rules) | OpenAI-compatible chat-completions endpoint       |
| `VOICEFORGE_LLM_API_KEY`    | unset                | Bearer token. Falls back to `OPENAI_API_KEY`      |
| `VOICEFORGE_LLM_MODEL`      | unset (server pick)  | Model name (e.g. `gpt-4o-mini`)                   |
| `VOICEFORGE_LLM_TIMEOUT_MS` | `2000`               | Per-request hard cap; bump for local Ollama       |
| `VOICEFORGE_LLM_STRICT`     | unset (silent fall)  | `1` to surface LLM errors instead of falling back |

Any LLM failure (network, timeout, bad JSON, unknown voice, HTTP 4xx/5xx) silently falls through to the static rules so the daemon never goes silent. After 5 failures within 60s the LLM is skipped for 5 minutes (circuit breaker), then probed again. `voiceforge doctor` reports the active provider, endpoint reachability, and timeout. Works with OpenAI, local Ollama (`http://localhost:11434/v1/chat/completions`), or any chat-completions-shaped endpoint.

---

### 7c. Multi-voice casts: a Family Guy argument over your build failure

```bash
# Configure a cast for an event:
cat > ~/.voiceforge/casts.toml <<'EOF'
[casts.build_failed]
voices = ["peter", "brian"]
max_turns = 3
EOF

# Casts only fire through the LLM provider:
export VOICEFORGE_LLM_URL=https://api.openai.com/v1/chat/completions
export OPENAI_API_KEY=sk-...
voiceforge daemon &

# Now build_failed plays a 2-3 turn exchange:
#   peter: oh no the compiler broke again
#   brian: this is what tests are for, peter
#   peter: tests are for cowards
```

Played sequentially through a single-consumer playback queue — turns never overlap and concurrent events from multiple sources are also serialized (fixes a latent single-voice race in the process). Without `VOICEFORGE_LLM_URL` the daemon warns at startup and falls back to the single-voice rules.json path. `voiceforge doctor` previews the configured casts so you can sanity-check `casts.toml` without launching the daemon.

**Wire-compat note:** the daemon's `spoken` reply field is now `String | Array<{voice, line}>` — single-voice replies keep the legacy string shape; cast replies use the array shape. Existing 3.3 hooks check `ok` only and are unaffected.

---

### 7. Mirror every spoken line as a macOS notification

```bash
VOICEFORGE_MIRROR_NOTIFICATIONS=1 voiceforge daemon &
voiceforge say --voice peter "build failed"
# → audio plays AND a Notification Center banner appears with the line.
```

Opt-in visual mirror so you don't miss reactions when AirPods are off, audio is muted, or you're heads-down on another desktop. macOS only (uses `osascript`). First run triggers a one-time permission prompt for "Script Editor" — if you say no, mirroring silently no-ops. `voiceforge doctor` reports current state.

---

### Two quality tiers, by use case

| You want…                                                             | Use        |
| --------------------------------------------------------------------- | ---------- |
| ~50 ms playback of a fixed event (build_failed, deploy_done, …)       | **Pack**   |
| Arbitrary text spoken in any voice you have an audio sample of        | **Cloned** |
| Zero install beyond the binary; want to demo voiceforge in 30 seconds | **Pack**   |
| Custom phrases for your team's specific events                        | **Cloned** |
| Linux/Intel-Mac (cloning runtime is macOS-arm64 today)                | **Pack**   |

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

### Voice packs (community-contributed)

Install with one command. Plays in ~50ms at runtime. Pulls from [voice-forge-packs](https://github.com/humancto/voice-forge-packs).

```bash
voiceforge pack install peter
voiceforge play --pack peter --event tests_passed
```

<table>
  <tr>
    <td align="center" width="120">🍔<br/><sub><b>Peter Griffin</b></sub><br/><sub>✅ shipping</sub></td>
    <td align="center" width="120"><img src="docs/assets/voices/kimmel.jpg" width="80" alt="Jimmy Kimmel"/><br/><sub><b>Jimmy Kimmel</b></sub><br/><sub>✅ shipping</sub></td>
    <td align="center" width="120"><img src="docs/assets/voices/neil_tyson.jpg" width="80" alt="Neil deGrasse Tyson"/><br/><sub><b>Neil deGrasse Tyson</b></sub><br/><sub>✅ shipping</sub></td>
    <td align="center" width="120"><img src="docs/assets/voices/trump.jpg" width="80" alt="Donald Trump"/><br/><sub><b>Donald Trump</b></sub><br/><sub>✅ shipping</sub></td>
  </tr>
  <tr>
    <td align="center" width="120"><img src="docs/assets/voices/musk.jpg" width="80" alt="Elon Musk"/><br/><sub><b>Elon Musk</b></sub><br/><sub>✅ shipping</sub></td>
    <td align="center" width="120">👶<br/><sub><b>Stewie Griffin</b></sub><br/><sub>queued</sub></td>
    <td align="center" width="120"><img src="docs/assets/voices/bob_ross.jpg" width="80" alt="Bob Ross"/><br/><sub><b>Bob Ross</b></sub><br/><sub>queued</sub></td>
    <td align="center" width="120"><img src="docs/assets/voices/herzog.jpg" width="80" alt="Werner Herzog"/><br/><sub><b>Werner Herzog</b></sub><br/><sub>queued</sub></td>
  </tr>
  <tr>
    <td align="center" width="120"><img src="docs/assets/voices/ramsay.png" width="80" alt="Gordon Ramsay"/><br/><sub><b>Gordon Ramsay</b></sub><br/><sub>queued</sub></td>
    <td align="center" width="120"><img src="docs/assets/voices/obama.jpg" width="80" alt="Barack Obama"/><br/><sub><b>Barack Obama</b></sub><br/><sub>queued</sub></td>
    <td align="center" width="120">🤖<br/><sub><b>Bender</b></sub><br/><sub>queued</sub></td>
    <td align="center" width="120">🎨<br/><sub><b>your voice</b></sub><br/><sub><a href="docs/PACK_RENDERING.md">render</a></sub></td>
  </tr>
</table>

> **🔊 Hear them react to a build failure**: [Peter](https://github.com/humancto/voice-forge-packs/raw/main/packs/peter/wav/build_failed.wav) · [Kimmel](https://github.com/humancto/voice-forge-packs/raw/main/packs/kimmel/wav/build_failed.wav) · [Neil deGrasse Tyson](https://github.com/humancto/voice-forge-packs/raw/main/packs/neil_tyson/wav/build_failed.wav) · [Trump](https://github.com/humancto/voice-forge-packs/raw/main/packs/trump/wav/build_failed.wav) · [Musk](https://github.com/humancto/voice-forge-packs/raw/main/packs/musk/wav/build_failed.wav)

> Photos for real public figures from Wikimedia Commons / Wikipedia (small thumbnails, fair use; see [`docs/assets/voices/ATTRIBUTION.md`](docs/assets/voices/ATTRIBUTION.md)). Cartoon characters use emoji placeholders — we don't ship FOX/Disney cartoon art. Want to contribute a pack? See the [pack content style guide](docs/PACK_CONTENT_GUIDE.md).

### Plug into any AI coding agent

Full guide: **[`docs/AGENTS.md`](docs/AGENTS.md)** — Claude Code, Cursor, Continue, Aider, generic streaming sources, plus the four moving pieces (daemon / send / hook / shell-init) and how to pick which one for which agent.

The headline setup, by surface area:

```bash
# Always-on prerequisite: the daemon (one-time)
voiceforge daemon &
disown

# Claude Code — uses its hook system + voiceforge hook --profile
# In ~/.claude/settings.json:
#   { "hooks": { "Notification": [{ "command": "voiceforge hook --profile claude-code" }] } }

# Cursor / Continue / Aider — anything that runs shell on events
voiceforge send agent_done --message "deploy finished"

# Generic streaming source (logfile, MCP server, custom integration)
tail -f agent.log | voiceforge hook --event-from event_type

# Catch-all for any terminal command the agent runs (>3s)
voiceforge shell-init --install zsh   # idempotent; one-time

# Pre-rendered pack (sub-100ms playback, no synth)
voiceforge play --pack peter --event tests_passed
```

Each surface ships today (ROADMAP 1.8 / 1.9 / 3.3 / 3.1 / 6.5). The daemon does voice routing automatically — pre-rendered packs play in ~50ms, live cloning in ~2-3s, embedded fallback in <500ms.

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

📖 **Project site:** https://humancto.github.io/voice-forge/ — install steps, capability tour, architecture diagram. Also serves the install script: `curl -fsSL https://humancto.github.io/voice-forge/install.sh | bash` works alongside the canonical `raw.githubusercontent.com` URL.

## Quick start

**Step 1 — install the binary.** Two paths, same binary:

```bash
# macOS via Homebrew (auto-tracks new releases via brew upgrade):
brew tap humancto/voiceforge
brew install voiceforge

# Or universal installer (curl | bash, ~5 seconds, all 4 platforms):
curl -fsSL https://raw.githubusercontent.com/humancto/voice-forge/main/install.sh | bash

voiceforge say --text "VoiceForge is ready"
```

The installer fetches a prebuilt binary for `darwin-arm64`, `darwin-x86_64`, `linux-x86_64`, or `linux-aarch64` (glibc ≥ 2.35) from the [latest release](https://github.com/humancto/voice-forge/releases/latest), sha256-verifies, and installs to `/usr/local/bin/voiceforge` (or `~/.local/bin` if `/usr/local` isn't writable). Falls through to a from-source build only on unsupported platforms or download failure.

That alone gets you `voiceforge run -- <cmd>` reactions in the OS default voice (`say` on macOS, `espeak-ng` on Linux), plus the entire pack ecosystem (`voiceforge pack install peter && voiceforge play --pack peter --event build_failed`). For voice cloning from your own audio, continue:

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

Two paths:

**URL** — paste a YouTube/Vimeo/Twitter/podcast/etc. URL directly:

```bash
voiceforge clone peter https://www.youtube.com/watch?v=T2w5SQ0L65I
voiceforge ingest https://www.youtube.com/watch?v=T2w5SQ0L65I /tmp/peter_60s.wav
```

VoiceForge shells out to `yt-dlp` (install via `brew install yt-dlp` or `pipx install yt-dlp`) into a tempdir, then feeds the WAV through the cloning pipeline. Hardened: `--no-playlist`, `--max-filesize 250M`, `--socket-timeout 30`, `--retries 3`. Schemeless hostnames (`youtube.com/...`) are deliberately not auto-detected — prefix with `https://`.

**Local file** — download with whatever tool you like, then:

```bash
voiceforge clone peter ./peter_griffin_60s.wav
```

The recipe wants:

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
# Pin a specific prebuilt release (default: latest):
VOICEFORGE_VERSION=v0.2.0 bash install.sh

# Force from-source build (e.g. for an unsupported platform):
VOICEFORGE_FORCE_SOURCE=1 VOICEFORGE_REF=main bash install.sh

# Override the install dir:
VOICEFORGE_INSTALL_DIR=$HOME/.local/bin bash install.sh
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

## What's new since v0.2.0 (currently on `main`)

The v0.2.0 release shipped the binary distribution + the daemon. Since then, a stack of user-visible features have landed and are live on `main` ahead of the next tagged release:

| PR  | What                                                                                                                                                                                                        | Roadmap |
| --- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------- |
| #29 | `voiceforge say --voice <pack>` resolves to pre-rendered phrases (3-tier match: event_id → exact phrase → fuzzy contains). `voiceforge voices` lists installed packs in their own section.                  | UX      |
| #28 | `voiceforge clone <URL>` and `voiceforge ingest <URL>` accept any URL `yt-dlp` resolves. Hardened: `--no-playlist --max-filesize 250M --socket-timeout 30 --retries 3`.                                     | 2.3     |
| #27 | `docs/AGENTS.md` — full Claude Code / Cursor / generic-stream integration guide. New "agents" section on the docs site with copy-paste config.                                                              | docs    |
| #26 | Pack tables refreshed (5 packs marked shipping); embedded `<audio>` players on the docs site for instant in-browser preview.                                                                                | docs    |
| #25 | **install.sh `awk -F'/'` bug fix** — was silently sending 100% of installs through the slow from-source path. Now actual prebuilts work.                                                                    | P0 fix  |
| #24 | `voiceforge hook` — NDJSON forwarder with `--profile claude-code`, exit codes 0/1/2/4, env-tunable thresholds.                                                                                              | 3.3     |
| #23 | `voiceforge shell-init` — zsh/bash hook installer with sentinel-bounded blocks.                                                                                                                             | 3.1     |
| #22 | `voiceforge send <event>` — single-frame daemon client with retry-loop bind-race protection.                                                                                                                | 1.9     |
| #21 | Unix-socket NDJSON daemon at `~/.voiceforge/voiceforge.sock`.                                                                                                                                               | 1.8     |
| #30 | `voiceforge install git-hooks` — per-repo git hooks installer (post-commit, post-merge, post-rewrite, pre-push). Honors `core.hooksPath`; chases worktree `.git`-file via `git rev-parse --git-common-dir`. | 3.2     |
| #31 | Ingest now applies EBU R128 loudnorm (I=-16 LUFS) + rejects silent input. Quiet recordings no longer produce quiet clones.                                                                                  | 2.2.1   |

Total: **220 tests green**, clippy + fmt clean. The next tagged release will roll all of these.

## What's shipped

CLI subcommands (run any with `--help`):

|                                               |                                                                                                               |
| --------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| `voiceforge install-cloning`                  | One-shot install of Python 3.11 + ffmpeg@6 + GPT-SoVITS v2 (`--check`, `--force`, `--uninstall`; macOS arm64) |
| `voiceforge clone <name> <source>`            | Clone a voice from a local audio file ≥ 60 s                                                                  |
| `voiceforge say --voice <name> --text "..."`  | One-shot synthesis through embedded / server / cloning engines (auto-routed)                                  |
| `voiceforge run -- <cmd>`                     | Run a command, react on success/failure with a random line from `events.json`                                 |
| `voiceforge ingest <input> <output>`          | Transcode any audio source to canonical 22050 Hz mono 16-bit PCM                                              |
| `voiceforge doctor`                           | 10-check system health (incl. daemon socket probe), JSON via `--json`                                         |
| `voiceforge voices`                           | List built-in presets + cloned voices, `voices remove <name>` deletes                                         |
| `voiceforge use <name>`                       | Set active default voice in `~/.voiceforge/config.toml`                                                       |
| `voiceforge daemon`                           | Unix-socket NDJSON server at `~/.voiceforge/voiceforge.sock` (ROADMAP 1.8)                                    |
| `voiceforge send <event> [--text ...]`        | One-shot daemon client; exits 0/1/2/4 on outcome (ROADMAP 1.9)                                                |
| `voiceforge hook [--profile claude-code]`     | Pipe NDJSON events from AI agents into the daemon (ROADMAP 3.3)                                               |
| `voiceforge shell-init <zsh\|bash> --install` | Idempotent shell hook install for command-success/fail events (ROADMAP 3.1)                                   |
| `voiceforge play --pack <name> --event <id>`  | Sub-100ms playback of pre-rendered pack WAVs (ROADMAP 6.5)                                                    |
| `voiceforge pack {list,install,remove,info}`  | Manage installed voice packs from the static index (ROADMAP 6.2)                                              |

Other shipped infrastructure:

- ✅ Three-engine facade (`Embedded` / `Server` / `Cloning`) with per-call dispatch by voice name
- ✅ Long-lived NDJSON synth child — model loads once per process, warm for the rest
- ✅ Atomic-rename cache at `~/.voiceforge/cache/<sha>.wav`
- ✅ Path-traversal-proof voice profile loader (canonicalize + reserved-name list)
- ✅ CI: rustfmt, clippy `-D warnings`, doc-link check, integration tests on macOS + Linux
- ✅ Verified end-to-end: 100% Whisper round-trip on real Trump speech via the cloning pipeline

The full backlog and per-item status lives in [`ROADMAP.md`](ROADMAP.md). Currently **25 items shipped**, headline ones: 1.1 (embedded fallback), 1.2 (first-run bootstrap), 1.5 (cross-platform binary releases), 1.6 (install.sh), 1.7 (doctor), **1.8 (Unix-socket daemon)**, 1.9 (`voiceforge send`), 2.1 (install-cloning), **2.5 (clone)**, **3.1 (shell-init for zsh+bash)**, **3.3 (`voiceforge hook` for AI agents)**, 6.2 (`voiceforge pack` subcommand), 6.4 (5-pack initial release).

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
