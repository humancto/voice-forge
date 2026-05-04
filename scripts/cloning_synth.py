#!/usr/bin/env python3
"""Long-lived NDJSON synth worker. Reads request lines from stdin,
emits response lines on stdout. Loaded once per voiceforge process.

Request:  {"text": "...", "voice": "peter", "out": "/tmp/.../out.wav"}
Response: {"ok": true, "sample_rate": 32000, "duration": 6.13}
       or {"ok": false, "error": "..."}

Constants pinned to the install-cloning marker so a torn install
between marker write and synth call fails loudly on startup.
"""

import json
import os
import sys
import time
from pathlib import Path

EXPECTED_GPT_SOVITS_SHA = "08d627c3338173c3229286d8787060d6559fe0f8"


def fail(error: str, exit_code: int = 1) -> None:
    sys.stdout.write(json.dumps({"ok": False, "error": error}) + "\n")
    sys.stdout.flush()
    sys.exit(exit_code)


def parse_marker(path: Path) -> dict:
    """Tiny TOML parser sufficient for our flat marker. Avoids pulling
    a TOML lib in for one file."""
    out = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or line.startswith("["):
            continue
        if "=" not in line:
            continue
        k, v = line.split("=", 1)
        out[k.strip()] = v.strip().strip('"')
    return out


def load_tts(repo_dir: Path):
    sys.path.insert(0, str(repo_dir))
    sys.path.insert(0, str(repo_dir / "GPT_SoVITS"))
    os.chdir(repo_dir)
    from GPT_SoVITS.TTS_infer_pack.TTS import TTS, TTS_Config

    cfg = TTS_Config(str(repo_dir / "GPT_SoVITS/configs/tts_infer.yaml"))
    cfg.version = "v2"
    cfg.device = "cpu"
    cfg.is_half = False
    return TTS(cfg)


def synth(tts, voices_dir: Path, voice: str, text: str, out_path: Path):
    voice_dir = voices_dir / voice
    if not voice_dir.is_dir():
        raise RuntimeError(f"voice {voice!r} not found at {voice_dir}")

    main_ref = voice_dir / "ref_main.wav"
    main_prompt = (voice_dir / "ref_main.txt").read_text().strip()
    aux_refs = [str(voice_dir / f"aux_{i}.wav") for i in range(1, 6)]

    req = {
        "text": text,
        "text_lang": "en",
        "ref_audio_path": str(main_ref),
        "aux_ref_audio_paths": aux_refs,
        "prompt_text": main_prompt,
        "prompt_lang": "en",
        "top_k": 5,
        "top_p": 1.0,
        "temperature": 1.0,
        "text_split_method": "cut5",
        "batch_size": 1,
        "speed_factor": 1.1,
        "split_bucket": True,
        "fragment_interval": 0.3,
        "return_fragment": False,
    }

    import numpy as np
    import soundfile as sf

    chunks = []
    sr = None
    for chunk_sr, chunk_audio in tts.run(req):
        sr = chunk_sr
        chunks.append(chunk_audio)
    audio = np.concatenate(chunks) if len(chunks) > 1 else chunks[0]

    tmp = out_path.with_suffix(out_path.suffix + ".tmp")
    sf.write(str(tmp), audio, sr)
    tmp.replace(out_path)

    return int(sr), float(len(audio) / sr)


def main() -> None:
    home_env = os.environ.get("VOICEFORGE_HOME")
    home = Path(home_env) if home_env else Path.home() / ".voiceforge"

    marker = home / "cloning/INSTALLED.toml"
    if not marker.is_file():
        fail(f"cloning marker missing: {marker}", exit_code=2)

    state = parse_marker(marker)
    sha = state.get("gpt_sovits_sha", "")
    if sha != EXPECTED_GPT_SOVITS_SHA:
        fail(
            f"GPT-SoVITS SHA mismatch: marker={sha!r} script_pinned={EXPECTED_GPT_SOVITS_SHA!r}; "
            "run `voiceforge install-cloning --force`",
            exit_code=2,
        )

    repo_dir = Path(state.get("repo_path") or (home / "cloning/repo"))
    voices_dir = home / "voices"

    tts = None
    sys.stdout.write(json.dumps({"ok": True, "ready": True}) + "\n")
    sys.stdout.flush()

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
            text = req["text"]
            voice = req["voice"]
            out = Path(req["out"])
            out.parent.mkdir(parents=True, exist_ok=True)

            if tts is None:
                t0 = time.time()
                tts = load_tts(repo_dir)
                print(
                    json.dumps({"ok": True, "loaded_seconds": round(time.time() - t0, 2)}),
                    flush=True,
                )

            t0 = time.time()
            sr, duration = synth(tts, voices_dir, voice, text, out)
            sys.stdout.write(
                json.dumps(
                    {
                        "ok": True,
                        "sample_rate": sr,
                        "duration": duration,
                        "synth_seconds": round(time.time() - t0, 2),
                    }
                )
                + "\n"
            )
            sys.stdout.flush()
        except Exception as exc:  # noqa: BLE001 — per-request boundary
            sys.stdout.write(json.dumps({"ok": False, "error": str(exc)}) + "\n")
            sys.stdout.flush()


if __name__ == "__main__":
    main()
