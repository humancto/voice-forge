# Plan: install.sh — one-curl onboarding (ROADMAP 1.5)

## Goal

```bash
curl -fsSL https://raw.githubusercontent.com/humancto/voice-forge/main/install.sh | bash
```

That single command leaves the user with a working `voiceforge` binary
on their `PATH`, a populated `~/.voiceforge/`, and a friendly `voiceforge
doctor`-like summary at the end.

## Out of scope (explicit)

- **Pre-built binary downloads.** ROADMAP 1.7 ships GitHub-release
  automation; until then install.sh **builds from source**. That keeps
  the script honest: when 1.7 lands we add a binary-fetch path with a
  source-build fallback, and never break the URL contract.
- **Voice cloning install.** Coqui-TTS / torch are multi-GB and need
  Python 3.11. They go behind `voiceforge install-cloning` (item 2.1),
  not this script.
- **`voiceforge doctor`** is item 1.6 — install.sh does an inline
  smoke-test instead until then.

## What the script does, in order

1. Strict mode: `set -euo pipefail`, `IFS=$'\n\t'`, `LC_ALL=C`.
2. Detect `OS` (`darwin` or `linux`) and `ARCH` (`arm64`/`x86_64`).
   Exit cleanly on Windows / FreeBSD with a "not yet supported, see
   ROADMAP 1.5+" message.
3. Check prereqs: `git`, `cargo`, `ffmpeg`. For each missing one print
   the right `brew` / `apt` install command for the detected OS, then
   exit 1.
4. Pick `INSTALL_DIR`:
   - `/usr/local/bin` if writable
   - else `~/.local/bin` (creating it if needed)
   - else `~/bin` as a final fallback
     Print which one was picked and warn if the chosen dir isn't on
     `$PATH`.
5. Pick a working dir under `~/.cache/voiceforge/checkout`. Reuse if it
   exists; otherwise `git clone https://github.com/humancto/voice-forge`.
   Always `git fetch && git checkout origin/main`.
6. `cargo build --release --manifest-path apps/voiceforge-cli/Cargo.toml`
   with progress visible (no `--quiet`).
7. Copy the built binary to `INSTALL_DIR/voiceforge`. `chmod +x`.
8. **Skip dir creation — ROADMAP 1.2 already shipped.** The first
   invocation of the installed binary populates `~/.voiceforge/`
   itself via `bootstrap::ensure_voiceforge_home()`. install.sh just
   runs the binary's smoke step and lets the bootstrap banner fire.
9. Smoke test: run `INSTALL_DIR/voiceforge voices` once. This both
   verifies the binary works AND triggers the first-run bootstrap so
   the user sees the layout banner immediately. Capture exit code;
   fail loud with stderr on non-zero.
10. Final install.sh banner: a 4-line summary with the install
    location and the next-step command (`voiceforge say --text
"VoiceForge is ready"`).

## Idempotency contract

Running install.sh twice in a row does **not**:

- Re-clone if checkout exists (just `git fetch && checkout`).
- Duplicate `PATH` warnings.
- Re-print the bootstrap banner — that's owned by the binary itself
  via 1.2, which already prints exactly once on first run.

The preset-preservation guarantee belongs to `bootstrap.rs` (1.2),
not install.sh.

## Files

### New

- `install.sh` at repo root (executable, shellchecked).
- `tests/install/test_install.bats` — Bats-core test suite that runs
  install.sh in a Docker container and asserts the resulting binary
  works. Bats only required if `bats` is on `PATH`; CI installs it.
  _(Defer Bats to follow-up if the harness is heavier than the script:
  if so, ship a single `tests/install/smoke.sh` instead and call from
  CI later.)_

### Modified

- `README.md` — replace the multi-step Quick Start with the curl
  one-liner; keep the manual path under a "Build from source manually"
  collapsed section.
- `ROADMAP.md` — flip 1.5 to `[x]` (in the merge commit per the loop).

## Verify

1. **Fresh-checkout smoke** — `rm -rf ~/.cache/voiceforge ~/.voiceforge
/usr/local/bin/voiceforge`, then run the script from a freshly
   cloned checkout (simulate the curl flow). Assert exit 0,
   `voiceforge --version` works, `~/.voiceforge/presets/default.json`
   exists.
2. **Idempotent re-run** — run a second time. Assert no "cloning"
   message, no preset overwrite, exit 0, banner shows "already
   installed" or equivalent.
3. **Customized preset preserved** — already covered by the 1.2
   tests; install.sh doesn't touch presets directly.
4. **Missing prereq** — `PATH=/usr/bin sh install.sh`. Assert exit 1
   with a clear "install ffmpeg via brew" message.
5. **Non-PATH install dir** — force `INSTALL_DIR=~/bin` on a system
   where it's not on PATH. Assert the script warns the user how to add
   it.
6. `shellcheck install.sh` — zero warnings (CI already runs this).

## Risks

- **The curl-pipe-bash pattern.** Many devs (rightly) distrust it. The
  README copy includes the inspect-first variant: `curl -fsSL ... -o
install.sh && less install.sh && bash install.sh`. We don't try to
  hide what the script does.
- **Source build is slow.** First-run build is ~2 min on a clean Rust
  cache. Banner notes this explicitly. ROADMAP 1.7 fixes it with
  pre-built binaries.
- **Cargo not installed.** Script can offer to install rustup
  non-interactively (`curl --proto '=https' --tlsv1.2 -sSf
https://sh.rustup.rs | sh -s -- -y`), but that's another
  curl-pipe-bash. Default: detect, error, point at rustup.rs. User
  installs Rust themselves.

## Atomic commits

1. `feat(install): add install.sh + idempotent first-run bootstrap`
2. `docs(readme): switch Quick Start to the one-curl install`
