# VoiceForge Roadmap

```text
curl install -> voiceforge clone <anything> as <name> -> hear it react to your terminal
```

You point at any audio source — local wav, mp3, m4a, a YouTube URL, your mic — and that voice now lives on your machine and reacts to terminal events. Built on what we have: Rust CLI (clap, tokio, cpal, rodio), Flask + XTTS server, sha256 cache.

Each unchecked item is one PR with tests. Expert agent (auto-detected from stack) reviews before merge.

---

## Phase 0 — Bootstrap

- [ ] **0.1 Git init, push to GitHub, branch protection on `main`.**
- [x] **0.2 CI matrix on macOS + Linux: `cargo fmt`, `clippy -D warnings`, `cargo test`, `pytest`, `ruff`, `shellcheck`.** _(Rust + shellcheck shipped; pytest + ruff deferred to ROADMAP 0.4.)_
- [ ] **0.3 Rust integration test harness with stub TTS server + injectable audio sink behind a trait. Introduces `VOICEFORGE_TTS_URL`.**
- [ ] **0.4 Python test harness: pytest + Flask test client over `/health`, `/voices`, `/tts` cache hit/miss, empty-text rejection.**

## Phase 1 — Curl install, zero Python required

- [x] **1.1 Embedded fallback TTS in Rust: `say` (macOS), `espeak-ng` (Linux), SAPI (Windows). `voiceforge say` works without the Python server.** _(Windows SAPI deferred to ROADMAP 1.1.2.)_
- [ ] **1.2 First-run bootstrap: create `~/.voiceforge/{presets,cache,voices,embeddings,logs,config.toml}`, copy in built-in presets.**
- [x] **1.3 Fix preset path: `$VOICEFORGE_HOME` → `~/.voiceforge/presets` → repo-relative fallback. Kills the `../../configs/presets` bug in `config.rs`.**
- [x] **1.4 Wire `configs/rules/events.json` into `runner.rs` so the random-line picker actually reads it.**
- [ ] **1.5 GitHub release automation: tagged builds for darwin-arm64/x86_64 + linux-x86_64/aarch64.**
- [ ] **1.6 `install.sh` at repo root + served from `voiceforge.sh`: detects OS+arch, fetches binary from latest release, drops in `/usr/local/bin` or `~/.local/bin`, runs `voiceforge doctor`.**
- [ ] **1.7 `voiceforge doctor`: OS, audio backend, TTS engine, cache size, server reachability. JSON mode behind `--json`.**
- [ ] **1.8 Real Unix-socket daemon at `~/.voiceforge/voiceforge.sock` accepting `{event, message, voice}`. Replaces the heartbeat placeholder.**
- [ ] **1.9 `voiceforge send <event> [--message ...] [--voice ...]` client for the daemon.**

## Phase 2 — Clone from anything

- [ ] **2.1 `voiceforge install-cloning`: sets up Python venv + XTTS, smoke-tests, writes a marker. Until run, clone commands fail closed with a clear message.**
- [x] **2.2 Audio ingest pipeline: accept wav, mp3, m4a, ogg, flac, aiff, webm. Transcode to 22050Hz mono 16-bit pcm via `ffmpeg`. Validate 10–60s duration. (Silence rejection deferred to 2.2.1.)**
- [ ] **2.2.1 Reject silent input + apply loudnorm (`-af loudnorm=I=-16:TP=-1.5:LRA=11`) to ingested audio. Cheap follow-up to 2.2; gated separately so the base pipeline can ship first.**
- [ ] **2.3 URL ingest: any URL `yt-dlp` can resolve (YouTube, Vimeo, Twitter/X, TikTok, direct media). Pipes into the audio pipeline.**
- [ ] **2.4 `voiceforge record <name>`: cpal mic capture, 20–30s, live waveform meter + countdown, writes to `~/.voiceforge/voices/<name>.wav`.**
- [ ] **2.5 `voiceforge clone <source> as <name>`: source = local path or URL. Runs the ingest pipeline, calls Python `/embed`, writes preset JSON, registers the voice. One command, anything in.**
- [ ] **2.6 `voiceforge use <name>`: sets active default voice in `~/.voiceforge/config.toml`.**
- [ ] **2.7 `voiceforge voices`: list + show source, duration, embedding path, last-used. `voiceforge voices remove <name>` deletes the lot.**

## Phase 3 — Plug into the dev workflow

- [ ] **3.1 `voiceforge shell-init` for zsh + bash: `preexec`/`precmd` hooks fire daemon events for commands over a configurable threshold.**
- [ ] **3.2 `voiceforge install git-hooks`: `post-commit`, `post-merge`, `post-rewrite`, `pre-push` send daemon events. Idempotent, with uninstaller.**
- [ ] **3.3 `voiceforge ingest`: reads JSON Lines from stdin, forwards to daemon. Lets Claude Code / Codex / Cursor pipe their hook events in.**
- [ ] **3.4 `voiceforge watch <path>`: speaks on filesystem changes via `notify`.**
- [ ] **3.5 macOS notification bridge: `osascript` notification mirrors every spoken line.**

## Phase 4 — Smarter reactions

- [ ] **4.1 `ReactionProvider` trait with `Static` (rules.json) + `Llm` (OpenAI-compatible endpoint via `VOICEFORGE_LLM_URL`). Static is default, LLM falls back to static on failure.**
- [ ] **4.2 Streaming TTS: `POST /tts/stream` chunked over WebSocket, CLI plays as it arrives via rodio.**
- [ ] **4.3 Multi-voice personality presets: a preset can declare a cast, LLM returns `(voice, line)` tuples played in sequence.**

## Phase 5 — Distribution

- [ ] **5.1 `voiceforge.sh` static site on Cloudflare Pages: serves install.sh + three demo asciicasts with synced audio.**
- [ ] **5.2 Homebrew tap as second install path.**
- [ ] **5.3 `voiceforge share`: emits an asciinema cast with synced WAV track, one-shot upload.**
