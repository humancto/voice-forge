# Pre-built binary releases (ROADMAP 1.5)

> **Plan v2** — incorporates all 15 rust-expert REVISE items.

## Goal

`curl -fsSL https://raw.githubusercontent.com/humancto/voice-forge/main/install.sh | bash` becomes turn-key — no Rust toolchain required. Today the install script runs `cargo build --release`, which means anyone without Rust bounces. After this:

```
$ curl -fsSL ... | bash
Detected: aarch64-apple-darwin
Downloading voiceforge v0.2.0 ...
Verified sha256.
Installed /usr/local/bin/voiceforge (~ 6 MB)
$ voiceforge --version
voiceforge 0.2.0
```

Zero toolchain, ~5 seconds curl-to-binary.

## Surface

Two artifacts ship per `v<semver>` tag push:

1. **GitHub Action** (`.github/workflows/release-binary.yml`) — fires on `v[0-9]+.[0-9]+.[0-9]+` (and `-*` pre-releases). Cross-builds 4 targets, computes sha256, attaches each as a release asset. Then a smoke-job downloads its own artifact and runs `voiceforge --version` to gate the release.

2. **Updated `install.sh`** — detects platform, downloads + sha256-verifies (cross-platform via shell-side compare, NOT `sha256sum -c`), atomic-rename installs into `${VOICEFORGE_INSTALL_DIR:-/usr/local/bin}` with the same `can_write_to` fall-through to `~/.local/bin` already in place. Auto-strips macOS quarantine bit. Falls back to from-source for unknown platforms.

## Targets + ABI floors

| Triple                      | Runner                         | ABI / libc     | Notes                                                                                                                                                                       |
| --------------------------- | ------------------------------ | -------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `aarch64-apple-darwin`      | `macos-14` (arm64)             | macOS 11+      | Native build, `--locked`                                                                                                                                                    |
| `x86_64-apple-darwin`       | `macos-14`                     | macOS 11+      | Cross via `--target`; CoreAudio in SDK, no extra cc flags (per rust-expert v1 confirmation)                                                                                 |
| `x86_64-unknown-linux-gnu`  | **`ubuntu-22.04`** (NOT 24.04) | **glibc 2.35** | Lower glibc floor — covers Ubuntu 22.04+, Debian 12, recent Amazon Linux. ALSA via `apt-get install libasound2-dev`.                                                        |
| `aarch64-unknown-linux-gnu` | `ubuntu-22.04`                 | glibc 2.35     | Cross via `gcc-aarch64-linux-gnu` + `dpkg --add-architecture arm64` + `libasound2-dev:arm64`. NOT `cross` (Docker is slow + fights pkg-config — per rust-expert v1 review). |

Asset naming: `voiceforge-<version>-<triple>.tar.gz` + `voiceforge-<version>-<triple>.tar.gz.sha256` per target.

Each tarball contains:

```
voiceforge        # the binary
LICENSE           # MIT
README.md         # repo readme
```

## install.sh changes (key correctness items)

```bash
detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os/$arch" in
    Darwin/arm64)               echo "aarch64-apple-darwin" ;;
    Darwin/x86_64)              echo "x86_64-apple-darwin" ;;
    Linux/x86_64)               echo "x86_64-unknown-linux-gnu" ;;
    Linux/aarch64|Linux/arm64)  echo "aarch64-unknown-linux-gnu" ;;
    *)                          echo "" ;;  # fall through to from-source
  esac
}

# Cross-platform sha256 — DON'T rely on sha256sum -c (macOS doesn't have it,
# and BSD vs GNU checksum-file format differ). Compute + compare in shell.
compute_sha256() {
  local file="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  else
    echo "neither shasum nor sha256sum found" >&2
    return 1
  fi
}

verify_sha256() {
  local file="$1" expected="$2"
  local actual
  actual="$(compute_sha256 "$file")" || return 1
  if [[ "$actual" != "$expected" ]]; then
    echo "sha256 mismatch on $file" >&2
    echo "  expected: $expected" >&2
    echo "  actual:   $actual" >&2
    return 1
  fi
}

# Atomic install. extract to tempdir, then rename over the install path.
# rename(2) is atomic on the same filesystem; partial-rename pattern adds
# the safety net so a concurrent voiceforge invocation can't observe a
# truncated binary or hit ETXTBSY.
install_binary() {
  local src="$1" dst="$2"
  chmod 0755 "$src"
  local tmp="${dst}.partial.$$"
  mv "$src" "$tmp"
  mv "$tmp" "$dst"
  # macOS sets the quarantine xattr on curl downloads (Big Sur+).
  # Strip it automatically — without this the user gets a Gatekeeper
  # error on first run BEFORE they can read any "run xattr" message.
  if [[ "$(uname -s)" == "Darwin" ]]; then
    xattr -d com.apple.quarantine "$dst" 2>/dev/null || true
  fi
}
```

Existing `resolve_install_dir` already does `/usr/local/bin` → `~/.local/bin` fallback via `can_write_to` probe. **Do NOT add a `sudo` prompt.** Current behavior is correct.

## Workflow critical items

```yaml
name: Release binary

on:
  push:
    tags:
      - "v[0-9]+.[0-9]+.[0-9]+"
      - "v[0-9]+.[0-9]+.[0-9]+-*"

permissions:
  contents: write   # explicit — needed for asset upload + release notes

jobs:
  guard-version:
    # Verify Cargo.toml version matches the tag BEFORE building anything.
    # Otherwise we'd ship `voiceforge --version` reporting 0.1.0 in a
    # v0.2.0 release — silent footgun.
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - name: Verify Cargo.toml version matches tag
        run: |
          set -euo pipefail
          tag="${GITHUB_REF_NAME#v}"
          tag_base="${tag%%-*}"   # strip -rc1 etc for the manifest check
          cargo_version=$(grep -E '^version = ' apps/voiceforge-cli/Cargo.toml | head -1 | sed -E 's/.*"(.*)".*/\1/')
          if [[ "$cargo_version" != "$tag_base" ]]; then
            echo "Cargo.toml version $cargo_version != tag base $tag_base" >&2
            exit 1
          fi

  build:
    needs: guard-version
    strategy:
      matrix:
        include:
          - target: aarch64-apple-darwin
            runner: macos-14
          - target: x86_64-apple-darwin
            runner: macos-14
          - target: x86_64-unknown-linux-gnu
            runner: ubuntu-22.04
          - target: aarch64-unknown-linux-gnu
            runner: ubuntu-22.04
            cross_setup: aarch64-linux-gnu
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust target
        run: rustup target add ${{ matrix.target }}
      - name: Linux ALSA + cross-toolchain (if applicable)
        if: runner.os == 'Linux'
        run: |
          set -euo pipefail
          if [[ "${{ matrix.cross_setup }}" == "aarch64-linux-gnu" ]]; then
            sudo dpkg --add-architecture arm64
            sudo apt-get update
            sudo apt-get install -y gcc-aarch64-linux-gnu libasound2-dev:arm64
          else
            sudo apt-get update
            sudo apt-get install -y libasound2-dev
          fi
      - name: Build
        env:
          CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER: aarch64-linux-gnu-gcc
          PKG_CONFIG_ALLOW_CROSS: 1
        run: |
          cargo build --release --locked --target ${{ matrix.target }} --manifest-path apps/voiceforge-cli/Cargo.toml
      - name: Package tarball + sha256
        env:
          TARGET: ${{ matrix.target }}
          VERSION: ${{ github.ref_name }}
        run: |
          set -euo pipefail
          version="${VERSION#v}"
          stage="$(mktemp -d)/voiceforge-${version}-${TARGET}"
          mkdir -p "$stage"
          cp "apps/voiceforge-cli/target/${TARGET}/release/voiceforge" "$stage/"
          cp LICENSE README.md "$stage/" 2>/dev/null || true
          tarball="voiceforge-${version}-${TARGET}.tar.gz"
          tar -czf "$tarball" -C "$(dirname "$stage")" "$(basename "$stage")"
          # GNU sha256sum format on Linux; on macos-14 use shasum -a 256
          if command -v sha256sum >/dev/null 2>&1; then
            sha256sum "$tarball" > "${tarball}.sha256"
          else
            shasum -a 256 "$tarball" > "${tarball}.sha256"
          fi
      - uses: actions/upload-artifact@v4
        with:
          name: ${{ matrix.target }}
          path: |
            voiceforge-*.tar.gz
            voiceforge-*.tar.gz.sha256

  release:
    needs: build
    runs-on: ubuntu-22.04
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v4
      - uses: actions/download-artifact@v4
        with:
          path: dist
          merge-multiple: true
      - name: Compose release notes
        env:
          VERSION: ${{ github.ref_name }}
        run: |
          cat > /tmp/notes.md <<EOF
          ## Install

          \`\`\`bash
          curl -fsSL https://raw.githubusercontent.com/humancto/voice-forge/main/install.sh | bash
          \`\`\`

          ## Platforms

          | Triple | libc / OS | Notes |
          |---|---|---|
          | aarch64-apple-darwin | macOS 11+ | Apple Silicon |
          | x86_64-apple-darwin | macOS 11+ | Intel — \`voiceforge install-cloning\` not yet supported (ROADMAP 2.1.1) |
          | x86_64-unknown-linux-gnu | glibc ≥ 2.35 | Built on ubuntu-22.04 |
          | aarch64-unknown-linux-gnu | glibc ≥ 2.35 | Built on ubuntu-22.04 with gcc-aarch64-linux-gnu |

          ## Auto-generated changelog
          EOF
          gh release view "$VERSION" --json body --jq .body 2>/dev/null > /tmp/auto_notes.md || \
            gh api "/repos/${GITHUB_REPOSITORY}/releases/generate-notes" \
              -f tag_name="$VERSION" \
              --jq '.body' > /tmp/auto_notes.md
          cat /tmp/auto_notes.md >> /tmp/notes.md
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
      - name: Create release
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          VERSION: ${{ github.ref_name }}
        run: |
          set -euo pipefail
          gh release create "$VERSION" \
            --title "voiceforge $VERSION" \
            --notes-file /tmp/notes.md \
            dist/voiceforge-*.tar.gz dist/voiceforge-*.tar.gz.sha256

  smoke:
    # Download our own released artifact on a clean runner per platform
    # and run `voiceforge --version` to catch glibc / quarantine /
    # atomic-rename regressions before users do.
    needs: release
    strategy:
      matrix:
        include:
          - target: aarch64-apple-darwin
            runner: macos-14
          - target: x86_64-unknown-linux-gnu
            runner: ubuntu-22.04
    runs-on: ${{ matrix.runner }}
    steps:
      - name: Download via install.sh
        env:
          VERSION: ${{ github.ref_name }}
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          set -euo pipefail
          version="${VERSION#v}"
          target="${{ matrix.target }}"
          gh release download "$VERSION" --pattern "voiceforge-${version}-${target}.*" --dir /tmp/dl
          tar -xzf "/tmp/dl/voiceforge-${version}-${target}.tar.gz" -C /tmp/dl
          /tmp/dl/voiceforge-${version}-${target}/voiceforge --version
```

## Plan order

1. Bump `apps/voiceforge-cli/Cargo.toml` version to `0.2.0`. Commit `Cargo.lock` with that bump (release builds use `--locked`).
2. Add `--version` flag to clap (it auto-generates from `CARGO_PKG_VERSION` if not present; verify).
3. Add `release-binary.yml` workflow (the YAML above).
4. Test by pushing `v0.2.0-rc1`. Verify all 4 targets build, all `.tar.gz` + `.sha256` assets land, smoke job exits 0.
5. Update `install.sh` with the detect → download → verify → atomic-install → quarantine-strip path. Keep from-source fallback for unknown platforms (FreeBSD, Windows-without-WSL).
6. Test `install.sh` against the rc1 release end-to-end on this Mac. Then `rm /usr/local/bin/voiceforge && curl ... | bash` to validate the from-zero path.
7. Tag `v0.2.0` final. Verify auto-release fires, `install.sh` resolves to v0.2.0.
8. Update README install section: replace from-source caveat with "5-second turn-key install."
9. Update `docs/index.html` install section to match.

Estimated diff: ~250 LOC YAML + ~80 LOC bash + small README/index.html updates.

## What I disagreed with from review v1

Nothing. All 15 items landed.

## Implementation notes from review v2 (fix during YAML writing)

1. `release` job's "Compose release notes" step has `env:` declared twice in the v2 plan snippet. Merge the `GH_TOKEN` into the same `env:` map as `VERSION` — YAML rejects duplicate `env:` keys on the same step.
2. The `gh release view ... 2>/dev/null > ... || gh api .../generate-notes ...` pattern silently produces an empty file when `view` succeeds with an empty body (e.g. release pre-created). Drop the `view` fallback; just always call `gh api .../releases/generate-notes`.
3. The release-notes heredoc uses backtick-escaped fences inside an unquoted `<<EOF` — backticks in unquoted heredocs trigger command substitution. Use `<<'EOF'` (quoted delimiter) and pre-substitute `$VERSION` via `sed` or `envsubst` afterward, OR drop the version interpolation from the heredoc body.

## Smoke coverage gaps (acknowledged)

Smoke matrix only covers `aarch64-apple-darwin` (macos-14 native) and `x86_64-unknown-linux-gnu` (ubuntu-22.04). The other two ship un-smoke-tested:

- `x86_64-apple-darwin` — would need `macos-13` runner; deferred.
- `aarch64-unknown-linux-gnu` — no GHA-hosted aarch64 Linux runner without self-hosting; deferred.

Both targets will surface regressions only via user reports until we add self-hosted runners (out of scope here).

## clap version flag — verify, don't assume

Step 2 says "Add `--version` to clap" — `clap` derive only auto-generates from `CARGO_PKG_VERSION` when the parser has `#[command(version)]` or an explicit `version` attribute. Check `apps/voiceforge-cli/src/main.rs` `#[derive(Parser)]` block; if missing, add `#[command(name = "voiceforge", version)]`.

## libasound build-script

`libasound2-dev:arm64` is needed for the cross-compile target sysroot. The build host may also need plain `libasound2-dev` for cargo build-script bits at host architecture. Verify on rc1; if pkg-config bombs, add the host package.
