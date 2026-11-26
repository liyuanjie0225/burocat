#!/usr/bin/env bash
# Agent embodiment demo: drives the cat through a full AI-coworker turn without a
# real agent. POSTs hook-style events to the loopback registry (127.0.0.1:27422)
# exactly like `KuroCat --agent-hook zcode` does.
#
# Usage:
#   ./scripts/agent-embodiment-demo.sh              # full turn: work -> wait -> done (confetti)
#   ./scripts/agent-embodiment-demo.sh --interrupted # same, but the turn ends interrupted (sigh)
#
# The cat should: fake-type with tool-name bubbles while working -> look up at you
# with a "⏸ tool" bubble on the permission request -> resume typing once answered
# -> celebrate with confetti (or sigh when interrupted). The pill + done notice
# ride along as usual. Quit the production app first so the dev build owns 27422.
set -euo pipefail

PORT="${KUROCAT_AGENT_PORT:-27422}"
SESSION="demo-$(date +%s)"
MODE="full"
for arg in "$@"; do
  case "$arg" in
    --interrupted) MODE="interrupted" ;;
    *) echo "unknown flag: $arg (supported: --interrupted)" >&2; exit 2 ;;
  esac
done

post() {
  local payload="$1"
  curl -sf -o /dev/null -X POST "http://127.0.0.1:${PORT}/agent-event" \
    -H 'Content-Type: application/json' -d "$payload" \
    || { echo "event lost (is the pet running and owning port ${PORT}?)" >&2; exit 1; }
}

step() { echo "→ $1"; }
hold() { echo "  … hold ${1}s"; sleep "$1"; }

if ! curl -sf -o /dev/null "http://127.0.0.1:${PORT}/agent-state"; then
  echo "No agent registry on 127.0.0.1:${PORT} — is KuroCat running?" >&2
  exit 1
fi

step "session info (model)"
post "{\"source\":\"zcode\",\"event\":\"info\",\"session\":\"${SESSION}\",\"model\":\"GLM-5.4\"}"

step "turn starts — the cat sits down and starts typing"
post "{\"source\":\"zcode\",\"event\":\"start\",\"session\":\"${SESSION}\",\"label\":\"写一个排序脚本\"}"
hold 3

for tool in Bash Read Edit; do
  step "tool: ${tool} — strike burst + tool bubble"
  post "{\"source\":\"zcode\",\"event\":\"tool\",\"session\":\"${SESSION}\",\"tool\":\"${tool}\"}"
  hold 4
done

step "permission needed — cat looks up at you, waiting"
post "{\"source\":\"zcode\",\"event\":\"permission\",\"session\":\"${SESSION}\",\"tool\":\"Bash\"}"
hold 8

step "answered (tool ran) — cat goes back to typing"
post "{\"source\":\"zcode\",\"event\":\"tool\",\"session\":\"${SESSION}\",\"tool\":\"Write\"}"
hold 5

if [ "$MODE" = "interrupted" ]; then
  step "turn interrupted — cat sighs"
  post "{\"source\":\"zcode\",\"event\":\"done\",\"session\":\"${SESSION}\",\"outcome\":\"interrupted\"}"
else
  step "turn done — confetti + done notice"
  post "{\"source\":\"zcode\",\"event\":\"done\",\"session\":\"${SESSION}\",\"label\":\"写一个排序脚本\",\"message\":\"写好了 sort.sh\"}"
fi

echo
echo "Done. (Quiet yawn path: run a start, then wait 90s with no events.)"
echo "Live registry state: curl -s http://127.0.0.1:${PORT}/agent-state | jq ."
