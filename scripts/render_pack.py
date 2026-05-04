#!/usr/bin/env python3
"""Pre-render a voice pack with fish-speech S2 Pro.

Reads <pack_dir>/phrases.json, runs the 3-step fish-speech pipeline
for each phrase, saves output WAVs to <pack_dir>/wav/<event>.wav.

Resumable: skips any phrase whose output WAV already exists. Crashes
mid-batch are safe — re-run and it picks up where it left off.

Usage:
    render_pack.py <pack_dir> [--fish-speech-dir DIR] [--device cpu]

Expected layout:
    packs/<voice>/phrases.json          # manifest
    packs/<voice>/                      # output goes under wav/
    <fish-speech-dir>/checkpoints/s2-pro/  # S2 Pro weights

Pipeline per phrase:
    1. text2semantic: produces codes_<event>.npy
    2. DAC decode: codes_<event>.npy -> wav/<event>.wav
The reference audio's DAC encoding (fake.npy) is computed once and reused.
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import time
from pathlib import Path


def run(cmd: list[str], log_file: Path) -> None:
    """Run a subprocess, tee output to a log file, raise on nonzero exit."""
    with log_file.open("a") as log:
        log.write(f"\n$ {' '.join(cmd)}\n")
        log.flush()
        proc = subprocess.run(cmd, stdout=log, stderr=subprocess.STDOUT)
    if proc.returncode != 0:
        raise RuntimeError(
            f"command failed (exit {proc.returncode}): {' '.join(cmd)}\n"
            f"see log: {log_file}"
        )


def encode_reference(
    fish_dir: Path,
    venv_python: Path,
    ref_wav: Path,
    npy_out: Path,
    device: str,
    log_file: Path,
) -> None:
    """Step 0: DAC-encode the reference audio into VQ tokens (cached)."""
    if npy_out.is_file():
        print(f"  [skip] reference npy already at {npy_out}")
        return
    print(f"  [encode] {ref_wav.name} -> {npy_out.name} ...", flush=True)
    t0 = time.time()
    fake_wav = npy_out.with_suffix(".wav")
    run(
        [
            str(venv_python),
            str(fish_dir / "fish_speech/models/dac/inference.py"),
            "-i", str(ref_wav),
            "--checkpoint-path", str(fish_dir / "checkpoints/s2-pro/codec.pth"),
            "--output-path", str(fake_wav),
            "-d", device,
        ],
        log_file,
    )
    # The DAC inference script writes both fake.wav and fake.npy beside the
    # output; if --output-path was /tmp/foo.wav, the npy lives at /tmp/foo.npy.
    src_npy = fake_wav.with_suffix(".npy")
    if src_npy != npy_out and src_npy.is_file():
        shutil.move(str(src_npy), str(npy_out))
    print(f"  [encode] done in {time.time() - t0:.1f}s")


def render_phrase(
    fish_dir: Path,
    venv_python: Path,
    pack_dir: Path,
    ref_npy: Path,
    prompt_text: str,
    event: str,
    text: str,
    device: str,
    log_file: Path,
) -> None:
    """Run text2semantic + DAC decode for a single phrase. Resumable."""
    if not event or "/" in event or event.startswith("."):
        raise ValueError(
            f"invalid event id {event!r}: must be non-empty, no slashes, no leading dot"
        )
    if not text or not text.strip():
        raise ValueError(
            f"empty text for event {event!r}: fish-speech produces unpredictable output on empty prompts"
        )
    out_wav = pack_dir / "wav" / f"{event}.wav"
    out_wav.parent.mkdir(parents=True, exist_ok=True)
    if out_wav.is_file():
        print(f"[skip]   {event}: {out_wav} already exists")
        return

    work_dir = pack_dir / ".work" / event
    work_dir.mkdir(parents=True, exist_ok=True)
    codes_npy = work_dir / "codes_0.npy"

    print(f"[render] {event}: '{text}'")
    t0 = time.time()

    # Step 1: text2semantic
    if not codes_npy.is_file():
        run(
            [
                str(venv_python),
                str(fish_dir / "fish_speech/models/text2semantic/inference.py"),
                "--text", text,
                "--prompt-text", prompt_text,
                "--prompt-tokens", str(ref_npy),
                "--checkpoint-path", str(fish_dir / "checkpoints/s2-pro"),
                "--output-dir", str(work_dir),
                "--device", device,
                "--no-compile",
            ],
            log_file,
        )

    # Step 2: DAC decode
    run(
        [
            str(venv_python),
            str(fish_dir / "fish_speech/models/dac/inference.py"),
            "-i", str(codes_npy),
            "--checkpoint-path", str(fish_dir / "checkpoints/s2-pro/codec.pth"),
            "--output-path", str(out_wav),
            "-d", device,
        ],
        log_file,
    )

    print(f"[done]   {event}: {out_wav} ({time.time() - t0:.1f}s)")


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
        "--venv-python",
        type=Path,
        default=Path.home() / "fish-experiment" / "venv" / "bin" / "python",
        help="path to the fish-speech venv's python",
    )
    ap.add_argument("--device", default="cpu", choices=["cpu", "cuda", "mps"])
    args = ap.parse_args()

    pack_dir = args.pack_dir.resolve()
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

    # reference_clip is relative to the pack dir (portable across check-out
    # locations). Earlier we used repo-root-relative paths, which broke when
    # users cloned to different locations.
    ref_clip = (pack_dir / manifest["reference_clip"]).resolve()
    if not ref_clip.is_file():
        print(f"error: reference clip not found: {ref_clip}", file=sys.stderr)
        return 2

    # Voice name is implicit from the pack directory name (the rust-side
    # PackId == directory name); no need to duplicate it in the manifest.
    voice_name = pack_dir.name

    # phrases is a {event: text} map (v1 schema). Iteration order = insertion
    # order in Python 3.7+ for dicts, so the JSON authoring order is preserved.
    phrases: dict[str, str] = manifest["phrases"]
    if not isinstance(phrases, dict):
        print(
            f"error: phrases must be an object {{event: text, ...}} "
            f"in schema_version 1; got {type(phrases).__name__}",
            file=sys.stderr,
        )
        return 2

    log_file = pack_dir / "render.log"
    pack_dir.mkdir(parents=True, exist_ok=True)
    log_file.touch()

    print(f"pack:        {pack_dir}")
    print(f"voice:       {voice_name}")
    print(f"source:      {manifest.get('voice_source', '(unspecified)')}")
    print(f"reference:   {ref_clip}")
    print(f"phrases:     {len(phrases)}")
    print(f"device:      {args.device}")
    print(f"fish-speech: {args.fish_speech_dir}")
    print(f"log:         {log_file}")
    print()

    # Step 0: encode reference once (cached at <pack>/.work/reference.npy)
    work_root = pack_dir / ".work"
    work_root.mkdir(parents=True, exist_ok=True)
    ref_npy = work_root / "reference.npy"
    encode_reference(
        args.fish_speech_dir,
        args.venv_python,
        ref_clip,
        ref_npy,
        args.device,
        log_file,
    )

    # Step 1+2: per-phrase render, resumable
    failed = []
    for event, text in phrases.items():
        try:
            render_phrase(
                args.fish_speech_dir,
                args.venv_python,
                pack_dir,
                ref_npy,
                manifest["reference_prompt_text"],
                event,
                text,
                args.device,
                log_file,
            )
        except Exception as e:  # noqa: BLE001
            print(f"[FAIL]   {event}: {e}", file=sys.stderr)
            failed.append(event)

    print()
    print(f"rendered: {len(phrases) - len(failed)} / {len(phrases)}")
    if failed:
        print(f"failed:   {failed}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
