# Pre-rendering voice packs with fish-speech S2 Pro

**TL;DR — clone any character voice on a laptop, locally, in under an hour, no GPU needed.**

This guide reproduces the exact pipeline VoiceForge uses to pre-render voice packs (e.g. the Peter Griffin pack). You only need:

- A Mac (M-series tested) or Linux box with ≥16 GB RAM
- ~12 GB free disk
- ~1 hour for first phrase, ~10 min/phrase after

The output: a folder of WAV files keyed by event name (`build_failed.wav`, `tests_passed.wav`, etc.) that VoiceForge can play instantly at runtime.

---

## Why pre-render?

Realtime voice cloning has tradeoffs:

|                       | Realtime (GPT-SoVITS v2) | Pre-rendered (fish-speech S2 Pro) |
| --------------------- | ------------------------ | --------------------------------- |
| Speaks arbitrary text | yes                      | only pre-rendered phrases         |
| Latency at runtime    | ~2 sec                   | ~50 ms (just plays a WAV)         |
| Quality on cartoon    | mediocre (echoey)        | **genuinely recognizable**        |
| Quality on real human | good                     | excellent                         |
| Render-once cost      | n/a                      | ~10 min/phrase on Mac CPU         |

For terminal feedback, **the set of phrases is small and fixed** ("build failed", "tests passed", etc.). Pre-rendering costs you a one-time hour, and you get instant playback at runtime forever after.

---

## Prerequisites

```bash
# macOS
brew install ffmpeg python@3.11 portaudio
xcode-select --install   # for whisper compile

# Linux
sudo apt install ffmpeg python3.11 python3.11-venv portaudio19-dev
```

You will also want `yt-dlp` if your reference audio comes from YouTube:

```bash
brew install yt-dlp        # macOS
pip install yt-dlp         # cross-platform
```

---

## Step 1 — Get a clean reference clip

This is **the most important step**. Bad reference = bad clone, no matter the model. Aim for:

| Quality bar         | Description                                                                                    |
| ------------------- | ---------------------------------------------------------------------------------------------- |
| **Single speaker**  | No other character in the audio. No laughter. No music bed.                                    |
| **Continuous**      | One scene, not a compilation. Cuts produce artifacts at boundaries.                            |
| **Length ≥ 30 sec** | 60 seconds is ideal. Less than 30 produces unstable timbre.                                    |
| **Dry acoustic**    | Close-mic, no obvious reverb. Avoid commercial-style "vocal booth" effects.                    |
| **Recognizable**    | The clip itself should sound like the character. If the source sounds off, the clone will too. |

### Verify with whisper before committing

YouTube titles lie. Always whisper-transcribe and read the transcript before using a clip:

```bash
yt-dlp -f "bestaudio" -x --audio-format wav -o "ref.%(ext)s" "<youtube-url>"
whisper ref.wav --model base --language en --output_format txt --fp16 False
cat ref.txt
```

If the transcript reads like the character (or, in the case of a real human, sounds like things they would actually say) — good. If it's `"this is my real voice, douche castle"` — bad (we caught this exact troll on a Peter clip).

### Trim and convert to fish-speech-friendly format

```bash
ffmpeg -i ref.wav -ss 0.0 -to 64.0 \
  -af "loudnorm=I=-16:TP=-1.5:LRA=11" \
  -ar 32000 -ac 1 \
  reference_32k.wav
```

`-ar 32000` is critical — fish-speech S2 Pro is trained at 32 kHz. Anything else gets internally resampled and produces phasing artifacts.

---

## Step 2 — Install fish-speech S2 Pro

```bash
mkdir -p ~/fish-experiment && cd ~/fish-experiment
git clone --depth 1 https://github.com/fishaudio/fish-speech.git
python3.11 -m venv venv
source venv/bin/activate
pip install --upgrade pip wheel
cd fish-speech
pip install -e ".[cpu]"
```

This installs ~3 GB of Python deps (PyTorch 2.8.0, transformers, librosa, etc.). Takes 5–10 min on a fast connection.

### Download S2 Pro weights

```bash
cd ~/fish-experiment/fish-speech
hf download fishaudio/s2-pro --local-dir checkpoints/s2-pro
```

~10 GB of model weights. Coffee time — 5 to 15 min depending on bandwidth.

> **License note**: fish-speech and S2 Pro are released under the **Fish Audio Research License** (see `~/fish-experiment/fish-speech/LICENSE`). Read it. It's research-friendly but has commercial restrictions. For pre-rendering character voice packs as an open-source community resource, the educational/research framing applies.

---

## Step 3 — Get the prompt text

fish-speech S2 Pro needs to know **what the speaker says** in your reference clip. The prompt text must match the reference audio word-for-word — minor whisper transcription errors are tolerated, but big gaps break things.

```bash
whisper reference_32k.wav --model base --language en --output_format txt --fp16 False
cat reference_32k.txt
```

Save this text. You'll feed it to the model below.

---

## Step 4 — Run the 3-step pipeline (one phrase)

```bash
cd ~/fish-experiment/fish-speech
source ~/fish-experiment/venv/bin/activate

# Step A: DAC-encode the reference into VQ tokens. Done once per voice.
python fish_speech/models/dac/inference.py \
  -i reference_32k.wav \
  --checkpoint-path checkpoints/s2-pro/codec.pth \
  --output-path reference.wav \
  -d cpu
# Produces reference.npy beside reference.wav. ~20 sec.

# Step B: text2semantic — generate semantic tokens for the target text.
python fish_speech/models/text2semantic/inference.py \
  --text "Holy crap Lois, the build is on fire." \
  --prompt-text "<the whisper transcript of reference_32k.wav>" \
  --prompt-tokens reference.npy \
  --checkpoint-path checkpoints/s2-pro \
  --output-dir out/ \
  --device cpu \
  --no-compile
# ~10 minutes per sentence on Mac CPU. Produces out/codes_0.npy.

# Step C: DAC-decode the semantic tokens into audio.
python fish_speech/models/dac/inference.py \
  -i out/codes_0.npy \
  --checkpoint-path checkpoints/s2-pro/codec.pth \
  --output-path peter_says.wav \
  -d cpu
# ~1 sec. Produces peter_says.wav at 44.1 kHz.

afplay peter_says.wav   # macOS
```

**You now have one sentence in your character's voice.** Repeat steps B + C for additional phrases (step A is one-time per reference).

---

## Step 5 — Batch render a whole pack

VoiceForge ships a batch renderer that handles all of the above with **resumability** (crashes are safe — re-run picks up where it left off):

```bash
git clone https://github.com/humancto/voice-forge.git
cd voice-forge
```

Create `packs/<voicename>/phrases.json`:

```json
{
  "schema_version": 1,
  "voice_source": "Peter Griffin (Family Guy)",
  "reference_clip": "../../tests/fixtures/peter_nike_clean_32k.wav",
  "reference_prompt_text": "<the whisper transcript of the reference clip>",
  "phrases": {
    "build_success": "Holy crap Lois, the build passed. Sweet.",
    "build_failed": "Holy crap Lois, the build is on fire. Somebody call the fire department.",
    "tests_passed": "Hey Lois, the tests just passed. Get me a sandwich to celebrate."
  }
}
```

Schema notes (v1):

- `schema_version: 1` is required. The renderer rejects unknown schemas.
- The voice name is implicit — it's the directory name (`packs/peter/` ⇒ voice `peter`).
- `reference_clip` is **relative to the pack directory**, not the repo root. From `packs/peter/`, the path `../../tests/fixtures/...` reaches the repo's fixtures.
- `phrases` is a `{event: text}` object so events are unique by construction and lookup at runtime is O(1).

Then:

```bash
scripts/render_pack.py packs/peter/
```

Output lands at `packs/peter/wav/<event>.wav`. The script is idempotent — phrases whose WAV already exists are skipped.

For 13 phrases, expect ~2.5 hours of CPU time. Run it overnight. You can `tail -f packs/peter/render.log` to follow progress.

### Faster: persistent-process renderer

`render_pack.py` shells out to fish-speech's CLI scripts per phrase, paying the ~40-second model-load cost each time. For batch rendering (3+ phrases), use `render_pack_persistent.py` instead — it imports `fish_speech` directly, loads the text2semantic model + DAC codec **once**, encodes the reference **once**, and loops over all phrases in the same Python process.

```bash
~/fish-experiment/venv/bin/python scripts/render_pack_persistent.py packs/peter/
```

Saves ~9 minutes on a 13-phrase pack (~2h00m → ~1h50m on Mac CPU). The savings scale with phrase count. **Same `phrases.json` schema, same output layout, same resumability** as `render_pack.py` — they're interchangeable. Use whichever is easier to reach for; persistent is the default for serious renders. Both are documented as first-class paths.

---

## Step 6 — Quality control

Listen to every WAV before publishing. Common failure modes:

| Symptom                                   | Likely cause                                      | Fix                                                         |
| ----------------------------------------- | ------------------------------------------------- | ----------------------------------------------------------- |
| "From a well" / echoey                    | Reference audio at 22 kHz, or has reverb baked in | Re-trim reference at 32 kHz; pick a dryer scene             |
| Clone speaks in a different voice partway | Reference contained 2+ speakers                   | Hand-cut reference to single-speaker only                   |
| Robotic / clipped                         | top_k too low, speed_factor != 1.0                | Use the default sampling params (already set in our script) |
| Output too fast or too slow               | Model interpreting prompt_text drift              | Re-whisper the reference, fix typos in prompt_text          |
| Word missing or doubled                   | Sampling variance — happens occasionally          | Re-render that single phrase (different seed)               |

If a single phrase consistently fails, try: shorter phrase, simpler punctuation, or break into two sentences.

---

## Pack format (for distribution)

A pack ships as:

```
packs/<voicename>/
├── manifest.toml          # name, license, attribution, source URL
├── phrases.json           # the manifest used to render
├── reference.wav          # the trimmed source clip — included so others can re-clone locally
├── wav/
│   ├── build_success.wav
│   ├── build_failed.wav
│   └── ...
└── checksums.txt          # sha256 per file
```

`manifest.toml`:

```toml
name = "peter"
display_name = "Peter Griffin"
license = "Educational / research / local-testing use only"
source_url = "https://www.youtube.com/watch?v=T2w5SQ0L65I"
source_description = "Family Guy Nike commercial parody, S5"
rendered_with = "fishaudio/fish-speech S2 Pro"
rendered_at = "2026-05-04"
sample_rate = 44100
phrases = 13
```

---

## Reproducing the Peter pack

If you just want to reproduce what we have, the inputs are versioned:

```bash
git clone https://github.com/humancto/voice-forge.git
cd voice-forge

# Reference clip is gitignored (large + license-sensitive). Download:
yt-dlp -f "bestaudio" -x --audio-format wav \
  -o "tests/fixtures/peter_nike_raw.%(ext)s" \
  "https://www.youtube.com/watch?v=T2w5SQ0L65I"

ffmpeg -i tests/fixtures/peter_nike_raw.wav -ss 0.0 -to 64.0 \
  -af "loudnorm=I=-16:TP=-1.5:LRA=11" \
  -ar 32000 -ac 1 \
  tests/fixtures/peter_nike_clean_32k.wav

# Render the pack
scripts/render_pack.py packs/peter/
```

`packs/peter/wav/` should match the reference pack within 1–2% of acoustic similarity (sampling has variance run-to-run).

---

## Hardware notes

| Box                             | Step B time per phrase      | Step C time | Total for 13 phrases |
| ------------------------------- | --------------------------- | ----------- | -------------------- |
| **M-series Mac, CPU**           | ~10 min                     | ~1 sec      | ~2.5 hours           |
| **Mac MPS** _(experimental)_    | not yet supported by S2 Pro |             |                      |
| **NVIDIA A10 / 4090, FP16**     | ~5–15 sec                   | ~1 sec      | ~5 minutes           |
| **NVIDIA A100, BF16 + compile** | ~3 sec                      | ~1 sec      | ~1 minute            |

For renderings beyond 1–2 packs, renting a GPU box for an hour ($0.30–1.00 on RunPod / Lambda Labs) is cheaper than a single afternoon of laptop CPU.

---

## Ethics + legal

This pipeline can clone any voice. **Just because you can doesn't mean you should.**

- **Your own voice / friends with consent**: knock yourself out.
- **Public figures (politicians, celebrities)**: educational/research/local-testing use is the framing community projects use; do not distribute commercial product impersonating them.
- **Anyone, ever, doing or saying things they didn't actually do or say in a way meant to deceive**: don't.

VoiceForge does not host pre-rendered packs of celebrity voices in its main repository. Community-contributed packs live in a separate, takedown-friendly index.

---

## Troubleshooting

**`AttributeError: module 'torch.mps' has no attribute 'current_device'`**
You're on Mac and the script tried to auto-pick MPS. Force CPU with `-d cpu` (or `--device cpu` for text2semantic).

**`expected model file missing: gsv-v2final-pretrained/...`**
This is a GPT-SoVITS install error, not fish-speech. Different pipeline. See the main install troubleshooting docs.

**`prompt_text` lengths don't match what's said in the reference**
Re-run whisper with a larger model (`--model small` or `medium`). Trim the prompt_text to exactly what's audible in the trimmed reference.

**Output is "Holy crap Lois the build is on fire" but said by someone who isn't Peter**
Either your reference is too short (<10 sec), or the reference contained multiple speakers.

---

## Credits

- [fishaudio/fish-speech](https://github.com/fishaudio/fish-speech) — S2 Pro model and inference scripts.
- [openai/whisper](https://github.com/openai/whisper) — reference transcription.
- [yt-dlp/yt-dlp](https://github.com/yt-dlp/yt-dlp) — source audio acquisition.

If this guide saved you a weekend of trial and error: 🌟 the [voice-forge repo](https://github.com/humancto/voice-forge) and post your packs.
