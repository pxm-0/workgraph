---
name: work
description: Run a coding task in the current repo as a graph under a Haruspex Reading. It plans units mapped to acceptance criteria, mitigates non-critical risks itself, builds test-first in parallel on the cheapest proven model, gates on real checks, reviews independently against the criteria, and learns weighted rules. Only when the user types /workgraph:work.
disable-model-invocation: true
argument-hint: <task>
---

If no task came with the command, ask for it in one line.

## 1. Find the Haruspex CLI

Use the first that exists, as an absolute path:

1. `<repo root>/.haruspex/bin/haruspex.py`
2. The newest `~/.claude/plugins/cache/haruspex/haruspex/*/scripts/haruspex.py`
3. `~/Code/nodens/haruspex/scripts/haruspex.py`

If none exists, stop and tell the user to install it:
`claude plugin marketplace add pxm-0/haruspex` then `claude plugin install haruspex@haruspex`.

## 2. Run the workflow

Call the Workflow tool with `name: "workgraph:run"` and
`args: {"task": <the user's task text, verbatim>, "haruspexCli": <absolute CLI path>}`.
If that name is not found, use `scriptPath: "${CLAUDE_PLUGIN_ROOT}/workflows/run.js"` with the same args.

## 3. If it returns `needs_decision`

Nothing was built. The user's policy is to auto-proceed and mitigate everything below critical,
so these are the decisions only they can make. Ask them in the style of the
`mattpocock-skills:grilling` skill: one concrete decision per question, your recommendation first
and marked "(Recommended)", using AskUserQuestion (up to 4 questions per call). Then call the
workflow again with the same args plus `"taskId"`, `"initialized"` and `"userChangesBefore"` copied
from that result, and `"decisions": [{"id": <escalation id>, "answer": <the user's answer in their
words>}]`. Repeat until the status is no longer `needs_decision`. For the `DIRTY` escalation, if the
user chooses to commit or stash first, stop and let them; send `DIRTY` as a decision only when they
choose to let the run edit on top of their changes.

## 4. Report

In a few lines: status, branch, Haruspex task id (and whether the harness was just initialized),
done / skipped / stuck units with reasons, whether checks gated the result (`verified: false`
means nothing was checked), the independent review (passed, criterion verdicts, open findings),
files changed outside the plan, the model each unit ran on, rules added / revised / pruned with
top weights, and the `proposals` (guardrail ideas from the retro lens: list them, never apply
them unasked). Run `git diff --stat` and include it. If `memorySaved` is false, say so.
Skipped risky units are the user's to do, or to approve one at a time.

## 5. The user's one approval

Ask once, with AskUserQuestion: "Approve the Haruspex build gate for <taskId> and commit?
It is recorded as approved after the build, under your auto-proceed policy." Options: approve and
commit (Recommended when the review passed), change something, or discard. Never approve on the
user's behalf; for discard, show what would be deleted and confirm before deleting anything.

Before that question, if `checksAlreadyFailing` is not empty: Haruspex `verify` runs every check in
the task's `required_checks`, so those already-broken checks would block the verification gate.
Name them (their names are the keys in `.haruspex/project.json` `commands` whose value matches),
and ask the user to either fix them first or remove them from `required_checks` with a note in the
task's `notes` saying they were already failing before this task. Never remove one without their yes.

On approval, from the repo root, with `<cli>` = `.haruspex/bin/haruspex.py`:

1. Stage everything the run changed: every path in `git status` except those in
   `userChangesBefore`. Commit on the run's branch with a conventional message
   (`feat: …` or `fix: …`) and the session's attribution trailer.
2. `python3 <cli> approve ready-to-build . --by "<git config user.name>" --note "Approved after the autonomous build under the auto-proceed policy"`
3. `python3 <cli> gate ready-to-build . --record`. If blocked, show the blockers and stop.
4. Set `"status": "verifying"` in `.haruspex/tasks/<taskId>.json`.
5. `python3 <cli> verify .`, then `python3 <cli> gate verification . --record`. If blocked, show
   the blockers and stop: no PR.
6. Commit `.haruspex/` as `chore: record Haruspex verification for <taskId>`, push the branch
   with `-u`, and open a PR whose body lists the criteria with their evidence, the review
   findings, and a test plan. Then report the PR link.
