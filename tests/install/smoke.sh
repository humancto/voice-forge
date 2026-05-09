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

if command -v shellcheck >/dev/null 2>&1; then
  echo "==> shellcheck install.sh"
  shellcheck -e SC1091 "$INSTALL_SH"
else
  echo "==> shellcheck not on PATH, skipping (dedicated CI shellcheck job covers this)"
fi

# Regression guard: a previous version parsed the redirect Location
# header with `awk -F'/'`. With a `/` field separator, $1 became
# "location: https:" (the URL itself contains `/`), so the
# `tolower($1) == "location:"` check never matched and 100% of
# installs silently fell through to the slow from-source path. Refuse
# to ship that pattern again.
echo "==> resolve_version regression guard"
# Strip comments + blank lines, then look for the forbidden pattern.
# Avoids false-positives on the don't-do-this comment in install.sh.
if grep -vE '^[[:space:]]*#' "$INSTALL_SH" \
    | grep -qE 'awk[[:space:]]+-F[[:space:]]*.?/'; then
  echo "FAIL: install.sh contains a forbidden 'awk -F/' pattern."
  echo "  This is the version-parser bug fixed by the install.sh"
  echo "  refresh -- a / field-separator on the Location header"
  echo "  silently broke the prebuilt-binary install path."
  echo "  Use grep + sed against the Location header instead."
  exit 1
fi

# Live test of resolve_version against the actual GitHub redirect
# format. Skipped if curl or network are unavailable (CI without
# net should still pass the regression-guard above).
if command -v curl >/dev/null 2>&1 && curl -fsI -m 5 \
    https://github.com/humancto/voice-forge/releases/latest >/dev/null 2>&1; then
  echo "==> resolve_version live check"
  url="https://github.com/humancto/voice-forge/releases/latest"
  raw="$(curl -fsI "$url" 2>/dev/null)"
  resolved="$(printf '%s\n' "$raw" \
    | grep -i '^location:' \
    | sed -E 's|.*/tag/||' \
    | tr -d '\r\n')"
  if [[ -z "$resolved" ]]; then
    echo "FAIL: resolve_version parser produced empty result against live GitHub"
    echo "  raw header: $raw"
    exit 1
  fi
  if [[ ! "$resolved" =~ ^v[0-9]+\.[0-9]+\.[0-9]+ ]]; then
    echo "FAIL: resolve_version produced unexpected value: '$resolved'"
    exit 1
  fi
  echo "    resolved -> $resolved"
fi

echo "==> all install.sh smoke checks passed"
