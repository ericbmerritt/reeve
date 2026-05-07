#!/usr/bin/env bash
# Smoke test: verify reeve works end-to-end from fresh state.
#
# Requirements:
#   - reeve binary at ./target/release/reeve (or REEVE_BIN set)
#   - OS keychain accessible (macOS Keychain or Linux Secret Service)
#
# Does NOT require:
#   - Anthropic API key
#   - TTY
#
# Skips:
#   - TUI launch (requires TTY)
#   - Model calls (require API key)
#
# On Linux the Secret Service daemon (e.g. gnome-keyring or kwallet) must be
# running. In headless CI, set up a keyring session before invoking this script.
set -euo pipefail

REEVE_BIN="${REEVE_BIN:-./target/release/reeve}"

if [[ ! -x "$REEVE_BIN" ]]; then
  echo "FAIL: $REEVE_BIN not found or not executable. Run: cargo build --release"
  exit 1
fi

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT
export XDG_STATE_HOME="$TMPDIR/state"
export XDG_DATA_HOME="$TMPDIR/data"

echo "==> Enrolling operator identity..."
# prompt_one_line writes the prompt to stderr and reads one line from stdin.
printf "Smoke Test Operator\n" | "$REEVE_BIN" identity enroll

echo "==> Listing identities..."
"$REEVE_BIN" identity list | grep -q "Smoke Test Operator"

echo "==> Checking daemon status (should be 'no runtime')..."
STATUS="$("$REEVE_BIN" daemon status)"
echo "    $STATUS"
echo "$STATUS" | grep -q "no runtime"

echo "==> Unenrolling operator identity..."
"$REEVE_BIN" identity unenroll --confirm

echo "PASS: smoke test complete"
