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

# On any unexpected failure, print the failing line and command so the operator
# does not have to re-read the script to figure out where it bailed.
on_err() {
  local exit_code=$?
  local line=$1
  echo ""
  echo "FAIL: smoke test aborted at line ${line} (exit ${exit_code})" >&2
  echo "      command: ${BASH_COMMAND}" >&2
  exit "$exit_code"
}
trap 'on_err $LINENO' ERR

# expect_contains <haystack> <needle> <context>
# Prints the failure with full context if the substring is missing. Use this
# instead of `grep -q` so silent grep failures cannot mask a regression under
# `set -e`.
expect_contains() {
  local haystack=$1
  local needle=$2
  local context=$3
  if ! printf '%s' "$haystack" | grep -qF -- "$needle"; then
    echo "FAIL: ${context}" >&2
    echo "      expected substring: ${needle}" >&2
    echo "      actual output:" >&2
    printf '%s\n' "$haystack" | sed 's/^/        /' >&2
    exit 1
  fi
}

REEVE_BIN="${REEVE_BIN:-./target/release/reeve}"

if [[ ! -x "$REEVE_BIN" ]]; then
  echo "FAIL: $REEVE_BIN not found or not executable. Run: cargo build --release" >&2
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
LIST_OUTPUT="$("$REEVE_BIN" identity list)"
expect_contains "$LIST_OUTPUT" "Smoke Test Operator" \
  "'identity list' did not include the just-enrolled operator"

echo "==> Checking daemon status (should be 'not running')..."
STATUS="$("$REEVE_BIN" daemon status)"
echo "    $STATUS"
expect_contains "$STATUS" "not running" \
  "'daemon status' did not report 'not running' on a fresh state dir"

echo "==> Unenrolling operator identity..."
"$REEVE_BIN" identity unenroll --confirm

echo "PASS: smoke test complete"
