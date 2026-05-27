#!/usr/bin/env bash
# Install the fish-speech S2 Pro cloning stack at ~/.voiceforge/cloning/.
#
# v2 installer (ROADMAP v0.4 PR-AB step 6a). Replaces the placeholder
# stub. Mirrors the v1 GPT-SoVITS installer's battle-tested structure
# (traps, retries, modes, dry-run) with fish-speech-specific deps,
# model repo, and SHA pins.
#
# Idempotent: re-runs are fast (skip-if-installed). --force rebuilds
# the venv. --check verifies state without mutating. --uninstall removes
# the venv + repo + marker (preserves HuggingFace + whisper caches so
# a re-install reuses the ~10 GB of model downloads).
#
# Honors:
#   VOICEFORGE_INSTALL_CLONING_DRY_RUN=1     print every action, do not execute
#   VOICEFORGE_INSTALL_CLONING_MODE          normal | check | uninstall   (default: normal)
#   VOICEFORGE_INSTALL_CLONING_FORCE=1       wipe venv + marker before installing
#   VOICEFORGE_INSTALL_CLONING_SKIP_WEIGHTS=1        skip the ~10 GB HF download (for testing)
#   VOICEFORGE_INSTALL_CLONING_SKIP_PLATFORM_CHECK=1 warn instead of die on non-arm64-macOS
#                                                     (test-only — production users get the
#                                                      strict check; the orchestrator never
#                                                      sets this)
#   VOICEFORGE_FISH_SPEECH_REPO_OVERRIDE              use an existing local fish-speech clone
#                                                     (symlinked instead of cloned fresh)
#
# The orchestrator in apps/voiceforge-cli/src/install_cloning.rs parses
# `==>` lines as phase boundaries for the indicatif install wizard.

set -Eeuo pipefail
IFS=$'\n\t'
export LC_ALL=C
unset CDPATH

# -- pinned values (single source of truth) ---------------------------

readonly FISH_SPEECH_REPO="https://github.com/fishaudio/fish-speech.git"
readonly FISH_SPEECH_SHA="3dd1f85c402ee6f0a17c2971d3b0dd8d881ca139"
readonly HF_MODEL_REPO="fishaudio/s2-pro"
readonly PYTHON_VERSION="3.11"
readonly WHISPER_MODEL="medium"

# SHA256 of load-bearing model files. Captured from a known-good install
# at $HOME/fish-experiment/fish-speech on 2026-05-11. Update when bumping
# FISH_SPEECH_SHA / HF_MODEL_REPO.
readonly SHA256_CODEC="74fc41c5a7151c6f350af8bd7e5d6e3accfcc7f3dfbfac23afd35af07052bb2f"
readonly SHA256_MODEL_01="c4218e8ac93be83b35eee30b4f94cb2e9b5ecff40f3e21611438d2f4f8804aad"
readonly SHA256_MODEL_02="76738d23465deaac431433232c0762908cc99a6eddc3d49f67307d92680827be"
readonly SHA256_MODEL_INDEX="c8cb9974d3d17663a95dba1d6c3ea531fc42d377c0dff946f3a01a0ec48d45a3"
readonly SHA256_CONFIG="261b519a2a9576710fc8533a77297fae007f0e7b3aa28a217f8352b7f32fe993"

readonly REQUIRED_REPO_PATHS=(
  "fish_speech/models/text2semantic/inference.py"
  "fish_speech/models/dac/inference.py"
  "pyproject.toml"
)

readonly MARKER_SCHEMA_VERSION=2

# -- paths ------------------------------------------------------------

VOICEFORGE_HOME="${VOICEFORGE_HOME:-$HOME/.voiceforge}"
readonly VOICEFORGE_HOME
readonly CLONING_ROOT="$VOICEFORGE_HOME/cloning"
readonly VENV_DIR="$CLONING_ROOT/venv"
readonly REPO_DIR="$CLONING_ROOT/repo"
readonly CHECKPOINT_DIR="$REPO_DIR/checkpoints/s2-pro"
readonly MARKER_FILE="$CLONING_ROOT/INSTALLED.toml"
readonly LOG_FILE="$CLONING_ROOT/install.log"
readonly V1_BACKUP_FILE="$CLONING_ROOT/INSTALLED.v1.bak"

readonly BREW_BIN="/opt/homebrew/bin/brew"
readonly ARM64_PYTHON="/opt/homebrew/bin/python${PYTHON_VERSION}"
readonly FFMPEG6_PREFIX="/opt/homebrew/opt/ffmpeg@6"

DRY_RUN="${VOICEFORGE_INSTALL_CLONING_DRY_RUN:-0}"
MODE="${VOICEFORGE_INSTALL_CLONING_MODE:-normal}"
FORCE="${VOICEFORGE_INSTALL_CLONING_FORCE:-0}"
SKIP_WEIGHTS="${VOICEFORGE_INSTALL_CLONING_SKIP_WEIGHTS:-0}"
SKIP_PLATFORM_CHECK="${VOICEFORGE_INSTALL_CLONING_SKIP_PLATFORM_CHECK:-0}"
REPO_OVERRIDE="${VOICEFORGE_FISH_SPEECH_REPO_OVERRIDE:-}"

# -- io helpers -------------------------------------------------------
# `step` (==>) marks phase boundaries. The Rust orchestrator parses
# these lines to drive the indicatif progress wizard, so the prefix
# is load-bearing — don't change it.

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

trap 'die "install_cloning_fish.sh failed at line $LINENO. See $LOG_FILE."' ERR

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

step "voiceforge install-cloning v2 (fish-speech S2 Pro)  (mode=$MODE force=$FORCE dry_run=$DRY_RUN skip_weights=$SKIP_WEIGHTS)"

# -- v1 marker backup (if present) ------------------------------------
# If a schema-1 install exists, preserve the marker before the v2 flow
# touches anything. install_cloning.rs reads INSTALLED.v1.bak to power
# the schema-1 → schema-2 migration hint surfaced in `voiceforge doctor`.

if [[ "$MODE" == "normal" ]] && [[ -f "$MARKER_FILE" ]]; then
  # R6 fix (rust-expert review pass 1): tighten the regex to require
  # `=` after schema_version. Without `[[:space:]]*=`, a hypothetical
  # future field like `schema_version_old = 1` would falsely register
  # as a v1 install and trigger an unwanted backup.
  existing_schema="$(awk -F'=' '/^schema_version[[:space:]]*=/ {gsub(/ /, "", $2); print $2; exit}' "$MARKER_FILE" 2>/dev/null || echo "")"
  if [[ "$existing_schema" == "1" ]]; then
    step "detected schema-1 (GPT-SoVITS) install — backing up to $V1_BACKUP_FILE"
    run cp -f "$MARKER_FILE" "$V1_BACKUP_FILE"
  fi
fi

# -- mode dispatch ----------------------------------------------------

case "$MODE" in
  uninstall)
    step "uninstall: removing venv + repo + marker + embedded-extract (HF + whisper caches preserved)"
    EXTRACT_DIR="$CLONING_ROOT/.install"
    run rm -rf "$VENV_DIR" "$REPO_DIR" "$MARKER_FILE" "$EXTRACT_DIR"
    say "removed $VENV_DIR"
    say "removed $REPO_DIR"
    say "removed $MARKER_FILE"
    say "removed $EXTRACT_DIR (embedded runtime scripts; re-extracted on next install)"
    say "preserved: ~/.cache/huggingface/    (re-install reuses the model download)"
    say "preserved: ~/.cache/whisper/        (re-install reuses the whisper-medium download)"
    exit 0
    ;;
  check)
    step "check: verifying install state without mutations"
    [[ -f "$MARKER_FILE" ]] || die "marker missing: $MARKER_FILE — run 'voiceforge install-cloning'"
    [[ -x "$VENV_DIR/bin/python" ]] || die "venv missing: $VENV_DIR — run 'voiceforge install-cloning --force'"
    [[ -d "$REPO_DIR/.git" ]] || [[ -L "$REPO_DIR" ]] || die "repo missing: $REPO_DIR — run 'voiceforge install-cloning --force'"
    step "smoke import"
    if [[ "$DRY_RUN" != "1" ]]; then
      ( cd "$REPO_DIR" && DYLD_FALLBACK_LIBRARY_PATH="$FFMPEG6_PREFIX/lib" \
        "$VENV_DIR/bin/python" -c "
import sys
sys.path.insert(0, '.')
from fish_speech.models.text2semantic.inference import GenerateRequest, load_codec_model  # noqa
print('OK')
" >/dev/null ) || die "smoke import failed — re-install with --force"
    fi
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
# Fish-speech S2 Pro weights are ~10 GB; whisper medium is ~1.5 GB;
# venv + repo + pip cache adds ~3 GB. Require 15 GB free.

step "disk-space precheck (need >=15 GB free at \$HOME for weights + venv)"
free_kb="$(df -k "$HOME" | awk 'NR==2 {print $4}')"
free_gb=$(( free_kb / 1024 / 1024 ))
say "free: ${free_gb} GB"
(( free_gb >= 15 )) || die "need >=15 GB free at $HOME, have ${free_gb} GB"

# -- 2. platform check -----------------------------------------------

step "platform check"
if [[ "$(uname -s)" != "Darwin" ]] || [[ "$(uname -m)" != "arm64" ]]; then
  if [[ "$SKIP_PLATFORM_CHECK" == "1" ]]; then
    warn "platform check bypassed via VOICEFORGE_INSTALL_CLONING_SKIP_PLATFORM_CHECK=1
You are on: $(uname -s)/$(uname -m). Production users get the strict check;
this bypass exists for cargo test on Rosetta-translated x86_64 subprocesses."
  else
    die "macOS arm64 only for now (Linux/Windows is v0.4.1).
You are on: $(uname -s)/$(uname -m)"
  fi
fi
say "macOS arm64 confirmed (or platform-check bypassed)"

# -- 3. arm64 brew check ---------------------------------------------

step "arm64 Homebrew check"
if [[ ! -x "$BREW_BIN" ]]; then
  if [[ "$SKIP_PLATFORM_CHECK" == "1" ]]; then
    warn "arm64 Homebrew check bypassed (no $BREW_BIN); test mode only"
  else
    die "arm64 Homebrew not found at $BREW_BIN.
Install with:
  /bin/bash -c \"\$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\""
  fi
else
  say "found $BREW_BIN"
fi

# -- 4. brew install ffmpeg@6 + python@3.11 (in that order) ----------

if [[ -x "$BREW_BIN" ]]; then
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
fi

# -- 5. arm64 python verification ------------------------------------

step "verifying $ARM64_PYTHON is arm64"
if [[ "$DRY_RUN" != "1" ]] && [[ -x "$ARM64_PYTHON" ]]; then
  if ! "$ARM64_PYTHON" -c 'import platform,sys; sys.exit(0 if platform.machine()=="arm64" else 1)'; then
    die "$ARM64_PYTHON is not arm64. Reinstall with:
  arch -arm64 $BREW_BIN reinstall python@${PYTHON_VERSION}"
  fi
elif [[ ! -x "$ARM64_PYTHON" ]]; then
  if [[ "$SKIP_PLATFORM_CHECK" == "1" ]]; then
    warn "arm64 python verification bypassed (no $ARM64_PYTHON); test mode only"
  else
    die "$ARM64_PYTHON not found — install with: arch -arm64 $BREW_BIN install python@${PYTHON_VERSION}"
  fi
fi
say "arm64 python confirmed"

# -- 6. force-mode wipe ----------------------------------------------

if [[ "$FORCE" == "1" ]]; then
  step "force: removing venv + marker (HF + whisper caches preserved)"
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

# -- 8. clone or symlink fish-speech repo ----------------------------
# REPO_OVERRIDE lets developers point at an existing local fish-speech
# clone (saves 10+ GB on a re-install). Production users go through
# the normal git-clone path.

if [[ -n "$REPO_OVERRIDE" ]]; then
  if [[ ! -d "$REPO_OVERRIDE/.git" ]]; then
    die "VOICEFORGE_FISH_SPEECH_REPO_OVERRIDE=$REPO_OVERRIDE is not a git repo"
  fi
  step "symlinking $REPO_DIR -> $REPO_OVERRIDE (override mode)"
  run rm -rf "$REPO_DIR"
  run ln -sf "$REPO_OVERRIDE" "$REPO_DIR"
else
  if [[ ! -d "$REPO_DIR/.git" ]]; then
    step "cloning fish-speech @ $FISH_SPEECH_SHA"
    run retry 3 5 git clone --quiet "$FISH_SPEECH_REPO" "$REPO_DIR"
  fi
  step "checking out pinned SHA $FISH_SPEECH_SHA"
  run git -C "$REPO_DIR" fetch --quiet origin "$FISH_SPEECH_SHA"
  # reset --hard (not checkout) so a previously-corrupted worktree from
  # a partial install can never block the pinned SHA from landing.
  run git -C "$REPO_DIR" reset --quiet --hard "$FISH_SPEECH_SHA"
fi

step "asserting required repo paths exist"
for p in "${REQUIRED_REPO_PATHS[@]}"; do
  if [[ "$DRY_RUN" != "1" ]] && [[ ! -e "$REPO_DIR/$p" ]]; then
    die "expected path missing after checkout of $FISH_SPEECH_SHA: $p
The pin may need bumping; verify upstream still has this layout."
  fi
  say "ok: $p"
done

# -- 9. pip install fish-speech (via its own pyproject.toml) ---------

step "upgrading pip + wheel"
run retry 3 5 "$PIP" install --quiet --upgrade pip wheel setuptools

step "installing fish-speech + deps (~5-10 min on first run)"
# fish-speech's pyproject.toml declares torch==2.8.0, transformers,
# librosa, etc. Editable install so the smoke + runtime can import
# fish_speech.* from the cloned repo.
run retry 3 10 "$PIP" install --quiet --prefer-binary -e "$REPO_DIR"

# Additional packages voiceforge needs that fish-speech does NOT pull:
# - openai-whisper (clone-recipe reference transcription, ROADMAP v0.4 PR-C)
# - huggingface_hub CLI (model download)
step "installing voiceforge-side python helpers (whisper + huggingface-cli)"
run retry 3 10 "$PIP" install --quiet --prefer-binary \
  "openai-whisper" \
  "huggingface_hub[cli]"

# -- 10. download HF model subset (~10 GB) ---------------------------

if [[ "$SKIP_WEIGHTS" == "1" ]]; then
  warn "SKIP_WEIGHTS=1 — leaving $CHECKPOINT_DIR untouched"
else
  mkdir -p "$CHECKPOINT_DIR"
  step "downloading $HF_MODEL_REPO (S2 Pro weights, ~10 GB) — resumable via huggingface-cli"
  # huggingface-cli download is resumable across re-runs and respects
  # the HF cache so a partial download survives a Ctrl-C + re-invoke.
  run retry 3 10 "$VENV_DIR/bin/huggingface-cli" download "$HF_MODEL_REPO" \
    --local-dir "$CHECKPOINT_DIR"
fi

# -- 11. sha256-verify load-bearing files ----------------------------

if [[ "$SKIP_WEIGHTS" != "1" ]]; then
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
This usually means a partial download. Re-run install_cloning_fish.sh."
    fi
    say "ok: $(basename "$file")"
  }

  verify_sha "$CHECKPOINT_DIR/codec.pth" "$SHA256_CODEC"
  verify_sha "$CHECKPOINT_DIR/model-00001-of-00002.safetensors" "$SHA256_MODEL_01"
  verify_sha "$CHECKPOINT_DIR/model-00002-of-00002.safetensors" "$SHA256_MODEL_02"
  verify_sha "$CHECKPOINT_DIR/model.safetensors.index.json" "$SHA256_MODEL_INDEX"
  verify_sha "$CHECKPOINT_DIR/config.json" "$SHA256_CONFIG"
fi

# -- 12. whisper medium download (~1.5 GB) ---------------------------
# whisper.load_model() downloads to ~/.cache/whisper/. Idempotent —
# re-runs with the file present are a no-op.

step "downloading whisper $WHISPER_MODEL model (~1.5 GB) — used by clone-recipe to transcribe reference clip"
if [[ "$DRY_RUN" == "1" ]]; then
  printf '[dry-run] %s -c "import whisper; whisper.load_model(\"%s\")"\n' "$PYTHON" "$WHISPER_MODEL"
else
  "$PYTHON" -c "
import whisper, sys
try:
    whisper.load_model('$WHISPER_MODEL')
    print('whisper $WHISPER_MODEL ready')
except Exception as e:
    print(f'whisper download failed: {e}', file=sys.stderr)
    sys.exit(1)
"
fi

# -- 13. smoke test ---------------------------------------------------

step "smoke test: importing fish_speech.models.text2semantic"
if [[ "$SKIP_WEIGHTS" != "1" ]] && [[ "$DRY_RUN" != "1" ]]; then
  # Real fish-speech at SHA 3dd1f85c exports GenerateRequest +
  # load_codec_model + encode_audio + decode_to_audio + init_model +
  # generate_long ALL from fish_speech.models.text2semantic.inference.
  # The previous v0.4 PR-AB code had `load_codec_model` imported from
  # fish_speech.models.dac.inference which fails ImportError at runtime
  # — caught by the first real install end-to-end on 2026-05-12.
  ( cd "$REPO_DIR" && DYLD_FALLBACK_LIBRARY_PATH="$FFMPEG6_PREFIX/lib" \
    "$PYTHON" -c "
import sys
sys.path.insert(0, '.')
from fish_speech.models.text2semantic.inference import (
    GenerateRequest,
    init_model,
    generate_long,
    load_codec_model,
    encode_audio,
    decode_to_audio,
)
print('fish-speech imports clean')
" ) || die "smoke import failed; check $LOG_FILE"
fi

# -- 14. write marker -------------------------------------------------

step "writing schema-v$MARKER_SCHEMA_VERSION marker $MARKER_FILE"
TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
if [[ "$DRY_RUN" == "1" ]]; then
  printf '[dry-run] would write %s\n' "$MARKER_FILE"
else
  # Atomic write: tmp file + rename. If the rename fails the v1 marker
  # (if any) stays intact — `voiceforge doctor` keeps reporting the
  # working schema-1 install rather than a half-installed schema-2.
  MARKER_TMP="${MARKER_FILE}.tmp.$$"
  cat > "$MARKER_TMP" <<MARKER
schema_version = $MARKER_SCHEMA_VERSION
version = "0.4.0"
installed_at = "$TIMESTAMP"
engine = "fish-speech-s2-pro"
fish_speech_sha = "$FISH_SPEECH_SHA"
python_path = "$ARM64_PYTHON"
ffmpeg6_prefix = "$FFMPEG6_PREFIX"
venv_path = "$VENV_DIR"
repo_path = "$REPO_DIR"
checkpoint_path = "$CHECKPOINT_DIR"
whisper_model = "$WHISPER_MODEL"

[model_sha256]
codec_pth = "$SHA256_CODEC"
model_safetensors_01 = "$SHA256_MODEL_01"
model_safetensors_02 = "$SHA256_MODEL_02"
model_safetensors_index = "$SHA256_MODEL_INDEX"
config_json = "$SHA256_CONFIG"
MARKER
  mv -f "$MARKER_TMP" "$MARKER_FILE"
fi
say "$MARKER_FILE"

step "done — fish-speech S2 Pro cloning stack ready"
cat <<EOF

  next:    voiceforge clone <name> <source>      # ROADMAP v0.4 PR-C
  doctor:  voiceforge doctor                     # confirms install
  log:     $LOG_FILE
EOF
