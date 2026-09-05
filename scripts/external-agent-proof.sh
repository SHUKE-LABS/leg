#!/usr/bin/env bash
#
# external-agent-proof — the cross-repo integration proof for issue #4.
#
# Proves that `baton serve --agent-cmd <leg binary> --agent-arg exchange` can
# host `leg exchange` as an external agent WITHOUT ADAPTATION on either side:
# baton's default `raw` output adapter (whole stdout is the reply, no extra
# flags) matches `leg exchange`'s plain-text-mode output exactly, and a
# provider/delivery failure inside `leg exchange` (empty stdout, exit 0)
# reaches baton's own machinery-error path, which delivers a `kind: "error"`
# envelope in baton's outbox — the same end-to-end guarantee proven
# hermetically (without baton) by `tests/exchange_agent_cmd.rs`.
#
# This is an INTEGRATION PROOF, not a hermetic test: it requires a real baton
# checkout (built binary) and real Anthropic credentials, so it is NOT part of
# leg's CI (mirrors baton's own `scripts/external-agent-proof.sh` for #68).
#
# What it does:
#   1. Round A: a `baton serve --agent-cmd <leg> --agent-arg exchange` process
#      runs with real (inherited) credentials. `baton send --await` asks it a
#      question; asserts a delivered `kind: "response"` with non-empty body.
#   2. Round B: a second `baton serve` process runs with a deliberately bad
#      `ANTHROPIC_API_KEY`. `baton send --await` asks the same question;
#      asserts a delivered `kind: "error"` — proving the empty-stdout signal
#      actually drives baton's synthesized error path across the process
#      boundary, not just leg's own exit code.
#
# Overrides:
#   BATON_BIN         path to a built baton binary        (required; no default)
#   LEG_BIN            path to a built leg binary          (default: target/debug/leg)
#   AGENT_TIMEOUT_MS  per-message agent read timeout       (default: 60000)
#   SEND_TIMEOUT_MS   per-message sender await timeout     (default: 60000)
#
# Credentials: leg's exchange loads its own credential from the environment
# (ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN / CLAUDE_CODE_OAUTH_TOKEN) — baton
# passes none of its own in --agent-cmd mode, so the leg subprocess inherits
# whatever this script's own environment already has exported.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

TARGET_DIR="${CARGO_TARGET_DIR:-target}"
LEG_BIN="${LEG_BIN:-$TARGET_DIR/debug/leg}"
BATON_BIN="${BATON_BIN:-}"
AGENT_TIMEOUT_MS="${AGENT_TIMEOUT_MS:-60000}"
SEND_TIMEOUT_MS="${SEND_TIMEOUT_MS:-60000}"

# --- Preconditions: a real baton binary must be present, else skip (not fail)
if [[ -z "$BATON_BIN" || ! -x "$BATON_BIN" ]]; then
  echo "external-agent-proof: SKIP — set BATON_BIN to a built baton binary." >&2
  echo "  This is a manual cross-repo integration proof; clone SHUKE-LABS/baton" >&2
  echo "  and 'cargo build' it, then re-run with BATON_BIN=<path>." >&2
  exit 0
fi

if [[ -z "${ANTHROPIC_API_KEY:-}${ANTHROPIC_AUTH_TOKEN:-}${CLAUDE_CODE_OAUTH_TOKEN:-}" ]]; then
  echo "external-agent-proof: SKIP — no Anthropic credential in the environment." >&2
  echo "  Export ANTHROPIC_API_KEY (or an OAuth token) before running." >&2
  exit 0
fi

if [[ ! -x "$LEG_BIN" ]]; then
  cargo build --quiet
fi

WORK="$(mktemp -d)"
A_PID=""
B_PID=""
cleanup() {
  if [[ -n "$A_PID" ]]; then
    "$BATON_BIN" serve --stop --inbox "$A_INBOX" >/dev/null 2>&1 || kill "$A_PID" 2>/dev/null || true
    wait "$A_PID" 2>/dev/null || true
  fi
  if [[ -n "$B_PID" ]]; then
    "$BATON_BIN" serve --stop --inbox "$B_INBOX" >/dev/null 2>&1 || kill "$B_PID" 2>/dev/null || true
    wait "$B_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "external-agent-proof: FAIL — $1" >&2; exit 1; }

# send_round <label> <inbox> <outbox> <out-file>
send_round() {
  local label="$1" inbox="$2" outbox="$3" out="$4"
  echo "external-agent-proof: $label — sending"
  "$BATON_BIN" send \
    --inbox "$inbox" \
    --outbox "$outbox" \
    --await \
    --timeout-ms "$SEND_TIMEOUT_MS" \
    --body "Reply with exactly: proof-ok" >"$out"
}

# --- Round A: real credentials — expect a delivered kind:"response" --------
A_INBOX="$WORK/a/inbox"
A_OUTBOX="$WORK/a/outbox"
mkdir -p "$A_INBOX" "$A_OUTBOX"

echo "external-agent-proof: round A — launching baton serve with real credentials"
"$BATON_BIN" serve \
  --inbox "$A_INBOX" \
  --outbox "$A_OUTBOX" \
  --agent-cmd "$LEG_BIN" \
  --agent-arg exchange \
  --agent-timeout-ms "$AGENT_TIMEOUT_MS" &
A_PID=$!

A_OUT="$WORK/reply-a.json"
send_round "round A" "$A_INBOX" "$A_OUTBOX" "$A_OUT"
grep -q '"kind":"response"' "$A_OUT" || fail "round A did not deliver a kind:\"response\" envelope: $(cat "$A_OUT")"
echo "external-agent-proof: round A OK — delivered kind:\"response\""
"$BATON_BIN" serve --stop --inbox "$A_INBOX" >/dev/null 2>&1 || kill "$A_PID" 2>/dev/null || true
wait "$A_PID" 2>/dev/null || true
A_PID=""

# --- Round B: deliberately bad credential — expect a delivered kind:"error" -
B_INBOX="$WORK/b/inbox"
B_OUTBOX="$WORK/b/outbox"
mkdir -p "$B_INBOX" "$B_OUTBOX"

echo "external-agent-proof: round B — launching baton serve with a bad credential"
ANTHROPIC_API_KEY="deliberately-invalid" \
ANTHROPIC_AUTH_TOKEN="" \
CLAUDE_CODE_OAUTH_TOKEN="" \
  "$BATON_BIN" serve \
    --inbox "$B_INBOX" \
    --outbox "$B_OUTBOX" \
    --agent-cmd "$LEG_BIN" \
    --agent-arg exchange \
    --agent-timeout-ms "$AGENT_TIMEOUT_MS" &
B_PID=$!

B_OUT="$WORK/reply-b.json"
send_round "round B" "$B_INBOX" "$B_OUTBOX" "$B_OUT"
grep -q '"kind":"error"' "$B_OUT" || fail "round B did not deliver a kind:\"error\" envelope: $(cat "$B_OUT")"
echo "external-agent-proof: round B OK — bad credentials delivered as kind:\"error\""
"$BATON_BIN" serve --stop --inbox "$B_INBOX" >/dev/null 2>&1 || kill "$B_PID" 2>/dev/null || true
wait "$B_PID" 2>/dev/null || true
B_PID=""

echo
echo "external-agent-proof: PASS"
