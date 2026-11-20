---
name: work
description: Run a coding task in the current repo as a graph under a Haruspex Reading. It plans units mapped to acceptance criteria, mitigates non-critical risks itself, builds test-first in parallel on the cheapest proven model, gates on real checks, reviews independently against the criteria, and learns weighted rules. Only when the user types /workgraph:work.
disable-model-invocation: true
argument-hint: <task>
---

If no task came with the command, ask for it in one line. Run every command below from the repo
root. Mechanical steps go through the `wg` binary, which prints one JSON line; read its fields,
don't redo its work by hand.

## 1. Find the tools

**Haruspex CLI**, the first that exists, as an absolute path:
`<repo root>/.haruspex/bin/haruspex.py`, the newest
`~/.claude/plugins/cache/haruspex/haruspex/*/scripts/haruspex.py`, `~/Code/nodens/haruspex/scripts/haruspex.py`.
If none exists, stop: `claude plugin marketplace add pxm-0/haruspex` then `claude plugin install haruspex@haruspex`.

**wg**, the first that works (`<wg> recall` prints JSON):

1. `${CLAUDE_PLUGIN_ROOT}/wg/target/release/wg`
2. `${CLAUDE_PLUGIN_DATA}/wg-<version>-<target>`, where `<version>` is `version` in
   `${CLAUDE_PLUGIN_ROOT}/.claude-plugin/plugin.json` and `<target>` maps `uname -sm`:
   `Darwin arm64` → `aarch64-apple-darwin`, `Darwin x86_64` → `x86_64-apple-darwin`,
   `Linux x86_64` → `x86_64-unknown-linux-gnu`.
3. Not there yet: ask the user once to download it, naming the URL
   `https://github.com/pxm-0/workgraph/releases/download/v<version>/wg-<target>`. On yes, download
   it and its `.sha256` into `${CLAUDE_PLUGIN_DATA}`, run `shasum -a 256 -c wg-<target>.sha256` there,
   and only if that passes rename it to `wg-<version>-<target>` and `chmod +x` it.
4. No release asset and `cargo` exists: `cargo build --release --manifest-path ${CLAUDE_PLUGIN_ROOT}/wg/Cargo.toml --target-dir ${CLAUDE_PLUGIN_DATA}/wg-build`.

## 2. Run the workflow

1. `<wg> recall` → keep the whole JSON object as `memory`.
2. `runId`: `openssl rand -hex 6`.
3. Call the Workflow tool with `name: "workgraph:run"` and
   `args: {"task": <the user's task text, verbatim>, "haruspexCli": <cli>, "wg": <wg>, "runId": <runId>, "memory": <memory>}`.
   If that name is not found, use `scriptPath: "${CLAUDE_PLUGIN_ROOT}/workflows/run.js"` with the same args.
4. If the result has `memoryToSave`, write it to a file in your scratchpad and run
   `<wg> save < that-file`. Report any `error` it prints.

## 3. If it returns `needs_decision`

Nothing was built. The user's policy is to auto-proceed and mitigate everything below critical,
so these are the decisions only they can make. Ask them in the style of the
`mattpocock-skills:grilling` skill: one concrete decision per question, your recommendation first
and marked "(Recommended)", using AskUserQuestion (up to 4 questions per call). Then run section 2
again (fresh `recall` and `runId`) with these extra args copied from that result: `"taskId"`,
`"initialized"`, `"userChangesBefore"`, plus `"decisions": [{"id": <escalation id>, "answer": <the
user's answer in their words>}]`. Repeat until the status is no longer `needs_decision`. For the
`DIRTY` escalation, if the user chooses to commit or stash first, stop and let them; send `DIRTY` as
a decision only when they choose to let the run edit on top of their changes.

## 4. Report

In a few lines: status, branch, Haruspex task id (and whether the harness was just initialized),
done / skipped / stuck units with reasons, whether checks gated the result (`verified: false`
means nothing was checked), the independent review (passed, criterion verdicts, open findings),
files changed outside the plan, the model each unit ran on, rules added / revised / pruned with
top weights, and the `proposals` (guardrail ideas from the retro lens: list them, never apply
them unasked). Run `git diff --stat` and include it. Skipped risky units are the user's to do, or
to approve one at a time.

## 5. The user's one approval

If `checksAlreadyFailing` is not empty: Haruspex `verify` runs every check in the task's
`required_checks`, so those already-broken checks would block the verification gate. Name them
(the keys in `.haruspex/project.json` `commands` whose value matches) and ask the user to either
fix them first or remove them from `required_checks` with a note in the task's `notes` saying they
were already failing before this task. Never remove one without their yes.

Then ask once, with AskUserQuestion: "Approve the Haruspex build gate for <taskId> and commit?
It is recorded as approved after the build, under your auto-proceed policy." Options: approve and
commit (Recommended when the review passed), change something, or discard. Never approve on the
user's behalf; for discard, show what would be deleted and confirm before deleting anything.

On approval, one command does the commit and every Haruspex step:

```
<wg> finalize --task <taskId> --by "$(git config user.name)" --cli <repo>/.haruspex/bin/haruspex.py \
  --exclude-json '<userChangesBefore as a JSON array>' \
  --message "<feat: or fix: summary>

<attribution trailer>" \
  --evidence-message "chore: record Haruspex verification for <taskId>

<attribution trailer>"
```

It stages everything but the excluded paths, commits, approves and records the build gate, sets
the task to verifying, runs `verify` and the verification gate, and commits `.haruspex/`, stopping
at the first failure. If `passed` is false, show `blockedAt` and that step's `outputTail`: no PR.
If it passed, push the branch with `-u` and open a PR whose body lists the criteria with their
evidence, the review findings, and a test plan. Then report the PR link.
