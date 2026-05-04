# Plan: voiceforge install-cloning (ROADMAP 2.1)

## Goal

```bash
voiceforge install-cloning
```

After it finishes, the user has the complete GPT-SoVITS v2 cloning
stack ready — venv, models, ffmpeg@6 — and `voiceforge clone` (item
2.5) can call straight into it.

The install steps we lived through manually become one idempotent
script. The exact recipe that produced 100%-Whisper-verified Trump
and Quagmire output is what gets encoded.

## Design decisions (locked from the manual run)

- **Backend:** GPT-SoVITS v2 (not XTTS, not v2Pro, not v4).
- **Reference recipe:** 1 main + 5 aux × 10 s, Whisper-transcribed.
- **Cloning runtime:** isolated venv at `~/.voiceforge/cloning/venv/`
  using `arm64` Python 3.11 from `/opt/homebrew/bin/python3.11`. NOT
  the existing `services/tts-server/.venv` (which is XTTS today;
  keep it for backwards compat).
- **GPT-SoVITS source:** clone of `RVC-Boss/GPT-SoVITS` to
  `~/.voiceforge/cloning/repo/`, pinned to a known-good commit so
  upstream churn doesn't break us.
- **Pretrained models:** `lj1995/GPT-SoVITS` from HuggingFace, only
  the v2 subset (~1.7 GB), not the full ~5 GB grab we did before.
- **macOS-arm64 prereqs:**
  - arm64 Homebrew at `/opt/homebrew/`
  - `/opt/homebrew/bin/python@3.11`
  - `/opt/homebrew/opt/ffmpeg@6/lib` (for torchcodec — ffmpeg 7+
    breaks it)
- **Linux-x86_64:** scoped to follow-up (1.1.3 + 2.1.1) — script
  bails clearly on Linux for now with the expected install command.
- **Marker:** writes `~/.voiceforge/cloning/INSTALLED` with the
  install timestamp, GPT-SoVITS commit, model checkpoint sha so
  `voiceforge doctor` can detect "cloning ready: yes".

## Out of scope

- The actual `voiceforge clone <source> as <name>` subcommand — 2.5.
- The synthesis-via-cloning-engine wiring (Rust `Engine::Cloning`
  variant) — 2.5b / new item.
- Linux + Windows ports — 1.1.3 / 2.1.1.
- Auto-resume on partial install failure — best-effort skip-if-done
  is the v1 contract; force-redo via `--force`.

## Files

### New

- `scripts/install_cloning.sh` — the heavy lifter. Bash, strict mode,
  idempotent, dry-run via env. Steps:
  1. Detect macOS + arm64; bail clearly on anything else.
  2. Verify `/opt/homebrew/bin/brew` exists; print install one-liner
     if missing.
  3. `brew install python@3.11 ffmpeg@6` (skip if both present).
  4. Create venv at `~/.voiceforge/cloning/venv/` if missing.
  5. `pip install` the pinned set we proved works (numpy<2, torch,
     torchaudio, "transformers>=4.43,<=4.50", librosa==0.10.2,
     pytorch-lightning>=2.4, ffmpeg-python, soundfile, matplotlib,
     funasr, g2p_en, openai-whisper, modelscope, pypinyin,
     fast_langdetect, jieba, jieba_fast, einops, x-transformers,
     loguru, fastapi, uvicorn, torchcodec, peft, sentencepiece,
     onnxruntime, tqdm, chardet, PyYAML, psutil, wordsegment,
     split-lang, huggingface_hub).
  6. `git clone` GPT-SoVITS to `~/.voiceforge/cloning/repo/`, pin to
     a specific commit (record it in this plan; CI will catch drift).
  7. Download `lj1995/GPT-SoVITS` v2 subset only:
     - `chinese-hubert-base/`
     - `chinese-roberta-wwm-ext-large/`
     - `gsv-v2final-pretrained/`
       ≈ 1.7 GB, not the 5 GB whole repo.
  8. Run `nltk.download('averaged_perceptron_tagger_eng', 'cmudict')`
     into the venv's nltk_data path.
  9. Smoke test: import `GPT_SoVITS.TTS_infer_pack.TTS` to confirm
     stack actually works.
  10. Write `~/.voiceforge/cloning/INSTALLED` with metadata.

- `apps/voiceforge-cli/src/install_cloning.rs` — Rust subcommand
  module:
  - `pub fn run(force: bool) -> anyhow::Result<()>` — sets env
    (paths, dry-run flag), invokes `scripts/install_cloning.sh` via
    `Command`, streams output to user's terminal in real time.
  - `pub fn is_installed() -> bool` — checks for the marker file.

### Modified

- `apps/voiceforge-cli/src/main.rs`:
  - `mod install_cloning;`
  - `Commands::InstallCloning { #[arg(long)] force: bool }`.
  - Dispatch to `install_cloning::run(force)`.
- `apps/voiceforge-cli/src/doctor.rs`:
  - Add `check_cloning()` — reports
    `Ok` if `INSTALLED` marker present and venv exists,
    `Warn` otherwise (with hint `voiceforge install-cloning`).
- `.github/workflows/ci.yml`:
  - Add a shellcheck pass on the new script.
  - **Don't** run the actual install in CI — it's ~3 GB / 10 minutes
    and YouTube/HF rate-limits will flake. Mark as a manual-trigger
    follow-up (workflow_dispatch).

### Tests

`apps/voiceforge-cli/tests/install_cloning_smoke.rs`:

- Dry-run via `VOICEFORGE_INSTALL_CLONING_DRY_RUN=1` exercises the
  script's platform detection + step ordering without doing real
  installs. Asserts banner + step labels print in expected order.
- `is_installed()` returns false when the marker is absent;
  true when a tempdir is staged with the marker file.

`shellcheck -e SC1091 scripts/install_cloning.sh` — clean.

## Verify (real, not "trust CI")

1. `bash scripts/install_cloning.sh` from a fresh state on this
   exact machine — completes in under 15 minutes, smoke-test prints
   "TTS imports clean", marker written.
2. Re-run the same command — finishes in under 30 seconds (every
   step is skip-if-done; brew/pip/git all idempotent).
3. `voiceforge doctor` shows `[OK] cloning` after install,
   `[WARN] cloning` before.
4. Delete the marker file, re-run with `--force` — every step
   re-executes, ends with new marker.
5. Pre-install a known broken state (delete one model file) — script
   detects + re-downloads only the missing piece.

## Risks

- **Homebrew interactivity.** `brew install python@3.11` is
  non-interactive but `brew install ffmpeg` is huge. ffmpeg@6 is
  smaller (~50 MB) and keg-only — won't conflict with the user's
  existing ffmpeg. Use it.
- **HuggingFace rate-limiting** on the model download. The
  `huggingface-cli` we used has built-in retries and resumable
  downloads via etag-based hashes. Acceptable.
- **GPT-SoVITS upstream commit churn.** Pin to a known-good SHA
  inside the script. Bumping it is a deliberate PR.
- **NLTK data download** has a quirk where it tries the OS-default
  nltk_data dir before the venv-local one. Set `NLTK_DATA` env var
  to force venv-local placement.

## Atomic commits

1. `feat(install): scripts/install_cloning.sh + marker contract`
2. `feat(cli): wire 'voiceforge install-cloning [--force]' subcommand`
3. `feat(doctor): cloning install state check`
4. `test(install-cloning): dry-run smoke + shellcheck wiring`
5. `ci: shellcheck on install_cloning.sh + workflow_dispatch for the heavy install`
