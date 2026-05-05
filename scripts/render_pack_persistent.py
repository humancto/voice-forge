#!/usr/bin/env python3
"""Pre-render a voice pack with fish-speech S2 Pro — **persistent process** variant.

Same end result as `render_pack.py`, but loads the text2semantic model
+ DAC codec **once** at startup and runs all phrases in the same Python
process. Saves ~40 sec of model-load time per phrase.

For a 13-phrase pack on Mac CPU, the cumulative savings are ~9 minutes
(~2h00m → ~1h50m). Not transformative, but free, and gets larger the
more phrases per pack.

Run inside the fish-speech venv:

    ~/fish-experiment/venv/bin/python scripts/render_pack_persistent.py packs/peter/

Resumable: skips any phrase whose output WAV already exists. Crashes
mid-batch are safe — re-run picks up where it left off.

Compared to the shell-out `render_pack.py`:

| Aspect              | shell-out                | persistent (this script) |
|---------------------|--------------------------|--------------------------|
| Model load          | per phrase (~40s × N)    | once at startup          |
| DAC codec load      | per encode + per decode  | once                     |
| Reference encode    | once                     | once                     |
| Code lines          | minimal                  | larger                   |
| Upstream coupling   | none (CLI args only)     | imports fish_speech directly |
| Recommended for     | one-off / first-time use | batch rendering          |

Both are documented in docs/PACK_RENDERING.md.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path

# These imports require fish-speech installed in the active Python env.
# Run with the fish-speech venv's python; see the docstring above.
import numpy as np  # noqa: E402
import soundfile as sf  # noqa: E402
import torch  # noqa: E402
from loguru import logger  # noqa: E402

from fish_speech.models.text2semantic.inference import (  # noqa: E402
    decode_to_audio,
    encode_audio,
    generate_long,
    init_model,
    load_codec_model,
)


def render_pack(
    pack_dir: Path,
    fish_speech_dir: Path,
    device: str,
    seed: int,
) -> int:
    manifest_path = pack_dir / "phrases.json"
    if not manifest_path.is_file():
        print(f"error: no phrases.json at {manifest_path}", file=sys.stderr)
        return 2
    manifest = json.loads(manifest_path.read_text())

    schema = manifest.get("schema_version")
    if schema != 1:
        print(
            f"error: unsupported phrases.json schema_version: {schema!r} "
            f"(this script handles v1)",
            file=sys.stderr,
        )
        return 2

    phrases: dict[str, str] = manifest["phrases"]
    if not isinstance(phrases, dict):
        print(
            f"error: phrases must be an object {{event: text, ...}} in v1; got {type(phrases).__name__}",
            file=sys.stderr,
        )
        return 2

    ref_clip = (pack_dir / manifest["reference_clip"]).resolve()
    if not ref_clip.is_file():
        print(f"error: reference clip not found: {ref_clip}", file=sys.stderr)
        return 2

    voice_name = pack_dir.name
    prompt_text = manifest["reference_prompt_text"]
    wav_dir = pack_dir / "wav"
    wav_dir.mkdir(parents=True, exist_ok=True)
    work_dir = pack_dir / ".work-persistent"
    work_dir.mkdir(parents=True, exist_ok=True)

    # Skip phrases whose output already exists (resumability). Compute
    # this BEFORE loading the model — if everything is already done, no
    # need to pay the ~40s model-load cost.
    pending: list[tuple[str, str]] = []
    for event, text in phrases.items():
        if not event or "/" in event or event.startswith("."):
            print(f"[FAIL] invalid event id {event!r}", file=sys.stderr)
            continue
        if not text or not text.strip():
            print(f"[FAIL] empty text for {event!r}", file=sys.stderr)
            continue
        out_wav = wav_dir / f"{event}.wav"
        if out_wav.is_file():
            print(f"[skip]   {event}: {out_wav} already exists", flush=True)
            continue
        pending.append((event, text))

    print(f"pack:        {pack_dir}", flush=True)
    print(f"voice:       {voice_name}", flush=True)
    print(f"reference:   {ref_clip}", flush=True)
    print(f"phrases:     {len(phrases)} total, {len(pending)} pending", flush=True)
    print(f"device:      {device}", flush=True)
    print(f"fish-speech: {fish_speech_dir}", flush=True)
    print(file=sys.stderr)  # Whitespace before the model load logs.

    if not pending:
        print("nothing to render — pack is complete.", flush=True)
        return 0

    checkpoint_path = fish_speech_dir / "checkpoints/s2-pro"
    codec_checkpoint = checkpoint_path / "codec.pth"

    # CPU only (Mac); MPS path was broken at the time of writing. The
    # script accepts a --device flag for forward-compat with cuda boxes.
    if device == "cuda":
        precision = torch.bfloat16
    else:
        precision = torch.float32

    # === Load the heavy artifacts ONCE ===
    t0 = time.time()
    logger.info("loading text2semantic model ...")
    model, decode_one_token = init_model(
        checkpoint_path, device, precision, compile=False
    )
    with torch.device(device):
        model.setup_caches(
            max_batch_size=1,
            max_seq_len=model.config.max_seq_len,
            dtype=next(model.parameters()).dtype,
        )
    logger.info(f"text2semantic loaded in {time.time() - t0:.1f}s")

    t0 = time.time()
    logger.info("loading DAC codec ...")
    codec = load_codec_model(codec_checkpoint, device, precision)
    logger.info(f"DAC codec loaded in {time.time() - t0:.1f}s")

    # === Encode the reference once ===
    t0 = time.time()
    logger.info(f"encoding reference {ref_clip.name} ...")
    prompt_tokens = encode_audio(ref_clip, codec, device).cpu()
    logger.info(f"reference encoded in {time.time() - t0:.1f}s")

    torch.manual_seed(seed)

    # === Per-phrase loop ===
    failed: list[str] = []
    for event, text in pending:
        out_wav = wav_dir / f"{event}.wav"
        print(f"[render] {event}: {text!r}", flush=True)
        t_phrase = time.time()
        try:
            generator = generate_long(
                model=model,
                device=device,
                decode_one_token=decode_one_token,
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

            codes_chunks: list[torch.Tensor] = []
            for response in generator:
                if response.action == "sample" and response.codes is not None:
                    codes_chunks.append(response.codes)

            if not codes_chunks:
                raise RuntimeError("generator produced no sample codes")

            merged_codes = torch.cat(codes_chunks, dim=1)

            # DAC decode codes → audio in-process. ~1 sec.
            audio = decode_to_audio(merged_codes.to(device), codec)
            audio_np = audio.float().cpu().numpy()

            # Atomic write: tmp + rename. Same pattern as cloning_synth.py.
            tmp = out_wav.with_suffix(out_wav.suffix + ".tmp")
            sf.write(
                str(tmp),
                audio_np,
                int(codec.sample_rate),
                format="WAV",
                subtype="PCM_16",
            )
            tmp.replace(out_wav)

            print(f"[done]   {event}: {out_wav} ({time.time() - t_phrase:.1f}s)", flush=True)
        except Exception as e:  # noqa: BLE001
            print(f"[FAIL]   {event}: {e}", file=sys.stderr, flush=True)
            failed.append(event)

    print(file=sys.stderr)
    print(f"rendered: {len(pending) - len(failed)} / {len(pending)}", flush=True)
    if failed:
        print(f"failed:   {failed}", file=sys.stderr)
        return 1
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("pack_dir", type=Path, help="e.g. packs/peter/")
    ap.add_argument(
        "--fish-speech-dir",
        type=Path,
        default=Path.home() / "fish-experiment" / "fish-speech",
        help="path to the fish-speech repo with checkpoints/s2-pro/ inside",
    )
    ap.add_argument(
        "--device",
        default="cpu",
        choices=["cpu", "cuda", "mps"],
        help="inference device. mps is currently broken upstream; default is cpu",
    )
    ap.add_argument("--seed", type=int, default=42)
    args = ap.parse_args()

    # Make the fish-speech repo importable. Same pattern as
    # cloning_synth.py uses for GPT-SoVITS.
    fish_repo = args.fish_speech_dir.resolve()
    if str(fish_repo) not in sys.path:
        sys.path.insert(0, str(fish_repo))

    # On macOS, fish-speech's torchaudio path can require ffmpeg@6.
    # Inherit DYLD_FALLBACK_LIBRARY_PATH if the user has set it; don't
    # override.
    if sys.platform == "darwin" and "DYLD_FALLBACK_LIBRARY_PATH" not in os.environ:
        # Don't set it ourselves — let the user's env handle it. Just
        # warn if torchaudio import fails downstream.
        pass

    return render_pack(
        pack_dir=args.pack_dir.resolve(),
        fish_speech_dir=fish_repo,
        device=args.device,
        seed=args.seed,
    )


if __name__ == "__main__":
    sys.exit(main())
