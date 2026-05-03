#!/usr/bin/env bash
# Fetch and prepare test fixtures for VoiceForge.
#
# Idempotent: re-running with fixtures already present is a no-op.
# Audio binaries are gitignored — every contributor runs this once.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
FIXTURES_DIR="${REPO_ROOT}/tests/fixtures"

mkdir -p "${FIXTURES_DIR}"

require() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: '$1' not found on PATH. Install it and retry." >&2
    echo "  macOS: brew install $1" >&2
    exit 1
  fi
}

require yt-dlp
require ffmpeg

# Peter Griffin canonical fixture: 20 s, mono, 22050 Hz, 16-bit PCM.
# This is what audio_ingest tests exercise. The exact clip URL is pinned
# to keep test behavior stable across yt-dlp versions and search ranking.
TARGET="${FIXTURES_DIR}/peter_griffin.wav"
SOURCE_URL="https://www.youtube.com/watch?v=OShWNK4zGQE"

if [[ -f "${TARGET}" ]]; then
  echo "fixture already present: ${TARGET}"
  exit 0
fi

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

echo "transcoding to canonical 22050/1/16-bit, 20s..."
ffmpeg -hide_banner -loglevel error -y \
  -i "${RAW}" \
  -ar 22050 -ac 1 -acodec pcm_s16le -t 20 \
  -- "${TARGET}"

echo "ready: ${TARGET}"
ffprobe -v error -of default=noprint_wrappers=1 \
  -select_streams a:0 \
  -show_entries stream=sample_rate,channels,bits_per_sample,codec_name \
  -show_entries format=duration \
  "${TARGET}"
