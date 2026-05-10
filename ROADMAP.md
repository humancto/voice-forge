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
- [x] **1.2 First-run bootstrap: create `~/.voiceforge/{presets,cache,voices,embeddings,logs,config.toml}`, copy in built-in presets.**
- [x] **1.3 Fix preset path: `$VOICEFORGE_HOME` → `~/.voiceforge/presets` → repo-relative fallback. Kills the `../../configs/presets` bug in `config.rs`.**
- [x] **1.4 Wire `configs/rules/events.json` into `runner.rs` so the random-line picker actually reads it.**
- [x] **1.5 GitHub release automation: tagged builds for darwin-arm64/x86_64 + linux-x86_64/aarch64.**
- [x] **1.6 `install.sh` at repo root + served from `voiceforge.sh`: detects OS+arch, fetches binary from latest release, drops in `/usr/local/bin` or `~/.local/bin`, runs `voiceforge doctor`.** _(Builds from source until 1.5 ships releases; URL contract stays the same when binary fetch lands. `voiceforge doctor` smoke deferred to 1.7.)_
- [x] **1.7 `voiceforge doctor`: OS, audio backend, TTS engine, cache size, server reachability. JSON mode behind `--json`.**
- [x] **1.8 Real Unix-socket daemon at `~/.voiceforge/voiceforge.sock` accepting `{event, message, voice}`. Replaces the heartbeat placeholder.**
- [x] **1.9 `voiceforge send <event> [--message ...] [--voice ...]` client for the daemon.**

## Phase 2 — Clone from anything

- [x] **2.1 `voiceforge install-cloning`: sets up Python venv + GPT-SoVITS v2, smoke-tests, writes a marker. Until run, clone commands fail closed with a clear message.** _(Backend switched from XTTS to GPT-SoVITS v2 after A/B comparison; multi-aux-ref pattern proved to deliver 100% Whisper-verified output.)_
- [x] **2.2 Audio ingest pipeline: accept wav, mp3, m4a, ogg, flac, aiff, webm. Transcode to 22050Hz mono 16-bit pcm via `ffmpeg`. Validate 10–60s duration. (Silence rejection deferred to 2.2.1.)**
- [x] **2.2.1 Reject silent input + apply loudnorm (`-af loudnorm=I=-16:TP=-1.5:LRA=11`) to ingested audio. Cheap follow-up to 2.2; gated separately so the base pipeline can ship first.**
- [x] **2.3 URL ingest: any URL `yt-dlp` can resolve (YouTube, Vimeo, Twitter/X, TikTok, direct media). Pipes into the audio pipeline.** _(Hardened invocation: `--no-playlist --max-filesize 250M --socket-timeout 30 --retries 3`. Schemeless hostnames intentionally not auto-detected — error teaches the user to add `https://`.)_
- [ ] **2.4 `voiceforge record <name>`: cpal mic capture, 20–30s, live waveform meter + countdown, writes to `~/.voiceforge/voices/<name>.wav`.**
- [x] **2.5 `voiceforge clone <name> <source>`: source = local file path. Runs the proven multi-aux-ref pipeline (ffmpeg trim+loudnorm → 6×10s split → Whisper transcribe), saves a voice profile under `~/.voiceforge/voices/<name>/`. URL/yt-dlp ingestion deferred — bring your own local file.** _(Engine::Cloning facade also shipped: `voiceforge say --voice <name>` routes through GPT-SoVITS v2 with the long-lived NDJSON synth child.)_
- [x] **2.6 `voiceforge use <name>`: sets active default voice in `~/.voiceforge/config.toml`.**
- [x] **2.7 `voiceforge voices`: list + show source, duration, embedding path, last-used. `voiceforge voices remove <name>` deletes the lot.**

## Phase 3 — Plug into the dev workflow

- [x] **3.1 `voiceforge shell-init` for zsh + bash: `preexec`/`precmd` hooks fire daemon events for commands over a configurable threshold.**
- [x] **3.2 `voiceforge install git-hooks`: `post-commit`, `post-merge`, `post-rewrite`, `pre-push` send daemon events. Idempotent, with uninstaller.** _(Honors `core.hooksPath`; chases worktree `.git`-file via `git rev-parse --git-common-dir`; detects + warns on husky/lefthook/pre-commit framework collisions; pre-push has terminal `exit 0` + consumes stdin so a daemon-down case can never refuse a push.)_
- [x] **3.3 `voiceforge hook`: reads JSON Lines from stdin, forwards to daemon. Lets Claude Code / Codex / Cursor pipe their hook events in. Distinct from `voiceforge ingest` (audio transcoder) — different surface, different concept.**
- [x] **3.4 `voiceforge watch <path>`: speaks on filesystem changes via `notify`.**
- [x] **3.5 macOS notification bridge: `osascript` notification mirrors every spoken line.**

## Phase 4 — Smarter reactions

- [x] **4.1 `ReactionProvider` trait with `Static` (rules.json) + `Llm` (OpenAI-compatible endpoint via `VOICEFORGE_LLM_URL`). Static is default, LLM falls back to static on failure.**
- [ ] **4.2 Streaming TTS: `POST /tts/stream` chunked over WebSocket, CLI plays as it arrives via rodio.**
- [ ] **4.3 Multi-voice personality presets: a preset can declare a cast, LLM returns `(voice, line)` tuples played in sequence.**

## Phase 5 — Distribution

- [ ] **5.1 `voiceforge.sh` static site on Cloudflare Pages: serves install.sh + three demo asciicasts with synced audio.**
- [x] **5.2 Homebrew tap as second install path.** _(Tap repo: https://github.com/humancto/homebrew-voiceforge. `Formula/voiceforge.rb` ships pinned to the latest engine release with real per-arch sha256 sidecars. Auto-bumped via `.github/workflows/bump-tap.yml` on each release. macOS arm64 + Intel; binary ships unsigned today — quarantine xattr workaround documented in formula `caveats`. Notarization queued as 5.2.1.)_
- [ ] **5.3 `voiceforge share`: emits an asciinema cast with synced WAV track, one-shot upload.**

## Phase 6 — Pre-rendered voice packs

> **GATED ON QUALITY MILESTONE.** Packs only ship after we've produced ≥1 voice
> sample that's instantly recognizable to a casual fan ("yeah, that's Peter").
> Bad packs are worse than no packs.

- [x] **6.0 Quality milestone — fish-speech S2 Pro produces recognizable Peter Griffin output where GPT-SoVITS v2 hits a ceiling. Pipeline documented in `docs/PACK_RENDERING.md`. Confirmed by ear on the Nike-commercial reference clip.**
- [x] **6.1 Pack format spec v1: `manifest.toml` (schema_version, voice_source, source_clip_url, reference_prompt_text, tier, per-event phrase text), `phrases.json` (rendering input), `wav/<event>.wav` (mono, 44.1 kHz from S2 Pro), `reference.wav` (source clip), `checksums.txt` (sha256). Shipped with the initial Peter pack.**
- [x] **6.2 `voiceforge pack {list,install,remove,info}` subcommand. Pulls index from `humancto/voice-forge-packs` over HTTPS, sha256-verifies, extracts to `~/.voiceforge/packs/<name>/`. Index already live (see 6.3).**
- [x] **6.3 Static GitHub-hosted pack index. `humancto/voice-forge-packs` repo created with `packs.json` schema_version 1 + LICENSE-AUDIO.md (educational use, attribution required, 48-hour takedown policy) + initial Peter pack (13/13 phrases). https://github.com/humancto/voice-forge-packs**
- [x] **6.4 Initial pack release v1: 5 voices × 13 events shipped to `voice-forge-packs`. Live: peter, kimmel, neil_tyson, trump, musk (all in `shipping` status per packs.json). Queued: stewie, bob_ross, herzog, ramsay, obama (phrase manifests authored, awaiting clean reference clips). Each tagged with `tier` (`character` / `public-figure` / `experimental`) in manifest.toml.**
- [x] **6.5 `voiceforge play --pack <name> --event <id>` for direct pack playback (sub-100ms, no synth). Separate subcommand from `say` because pack lookup bypasses the TTS engine entirely. Caller (e.g. ROADMAP 3.3 `voiceforge hook`) handles fallback on exit-3 — keeps `play` cold-start-free. Shipped in PR #17.**
- [x] **6.6 Pack-content style guide (`docs/PACK_CONTENT_GUIDE.md`): per-tier rules for phrase content. `character` packs lean into in-character idioms; `public-figure` packs use clearly-synthetic dev-feedback framing (no fake political statements, no fake endorsements, no anything that could read as "this person actually said this"). Required reading for any community pack PR.**
- [x] **6.7 Tarball release automation: GitHub Action in `voice-forge-packs` that builds `<voice>.tar.gz` per pack on tag push, computes sha256, attaches to release. Consumed by `voiceforge pack install <name>` (6.2). Triggered by `<pack>-v<semver>` tag (e.g. `peter-v0.2.0`). Validates manifest.toml schema, regenerates checksums.txt, builds flat tarball, updates packs.json, commits the bump back to main, cuts the release. Hardened against workflow injection (every `run:` step uses `env:` for tag-derived values).**
