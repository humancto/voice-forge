#!/usr/bin/env bash
set -e
cd "$(dirname "$0")/../services/tts-server"
source .venv/bin/activate
VOICEFORGE_TTS_ENGINE=${VOICEFORGE_TTS_ENGINE:-fallback} python server.py
