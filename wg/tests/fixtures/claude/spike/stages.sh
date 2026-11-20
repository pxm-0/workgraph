#!/bin/bash
# usage: stages.sh <t1|t2> "<task text>"   runs read, challenge, work, fix, review, learn shaped calls (haiku) in repo <t>.
S=$(cd "$(dirname "$0")" && pwd); t=$1; task=$2; W="$S/$t"
base=(--output-format json --model haiku --permission-mode dontAsk --setting-sources user --strict-mcp-config --mcp-config '{"mcpServers":{}}')
H='If a hook blocks a tool call and asks for facts, state them in one line and retry once.'
s_read='{"type":"object","properties":{"summary":{"type":"string"},"files":{"type":"array","items":{"type":"string"}},"risky":{"type":"boolean"}},"required":["summary","files","risky"],"additionalProperties":false}'
s_chal='{"type":"object","properties":{"objections":{"type":"array","items":{"type":"string"}},"ready":{"type":"boolean"}},"required":["objections","ready"],"additionalProperties":false}'
s_work='{"type":"object","properties":{"changed_files":{"type":"array","items":{"type":"string"}},"summary":{"type":"string"},"checks_passed":{"type":"boolean"}},"required":["changed_files","summary","checks_passed"],"additionalProperties":false}'
s_rev='{"type":"object","properties":{"verdict":{"type":"string","enum":["pass","fail"]},"findings":{"type":"array","items":{"type":"string"}}},"required":["verdict","findings"],"additionalProperties":false}'
s_learn='{"type":"object","properties":{"rules":{"type":"array","items":{"type":"string"}}},"required":["rules"],"additionalProperties":false}'
w=(Read Glob Grep "Edit(/$W/calc.py)" 'Bash(python3 check.py)')
REPO=$t "$S/run.sh" "stage_${t}_read" "Task: $task. Read the repo and return a Reading: a one-sentence summary, the files the change touches, and whether it is risky." "${base[@]}" --max-turns 8 --json-schema "$s_read" --allowedTools Read Glob Grep
REPO=$t "$S/run.sh" "stage_${t}_challenge" "Task: $task. The Reading says the change touches calc.py only. Challenge it: list objections (missing cases, wrong files) and say whether it is ready to build." "${base[@]}" --max-turns 8 --json-schema "$s_chal" --allowedTools Read Glob Grep
REPO=$t "$S/run.sh" "stage_${t}_work" "Task: $task. Edit calc.py only, then run the check with Bash: python3 check.py. $H" "${base[@]}" --max-turns 12 --json-schema "$s_work" --allowedTools "${w[@]}"
REPO=$t "$S/run.sh" "stage_${t}_fix" "Task: $task. A reviewer found: the function lacks a one-line docstring. Add it in calc.py, then run python3 check.py with Bash. $H" "${base[@]}" --max-turns 12 --json-schema "$s_work" --allowedTools "${w[@]}"
REPO=$t "$S/run.sh" "stage_${t}_review" "Task: $task. Review the uncommitted change: run git diff and python3 check.py with Bash, read calc.py, and give a verdict with findings." "${base[@]}" --max-turns 10 --json-schema "$s_rev" --allowedTools Read Glob Grep 'Bash(git diff)' 'Bash(python3 check.py)'
REPO=$t "$S/run.sh" "stage_${t}_learn" "A run of task '$task' finished: read, challenge, work and fix succeeded, review passed. A hook asked for facts before the first edit. Propose at most two short rules for future runs." "${base[@]}" --max-turns 3 --json-schema "$s_learn"
