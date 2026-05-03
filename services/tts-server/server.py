from flask import Flask, request, jsonify
from pathlib import Path
import hashlib
import json
import os
import subprocess
import sys

app = Flask(__name__)

BASE_DIR = Path(__file__).resolve().parent
REPO_ROOT = BASE_DIR.parent.parent.parent
PRESETS_DIR = REPO_ROOT / "configs" / "presets"
CACHE_DIR = BASE_DIR / "audio_cache"
VOICES_DIR = BASE_DIR / "voices"
EMBEDDINGS_DIR = BASE_DIR / "embeddings"

CACHE_DIR.mkdir(exist_ok=True)
VOICES_DIR.mkdir(exist_ok=True)
EMBEDDINGS_DIR.mkdir(exist_ok=True)

_xtts = None

def load_preset(voice_id: str) -> dict:
    path = PRESETS_DIR / f"{voice_id}.json"
    if path.exists():
        return json.loads(path.read_text())
    return {"id": voice_id, "display_name": voice_id, "language": "en", "temperature": 0.2, "speed": 1.0}

def cache_key(text: str, voice: str, preset: dict) -> str:
    payload = json.dumps({
        "text": text,
        "voice": voice,
        "temperature": preset.get("temperature", 0.2),
        "speed": preset.get("speed", 1.0),
        "model": os.environ.get("VOICEFORGE_TTS_ENGINE", "fallback")
    }, sort_keys=True)
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()

def fallback_tts(text: str, out_path: Path):
    if sys.platform == "darwin":
        aiff_path = out_path.with_suffix(".aiff")
        subprocess.run(["say", "-o", str(aiff_path), text], check=True)
        subprocess.run(["afconvert", "-f", "WAVE", "-d", "LEI16", str(aiff_path), str(out_path)], check=True)
        try:
            aiff_path.unlink()
        except Exception:
            pass
    else:
        subprocess.run(["espeak", text, "-w", str(out_path)], check=True)

def get_xtts():
    global _xtts
    if _xtts is None:
        from TTS.api import TTS
        _xtts = TTS(model_name="tts_models/multilingual/multi-dataset/xtts_v2")
    return _xtts

def xtts_generate(text: str, voice: str, preset: dict, out_path: Path):
    tts = get_xtts()
    wav_path = VOICES_DIR / f"{voice}.wav"
    language = preset.get("language", "en")

    if wav_path.exists():
        tts.tts_to_file(text=text, speaker_wav=str(wav_path), language=language, file_path=str(out_path))
    else:
        tts.tts_to_file(text=text, language=language, file_path=str(out_path))

@app.route("/health", methods=["GET"])
def health():
    return jsonify({"status": "ok"})

@app.route("/voices", methods=["GET"])
def voices():
    return jsonify([json.loads(path.read_text()) for path in sorted(PRESETS_DIR.glob("*.json"))])

@app.route("/tts", methods=["POST"])
def tts():
    data = request.json or {}
    text = data.get("text", "").strip()
    voice = data.get("voice", "default").strip()

    if not text:
        return jsonify({"error": "text is required"}), 400

    preset = load_preset(voice)
    key = cache_key(text, voice, preset)
    out_path = CACHE_DIR / f"{key}.wav"

    if out_path.exists():
        return jsonify({"audio_path": str(out_path), "cache_hit": True})

    engine = os.environ.get("VOICEFORGE_TTS_ENGINE", "fallback")

    try:
        if engine == "xtts":
            xtts_generate(text, voice, preset, out_path)
        else:
            fallback_tts(text, out_path)
    except Exception as e:
        return jsonify({"error": str(e), "hint": "Use VOICEFORGE_TTS_ENGINE=fallback first. For XTTS, install requirements and add a voice wav."}), 500

    return jsonify({"audio_path": str(out_path), "cache_hit": False})

if __name__ == "__main__":
    print("VoiceForge TTS server starting on http://localhost:5000")
    print("Engine:", os.environ.get("VOICEFORGE_TTS_ENGINE", "fallback"))
    app.run(host="127.0.0.1", port=5000)
