#!/usr/bin/env bash
set -e
cd "$(dirname "$0")/../services/tts-server"
python3 -m venv .venv
source .venv/bin/activate
pip install --upgrade pip
pip install -r requirements.txt
echo "TTS server environment ready."
