#!/usr/bin/env python3
"""Long-lived NDJSON synth worker for fish-speech S2 Pro.

Mirrors `scripts/cloning_synth.py` (the GPT-SoVITS v1 worker) so the
Rust client at `apps/voiceforge-cli/src/tts.rs::CloningEngine` can be
adapted for the v2 path with minimal surgery. Loaded once per
voiceforge process via the FishEngine (PR-AB step 8); reads request
lines from stdin, emits response lines on stdout.

Protocol:
  Request:  {"text": "...", "voice": "peter", "out": "/tmp/.../out.wav"}
  Response: {"ok": true, "sample_rate": 44100, "duration": 6.13}
         or {"ok": false, "error": "..."}

  Startup ready signal (emitted before any request is read):
            {"ok": true, "ready": true}

  Model-loaded signal (emitted after first request triggers load):
            {"ok": true, "loaded_seconds": 42.1}

Schema-2 marker (INSTALLED.toml) is validated up front. A mismatch
between the marker's `fish_speech_sha` and the SHA pinned here aborts
startup with exit code 2 so the Rust client surfaces a re-install
hint rather than a confusing inference failure.

Voice reference layout (PR-AB step 8 / PR-C will formalize the
recipe file at voices/<name>/recipe.json; for now we mirror the v1
ref_main.wav + ref_main.txt convention so step 7 ships independently):

  ~/.voiceforge/voices/<name>/ref.wav        # 8-30s reference clip
  ~/.voiceforge/voices/<name>/ref.txt        # transcript of ref.wav

The first request for a given voice encodes its reference into prompt
tokens (one-shot ~1-2s cost via the DAC codec). Subsequent requests
for the same voice reuse the cached tokens.

ENV VARS:
  VOICEFORGE_HOME            installation root (default ~/.voiceforge)
  VOICEFORGE_FISH_SYNTH_DEVICE  cpu | cuda | mps (default cpu)
  VOICEFORGE_FISH_SYNTH_SEED    int (default 0) — per-request seed for
                                bit-stable resume across re-runs

This file is shipped to disk as `<repo>/scripts/fish_speech_synth.py`
AND can be embedded via include_str! once the binary-release path
needs to extract it (mirror what embedded_install.rs does for the
bash installer; PR-AB step 8 will wire that side up).
"""

import json
import os
import sys
import time
from pathlib import Path

# Pinned SHA of the fish-speech repo we know works. MUST match the
# `fish_speech_sha` field in INSTALLED.toml schema_version=2. Bump in
# lockstep with scripts/install_cloning_fish.sh::FISH_SPEECH_SHA.
EXPECTED_FISH_SPEECH_SHA = "3dd1f85c402ee6f0a17c2971d3b0dd8d881ca139"

# fish-speech (like torch + transformers) chatters to stdout on import
# (model load progress, deprecation warnings, etc.). Our NDJSON parser
# on the Rust side would gag on that. Stash the real stdout BEFORE any
# imports and redirect sys.stdout to stderr so framework output stays
# visible to humans but never poisons the protocol stream.
_PROTOCOL_STDOUT = sys.stdout
sys.stdout = sys.stderr


def emit(obj: dict) -> None:
    """Send one NDJSON response line. Always flushed — the Rust side
    reads with `BufReader::lines()` and any lingering buffer means the
    client appears to hang."""
    _PROTOCOL_STDOUT.write(json.dumps(obj) + "\n")
    _PROTOCOL_STDOUT.flush()


def fail(error: str, exit_code: int = 1) -> None:
    emit({"ok": False, "error": error})
    sys.exit(exit_code)


def parse_marker(path: Path) -> dict:
    """Tiny TOML parser sufficient for our flat marker. Avoids pulling
    `tomllib` (3.11+) so the script stays portable down to 3.10.

    Risk R3 fix (rust-expert review pass 1): stop at the first `[table]`
    line. Previously we kept reading and table keys silently clobbered
    top-level keys with the same name (e.g. a future top-level
    `engine = "..."` plus a `[model_sha256] engine = "..."` would let
    the table value win). We only consume top-level fields, so bailing
    on first `[` is the correct + minimal defense.
    """
    out: dict[str, str] = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if line.startswith("["):
            break  # R3: stop on first table; we read top-level only
        if not line or line.startswith("#"):
            continue
        if "=" not in line:
            continue
        k, v = line.split("=", 1)
        out[k.strip()] = v.strip().strip('"')
    return out


# ----------------------------------------------------------------------------
# Model loading + synth
# ----------------------------------------------------------------------------


def load_fish(repo_dir: Path, checkpoint_path: Path, device: str):
    """Load the text2semantic model + DAC codec ONCE per process. Heavy
    (~30-90s on CPU for first load). Returns a dict of handles that
    synth() consumes."""
    sys.path.insert(0, str(repo_dir))
    os.chdir(repo_dir)

    import torch

    # Imports gated on the env so this file can be import-tested without
    # a real fish-speech install — see tests/fish_speech_synth_script.rs.
    # All five functions live in text2semantic.inference at fish-speech
    # SHA 3dd1f85c (the pinned commit). The dac.inference module exists
    # but exports a CLI-only `main` — NOT the synth helpers. Splitting
    # the imports across both modules was the first-real-install bug
    # caught on 2026-05-12; this consolidated import is what mirrors
    # the working `scripts/render_pack_persistent.py`.
    from fish_speech.models.text2semantic.inference import (  # noqa: E402
        decode_to_audio,
        encode_audio,
        generate_long,
        init_model,
        load_codec_model,
    )

    if device == "cuda":
        precision = torch.bfloat16
    else:
        precision = torch.float32

    codec_checkpoint = checkpoint_path / "codec.pth"

    model, decode_one_token = init_model(
        checkpoint_path, device, precision, compile=False
    )
    with torch.device(device):
        model.setup_caches(
            max_batch_size=1,
            max_seq_len=model.config.max_seq_len,
            dtype=next(model.parameters()).dtype,
        )

    codec = load_codec_model(codec_checkpoint, device, precision)

    return {
        "torch": torch,
        "model": model,
        "decode_one_token": decode_one_token,
        "codec": codec,
        "device": device,
        "precision": precision,
        "encode_audio": encode_audio,
        "generate_long": generate_long,
        "decode_to_audio": decode_to_audio,
    }


def encode_reference(handles: dict, ref_wav: Path):
    """Encode a reference clip to prompt tokens. Result is cached by
    the main() loop per voice name."""
    tokens = handles["encode_audio"](ref_wav, handles["codec"], handles["device"])
    return tokens.cpu()


def synth(handles: dict, prompt_text: str, prompt_tokens, text: str, out_path: Path, seed: int):
    """Single-text synth. prompt_tokens is the pre-encoded reference
    tokens for the chosen voice. Writes WAV atomically (`.tmp` +
    rename) so a partial write never leaves a malformed file behind."""
    torch = handles["torch"]
    torch.manual_seed(seed)
    if handles["device"] == "cuda":
        torch.cuda.manual_seed(seed)

    generator = handles["generate_long"](
        model=handles["model"],
        device=handles["device"],
        decode_one_token=handles["decode_one_token"],
        text=text,
        num_samples=1,
        max_new_tokens=0,
        top_p=0.9,
        top_k=30,
        temperature=1.0,
        compile=False,
        iterative_prompt=True,
        chunk_length=300,
        prompt_text=[prompt_text],
        prompt_tokens=[prompt_tokens],
    )

    codes_chunks = []
    for response in generator:
        if response.action == "sample" and response.codes is not None:
            codes_chunks.append(response.codes)
        # action == "next" delimits samples; num_samples=1 means we
        # never see > 1 sample so ignoring "next" is safe.

    if not codes_chunks:
        raise RuntimeError("generator produced no sample codes")

    merged_codes = torch.cat(codes_chunks, dim=1)
    audio = handles["decode_to_audio"](merged_codes.to(handles["device"]), handles["codec"])
    audio_np = audio.float().cpu().numpy()

    # DAC codec default sample rate is 44_100 Hz on the s2-pro
    # checkpoints we ship. If a future checkpoint changes this, the
    # WAV header will be wrong; protect by reading the codec's
    # `sample_rate` attribute when present.
    sr = getattr(handles["codec"], "sample_rate", 44_100)

    import soundfile as sf

    tmp = out_path.with_suffix(out_path.suffix + ".tmp")
    sf.write(str(tmp), audio_np, sr, format="WAV", subtype="PCM_16")
    tmp.replace(out_path)

    duration = float(len(audio_np) / sr)
    return int(sr), duration


# ----------------------------------------------------------------------------
# main loop
# ----------------------------------------------------------------------------


def main() -> int:
    home_env = os.environ.get("VOICEFORGE_HOME")
    home = Path(home_env) if home_env else Path.home() / ".voiceforge"
    device = os.environ.get("VOICEFORGE_FISH_SYNTH_DEVICE", "cpu")
    seed = int(os.environ.get("VOICEFORGE_FISH_SYNTH_SEED", "0"))

    marker = home / "cloning/INSTALLED.toml"
    if not marker.is_file():
        fail(f"cloning marker missing: {marker}", exit_code=2)

    state = parse_marker(marker)
    schema = state.get("schema_version", "")
    if schema != "2":
        fail(
            f"INSTALLED.toml schema_version={schema!r} but fish_speech_synth.py "
            "requires schema_version=2 (fish-speech). Run "
            "`voiceforge install-cloning --force` to upgrade.",
            exit_code=2,
        )

    sha = state.get("fish_speech_sha", "")
    if sha != EXPECTED_FISH_SPEECH_SHA:
        fail(
            f"fish-speech SHA mismatch: marker={sha!r} "
            f"script_pinned={EXPECTED_FISH_SPEECH_SHA!r}; run "
            "`voiceforge install-cloning --force`",
            exit_code=2,
        )

    repo_dir = Path(state.get("repo_path") or (home / "cloning/repo"))
    checkpoint_path = Path(state.get("checkpoint_path") or (repo_dir / "checkpoints/s2-pro"))
    voices_dir = home / "voices"

    handles: dict | None = None
    prompt_cache: dict[str, tuple[str, object]] = {}

    emit({"ok": True, "ready": True})

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
            text = req["text"]
            out = Path(req["out"])
            out.parent.mkdir(parents=True, exist_ok=True)

            if handles is None:
                t0 = time.time()
                handles = load_fish(repo_dir, checkpoint_path, device)
                emit({"ok": True, "loaded_seconds": round(time.time() - t0, 2)})

            # PR-AB step 6d-2: explicit-ref path takes precedence over
            # the voice-lookup path. The post-install smoke synth uses
            # this so it doesn't have to register a v1-schema voice
            # profile that wouldn't pass voices::load_voice.
            explicit_ref_wav = req.get("explicit_ref_wav")
            explicit_ref_txt = req.get("explicit_ref_txt")
            if explicit_ref_wav is not None:
                ref_wav_path = Path(explicit_ref_wav)
                if not ref_wav_path.is_file():
                    raise RuntimeError(
                        f"explicit_ref_wav not found at {ref_wav_path}"
                    )
                if explicit_ref_txt is None:
                    raise RuntimeError(
                        "explicit_ref_wav was set but explicit_ref_txt was missing"
                    )
                # Cache by the absolute path string — the Rust client
                # canonicalizes before sending, so identical refs hit
                # the same cache slot here too.
                cache_key = str(ref_wav_path)
                if cache_key not in prompt_cache:
                    prompt_tokens = encode_reference(handles, ref_wav_path)
                    prompt_cache[cache_key] = (explicit_ref_txt, prompt_tokens)
                prompt_text, prompt_tokens = prompt_cache[cache_key]
            else:
                voice = req["voice"]
                if voice not in prompt_cache:
                    voice_dir = voices_dir / voice
                    ref_wav = voice_dir / "ref.wav"
                    ref_txt = voice_dir / "ref.txt"
                    if not ref_wav.is_file():
                        raise RuntimeError(
                            f"voice {voice!r} ref.wav not found at {ref_wav}"
                        )
                    if not ref_txt.is_file():
                        raise RuntimeError(
                            f"voice {voice!r} ref.txt not found at {ref_txt}"
                        )
                    prompt_text = ref_txt.read_text().strip()
                    prompt_tokens = encode_reference(handles, ref_wav)
                    prompt_cache[voice] = (prompt_text, prompt_tokens)
                prompt_text, prompt_tokens = prompt_cache[voice]

            t0 = time.time()
            sr, duration = synth(handles, prompt_text, prompt_tokens, text, out, seed)
            emit(
                {
                    "ok": True,
                    "sample_rate": sr,
                    "duration": duration,
                    "synth_seconds": round(time.time() - t0, 2),
                }
            )
        except Exception as exc:  # noqa: BLE001 — per-request boundary
            emit({"ok": False, "error": str(exc)})

    return 0


if __name__ == "__main__":
    sys.exit(main())
