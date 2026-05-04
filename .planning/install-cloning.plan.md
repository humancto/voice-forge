# Plan: voiceforge install-cloning (ROADMAP 2.1) — REVISED post rust-expert review

## Goal

```bash
voiceforge install-cloning            # idempotent install of GPT-SoVITS v2 stack
voiceforge install-cloning --check    # verify install state, no mutations
voiceforge install-cloning --force    # rebuild venv + re-import-test, keep HF cache
voiceforge install-cloning --uninstall  # remove venv + repo + marker (preserves cache)
```

After it finishes, the cloning stack is ready for `voiceforge clone`
(2.5).

## Locked-in design

- **Backend:** GPT-SoVITS **v2** (selectable later via `--model
v2|v2Pro|v4` on `clone`).
- **Reference recipe:** 1 main + 5 aux × 10 s, Whisper-transcribed.
- **Cloning runtime:** isolated venv at `~/.voiceforge/cloning/venv/`
  via `arm64` Python 3.11 from `/opt/homebrew/bin/python3.11`.
- **GPT-SoVITS source:** clone of `RVC-Boss/GPT-SoVITS` to
  `~/.voiceforge/cloning/repo/`, **pinned to a SHA recorded in the
  script (single source of truth).** Post-clone we hard-assert every
  required path exists.
- **Pretrained models:** `lj1995/GPT-SoVITS` from HuggingFace, only
  the v2 subset via `huggingface-cli download --include` patterns
  (~1.7 GB). **sha256-verified** against pinned hashes for the
  three load-bearing files (`s2G2333k.pth`, `s1bert25hz...ckpt`,
  `chinese-hubert-base/pytorch_model.bin`).
- **macOS-arm64 only** for v1; Linux ports filed as 2.1.1.

## Out of scope (deferred items)

- `voiceforge clone` subcommand — 2.5.
- Synthesis-via-cloning Rust engine variant — 2.5b.
- Linux + Windows ports — 2.1.1.
- Fine-tuning per voice — 2.5.2.

## Files

### New

- **`scripts/install_cloning.sh`** — bash with strict mode. Steps in this exact order:
  1. Strict mode + `trap ERR` with line number; `LC_ALL=C`, `unset CDPATH`. Honor `VOICEFORGE_INSTALL_CLONING_DRY_RUN=1` for CI.
  2. **Disk-space precheck**: ≥4 GB free at `$HOME` (`df -k`); bail clearly if not.
  3. **Platform check**: `[[ "$(uname -s)" == "Darwin" && "$(uname -m)" == "arm64" ]]`. Bail with a clear "Linux/Windows is item 2.1.1" message otherwise.
  4. **arm64 brew check**: `[[ -x /opt/homebrew/bin/brew ]]`. If missing, print the exact `/bin/bash -c "$(curl -fsSL .../install.sh)"` install one-liner and exit.
  5. **`brew install ffmpeg@6` first** (smaller, keg-only — fail fast). Then `brew install python@3.11`. Skip-if-installed via `brew list --versions <name>`.
  6. **arm64 Python verification**: `/opt/homebrew/bin/python3.11 -c 'import platform,sys; sys.exit(0 if platform.machine()=="arm64" else 1)'`. On fail, print `arch -arm64 /opt/homebrew/bin/brew reinstall python@3.11` and exit.
  7. **Create venv** at `~/.voiceforge/cloning/venv/` if missing. Skip if `bin/python` already there.
  8. **Pip install** the proven set with `huggingface_hub==0.27.*` pinned (other versions break `--include`). Wrap each pip install in a 3-attempt retry. Set `MAKEFLAGS=-j$(sysctl -n hw.ncpu)`. Wheels-only via `--prefer-binary`.
  9. **Clone GPT-SoVITS** (shallow `--depth 1` then fetch the pinned SHA, then `git checkout <SHA>`). The SHA is a const at the top of the script — single source of truth. Hard-assert these paths exist post-checkout: `GPT_SoVITS/configs/tts_infer.yaml`, `GPT_SoVITS/TTS_infer_pack/TTS.py`, `GPT_SoVITS/AR/models/t2s_lightning_module.py`. Fail with the SHA + missing path on mismatch.
  10. **HF download** v2 subset: `huggingface-cli download lj1995/GPT-SoVITS --include 'gsv-v2final-pretrained/*' --include 'chinese-hubert-base/*' --include 'chinese-roberta-wwm-ext-large/*' --local-dir <repo>/GPT_SoVITS/pretrained_models --local-dir-use-symlinks=False`. Resumable on retry.
  11. **sha256-verify** load-bearing files against pinned hashes:
      - `s2G2333k.pth`
      - `s1bert25hz-5kh-longer-epoch=12-step=369668.ckpt`
      - `chinese-hubert-base/pytorch_model.bin`
        Hashes recorded as constants at top of script. Fail loudly with both expected and actual hash on mismatch.
  12. **NLTK data** to venv-local path: `NLTK_DATA="<venv>/share/nltk_data" python -m nltk.downloader -d "$NLTK_DATA" averaged_perceptron_tagger_eng cmudict`.
  13. **Smoke test**: `python -c "import sys; sys.path.insert(0,'<repo>'); sys.path.insert(0,'<repo>/GPT_SoVITS'); from GPT_SoVITS.TTS_infer_pack.TTS import TTS, TTS_Config; print('OK')"`. Fail with the import error if it doesn't print OK.
  14. **Write `~/.voiceforge/cloning/INSTALLED.toml`** (TOML, parseable):
      ```toml
      schema_version = 1
      version = "0.1.0"
      installed_at = "2026-05-04T..."
      gpt_sovits_sha = "<sha>"
      python_path = "/opt/homebrew/bin/python3.11"
      ffmpeg6_prefix = "/opt/homebrew/opt/ffmpeg@6"
      [model_sha256]
      s2G2333k = "..."
      s1bert25hz = "..."
      chinese_hubert_base = "..."
      ```
  15. Also write `~/.voiceforge/cloning/install.log` (tee-mirrored stdout) for `voiceforge doctor` to scrape on failure. Use `exec > >(tee -a "$LOG") 2>&1`.

- **`apps/voiceforge-cli/src/install_cloning.rs`** — Rust subcommand:
  - `pub fn run(force: bool, check: bool, uninstall: bool) -> Result<()>` — dispatches to script. Sets the env vars the script honors. Streams output via `Stdio::inherit()` for live progress (TTY-aware tools — brew/pip/hf-cli need real stdout).
  - `pub fn is_installed() -> bool` — reads `INSTALLED.toml`, validates `schema_version`.
  - `pub fn read_install_state() -> Result<InstallState>` — full TOML parse for doctor.
  - `pub struct InstallState { schema_version, gpt_sovits_sha, python_path, ffmpeg6_prefix, model_sha256: HashMap<String, String> }`.

### Modified

- **`apps/voiceforge-cli/src/main.rs`**:
  - `mod install_cloning;`
  - Three flags on the subcommand: `--force`, `--check`, `--uninstall` (mutually exclusive — clap `conflicts_with` group).
- **`apps/voiceforge-cli/src/doctor.rs`**:
  - New `check_cloning()` returning `Ok` (marker present + smoke import works) / `Warn` (marker absent — hint `voiceforge install-cloning`) / `Error` (marker exists but smoke import fails — corrupted, hint `--force`). Reads `install.log` last 5 lines on Error for the report detail.
- **`Cargo.toml`** — add `toml = "0.8"` (we're now actually parsing config).
- **`.github/workflows/ci.yml`**:
  - shellcheck the new script.
  - **Dry-run job**: stage stub binaries for `brew`/`pip`/`git`/`huggingface-cli`/`python3.11` on `$PATH` that just echo+exit 0; run script with `VOICEFORGE_INSTALL_CLONING_DRY_RUN=1`; assert step labels, banner, marker emission. Exercises platform detection + step ordering + TOML write — no real install, no 3 GB download.
  - **`workflow_dispatch`** for the heavy real install (manually triggered).

### Tests

- **`apps/voiceforge-cli/tests/install_cloning_smoke.rs`** integration test — uses the same stub-PATH trick as CI to dry-run the script. Asserts:
  - banner + step labels print in order
  - TOML marker emitted with all required fields
  - `voiceforge install-cloning --help` exits 0 without touching `~/.voiceforge/`
- **`#[cfg(test)] mod tests`** in `install_cloning.rs`:
  - `is_installed_false_when_marker_absent`
  - `is_installed_true_when_marker_present_and_schema_matches`
  - `is_installed_false_when_schema_version_mismatch`
  - `read_install_state_parses_real_toml`

## Verify

1. **Real install on this machine** — completes in < 15 min on first run, < 30 s on re-run. `voiceforge doctor` shows `[OK] cloning`.
2. **Force re-run** — re-creates venv + re-runs smoke import, preserves HF cache.
3. **Check mode** — runs only smoke import + marker parse, no mutations.
4. **Uninstall mode** — removes venv + repo + marker; HF cache preserved (re-running install is fast on cache hit).
5. **Tampered model** — `truncate -s -1024 <pth>`, run `--check` — reports Error with sha mismatch.
6. **Dry-run smoke (CI path)** — passes with stub PATH.

## `--force` semantics (documented)

- Deletes: `INSTALLED.toml`, `~/.voiceforge/cloning/venv/`.
- Preserves: HF model cache (re-download is the slow part), GPT-SoVITS clone (only re-fetches if SHA pinned ahead).
- Effect: re-creates venv + reruns pip install + reruns smoke + rewrites marker.

## Risks

- **Brew interactivity**: ffmpeg@6 + python@3.11 are non-interactive. License prompts shouldn't fire on these.
- **HF rate-limiting**: `huggingface-cli` retries built-in. Resumable.
- **GPT-SoVITS SHA churn**: pinned, asserted post-checkout.
- **NLTK data path**: forced to venv-local via `NLTK_DATA` env var.
- **`Cli::parse()` short-circuit on `--help`**: confirmed; `voiceforge install-cloning --help` does NOT mutate state. Tested.

## Atomic commits

1. `feat(install-cloning): scripts/install_cloning.sh + TOML marker`
2. `feat(cli): wire 'voiceforge install-cloning [--force|--check|--uninstall]'`
3. `feat(doctor): cloning install state check (parses INSTALLED.toml)`
4. `test(install-cloning): dry-run smoke + Rust unit tests on TOML marker`
5. `ci: shellcheck + dry-run job + workflow_dispatch for the real install`
