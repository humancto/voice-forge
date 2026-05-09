#!/usr/bin/env bash
# VoiceForge installer.
#
#   curl -fsSL https://raw.githubusercontent.com/humancto/voice-forge/main/install.sh | bash
#
# Wary? Inspect first:
#   curl -fsSL https://raw.githubusercontent.com/humancto/voice-forge/main/install.sh -o install.sh
#   less install.sh
#   bash install.sh
#
# Install order (best to worst):
#   1. Pre-built binary from the latest GitHub release (5 sec, no toolchain).
#   2. From-source build via cargo (2-4 min, requires Rust + ffmpeg).
#
# Pre-built binaries cover darwin-arm64/x86_64 + linux-x86_64/aarch64.
# Anything else (FreeBSD, Windows-without-WSL, glibc < 2.35) falls
# through to the from-source path.
#
# Env vars:
#   VOICEFORGE_VERSION       pin a release version (default: latest)
#   VOICEFORGE_REF           git ref to build from source (default: main)
#   VOICEFORGE_INSTALL_DIR   override install dir (default: auto-detected)
#   VOICEFORGE_FORCE_SOURCE  set to 1 to skip prebuilt and always build
#   VOICEFORGE_DRY_RUN=1     print every action, do not execute

set -Eeuo pipefail
IFS=$'\n\t'
export LC_ALL=C
unset CDPATH

REPO_OWNER="humancto"
REPO_NAME="voice-forge"
REPO_URL="https://github.com/${REPO_OWNER}/${REPO_NAME}.git"
RELEASES_URL="https://github.com/${REPO_OWNER}/${REPO_NAME}/releases"
REF="${VOICEFORGE_REF:-main}"
VERSION="${VOICEFORGE_VERSION:-}"
FORCE_SOURCE="${VOICEFORGE_FORCE_SOURCE:-0}"
CACHE_ROOT="${XDG_CACHE_HOME:-$HOME/.cache}/voiceforge"
CHECKOUT_DIR="$CACHE_ROOT/checkout"
DOWNLOAD_DIR="$CACHE_ROOT/dl"
DRY_RUN="${VOICEFORGE_DRY_RUN:-0}"

trap 'echo "install.sh failed at line $LINENO. Re-run with: bash -x install.sh" >&2' ERR

# -- helpers -----------------------------------------------------------

say() { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

run() {
  if [[ "$DRY_RUN" == "1" ]]; then
    printf '[dry-run] %s\n' "$*"
  else
    "$@"
  fi
}

require() {
  command -v "$1" >/dev/null 2>&1 || die "missing prereq: $1
  install with: $2"
}

# Probe whether we can actually create files in $1, not just check the
# permission bit. The permission bit is misleading on macOS Homebrew.
can_write_to() {
  local dir="$1"
  [[ -d "$dir" ]] || return 1
  local probe
  probe="$dir/.voiceforge-install-probe.$$"
  if (touch "$probe" 2>/dev/null); then
    rm -f "$probe"
    return 0
  fi
  return 1
}

retry() {
  local attempts="$1"; shift
  local sleep_secs="$1"; shift
  local n=1
  until "$@"; do
    if (( n >= attempts )); then
      return 1
    fi
    warn "command failed (attempt $n/$attempts), retrying in ${sleep_secs}s: $*"
    sleep "$sleep_secs"
    n=$(( n + 1 ))
  done
}

# Cross-platform sha256 — DON'T rely on `sha256sum -c` (macOS doesn't
# have it, and BSD vs GNU checksum-file format differ). Compute the
# hash and compare in shell.
compute_sha256() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  else
    die "neither sha256sum nor shasum found; install one"
  fi
}

verify_sha256() {
  local file="$1" expected="$2"
  local actual
  actual="$(compute_sha256 "$file")"
  if [[ "$actual" != "$expected" ]]; then
    die "sha256 mismatch on $(basename "$file")
  expected: $expected
  actual:   $actual"
  fi
}

# -- platform detection -----------------------------------------------

OS="$(uname -s)"
case "$OS" in
  Darwin) OS_ID="darwin" ;;
  Linux)  OS_ID="linux" ;;
  *)      die "unsupported OS: $OS (see ROADMAP 1.5+ for status)" ;;
esac

ARCH="$(uname -m)"
case "$ARCH" in
  arm64|aarch64) ARCH_ID="arm64" ;;
  x86_64|amd64)  ARCH_ID="x86_64" ;;
  *)             die "unsupported arch: $ARCH" ;;
esac

# Map (OS, arch) → Rust target triple matching what release-binary.yml
# publishes. Returns "" for unsupported combos so we fall through to
# from-source.
detect_target() {
  case "$OS_ID/$ARCH_ID" in
    darwin/arm64)   echo "aarch64-apple-darwin" ;;
    darwin/x86_64)  echo "x86_64-apple-darwin" ;;
    linux/x86_64)   echo "x86_64-unknown-linux-gnu" ;;
    linux/arm64)    echo "aarch64-unknown-linux-gnu" ;;
    *)              echo "" ;;
  esac
}

# -- install dir ------------------------------------------------------

resolve_install_dir() {
  if [[ -n "${VOICEFORGE_INSTALL_DIR:-}" ]]; then
    printf '%s' "$VOICEFORGE_INSTALL_DIR"
    return
  fi
  for candidate in "/usr/local/bin" "$HOME/.local/bin" "$HOME/bin"; do
    [[ -d "$candidate" ]] || mkdir -p "$candidate" 2>/dev/null || continue
    if can_write_to "$candidate"; then
      printf '%s' "$candidate"
      return
    fi
  done
  die "no writable install dir found in /usr/local/bin, ~/.local/bin, ~/bin
  pick one and pass VOICEFORGE_INSTALL_DIR=<path>
  (do NOT add a sudo prompt — \$HOME/.local/bin is the right fallback)"
}

INSTALL_DIR="$(resolve_install_dir)"
INSTALL_PATH="$INSTALL_DIR/voiceforge"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) warn "$INSTALL_DIR is not on \$PATH. Add this to your shell rc:
  export PATH=\"$INSTALL_DIR:\$PATH\""
;;
esac

# -- atomic install helper -------------------------------------------

# Write $1 to $INSTALL_PATH atomically (rename(2) on the same FS), then
# strip the macOS Gatekeeper quarantine xattr so the binary actually
# runs without the user having to find an obscure xattr command.
install_binary() {
  local src="$1"
  [[ -x "$src" ]] || die "expected executable not found: $src"
  local tmp="${INSTALL_PATH}.partial.$$"
  run cp "$src" "$tmp"
  run chmod 0755 "$tmp"
  run mv "$tmp" "$INSTALL_PATH"
  if [[ "$OS_ID" == "darwin" && "$DRY_RUN" != "1" ]]; then
    # The quarantine bit is set by curl on Big Sur+. Without stripping
    # it, the user gets a Gatekeeper warning on first run BEFORE they
    # can read any "run xattr" message. Auto-strip — silent on success
    # (xattr returns nonzero if the bit was already absent, which is
    # fine).
    xattr -d com.apple.quarantine "$INSTALL_PATH" 2>/dev/null || true
  fi
}

# -- prebuilt binary path --------------------------------------------

# Resolve the version to install. Default: query GitHub for the latest
# release tag. Override with VOICEFORGE_VERSION=v0.2.0 or similar.
resolve_version() {
  if [[ -n "$VERSION" ]]; then
    printf '%s' "$VERSION"
    return
  fi
  # Scrape the redirect target of /releases/latest.
  local url="${RELEASES_URL}/latest"
  local resolved
  resolved="$(curl -fsI "$url" 2>/dev/null \
    | awk -F'/' 'tolower($1) == "location:" {gsub(/\r/, "", $NF); print $NF}' \
    | tail -1)"
  if [[ -z "$resolved" ]]; then
    return 1   # caller falls through to from-source
  fi
  printf '%s' "$resolved"
}

try_install_prebuilt() {
  local target
  target="$(detect_target)"
  if [[ -z "$target" ]]; then
    say "no prebuilt binary for $OS_ID/$ARCH_ID — falling through to from-source"
    return 1
  fi

  local resolved_version
  if ! resolved_version="$(resolve_version)"; then
    warn "could not resolve latest release version (network down? new repo?) — falling through to from-source"
    return 1
  fi

  local plain_version="${resolved_version#v}"
  local tarball_name="voiceforge-${plain_version}-${target}.tar.gz"
  local tarball_url="${RELEASES_URL}/download/${resolved_version}/${tarball_name}"
  local sha_url="${tarball_url}.sha256"

  say "downloading $resolved_version for $target"
  mkdir -p "$DOWNLOAD_DIR"
  local tarball="$DOWNLOAD_DIR/$tarball_name"
  local shafile="${tarball}.sha256"

  if ! run retry 3 3 curl -fsSL "$tarball_url" -o "$tarball"; then
    warn "download failed: $tarball_url — falling through to from-source"
    return 1
  fi
  if ! run retry 3 3 curl -fsSL "$sha_url" -o "$shafile"; then
    warn "sha256 download failed: $sha_url — falling through to from-source"
    return 1
  fi

  if [[ "$DRY_RUN" != "1" ]]; then
    local expected
    expected="$(awk '{print $1}' "$shafile")"
    say "verifying sha256"
    verify_sha256 "$tarball" "$expected"
  fi

  local extract_dir
  extract_dir="$(mktemp -d)"
  run tar -xzf "$tarball" -C "$extract_dir"
  local binary="$extract_dir/voiceforge-${plain_version}-${target}/voiceforge"
  if [[ "$DRY_RUN" != "1" ]]; then
    [[ -x "$binary" ]] || die "tarball did not contain expected binary at $binary"
  fi

  install_binary "$binary"
  rm -rf "$extract_dir"

  INSTALL_VERSION="$resolved_version"
  INSTALL_METHOD="prebuilt"
  return 0
}

# -- from-source path -------------------------------------------------

build_from_source() {
  case "$OS_ID" in
    darwin)
      require git    "brew install git"
      require cargo  "install Rust via https://rustup.rs"
      require ffmpeg "brew install ffmpeg"
      ;;
    linux)
      require git    "sudo apt-get install -y git"
      require cargo  "install Rust via https://rustup.rs"
      require ffmpeg "sudo apt-get install -y ffmpeg libasound2-dev pkg-config"
      ;;
  esac

  mkdir -p "$CACHE_ROOT"

  if [[ -d "$CHECKOUT_DIR/.git" ]]; then
    say "updating cached checkout: $CHECKOUT_DIR"
    if ! (cd "$CHECKOUT_DIR" && git diff --quiet && git diff --cached --quiet); then
      die "cached checkout has local changes: $CHECKOUT_DIR
  refusing to clobber. Either commit/stash your edits there, or remove the dir:
    rm -rf $CHECKOUT_DIR"
    fi
    run retry 3 3 git -C "$CHECKOUT_DIR" fetch --quiet origin "$REF"
    run git -C "$CHECKOUT_DIR" checkout --quiet "origin/$REF"
  else
    say "cloning $REPO_URL → $CHECKOUT_DIR"
    run retry 3 3 git clone --quiet "$REPO_URL" "$CHECKOUT_DIR"
    run git -C "$CHECKOUT_DIR" checkout --quiet "origin/$REF" 2>/dev/null \
      || run git -C "$CHECKOUT_DIR" checkout --quiet "$REF"
  fi

  local git_sha
  if [[ "$DRY_RUN" == "1" && ! -d "$CHECKOUT_DIR/.git" ]]; then
    git_sha="<dry-run>"
  else
    git_sha="$(git -C "$CHECKOUT_DIR" rev-parse --short HEAD)"
  fi

  say "building voiceforge from source (first build is 2-4 min on a clean cache)"
  if [[ -t 1 ]]; then
    run cargo build \
      --manifest-path "$CHECKOUT_DIR/apps/voiceforge-cli/Cargo.toml" \
      --release --color always
  else
    run cargo build \
      --manifest-path "$CHECKOUT_DIR/apps/voiceforge-cli/Cargo.toml" \
      --release
  fi

  local built_bin="$CHECKOUT_DIR/apps/voiceforge-cli/target/release/voiceforge"
  if [[ "$DRY_RUN" != "1" ]]; then
    [[ -x "$built_bin" ]] || die "expected binary not found: $built_bin"
  fi

  install_binary "$built_bin"
  INSTALL_VERSION="(from-source $REF $git_sha)"
  INSTALL_METHOD="source"
}

# -- main flow --------------------------------------------------------

say "installing → $INSTALL_PATH"

INSTALL_VERSION=""
INSTALL_METHOD=""

if [[ "$FORCE_SOURCE" == "1" ]]; then
  say "VOICEFORGE_FORCE_SOURCE=1 set — skipping prebuilt"
  build_from_source
else
  if ! try_install_prebuilt; then
    build_from_source
  fi
fi

# -- smoke test (also fires the binary's first-run bootstrap banner) --

say "running smoke test (this also initializes ~/.voiceforge/ on first run)"
echo
run "$INSTALL_PATH" --version
run "$INSTALL_PATH" voices >/dev/null
echo

# -- final banner -----------------------------------------------------

cat <<EOF
voiceforge: installed
  binary:    $INSTALL_PATH
  version:   $INSTALL_VERSION
  method:    $INSTALL_METHOD
  platform:  $OS_ID/$ARCH_ID

next:    voiceforge say --text "VoiceForge is ready"
packs:   voiceforge pack list
clone:   voiceforge install-cloning      # ROADMAP 2.1; macOS arm64 today
EOF
