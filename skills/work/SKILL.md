---
name: work
description: Run a coding task in the current repo as a graph under a Haruspex Reading. It plans units mapped to acceptance criteria, mitigates non-critical risks itself, builds test-first in parallel on the cheapest proven model, gates on real checks, reviews independently against the criteria, and learns weighted rules. Only when the user types /workgraph:work.
disable-model-invocation: true
argument-hint: <task> | approve <owner/repo>#<pr>
---

If the command starts with `approve`, follow **section 8** (a shift's draft PR).

If no task came with the command, ask for it in one line. Run every command below from the repo
root. `wg run` drives the whole pipeline: the kernel, not this skill, runs every check, calls
every agent and holds the evidence. Every `wg` command prints one JSON line; read its fields,
don't redo its work by hand.

## 1. Find `wg`

The first that works (`<wg> recall` prints JSON):

1. `~/.workgraph/bin/current/wg`
2. `${CLAUDE_PLUGIN_ROOT}/target/release/wg`
3. `${CLAUDE_PLUGIN_DATA}/wg-<version>-<target>`, where `<version>` is `version` in
   `${CLAUDE_PLUGIN_ROOT}/.claude-plugin/plugin.json` and `<target>` maps `uname -sm`:
   `Darwin arm64` → `aarch64-apple-darwin`, `Darwin x86_64` → `x86_64-apple-darwin`,
   `Linux x86_64` → `x86_64-unknown-linux-gnu`.
4. `${CLAUDE_PLUGIN_DATA}/wg-build/release/wg`, built by step 6 on an earlier run.
5. None of these yet: ask the user once to download it, naming the URL
   `https://github.com/pxm-0/workgraph/releases/download/v<version>/wg-<target>`. On yes, download
   it and its `.sha256` into `${CLAUDE_PLUGIN_DATA}`, run `shasum -a 256 -c wg-<target>.sha256` there,
   and only if that passes rename it to `wg-<version>-<target>` and `chmod +x` it.
6. No release asset and `cargo` exists: `cargo build --release --locked -p wg --manifest-path
   ${CLAUDE_PLUGIN_ROOT}/Cargo.toml --target-dir ${CLAUDE_PLUGIN_DATA}/wg-build`, then use
   `${CLAUDE_PLUGIN_DATA}/wg-build/release/wg`.

Do not look for a Haruspex CLI: `wg` uses only the one pinned in `~/.workgraph/factory.toml`.

## 2. Once per Mac, with the owner's yes for each

- If `<wg> doctor` reports `haruspex pin` failing, show
  `<wg> config pin-haruspex --cli <absolute path to haruspex.py>` (the newest
  `~/.claude/plugins/cache/haruspex/haruspex/*/scripts/haruspex.py`, never one inside a repo) and
  run it only after a yes. If no Haruspex is installed, stop:
  `claude plugin marketplace add pxm-0/haruspex` then `claude plugin install haruspex@haruspex`.
  Report any other failing doctor check and stop.
- Offer to add `ask` rules to the owner's user settings for `Bash(*wg finalize*)`,
  `Bash(*wg approve*)`, `Bash(*wg self*)` and `Bash(*wg config*)`, so even a hijacked session hits
  a real prompt. Add them only after a yes; `wg doctor` accepts exactly these `ask` rules.

## 3. Run

1. Write the task text, verbatim, to a file in your scratchpad.
2. Run `<wg> run --task-file <file>` from the repo root with Bash `run_in_background` (runs
   outlast the foreground limit) and read its one JSON line when it finishes. `error` means it
   refused to start: show it and stop.
3. The fields: `status, branch, haruspex, verified, reviewed, review, checks, checksAlreadyFailing,
   done, skipped, stuck, fixRounds, reviewFixRounds, outOfScope, userChangesBefore, models, rules,
   proposals, next, runId, costUsd, spend, callsWithUnknownCost, memorySaved, logDir`.
   `verified`, `reviewed` and `checks` come from the kernel's own evidence.

## 4. If it returns `needs_decision`

Nothing was built. The user's policy is to auto-proceed and mitigate everything below critical,
so these are the decisions only they can make. Ask them in the style of the
`mattpocock-skills:grilling` skill: one concrete decision per question, your recommendation first
and marked "(Recommended)", using AskUserQuestion (up to 4 questions per call).

- `CHECKS`: the question names each command; on the owner's yes for one, run
  `<wg> config allow-check --repo <owner/repo> --name <name> --cmd <command> --factory-cmd <command>`
  (both strings as shown; a command that is not `docker compose ...` runs on the Mac, say so).
- `DIRTY`: if the user chooses to commit or stash first, stop and let them; send `DIRTY` as a
  decision only when they choose to let the run edit on top of their changes.
- `BASELINE` (every required check already fails): the owner fixes the checks or removes them from
  `required_checks` with a note; never you.

Then write the answers as `[{"id": <escalation id>, "answer": <the user's answer in their words>}]`
and `userChangesBefore` as a JSON array to scratch files and rerun with `--task-id <taskId>
--initialized --user-changes-file <f> --decisions-file <f>`. Repeat until the status is no longer
`needs_decision`.

## 5. If it returns `parked` or `failed`

Show `note` (or `incident`) and `logDir`; edits stay uncommitted. An `incident` means an agent
tried something its role forbids: say so plainly and stop.

## 6. Report

In a few lines: status, branch, Haruspex task id (and whether the harness was just initialized),
done / skipped / stuck units with reasons, whether checks gated the result (`verified: false`
means nothing was checked), the independent review (passed, criterion verdicts, open findings),
files changed outside the plan, the model each unit ran on, rules added / revised / pruned with
top weights, spend per stage and provider from `spend`, and the `proposals` (guardrail ideas from
the retro lens: list them, never apply them unasked). Run `git diff --stat` and include it.
Skipped risky units are the user's to do, or to approve one at a time.

## 7. The user's one approval

If `checksAlreadyFailing` is not empty: Haruspex `verify` runs every check in the task's
`required_checks`, so those already-broken checks would block the verification gate. Name them
(the keys in `.haruspex/project.json` `commands` whose value matches) and ask the user to either
fix them first or remove them from `required_checks` with a note in the task's `notes` saying they
were already failing before this task. Never remove one without their yes.

Then ask once, with AskUserQuestion: "Approve the Haruspex build gate for <taskId> and commit?
It is recorded as approved after the build, under your auto-proceed policy." Options: approve and
commit (Recommended when the review passed), change something, or discard. Never approve on the
user's behalf; for discard, show what would be deleted and confirm before deleting anything.

On approval:

```
<wg> finalize --run <runId> --by "$(git config user.name)" \
  --message "<feat: or fix: summary>

<attribution trailer>" \
  --evidence-message "chore: record Haruspex verification for <taskId>

<attribution trailer>"
```

It refuses unless the run's checks and independent review hold at the current tree; then it
commits (leaving `userChangesBefore` out), approves and records the build gate, verifies and
records the verification gate, and commits the evidence, stopping at the first failure. If
`passed` is false, show `blockedAt` and that step's `outputTail`, or the refusal: no PR. If it
passed, push the branch with `-u` and open a PR whose body lists the criteria with their evidence,
the review findings, and a test plan. Then report the PR link.

## 8. `approve <owner/repo>#<pr>`: a shift's draft PR

A nightly shift (`/workgraph:shift`) leaves draft PRs it built under the auto-proceed policy; the
owner approves each one here. Find `wg` as in section 1.

1. Run `<wg> approval-summary --repo <owner/repo> --pr <pr>` and show: title and URL, `verified`,
   `reviewed`, the review's criterion verdicts and open findings, `checks`, `checksAlreadyFailing`
   (handle them as in section 7), skipped risky units, and cost (`costUsd`, `spend` by provider).
   When `tainted` is true, also show `riskyBuilt` (each unit and its `risky:<category>` label) and
   `sandboxRuns` (job, label, steps): those were built and checked only in the sim sandbox, and
   approving verifies there too (a repo without `[repo.sandbox].verify` is refused; say so).
   If `headMatches` is false, someone pushed to the PR after the shift: say so and stop.
2. Ask once, with AskUserQuestion: "Approve the Haruspex build gate for PR #<pr> and make it ready
   for review? It is recorded as approved after the build, under your auto-proceed policy."
   Options: approve (Recommended when verified and reviewed), change something, leave it. Never
   approve on the owner's behalf.
3. On approve, run `<wg> approve --repo <owner/repo> --pr <pr> --by "$(git config user.name)"`. It
   hits the `ask` permission prompt, re-checks the run's kernel evidence at the PR head in the
   task worktree, approves and records the build gate, verifies, records the verification gate,
   commits the evidence, pushes the task branch, marks the PR ready and labels the issue
   `wg:verified`. If `passed` is false, show `blockedAt` and that step's `outputTail`; an `error`
   is a refusal, show it. If the push or `gh pr ready` failed after finalize passed, the error says
   so and running the same command again finishes them without finalizing twice.
