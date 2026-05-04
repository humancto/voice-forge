# Mac install — what works on which Mac

VoiceForge is built around two installation paths, both currently macOS-only. This doc covers what works on each Mac variant, what's broken and why, and the gotchas we hit so you don't have to.

> Linux/Windows ports are tracked as ROADMAP 1.1.2 (embedded TTS) and 2.1.1 (cloning runtime). The Rust CLI itself builds and tests cleanly on Linux today; only the cloning runtime is macOS-arm64-locked for v1.

## TL;DR

| Mac                                              | Basic CLI (say / run / doctor)                 | Voice cloning                                                                       |
| ------------------------------------------------ | ---------------------------------------------- | ----------------------------------------------------------------------------------- |
| **Apple Silicon** (M1, M2, M3, M4) — recommended | ✅ full support                                | ✅ full support                                                                     |
| **Intel macOS**                                  | ✅ works                                       | ❌ blocked on torch wheels (see below)                                              |
| **macOS 13+ (Ventura) on arm64**                 | ✅                                             | ✅                                                                                  |
| **macOS 12 (Monterey) on arm64**                 | ✅                                             | ⚠️ Homebrew formulae for `python@3.11` and `ffmpeg@6` may need a manual brew update |
| **macOS 11 (Big Sur)**                           | ⚠️ Rust stdlib still supports it; brew may not | ❌                                                                                  |

## Apple Silicon (the canonical path)

Everything works. The README's quick start is what you want:

```bash
curl -fsSL https://raw.githubusercontent.com/humancto/voice-forge/main/install.sh | bash
voiceforge install-cloning
voiceforge clone peter ./peter_griffin_60s.wav
voiceforge say --voice peter --text "Holy crap, the build is on fire."
```

### Apple Silicon gotcha #1 — Intel Homebrew on an arm64 machine

If your Homebrew lives at `/usr/local/bin/brew` (Intel side), `voiceforge install-cloning` will refuse with a clear error pointing at the arm64 install command. This is correct. Run:

```bash
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
```

The arm64 Homebrew installs to `/opt/homebrew` and **coexists with your Intel Homebrew at /usr/local** — nothing gets clobbered. You can have both. We never modify your `$PATH` to put `/opt/homebrew/bin` first; the cloning runtime calls `/opt/homebrew/bin/brew` and `/opt/homebrew/bin/python3.11` by full path so your shell behavior stays exactly the same.

### Apple Silicon gotcha #2 — `ffmpeg@6`, not `ffmpeg`

The system-wide `ffmpeg` is fine for the basic CLI's `voiceforge ingest` and `voiceforge run` paths. But **GPT-SoVITS' torchcodec wants `libavutil.58`** which is ffmpeg 6 — newer ffmpeg (7+) ships `libavutil.60` and torchcodec fails to load. `install-cloning` installs `ffmpeg@6` as a keg-only formula at `/opt/homebrew/opt/ffmpeg@6` and sets `DYLD_FALLBACK_LIBRARY_PATH` per cloning subprocess. Your `ffmpeg` (7 or 8) on `$PATH` is never touched.

### Apple Silicon gotcha #3 — `flock` doesn't ship with macOS

We use `mkdir`-based locking in `clone_voice.sh` for portability. No fix needed; this is just so you understand if you're forking the script.

## Intel macOS

The basic CLI works:

```bash
curl -fsSL https://raw.githubusercontent.com/humancto/voice-forge/main/install.sh | bash
voiceforge say --text "VoiceForge is ready"        # uses macOS `say`
voiceforge run -- npm test                          # speaks success/failure
voiceforge doctor                                   # 9-check health
```

**Voice cloning currently does not work on Intel macOS.** Three compounding reasons:

1. **PyTorch dropped Intel-mac wheels at torch 2.2.2.** Every newer torch release is Apple Silicon (arm64) + Linux + Windows only. The Intel-mac wheel is frozen at 2.2.2 (early 2024).

2. **torch 2.2.2 was built against numpy < 2.0.** GPT-SoVITS pulls numba which pulls numpy 2.x; the resulting ABI mismatch breaks torch on import.

3. **No good downgrade path.** Pinning numpy < 2 cascades into version conflicts with librosa, transformers, etc.

We hit all three of these manually and went through every workaround. The honest conclusion: **stable voice cloning on Intel mac requires either** a torch fork that maintains Intel wheels (none exists), **or** swapping the cloning backend to one that doesn't depend on torch (a real architectural change).

If you're on Intel macOS and want voice cloning today, your options are:

- **Borrow an arm64 machine** for a few minutes to clone the voice (`voiceforge clone <name> <source>`). The output `~/.voiceforge/voices/<name>/` is just files — `rsync` / `scp` it back to your Intel mac.
  - Then on Intel: `voiceforge say` against that voice will fail (no cloning runtime), but the voice profile is portable. You can use it for analytics / inspection / handoff.
- **Use the OS-native fallback** (`voiceforge say --voice default`). It's not a clone — it's the macOS `say` command with our cache layer — but it works on Intel.
- **Wait for ROADMAP 2.1.1 / 2.1.2** which scopes a CPU-only cloning backend (CTranslate2-based or onnxruntime-only). When it ships, Intel mac gets the cloning path back.

If you've got skin in the game on Intel, comment on https://github.com/humancto/voice-forge/issues — we'll prioritize the CPU-only backend if there's demand.

## Older macOS versions

The Rust CLI is built against the same toolchain as Cargo's own MSRV — works back to macOS 11 (Big Sur) at least. The cloning runtime's harder constraints are:

|                                         | min macOS             | reason                                                             |
| --------------------------------------- | --------------------- | ------------------------------------------------------------------ |
| `voiceforge` binary                     | 11 (Big Sur)          | Rust stdlib + `rodio` audio output                                 |
| `voiceforge install` (Homebrew install) | 12 (Monterey)         | Homebrew dropped Big Sur in late 2024                              |
| `voiceforge install-cloning`            | 13 (Ventura) on arm64 | `ffmpeg@6` formula + Python 3.11 arm64 are both well-tested on 13+ |

If you're on macOS 12, the cloning install **probably** works but we haven't verified. File an issue with your `voiceforge doctor` output if you hit gotchas.

If you're on macOS 11 or older, the basic CLI works; `install-cloning` likely fails on `brew install ffmpeg@6` (Big Sur formulae aren't maintained). Stick to the OS-native fallback or upgrade.

## Verifying your Mac before you start

```bash
# 1. Confirm Apple Silicon
uname -m
# expected: arm64

# 2. Confirm macOS version
sw_vers -productVersion
# 13+ recommended

# 3. Confirm arm64 Homebrew if you want cloning
test -x /opt/homebrew/bin/brew && echo "arm64 brew: yes" || echo "arm64 brew: missing — install it"

# 4. Disk space (cloning needs ~4 GB free)
df -h ~ | awk 'NR==2 {print "free at $HOME: " $4}'

# 5. Confirm Rust
cargo --version
# expected: cargo 1.7x.x or newer
```

If all five pass, the canonical install path will work end-to-end.

## When something does break

```bash
voiceforge doctor                         # 9 checks; copy the output into the issue
voiceforge install-cloning --check        # cloning-specific health
ls -la ~/.voiceforge/cloning/install.log  # tail of the last install attempt
```

File an issue at https://github.com/humancto/voice-forge/issues with all three. We'll either fix the gotcha or — if it's ROADMAP 2.1.1+ shaped — add it to the priority list.
