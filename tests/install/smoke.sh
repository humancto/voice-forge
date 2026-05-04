#!/usr/bin/env bash
# Smoke test for install.sh.
#
# CI runs the dry-run path so we exercise platform detection,
# prereq checks, install-dir resolution, retry plumbing, and the
# final banner WITHOUT actually building cargo or polluting the
# runner's $HOME. Real install is exercised when a contributor runs
# install.sh against a live repo.

set -Eeuo pipefail
IFS=$'\n\t'
export LC_ALL=C
unset CDPATH

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="$REPO_ROOT/install.sh"

[[ -x "$INSTALL_SH" ]] || { echo "missing: $INSTALL_SH"; exit 1; }

tmp_install_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_install_dir"' EXIT

echo "==> dry-run install with custom INSTALL_DIR"
output="$(VOICEFORGE_DRY_RUN=1 VOICEFORGE_INSTALL_DIR="$tmp_install_dir" bash "$INSTALL_SH" 2>&1)"

# Banner sanity
echo "$output" | grep -q "voiceforge: installed" \
  || { echo "FAIL: missing 'voiceforge: installed' banner"; echo "$output"; exit 1; }
echo "$output" | grep -q "platform:" \
  || { echo "FAIL: missing platform line"; echo "$output"; exit 1; }
echo "$output" | grep -q "$tmp_install_dir/voiceforge" \
  || { echo "FAIL: banner does not mention install path"; echo "$output"; exit 1; }

# Dry-run never actually copied the binary
if [[ -f "$tmp_install_dir/voiceforge" ]]; then
  echo "FAIL: dry-run leaked a real binary into $tmp_install_dir"
  exit 1
fi

# PATH warning when the install dir isn't on $PATH
echo "$output" | grep -qi "not on .*PATH" \
  || { echo "FAIL: missing PATH warning for non-PATH install dir"; echo "$output"; exit 1; }

echo "==> shellcheck install.sh"
shellcheck -e SC1091 "$INSTALL_SH"

echo "==> all install.sh smoke checks passed"
