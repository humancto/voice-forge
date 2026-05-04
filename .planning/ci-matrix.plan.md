# Plan: CI matrix on macOS + Linux (ROADMAP 0.2)

## Goal

Every PR runs `cargo fmt --check`, `cargo clippy -D warnings`,
`cargo test`, and `shellcheck` on `scripts/*.sh` against
`macos-latest` + `ubuntu-latest`. Status badge in README turns green.

## Out of scope

- Python test job — wait for ROADMAP 0.4 (Python test harness).
- Cross-compile / release artifacts — ROADMAP 1.7.
- Caching beyond `Swatinem/rust-cache@v2`.

## Files

### New

- `.github/workflows/ci.yml` — single workflow, three jobs:
  1. **`rust-checks` (matrix `os: [macos-latest, ubuntu-latest]`)**
     - `actions/checkout@v4`
     - Install ffmpeg + yt-dlp:
       - ubuntu: `sudo apt-get update && sudo apt-get install -y ffmpeg`
         - `pipx install yt-dlp` (or `pip install --user yt-dlp`)
       - macos: `brew install ffmpeg yt-dlp`
     - `dtolnay/rust-toolchain@stable` with `components: rustfmt, clippy`
     - `Swatinem/rust-cache@v2`
     - `bash scripts/fetch_fixtures.sh` (fetches the Peter Griffin clip
       so the integration tests can run for real).
     - `cargo fmt --manifest-path apps/voiceforge-cli/Cargo.toml --check`
     - `cargo clippy --manifest-path apps/voiceforge-cli/Cargo.toml
--all-targets -- -D warnings`
     - `VOICEFORGE_REQUIRE_FIXTURES=1 cargo test --manifest-path
apps/voiceforge-cli/Cargo.toml` — the env var flips the
       fixture-skip path to a hard fail, so CI catches missing
       fixtures.

  2. **`shellcheck` (ubuntu only)**
     - Run `shellcheck scripts/*.sh install.sh` (install.sh exists
       only on the install-script branch — guard with a glob that
       skips silently if no matching files).

  3. **`yaml-lint` (ubuntu only, optional)** — defer; skip for now to
     keep the workflow small.

### Modified

- None. README's CI badge already points at this workflow filename.

## Verify

1. **Push the branch.** GitHub Actions kicks the workflow on PR open.
   Both macOS and Linux jobs go green.
2. **Negative test 1: deliberately break formatting.** Add a
   trailing-space commit on a follow-up branch, push, confirm the
   `cargo fmt --check` job fails on both OSes.
3. **Negative test 2: introduce a clippy lint.** A `let _foo = bar;`
   without `#[allow]`. Confirm clippy job fails.
4. **Negative test 3: skip the fetch script.** Manually edit the
   workflow on a throwaway branch to remove the `fetch_fixtures.sh`
   step, confirm the test job fails because
   `VOICEFORGE_REQUIRE_FIXTURES=1` panics on the missing fixture.
5. After the workflow lands and is green, the README badge resolves
   green automatically.

## Risks

- **GitHub-hosted runner deprecations.** `macos-latest` is currently
  macOS 14 / arm64; `ubuntu-latest` is 24.04. Pinning to specific
  versions is safer for reproducibility but loses free upgrades. Use
  `-latest` for now; revisit in 6 months.
- **yt-dlp + ffmpeg fetching the Peter Griffin clip in CI.** YouTube
  may rate-limit or remove the clip. Mitigations:
  - The clip URL is pinned in `scripts/fetch_fixtures.sh` so we'd see
    the failure clearly, not a silent test skip.
  - If YouTube blocks the fetch in CI, follow-up: cache the fixture
    in a GitHub release artifact + have `fetch_fixtures.sh` fall
    back to the artifact URL.
- **First runner cache miss is slow.** Build will take ~3–4 minutes
  on first run, ~1 minute thereafter via `Swatinem/rust-cache@v2`.

## Atomic commits

1. `ci: add rust + shellcheck workflow on macos + ubuntu`
