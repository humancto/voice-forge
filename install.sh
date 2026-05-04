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
# Env vars:
#   VOICEFORGE_REF           git ref to build (default: main)
#   VOICEFORGE_INSTALL_DIR   override install dir (default: auto-detected)
#   VOICEFORGE_DRY_RUN=1     print every action, do not execute

set -Eeuo pipefail
IFS=$'\n\t'
export LC_ALL=C
unset CDPATH

REPO_URL="https://github.com/humancto/voice-forge.git"
REF="${VOICEFORGE_REF:-main}"
CACHE_ROOT="${XDG_CACHE_HOME:-$HOME/.cache}/voiceforge"
CHECKOUT_DIR="$CACHE_ROOT/checkout"
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

# -- prereqs ----------------------------------------------------------

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
  pick one and pass VOICEFORGE_INSTALL_DIR=<path>"
}

INSTALL_DIR="$(resolve_install_dir)"
INSTALL_PATH="$INSTALL_DIR/voiceforge"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) warn "$INSTALL_DIR is not on \$PATH. Add this to your shell rc:
  export PATH=\"$INSTALL_DIR:\$PATH\""
;;
esac

# -- checkout ---------------------------------------------------------

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

if [[ "$DRY_RUN" == "1" && ! -d "$CHECKOUT_DIR/.git" ]]; then
  GIT_SHA="<dry-run>"
else
  GIT_SHA="$(git -C "$CHECKOUT_DIR" rev-parse --short HEAD)"
fi

# -- build ------------------------------------------------------------

say "building voiceforge from source (first build is 2–4 min on a clean cache)"
if [[ -t 1 ]]; then
  run cargo build \
    --manifest-path "$CHECKOUT_DIR/apps/voiceforge-cli/Cargo.toml" \
    --release --color always
else
  run cargo build \
    --manifest-path "$CHECKOUT_DIR/apps/voiceforge-cli/Cargo.toml" \
    --release
fi

BUILT_BIN="$CHECKOUT_DIR/apps/voiceforge-cli/target/release/voiceforge"
if [[ "$DRY_RUN" != "1" ]]; then
  [[ -x "$BUILT_BIN" ]] || die "expected binary not found: $BUILT_BIN"
fi

# -- install ----------------------------------------------------------

say "installing → $INSTALL_PATH"
run install -m 0755 "$BUILT_BIN" "$INSTALL_PATH"

# -- smoke test (also fires the binary's first-run bootstrap banner) --

say "running smoke test (this also initializes ~/.voiceforge/ on first run)"
echo
run "$INSTALL_PATH" voices >/dev/null
echo

# -- final banner -----------------------------------------------------

cat <<EOF
voiceforge: installed
  binary:  $INSTALL_PATH
  source:  $CHECKOUT_DIR  ($GIT_SHA)
  platform: $OS_ID/$ARCH_ID

next:    $INSTALL_PATH say --text "VoiceForge is ready"
clone:   voiceforge install-cloning      # ROADMAP 2.1, not yet shipped
EOF
