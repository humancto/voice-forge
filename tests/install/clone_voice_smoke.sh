#!/usr/bin/env bash
# Dry-run smoke for scripts/clone_voice.sh.
#
# Stages a fake INSTALLED.toml + a tiny WAV source, then runs
# clone_voice.sh in DRY_RUN mode. Asserts step labels in order and the
# invalid-name + missing-source error paths.

set -Eeuo pipefail
IFS=$'\n\t'
export LC_ALL=C
unset CDPATH

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CLONE_SH="$REPO_ROOT/scripts/clone_voice.sh"

[[ -x "$CLONE_SH" ]] || { echo "missing: $CLONE_SH"; exit 1; }

tmp_home="$(mktemp -d)"
trap 'rm -rf "$tmp_home"' EXIT

mkdir -p "$tmp_home/cloning"
cat > "$tmp_home/cloning/INSTALLED.toml" <<EOF
schema_version = 1
gpt_sovits_sha = "fake"
python_path = "/x"
ffmpeg6_prefix = "/x"
EOF

fake_src="$tmp_home/source.wav"
printf 'RIFF\0\0\0\0WAVE' > "$fake_src"

echo "==> dry-run: clone trump_test \$fake_src"
output="$(VOICEFORGE_HOME="$tmp_home" VOICEFORGE_CLONE_DRY_RUN=1 \
  bash "$CLONE_SH" trump_test "$fake_src" 0 2>&1)"
echo "$output"
echo ""

required=(
  "voiceforge clone trump_test"
  "staging into"
  "writing profile.toml"
  "promoting"
  "done"
)
prev=-1
for label in "${required[@]}"; do
  pos=$(printf '%s\n' "$output" | grep -nF "$label" | head -1 | cut -d: -f1)
  [[ -n "$pos" ]] || { echo "FAIL: missing label '$label'"; exit 1; }
  (( pos > prev )) || { echo "FAIL: label '$label' out of order"; exit 1; }
  prev="$pos"
done

if [[ -d "$tmp_home/voices/trump_test" ]]; then
  echo "FAIL: dry-run leaked a real voice dir"
  exit 1
fi

echo "==> missing-source must error"
set +e
out2="$(VOICEFORGE_HOME="$tmp_home" VOICEFORGE_CLONE_DRY_RUN=1 \
  bash "$CLONE_SH" trump_test2 /nonexistent/source.wav 0 2>&1)"
rc=$?
set -e
[[ "$rc" -eq 0 ]] && { echo "FAIL: expected non-zero exit on missing source"; echo "$out2"; exit 1; }
echo "$out2" | grep -q "not found" \
  || { echo "FAIL: missing 'not found'"; echo "$out2"; exit 1; }

echo "==> invalid-name must error"
set +e
out3="$(VOICEFORGE_HOME="$tmp_home" VOICEFORGE_CLONE_DRY_RUN=1 \
  bash "$CLONE_SH" "../etc" "$fake_src" 0 2>&1)"
rc=$?
set -e
[[ "$rc" -eq 0 ]] && { echo "FAIL: expected non-zero exit on invalid name"; exit 1; }
echo "$out3" | grep -q "invalid voice name" \
  || { echo "FAIL: missing 'invalid voice name'"; echo "$out3"; exit 1; }

if command -v shellcheck >/dev/null 2>&1; then
  echo "==> shellcheck"
  shellcheck -e SC1091 "$CLONE_SH"
fi

echo "==> all clone_voice.sh smoke checks passed"
