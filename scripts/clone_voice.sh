#!/usr/bin/env bash
# Create a voice profile at ~/.voiceforge/voices/<name>/ from a source
# audio file or yt-dlp-resolvable URL.
#
# Recipe: 1 main 10s ref + 5 aux 10s refs (loudnorm'd, 32 kHz mono),
# whisper-transcribed for prompt_text. Source must be ≥60 s of single-
# speaker audio.
#
# Atomic: stages into voices/<name>.partial/, renames on success. SIGINT
# leaves an orphan .partial dir for next-run cleanup.
#
# Concurrency-safe: flock on voices/<name>.lock for the duration.
#
# Args (positional):
#   1: name      — voice name (must match ^[a-z0-9_-]{1,32}$)
#   2: source    — local file path (supports ~/, file://, abs/rel paths)
#   3: force     — "1" to replace an existing voice, "0" to refuse
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

# Cloning install state
INSTALLED_TOML="$VOICEFORGE_HOME/cloning/INSTALLED.toml"

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

trap 'die "clone_voice.sh failed at line $LINENO"' ERR

mkdir -p "$VOICES_DIR"

# Atomic mkdir-based lock (portable across macOS + Linux; no flock dep).
# Reclaim the lockdir if it's older than 60 min — covers SIGKILL leaks
# (the trap below handles SIGTERM/SIGINT/normal exit).
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

# Validate cloning install (script must run after `voiceforge install-cloning`)
if [[ "$DRY_RUN" != "1" ]]; then
  [[ -f "$INSTALLED_TOML" ]] || die "cloning not installed; run \`voiceforge install-cloning\` first"
fi

# Validate name (defense-in-depth — Rust caller already does this)
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

step "voiceforge clone $NAME (source: $SOURCE)"

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
step "transcoding local source"
run ffmpeg -hide_banner -loglevel error -y -i "$SOURCE" \
  -ac 1 -ar 32000 -acodec pcm_s16le -- "$RAW"

if [[ "$DRY_RUN" != "1" ]]; then
  [[ -f "$RAW" ]] || die "source resolution failed; no $RAW"
fi

# -- 2. duration check ------------------------------------------------

step "duration check (need ≥60s)"
if [[ "$DRY_RUN" != "1" ]]; then
  DUR_RAW=$(ffprobe -v error -show_entries format=duration -of default=nw=1:nk=1 "$RAW")
  DUR_INT=${DUR_RAW%.*}
  if (( DUR_INT < 60 )); then
    die "source is ${DUR_RAW}s; need ≥60s for the multi-aux-ref recipe"
  fi
  say "duration ${DUR_RAW}s"
fi

# -- 3. stage into .partial/ ------------------------------------------

step "staging into $PARTIAL_DIR"
run mkdir -p "$PARTIAL_DIR"

# 6 × 10s windows, skipping first 5s as intro buffer
for i in 0 1 2 3 4 5; do
  OFF=$(( 5 + i * 10 ))
  if [[ "$i" == "0" ]]; then
    OUT="$PARTIAL_DIR/ref_main.wav"
  else
    OUT="$PARTIAL_DIR/aux_${i}.wav"
  fi
  run ffmpeg -hide_banner -loglevel error -y \
    -ss "$OFF" -t 10 -i "$RAW" \
    -ac 1 -ar 32000 -acodec pcm_s16le \
    -af "loudnorm=I=-16:TP=-1.5:LRA=11" \
    -- "$OUT"
done

# -- 4. whisper transcribe each chunk ---------------------------------

step "whisper-transcribing each chunk"
if [[ "$DRY_RUN" != "1" ]]; then
  CLONING_VENV_PYTHON="$VOICEFORGE_HOME/cloning/venv/bin/python"
  [[ -x "$CLONING_VENV_PYTHON" ]] || die "cloning venv python missing: $CLONING_VENV_PYTHON"

  "$CLONING_VENV_PYTHON" - <<PYEOF
import json, whisper, sys
from pathlib import Path
m = whisper.load_model("base")
parts = ["ref_main"] + [f"aux_{i}" for i in range(1, 6)]
for p in parts:
    wav = Path("$PARTIAL_DIR") / f"{p}.wav"
    txt = Path("$PARTIAL_DIR") / f"{p}.txt"
    r = m.transcribe(str(wav), language="en", fp16=False)
    txt.write_text(r["text"].strip())
    print(f"  {p}: {r['text'].strip()[:60]}")
PYEOF
fi

# -- 5. write profile.toml --------------------------------------------

step "writing profile.toml"
TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
if [[ "$DRY_RUN" != "1" ]]; then
  DUR_FOR_TOML="${DUR_RAW:-60.0}"
  cat > "$PARTIAL_DIR/profile.toml" <<TOML
schema_version = 1
name = "$NAME"
source = "$SOURCE"
created_at = "$TIMESTAMP"
duration_seconds = $DUR_FOR_TOML
recipe = "gpt-sovits-v2-multi-aux-ref"
aux_count = 5
TOML
fi

# -- 6. atomic rename .partial -> final ------------------------------

step "promoting $PARTIAL_DIR -> $VOICE_DIR"
run mv "$PARTIAL_DIR" "$VOICE_DIR"

step "done"
cat <<EOF

  voice:   $NAME
  dir:     $VOICE_DIR
  next:    voiceforge say --voice $NAME --text "Holy crap, the build is on fire."
EOF
