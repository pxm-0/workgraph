---
name: work
description: Run a coding task in the current repo as a graph. It plans units, builds independent ones in parallel on the cheapest proven model, loops each on real check commands (max 3 fixes, one model tier up per round), and learns weighted rules for the next run. Only when the user types /workgraph:work.
disable-model-invocation: true
argument-hint: <task>
---

If no task came with the command, ask for it in one line. Otherwise call the Workflow tool
with `name: "workgraph:run"` and `args: {"task": <the user's task text, verbatim>}`. If that
name is not found, call it with `scriptPath: "${CLAUDE_PLUGIN_ROOT}/workflows/run.js"` and
the same `args`.

When it returns:

1. Report in a few lines: status, branch, done / skipped / stuck units with reasons, whether
   checks gated the result (`verified: false` means nothing was checked), files changed outside
   the plan, which model built each unit, and rules added / revised / pruned with the top
   weights. Run `git diff --stat` and include it. Memory lives in `.claude/work-memory.json`;
   if `memorySaved` is false, say so.
2. Nothing is committed. The last step is the user's: ask whether to commit and open the PR,
   change something, or discard.
3. Skipped risky units are the user's to do, or to approve one at a time. Never run one
   without a clear yes.
