"""
VoiceForge embedding extraction.

Usage:
    cd services/tts-server
    python extract_embedding.py angry_duck

Input:
    voices/angry_duck.wav

Output:
    embeddings/angry_duck.pt

Note:
XTTS public APIs vary by version. This script is intentionally isolated so an agent can adapt it
based on the installed TTS package version.
"""

from pathlib import Path
import sys
import torch

BASE_DIR = Path(__file__).resolve().parent
VOICES_DIR = BASE_DIR / "voices"
EMBEDDINGS_DIR = BASE_DIR / "embeddings"
EMBEDDINGS_DIR.mkdir(exist_ok=True)

def main():
    if len(sys.argv) < 2:
        print("Usage: python extract_embedding.py <voice_id>")
        sys.exit(1)

    voice_id = sys.argv[1]
    wav_path = VOICES_DIR / f"{voice_id}.wav"
    out_path = EMBEDDINGS_DIR / f"{voice_id}.pt"

    if not wav_path.exists():
        raise FileNotFoundError(f"Missing voice sample: {wav_path}")

    from TTS.api import TTS
    tts = TTS(model_name="tts_models/multilingual/multi-dataset/xtts_v2")

    embedding = tts.speaker_manager.compute_embedding_from_clip(str(wav_path))
    torch.save(embedding, out_path)

    print(f"Saved embedding: {out_path}")

if __name__ == "__main__":
    main()
