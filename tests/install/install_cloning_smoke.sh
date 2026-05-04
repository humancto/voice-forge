#!/usr/bin/env bash
# Dry-run smoke test for scripts/install_cloning.sh.
#
# CI runs this on macos-14 to exercise platform detection, step ordering,
# brew-list checks, and the marker-emission path WITHOUT doing the real
# ~3 GB install. Real install is tested manually + via workflow_dispatch.

set -Eeuo pipefail
IFS=$'\n\t'
export LC_ALL=C
unset CDPATH

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="$REPO_ROOT/scripts/install_cloning.sh"

[[ -x "$INSTALL_SH" ]] || { echo "missing: $INSTALL_SH"; exit 1; }

tmp_home="$(mktemp -d)"
trap 'rm -rf "$tmp_home"' EXIT

echo "==> dry-run with VOICEFORGE_HOME=$tmp_home"
output="$(VOICEFORGE_INSTALL_CLONING_DRY_RUN=1 VOICEFORGE_HOME="$tmp_home" \
  bash "$INSTALL_SH" 2>&1)"

echo "$output"
echo ""

# Required step labels in expected order
required_labels=(
  "voiceforge install-cloning"
  "disk-space precheck"
  "platform check"
  "arm64 Homebrew check"
  "verifying"
  "smoke test"
  "writing marker"
  "done"
)

prev_pos=-1
for label in "${required_labels[@]}"; do
  pos="$(printf '%s\n' "$output" | grep -n "$label" | head -1 | cut -d: -f1)"
  if [[ -z "$pos" ]]; then
    echo "FAIL: missing label '$label'"
    exit 1
  fi
  if (( pos <= prev_pos )); then
    echo "FAIL: label '$label' out of order (line $pos, prev $prev_pos)"
    exit 1
  fi
  prev_pos="$pos"
done

# Dry-run must NOT actually install anything
if [[ -d "$tmp_home/cloning/venv/bin" ]]; then
  echo "FAIL: dry-run leaked a real venv into $tmp_home"
  exit 1
fi
if [[ -f "$tmp_home/cloning/INSTALLED.toml" ]]; then
  echo "FAIL: dry-run leaked a real marker into $tmp_home"
  exit 1
fi

echo "==> shellcheck"
if command -v shellcheck >/dev/null 2>&1; then
  shellcheck -e SC1091 "$INSTALL_SH"
else
  echo "(shellcheck not on PATH, skipping — dedicated CI shellcheck job covers this)"
fi

echo "==> all install_cloning.sh smoke checks passed"
