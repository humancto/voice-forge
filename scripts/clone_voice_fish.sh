#!/usr/bin/env bash
# Create a v2 (fish-speech S2 Pro) voice profile at
# ~/.voiceforge/voices/<name>/ from a local audio file.
#
# Recipe: single 8-30s reference clip + Whisper-base transcript.
# Differs from v1 (clone_voice.sh) which writes 1 main + 5 aux clips
# for the gpt-sovits-v2-multi-aux-ref recipe. fish-speech S2 Pro
# encodes a single contiguous window into prompt tokens; aux clips
# don't improve quality.
#
# Window selection (rust-expert plan v2 Decision #5):
#   source <  8s  -> die (too short to encode meaningfully)
#   source 8-30s  -> use entire source as-is
#   source > 30s  -> middle-trim min(source-10s, 20s) seconds,
#                    skipping first 5s + last 5s (intro/outro noise)
#
# Atomic: stages into voices/<name>.partial/, renames on success.
# Concurrency-safe via mkdir-based lockdir at voices/<name>.lock.d/.
#
# Args (positional):
#   1: name      voice name (must match ^[a-z0-9_-]{1,32}$)
#   2: source    local file path (supports ~/, file://, abs/rel paths)
#   3: force     "1" to replace an existing voice, "0" to refuse
#
# Honors:
#   VOICEFORGE_HOME             override ~/.voiceforge
#   VOICEFORGE_CLONE_DRY_RUN=1  print steps, do not execute

set -Eeuo pipefail
IFS=$'\n\t'
export LC_ALL=C
unset CDPATH

NAME="${1:-}"
SOURCE="${2:-}"
FORCE="${3:-0}"

[[ -n "$NAME" && -n "$SOURCE" ]] || { echo "usage: $0 <name> <source> [force]" >&2; exit 2; }

VOICEFORGE_HOME="${VOICEFORGE_HOME:-$HOME/.voiceforge}"
VOICES_DIR="$VOICEFORGE_HOME/voices"
VOICE_DIR="$VOICES_DIR/$NAME"
PARTIAL_DIR="$VOICES_DIR/$NAME.partial"
LOCK_DIR="$VOICES_DIR/$NAME.lock.d"
DRY_RUN="${VOICEFORGE_CLONE_DRY_RUN:-0}"

# v2 cloning install state (schema-2 marker)
INSTALLED_TOML="$VOICEFORGE_HOME/cloning/INSTALLED.toml"

step() { printf '\n==> %s\n' "$*"; }
say()  { printf '    %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

run() {
  if [[ "$DRY_RUN" == "1" ]]; then
    printf '[dry-run] %s\n' "$*"
  else
    "$@"
  fi
}

trap 'die "clone_voice_fish.sh failed at line $LINENO"' ERR

mkdir -p "$VOICES_DIR"

# Atomic mkdir-based lockdir (same pattern as v1)
if ! mkdir "$LOCK_DIR" 2>/dev/null; then
  if [[ -n "$(find "$LOCK_DIR" -maxdepth 0 -mmin +60 2>/dev/null)" ]]; then
    warn "stale lockdir >60min, reclaiming: $LOCK_DIR"
    rmdir "$LOCK_DIR"
    mkdir "$LOCK_DIR"
  else
    die "another clone of $NAME is already running (lock held: $LOCK_DIR)"
  fi
fi
trap 'rmdir "$LOCK_DIR" 2>/dev/null || true' EXIT

step "voiceforge clone $NAME (fish-speech S2 Pro recipe)"

# Validate v2 install marker (skip on dry-run)
step "reading schema-2 marker"
if [[ "$DRY_RUN" != "1" ]]; then
  [[ -f "$INSTALLED_TOML" ]] || die "cloning not installed; run \`voiceforge install-cloning\` first"
  # Quick sanity check that this is a schema-2 marker (Rust side
  # already gated dispatch on is_installed_v2(); this is defense-in-
  # depth so a stale schema-1 marker doesn't sneak in)
  if ! grep -q '^schema_version *= *2' "$INSTALLED_TOML"; then
    die "INSTALLED.toml is not schema_version=2 — refusing to run v2 clone recipe.
This script is for fish-speech installs only. Either run
\`voiceforge install-cloning --force\` to upgrade, or use the legacy
v1 path with \`VOICEFORGE_INSTALL_CLONING_ENGINE=gpt-sovits-v2\`."
  fi
fi

# Defense-in-depth: re-validate the name (Rust caller already did this)
if [[ ! "$NAME" =~ ^[a-z0-9_-]{1,32}$ ]]; then
  die "invalid voice name $NAME (allowed: [a-z0-9_-], 1..=32 chars)"
fi

# Refuse if voice exists and not --force
if [[ -d "$VOICE_DIR" ]]; then
  if [[ "$FORCE" != "1" ]]; then
    die "voice $NAME already exists at $VOICE_DIR; pass --force to replace"
  fi
  step "force: removing existing voice $VOICE_DIR"
  run rm -rf "$VOICE_DIR"
fi

# Clean up any prior orphan partial
[[ -d "$PARTIAL_DIR" ]] && run rm -rf "$PARTIAL_DIR"

# -- 1. resolve source to a local WAV ---------------------------------

TMPDIR_=$(mktemp -d)
trap 'rmdir "$LOCK_DIR" 2>/dev/null || true; rm -rf "$TMPDIR_"' EXIT

# strip file:// prefix
case "$SOURCE" in
  file://*) SOURCE="${SOURCE#file://}" ;;
esac

# expand leading ~/
# shellcheck disable=SC2088  # we strip the literal ~/ prefix, not expand it
case "$SOURCE" in
  '~/'*) SOURCE="$HOME/${SOURCE#'~/'}" ;;
esac

RAW="$TMPDIR_/raw.wav"
case "$SOURCE" in
  http://*|https://*)
    die "URL sources are not supported by clone — provide a local file path.
Tip: download with your tool of choice (yt-dlp, browser, curl), then point at the file:
  voiceforge clone $NAME ./your_clip.wav"
    ;;
esac

if [[ ! -f "$SOURCE" ]]; then
  die "local source not found: $SOURCE"
fi

step "transcoding to 32 kHz mono PCM_16"
run ffmpeg -hide_banner -loglevel error -y -i "$SOURCE" \
  -ac 1 -ar 32000 -acodec pcm_s16le -- "$RAW"

if [[ "$DRY_RUN" != "1" ]]; then
  [[ -f "$RAW" ]] || die "source resolution failed; no $RAW"
fi

# -- 2. duration check + middle-trim ----------------------------------
#
# Rust-expert plan v2 Decision #5: 8s floor, 20s cap. Source <8s
# rejected; 8-30s used as-is; >30s middle-trimmed to min(dur-10s, 20s)
# seconds (skipping first 5s + last 5s for intro/outro noise).

step "duration check + middle-trim"
REF="$PARTIAL_DIR/ref.wav"

if [[ "$DRY_RUN" != "1" ]]; then
  DUR_RAW=$(ffprobe -v error -show_entries format=duration -of default=nw=1:nk=1 "$RAW")
  DUR_INT=${DUR_RAW%.*}

  if (( DUR_INT < 8 )); then
    die "source is ${DUR_RAW}s; need ≥8s for the fish-speech S2 Pro recipe"
  fi

  # Stage partial dir before writing the trimmed clip
  mkdir -p "$PARTIAL_DIR"

  if (( DUR_INT <= 30 )); then
    say "duration ${DUR_RAW}s (in 8-30s window; using as-is)"
    DUR_FOR_TOML="$DUR_RAW"
    run ffmpeg -hide_banner -loglevel error -y \
      -i "$RAW" \
      -ac 1 -ar 32000 -acodec pcm_s16le \
      -af "loudnorm=I=-16:TP=-1.5:LRA=11" \
      -- "$REF"
  else
    # Middle-trim: skip first 5s + last 5s, take a window up to 20s long
    AVAIL=$(( DUR_INT - 10 ))
    if (( AVAIL > 20 )); then AVAIL=20; fi
    # Center the window within (5s, DUR_INT-5s)
    CENTER=$(( DUR_INT / 2 ))
    START=$(( CENTER - AVAIL / 2 ))
    if (( START < 5 )); then START=5; fi
    say "duration ${DUR_RAW}s; middle-trimming ${AVAIL}s window starting at ${START}s"
    DUR_FOR_TOML="${AVAIL}.0"
    run ffmpeg -hide_banner -loglevel error -y \
      -ss "$START" -t "$AVAIL" -i "$RAW" \
      -ac 1 -ar 32000 -acodec pcm_s16le \
      -af "loudnorm=I=-16:TP=-1.5:LRA=11" \
      -- "$REF"
  fi
else
  # Dry-run: still create the partial so phase-line tests can assert
  # downstream phases also fire
  mkdir -p "$PARTIAL_DIR"
  DUR_FOR_TOML="12.0"
fi

# -- 3. whisper-base transcribe the chosen window ---------------------

step "whisper-base transcribe"
if [[ "$DRY_RUN" != "1" ]]; then
  CLONING_VENV_PYTHON="$VOICEFORGE_HOME/cloning/venv/bin/python"
  [[ -x "$CLONING_VENV_PYTHON" ]] || die "cloning venv python missing: $CLONING_VENV_PYTHON"

  # B1 fix (rust-expert PR #45 review): pass the partial dir via env
  # + quoted heredoc so a hostile $VOICEFORGE_HOME containing a quote,
  # a backslash, or a newline cannot escape into the Python source.
  # Previously the unquoted PYEOF interpolated $PARTIAL_DIR directly
  # into the Path("...") literal — known sharp edge.
  PARTIAL_DIR_ENV="$PARTIAL_DIR" "$CLONING_VENV_PYTHON" - <<'PYEOF'
import os
import whisper
from pathlib import Path

partial = Path(os.environ["PARTIAL_DIR_ENV"])
m = whisper.load_model("base")
r = m.transcribe(str(partial / "ref.wav"), language="en", fp16=False)
(partial / "ref.txt").write_text(r["text"].strip())
print(f"  ref: {r['text'].strip()[:80]}")
PYEOF
fi

# -- 4. write schema-2 profile.toml -----------------------------------

step "writing schema-2 profile.toml"
TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
if [[ "$DRY_RUN" != "1" ]]; then
  cat > "$PARTIAL_DIR/profile.toml" <<TOML
schema_version = 2
name = "$NAME"
source = "$SOURCE"
created_at = "$TIMESTAMP"
duration_seconds = $DUR_FOR_TOML
recipe = "fish-speech-s2-pro"
TOML
fi

# -- 5. atomic rename .partial -> final -------------------------------

step "promoting $PARTIAL_DIR -> $VOICE_DIR"
run mv "$PARTIAL_DIR" "$VOICE_DIR"

step "done"
cat <<EOF

  voice:   $NAME
  dir:     $VOICE_DIR
  recipe:  fish-speech-s2-pro
  next:    voiceforge say --voice $NAME --text "the universe is under no obligation to make sense to you"
EOF
