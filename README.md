# workgraph

A Claude Code plugin that runs a coding task as a graph and gets better per repo.

```
/workgraph:work add rate limiting to the login endpoint
```

1. **Plan** (opus, high effort): split the task into up to 5 units with the files each may
   touch, their dependencies, a size, and a risk flag. Find the repo's check commands.
2. **Baseline** (haiku): run the checks before any edit. Commands that already fail are left
   out of the gate.
3. **Build**: independent units run in parallel. Each unit gets the cheapest model tier that
   has proven itself on units of that size in this repo.
4. **Check** (haiku): exit codes decide. A failure goes back only to the unit it names, one
   model tier up per round, at most 3 rounds.
5. **Learn**: score the rules that were in play, turn fixed failures into new rules, update
   model stats, save to `.claude/work-memory.json`.
6. **You**: nothing is committed. Review the diff and decide.

Risky units (migrations, deletes, deploys, auth, payments, secrets, CI) never run; they are
listed for you.

## Memory

One file per repo, `.claude/work-memory.json`:

- **Rules**: each keeps `a` (for) and `b` (against) counts; weight is a ÷ (a + b). The top 8
  with weight ≥ 0.4 go into the planner or worker prompts. A rule in play gets +0.25 when its
  failure stays away and +1 against when it comes back. Below 0.3 after 4 observations it is
  deleted. Counts are capped at 20.
- **Models**: pass/fail per tier per unit size. A tier is proven after 3+ tries at ≥ 75%
  first-pass (Laplace-smoothed). Each run probes one cheaper unproven tier on one unit.

If the file can't be read or parsed, the run doesn't save, so it can't overwrite it.

## Tuning

Settings are constants at the top of `workflows/run.js`: `MIN_OBS`, `TARGET`, `WINDOW`,
`RULES_PER_PROMPT`, `MIN_WEIGHT`, `PRUNE_BELOW`, `MAX_RULES`, `HELD`, `RECURRED`,
`MAX_FIX_ROUNDS`, `MAX_UNITS`.

## Install

```
claude plugin marketplace add pxm-0/workgraph
claude plugin install workgraph@workgraph
```

## Test

Control-flow tests run the real workflow script against fake agents. No dependencies.

```
node --test tests/
```
