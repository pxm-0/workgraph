#!/bin/sh
cat >/dev/null; echo fired >> <spike>/markers/ask.log
printf '%s' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"spike ask"}}'
