#!/usr/bin/env bash
# Fetch and prepare test fixtures for VoiceForge.
#
# Two modes:
#   FIXTURE_MODE=youtube (default): downloads the canonical Peter
#   Griffin clip via yt-dlp + ffmpeg-trims to 20 s mono 22050 Hz.
#   This is the local-dev path — you hear the actual voice.
#
#   FIXTURE_MODE=synthetic: generates a 20 s sine-wave WAV via
#   ffmpeg's lavfi sine generator. Used in CI where YouTube blocks
#   shared runner egress IPs with bot-detection. The audio_ingest
#   tests only assert format and duration — content doesn't matter.
#
# Idempotent: re-running with the fixture present is a no-op.
# Audio binaries are gitignored — every contributor runs this once.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
FIXTURES_DIR="${REPO_ROOT}/tests/fixtures"
FIXTURE_MODE="${FIXTURE_MODE:-youtube}"
TARGET="${FIXTURES_DIR}/peter_griffin.wav"

mkdir -p "${FIXTURES_DIR}"

require() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: '$1' not found on PATH. Install it and retry." >&2
    echo "  macOS: brew install $1" >&2
    exit 1
  fi
}

if [[ -f "${TARGET}" ]]; then
  echo "fixture already present: ${TARGET}"
  exit 0
fi

case "${FIXTURE_MODE}" in
  synthetic)
    require ffmpeg
    echo "generating synthetic 22050/1/16-bit 20 s sine fixture..."
    ffmpeg -hide_banner -loglevel error -y \
      -f lavfi -i "sine=frequency=440:duration=20" \
      -ar 22050 -ac 1 -acodec pcm_s16le \
      -- "${TARGET}"
    ;;
  youtube)
    require yt-dlp
    require ffmpeg
    SOURCE_URL="https://www.youtube.com/watch?v=OShWNK4zGQE"
    TMPDIR_=$(mktemp -d)
    trap 'rm -rf "${TMPDIR_}"' EXIT

    echo "downloading source clip..."
    yt-dlp \
      --quiet --no-warnings \
      -x --audio-format wav --audio-quality 0 \
      -o "${TMPDIR_}/raw.%(ext)s" \
      "${SOURCE_URL}"

    RAW="${TMPDIR_}/raw.wav"
    if [[ ! -f "${RAW}" ]]; then
      echo "error: yt-dlp did not produce ${RAW}" >&2
      exit 1
    fi

    echo "transcoding to canonical 22050/1/16-bit, 20 s..."
    ffmpeg -hide_banner -loglevel error -y \
      -i "${RAW}" \
      -ar 22050 -ac 1 -acodec pcm_s16le -t 20 \
      -- "${TARGET}"
    ;;
  *)
    echo "error: unknown FIXTURE_MODE '${FIXTURE_MODE}' (expected 'youtube' or 'synthetic')" >&2
    exit 1
    ;;
esac

echo "ready: ${TARGET}"
ffprobe -v error -of default=noprint_wrappers=1 \
  -select_streams a:0 \
  -show_entries stream=sample_rate,channels,bits_per_sample,codec_name \
  -show_entries format=duration \
  "${TARGET}"
