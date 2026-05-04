#!/usr/bin/env bash
# Install the GPT-SoVITS v2 cloning stack at ~/.voiceforge/cloning/.
#
# Idempotent: re-runs are fast (skip-if-installed). --force rebuilds the
# venv. --check verifies state without mutating. --uninstall removes the
# venv + repo + marker (preserves HuggingFace model cache).
#
# Honors:
#   VOICEFORGE_INSTALL_CLONING_DRY_RUN=1   print every action, do not execute
#   VOICEFORGE_INSTALL_CLONING_MODE        normal | check | uninstall   (default: normal)
#   VOICEFORGE_INSTALL_CLONING_FORCE=1     wipe venv + marker before installing

set -Eeuo pipefail
IFS=$'\n\t'
export LC_ALL=C
unset CDPATH

# -- pinned values (single source of truth) ---------------------------

readonly GPT_SOVITS_REPO="https://github.com/RVC-Boss/GPT-SoVITS.git"
readonly GPT_SOVITS_SHA="08d627c3338173c3229286d8787060d6559fe0f8"
readonly HF_MODEL_REPO="lj1995/GPT-SoVITS"
readonly HF_HUB_VERSION="0.27.*"
readonly TRANSFORMERS_RANGE=">=4.43,<=4.50"
readonly LIBROSA_VERSION="0.10.2"
readonly NUMPY_RANGE="<2"
readonly PYTHON_VERSION="3.11"

readonly SHA256_S2G="924fdccaa3c574bf139c25c9759aa1ed3b3f99e19a7c529ee996c2bc17663695"
readonly SHA256_S1BERT="732f94e63b148066e24c7f9d2637f3374083e637635f07fbdb695dee20ddbe1f"
readonly SHA256_HUBERT="24164f129c66499d1346e2aa55f183250c223161ec2770c0da3d3b08cf432d3c"

readonly REQUIRED_REPO_PATHS=(
  "GPT_SoVITS/configs/tts_infer.yaml"
  "GPT_SoVITS/TTS_infer_pack/TTS.py"
  "GPT_SoVITS/AR/models/t2s_lightning_module.py"
)

readonly MARKER_SCHEMA_VERSION=1

# -- paths ------------------------------------------------------------

VOICEFORGE_HOME="${VOICEFORGE_HOME:-$HOME/.voiceforge}"
readonly VOICEFORGE_HOME
readonly CLONING_ROOT="$VOICEFORGE_HOME/cloning"
readonly VENV_DIR="$CLONING_ROOT/venv"
readonly REPO_DIR="$CLONING_ROOT/repo"
readonly MARKER_FILE="$CLONING_ROOT/INSTALLED.toml"
readonly LOG_FILE="$CLONING_ROOT/install.log"

readonly BREW_BIN="/opt/homebrew/bin/brew"
readonly ARM64_PYTHON="/opt/homebrew/bin/python${PYTHON_VERSION}"
readonly FFMPEG6_PREFIX="/opt/homebrew/opt/ffmpeg@6"

DRY_RUN="${VOICEFORGE_INSTALL_CLONING_DRY_RUN:-0}"
MODE="${VOICEFORGE_INSTALL_CLONING_MODE:-normal}"
FORCE="${VOICEFORGE_INSTALL_CLONING_FORCE:-0}"

# -- io helpers -------------------------------------------------------

step() { printf '\n==> %s\n' "$*"; }
say() { printf '    %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

run() {
  if [[ "$DRY_RUN" == "1" ]]; then
    printf '[dry-run] %s\n' "$*"
  else
    "$@"
  fi
}

trap 'die "install_cloning.sh failed at line $LINENO. See $LOG_FILE."' ERR

retry() {
  local attempts="$1"; shift
  local sleep_secs="$1"; shift
  local n=1
  until "$@"; do
    if (( n >= attempts )); then return 1; fi
    warn "command failed (attempt $n/$attempts), retrying in ${sleep_secs}s: $*"
    sleep "$sleep_secs"
    n=$(( n + 1 ))
  done
}

# -- early exits ------------------------------------------------------

mkdir -p "$CLONING_ROOT"
if [[ "$DRY_RUN" != "1" ]]; then
  exec > >(tee -a "$LOG_FILE") 2>&1
fi

step "voiceforge install-cloning  (mode=$MODE force=$FORCE dry_run=$DRY_RUN)"

# -- mode dispatch ----------------------------------------------------

case "$MODE" in
  uninstall)
    step "uninstall: removing venv + repo + marker (HF model cache preserved)"
    run rm -rf "$VENV_DIR" "$REPO_DIR" "$MARKER_FILE"
    say "removed $VENV_DIR"
    say "removed $REPO_DIR"
    say "removed $MARKER_FILE"
    say "preserved: ~/.cache/huggingface/  (re-install will reuse the model download)"
    exit 0
    ;;
  check)
    step "check: verifying install state without mutations"
    [[ -f "$MARKER_FILE" ]] || die "marker missing: $MARKER_FILE — run 'voiceforge install-cloning'"
    [[ -x "$VENV_DIR/bin/python" ]] || die "venv missing: $VENV_DIR — run 'voiceforge install-cloning --force'"
    [[ -d "$REPO_DIR/.git" ]] || die "repo missing: $REPO_DIR — run 'voiceforge install-cloning --force'"
    step "smoke import"
    "$VENV_DIR/bin/python" -c "
import sys
sys.path.insert(0, '$REPO_DIR'); sys.path.insert(0, '$REPO_DIR/GPT_SoVITS')
from GPT_SoVITS.TTS_infer_pack.TTS import TTS, TTS_Config
print('OK')
" >/dev/null || die "smoke import failed — re-install with --force"
    say "all checks passed"
    exit 0
    ;;
  normal)
    : # fall through to install
    ;;
  *)
    die "unknown VOICEFORGE_INSTALL_CLONING_MODE: $MODE"
    ;;
esac

# -- 1. disk-space precheck ------------------------------------------

step "disk-space precheck (need >=4 GB free at $HOME)"
free_kb="$(df -k "$HOME" | awk 'NR==2 {print $4}')"
free_gb=$(( free_kb / 1024 / 1024 ))
say "free: ${free_gb} GB"
(( free_gb >= 4 )) || die "need >=4 GB free at $HOME, have ${free_gb} GB"

# -- 2. platform check -----------------------------------------------

step "platform check"
if [[ "$(uname -s)" != "Darwin" ]] || [[ "$(uname -m)" != "arm64" ]]; then
  die "macOS arm64 only for now (Linux/Windows is ROADMAP 2.1.1).
You are on: $(uname -s)/$(uname -m)"
fi
say "macOS arm64 confirmed"

# -- 3. arm64 brew check ---------------------------------------------

step "arm64 Homebrew check"
if [[ ! -x "$BREW_BIN" ]]; then
  die "arm64 Homebrew not found at $BREW_BIN.
Install with:
  /bin/bash -c \"\$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\""
fi
say "found $BREW_BIN"

# -- 4. brew install ffmpeg@6 + python@3.11 (in that order) ----------

if ! "$BREW_BIN" list --versions ffmpeg@6 >/dev/null 2>&1; then
  step "installing ffmpeg@6 (keg-only, ~50 MB) — required for torchcodec"
  run retry 3 5 "$BREW_BIN" install ffmpeg@6
else
  say "ffmpeg@6 already installed"
fi

if ! "$BREW_BIN" list --versions "python@${PYTHON_VERSION}" >/dev/null 2>&1; then
  step "installing python@${PYTHON_VERSION}"
  run retry 3 5 "$BREW_BIN" install "python@${PYTHON_VERSION}"
else
  say "python@${PYTHON_VERSION} already installed"
fi

# -- 5. arm64 python verification ------------------------------------

step "verifying $ARM64_PYTHON is arm64"
if [[ "$DRY_RUN" != "1" ]]; then
  if ! "$ARM64_PYTHON" -c 'import platform,sys; sys.exit(0 if platform.machine()=="arm64" else 1)'; then
    die "$ARM64_PYTHON is not arm64. Reinstall with:
  arch -arm64 $BREW_BIN reinstall python@${PYTHON_VERSION}"
  fi
fi
say "arm64 python confirmed"

# -- 6. force-mode wipe ----------------------------------------------

if [[ "$FORCE" == "1" ]]; then
  step "force: removing venv + marker (HF cache preserved)"
  run rm -rf "$VENV_DIR" "$MARKER_FILE"
fi

# -- 7. venv ----------------------------------------------------------

if [[ ! -x "$VENV_DIR/bin/python" ]]; then
  step "creating venv at $VENV_DIR"
  run "$ARM64_PYTHON" -m venv "$VENV_DIR"
else
  say "venv already exists"
fi

PIP="$VENV_DIR/bin/pip"
PYTHON="$VENV_DIR/bin/python"

# -- 8. pip install ---------------------------------------------------

step "upgrading pip + wheel"
run retry 3 5 "$PIP" install --quiet --upgrade pip wheel setuptools

step "installing GPT-SoVITS English-only dep set (~10 min on first run)"
# Two phases: heavy bins first (torch, etc.), then the rest. If torch
# fails the whole install fails fast.
run retry 3 10 "$PIP" install --quiet --prefer-binary \
  "numpy${NUMPY_RANGE}" \
  torch torchaudio torchcodec \
  "transformers${TRANSFORMERS_RANGE}" \
  "huggingface_hub==${HF_HUB_VERSION}" \
  "librosa==${LIBROSA_VERSION}" \
  "pytorch-lightning>=2.4"

run retry 3 10 "$PIP" install --quiet --prefer-binary \
  ffmpeg-python soundfile matplotlib tensorboard onnxruntime tqdm \
  funasr cn2an pypinyin g2p_en modelscope sentencepiece \
  peft chardet PyYAML psutil jieba jieba_fast \
  fast_langdetect wordsegment split-lang \
  einops einx rotary-embedding-torch \
  x-transformers loguru fastapi uvicorn \
  openai-whisper

# -- 9. clone GPT-SoVITS at pinned SHA -------------------------------

if [[ ! -d "$REPO_DIR/.git" ]]; then
  step "cloning GPT-SoVITS @ $GPT_SOVITS_SHA"
  run retry 3 5 git clone --quiet "$GPT_SOVITS_REPO" "$REPO_DIR"
fi
step "checking out pinned SHA $GPT_SOVITS_SHA"
run git -C "$REPO_DIR" fetch --quiet origin "$GPT_SOVITS_SHA"
# reset --hard (not checkout) so a previously-corrupted worktree
# from a partial install can never block the pinned SHA from landing.
run git -C "$REPO_DIR" reset --quiet --hard "$GPT_SOVITS_SHA"

step "asserting required repo paths exist"
for p in "${REQUIRED_REPO_PATHS[@]}"; do
  if [[ "$DRY_RUN" != "1" ]] && [[ ! -e "$REPO_DIR/$p" ]]; then
    die "expected path missing after checkout of $GPT_SOVITS_SHA: $p
The pin may need bumping; verify upstream still has this layout."
  fi
  say "ok: $p"
done

# -- 10. download HF model subset (~1.7 GB) --------------------------

PRETRAIN_DIR="$REPO_DIR/GPT_SoVITS/pretrained_models"
mkdir -p "$PRETRAIN_DIR"
step "downloading $HF_MODEL_REPO (v2 subset + sv, ~1.8 GB) — resumable"
# huggingface-cli 0.27.x silently honors only one --include glob; loop one
# pattern per call to dodge that. sv/ added for ERes2NetV2 dep introduced at
# pinned SHA. --local-dir-use-symlinks dropped (warns "Ignoring" since 0.27).
for pat in 'gsv-v2final-pretrained/*' 'chinese-hubert-base/*' 'chinese-roberta-wwm-ext-large/*' 'sv/*'; do
  run retry 3 10 "$VENV_DIR/bin/huggingface-cli" download "$HF_MODEL_REPO" \
    --include "$pat" \
    --local-dir "$PRETRAIN_DIR"
done

# -- 11. sha256-verify load-bearing files ----------------------------

step "sha256-verifying load-bearing model files"
verify_sha() {
  local file="$1" expected="$2"
  if [[ "$DRY_RUN" == "1" ]]; then
    printf '[dry-run] verify_sha %s == %s\n' "$file" "$expected"
    return 0
  fi
  [[ -f "$file" ]] || die "expected model file missing: $file"
  local actual
  actual="$(shasum -a 256 "$file" | awk '{print $1}')"
  if [[ "$actual" != "$expected" ]]; then
    die "sha256 mismatch on $file
  expected: $expected
  actual:   $actual
This usually means a partial download. Re-run install_cloning.sh."
  fi
  say "ok: $(basename "$file") ($expected)"
}

verify_sha "$PRETRAIN_DIR/gsv-v2final-pretrained/s2G2333k.pth" "$SHA256_S2G"
verify_sha "$PRETRAIN_DIR/gsv-v2final-pretrained/s1bert25hz-5kh-longer-epoch=12-step=369668.ckpt" "$SHA256_S1BERT"
verify_sha "$PRETRAIN_DIR/chinese-hubert-base/pytorch_model.bin" "$SHA256_HUBERT"

# -- 12. nltk data ----------------------------------------------------

NLTK_DATA_DIR="$VENV_DIR/share/nltk_data"
mkdir -p "$NLTK_DATA_DIR"
step "downloading NLTK data into venv-local $NLTK_DATA_DIR"
run env NLTK_DATA="$NLTK_DATA_DIR" \
  "$PYTHON" -m nltk.downloader -d "$NLTK_DATA_DIR" \
  averaged_perceptron_tagger_eng cmudict

# -- 13. smoke test ---------------------------------------------------

step "smoke test: importing GPT-SoVITS TTS class"
# GPT_SoVITS/sv.py uses os.getcwd()-relative paths to find ERes2NetV2 + the
# sv ckpt; the clone runtime cd's into REPO_DIR, so this smoke test must too.
if [[ "$DRY_RUN" != "1" ]]; then
  ( cd "$REPO_DIR" && DYLD_FALLBACK_LIBRARY_PATH="$FFMPEG6_PREFIX/lib" \
    "$PYTHON" -c "
import sys
sys.path.insert(0, '.'); sys.path.insert(0, 'GPT_SoVITS')
from GPT_SoVITS.TTS_infer_pack.TTS import TTS, TTS_Config
print('TTS imports clean')
" ) || die "smoke import failed; check $LOG_FILE"
fi

# -- 14. write marker -------------------------------------------------

step "writing marker $MARKER_FILE"
TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
if [[ "$DRY_RUN" == "1" ]]; then
  printf '[dry-run] would write %s\n' "$MARKER_FILE"
else
  cat > "$MARKER_FILE" <<MARKER
schema_version = $MARKER_SCHEMA_VERSION
version = "0.1.0"
installed_at = "$TIMESTAMP"
gpt_sovits_sha = "$GPT_SOVITS_SHA"
python_path = "$ARM64_PYTHON"
ffmpeg6_prefix = "$FFMPEG6_PREFIX"
venv_path = "$VENV_DIR"
repo_path = "$REPO_DIR"

[model_sha256]
s2G2333k = "$SHA256_S2G"
s1bert25hz = "$SHA256_S1BERT"
chinese_hubert_base = "$SHA256_HUBERT"
MARKER
fi
say "$MARKER_FILE"

step "done — cloning stack ready"
cat <<EOF

  next:    voiceforge clone <source> as <name>      # ROADMAP 2.5 (next PR)
  doctor:  voiceforge doctor                         # confirms install
  log:     $LOG_FILE
EOF
