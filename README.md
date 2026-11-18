# workgraph

A Claude Code plugin that runs a coding task as a graph under a
[Haruspex](https://github.com/pxm-0/haruspex) Reading, and gets better per repo.

```
/workgraph:work add rate limiting to the login endpoint
```

| Step | Model | What happens |
|---|---|---|
| Recall | haiku | Load the repo's rules and model stats; note your uncommitted work so the run never touches or commits it |
| Read | opus, high effort | Init Haruspex if the repo has none. Write the Reading: outcome, scope, acceptance criteria, assumptions, failure modes, test plan, core reviews. Split the work into up to 5 units, each owning criteria |
| Challenge | sonnet, high effort | Adversarial pass: assume the Reading is incomplete and add what it missed |
| Escalate | none (code) | Anything critical, or high and not mitigable in code and tests, stops the run here before any edit; you're asked only those |
| Build | learned per unit size | Test-first workers (`mattpocock-skills:tdd`), independent units in parallel, on the cheapest model proven in this repo |
| Check / Fix | haiku / one tier up per round | Exit codes decide. Only the blamed unit goes back (`mattpocock-skills:diagnosing-bugs`), max 3 rounds |
| Review | opus | Independent Spec and Standards review (`mattpocock-skills:code-review`) against the Haruspex criteria; writes verdicts into the task. One fix and re-review round |
| Learn | sonnet | Score the rules in play, turn fixed failures into rules, and propose guardrails in the `mattpocock-skills:retro` style (never applied unasked) |
| You | — | Approve the build gate and commit. Then Haruspex `verify` and the verification gate run, and a PR opens |

**Risk policy.** Everything below critical is mitigated without asking: assumptions are validated
in code or given a reversible default, and high-severity failure modes get a criterion and a test.
Agents never accept high or critical risk; those are escalated. Risky units (migrations, deletes,
deploys, auth, payments, secrets, CI) never run.

**Haruspex gates.** Agents never approve a human gate. Because the build runs before you approve
it, your final approval is recorded as approved after the build, under the auto-proceed policy.
Verification evidence is tied to the commit, so it runs after the commit, and the PR only opens
if the verification gate passes.

## Memory

One file per repo, `.claude/work-memory.json`:

- **Rules**: each keeps `a` (for) and `b` (against) counts; weight is a ÷ (a + b). The top 8
  with weight ≥ 0.4 go into the Reading or worker prompts. A rule in play gets +0.25 when its
  failure stays away and +1 against when it comes back. Below 0.3 after 4 observations it is
  deleted. Counts are capped at 20.
- **Models**: pass/fail per tier per unit size. A tier is proven after 3+ tries at ≥ 75%
  first-pass (Laplace-smoothed). Each run probes one cheaper unproven tier on one unit.

If the file can't be read or parsed, the run doesn't save, so it can't overwrite it.

## Requirements

- [Haruspex](https://github.com/pxm-0/haruspex) and `python3`. The skill looks for the CLI in the
  repo's `.haruspex/bin/`, then the installed `haruspex` plugin, then `~/Code/nodens/haruspex`.
- [mattpocock-skills](https://github.com/mattpocock/skills) for the TDD, diagnosis, review and
  retro methods. Without it, each prompt falls back to a short inline version of the method.

## Tuning

Settings are constants at the top of `workflows/run.js`: `MIN_OBS`, `TARGET`, `WINDOW`,
`RULES_PER_PROMPT`, `MIN_WEIGHT`, `PRUNE_BELOW`, `MAX_RULES`, `HELD`, `RECURRED`,
`MAX_FIX_ROUNDS`, `MAX_REVIEW_FIXES`, `MAX_UNITS`.

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
