# workgraph

A Claude Code plugin that runs a coding task as a graph under a
[Haruspex](https://github.com/pxm-0/haruspex) Reading, and gets better per repo.

```
/workgraph:work add rate limiting to the login endpoint
```

| Step | Model | What happens |
|---|---|---|
| Recall | `wg run`, no model | Load the repo's rules and model stats; note your uncommitted work so the run never touches or commits it |
| Read | opus, high effort | Init Haruspex if the repo has none. Write the Reading: outcome, scope, acceptance criteria, assumptions, failure modes, test plan, core reviews. Split the work into up to 5 units, each owning criteria |
| Challenge | sonnet, high effort | Adversarial pass: assume the Reading is incomplete and add what it missed |
| Escalate | none (code) | Anything critical, or high and not mitigable in code and tests, stops the run here before any edit; you're asked only those |
| Build | learned per unit size | Test-first workers (`mattpocock-skills:tdd`), independent units in parallel, on the cheapest model proven in this repo |
| Check / Fix | `wg` / one tier up per round | The kernel runs the owner-approved check commands itself and exit codes decide; no agent reports a check result. Only the blamed unit goes back (`mattpocock-skills:diagnosing-bugs`), max 3 rounds |
| Review | opus | Independent Spec and Standards review (`mattpocock-skills:code-review`) against the Haruspex criteria; writes verdicts into the task. One fix and re-review round |
| Learn | sonnet, then `wg` | Score the rules in play, turn fixed failures into rules, and propose guardrails in the `mattpocock-skills:retro` style (never applied unasked). The kernel validates and writes the memory |
| You | `wg finalize --run`, no model | Approve the build gate and commit. One command commits, records the gates, runs Haruspex `verify` and the verification gate; a PR opens only if it passes |

**Risk policy.** Everything below critical is mitigated without asking: assumptions are validated
in code or given a reversible default, and high-severity failure modes get a criterion and a test.
Agents never accept high or critical risk; those are escalated. Risky units (migrations, deletes,
deploys, auth, payments, secrets, CI) never run.

**Haruspex gates.** Agents never approve a human gate. Because the build runs before you approve
it, your final approval is recorded as approved after the build, under the auto-proceed policy.
Verification evidence is tied to the commit, so it runs after the commit, and the PR only opens
if the verification gate passes.

**How it runs.** `/workgraph:work` drives `wg run`: one `claude -p` child per stage with a
cleared environment, a per-role tool allowlist and the fail-closed `wg guard` hook; the kernel runs the checks itself and keeps their
results, and `wg finalize --run` refuses unless they and the independent review hold at the
current tree. Every call is in `~/.workgraph/cost.jsonl` with its provider and model id. Setup
and the layout under `~/.workgraph/` are in `docs/design/factory.md` (§5, §7.1, §9) and
`docs/design/adr-001-claude-p-contract.md`. The Workflow-tool script `workflows/run.js` it
replaced is in git history; reverting the Phase 1b PR restores it.

## Memory

One file per repo, `~/.workgraph/memory/<owner>__<repo>.json` (a repo's old
`.claude/work-memory.json` is imported once on its first run):

- **Rules**: each keeps `a` (for) and `b` (against) counts; weight is a ÷ (a + b). The top 8
  with weight ≥ 0.4 go into the Reading or worker prompts. A rule in play gets +0.25 when its
  failure stays away and +1 against when it comes back. Below 0.3 after 4 observations it is
  deleted. Counts are capped at 20.
- **Models**: pass/fail per tier per unit size. A tier is proven after 3+ tries at ≥ 75%
  first-pass (Laplace-smoothed). Each run probes one cheaper unproven tier on one unit.

If the file can't be read or parsed, the run doesn't save, so it can't overwrite it.

## Requirements

- `wg`, the Rust binary in `wg/` (a crate of the root workspace) that runs the pipeline and every
  mechanical step and prints one JSON line (the older `recall`, `check`, `save` and `finalize`
  subcommands still work). Each `v*` release attaches builds for macOS (arm64, x86_64)
  and Linux x86_64 with `.sha256` checksums; the skill downloads the matching one on first use after
  asking. With Rust installed you can build it instead: `cargo build --release -p wg`, which
  writes `target/release/wg`.

- [Haruspex](https://github.com/pxm-0/haruspex) and `python3`. `wg` uses only the CLI pinned with
  `wg config pin-haruspex`; the skill offers to pin the installed plugin's on first use.
- [mattpocock-skills](https://github.com/mattpocock/skills) for the TDD, diagnosis, review and
  retro methods. Without it, each prompt falls back to a short inline version of the method.

## Tuning

Stage models, turn limits, `max_units` and the fix-round limits are in `pipelines/dev.toml`.
The learning constants are in `wg-factory`: `MIN_OBS` and `TARGET` in `alloc_tiers`, `WINDOW` in
`learn`, `RULES_PER_PROMPT`, `MIN_WEIGHT`, `PRUNE_BELOW`, `MAX_RULES`, `HELD` and `RECURRED` in
`learn/rules.rs`.

## Install

```
claude plugin marketplace add pxm-0/workgraph
claude plugin install workgraph@workgraph
```

## Develop

Builds and tests run in the compose dev stack (`compose.dev.yml`, image from
`.devcontainer/Dockerfile`, pinned by digest), the same image CI uses. It publishes no ports, and
the cargo registry and build output live in the named volumes `cargo-registry` and `cargo-target`
(one build directory per checkout, so worktrees never share a `wg` binary).
Open the repo in the devcontainer, or run commands through the stack:

```
docker compose -f compose.dev.yml run --rm workspace cargo test --workspace
docker compose -f compose.dev.yml run --rm workspace cargo clippy --workspace --all-targets -- -D warnings
docker compose -f compose.dev.yml run --rm workspace cargo xtask size        # no .rs file over 800 lines
```

The finalize tests need the Haruspex CLI; without it they skip. Mount a checkout read-only:

```
docker compose -f compose.dev.yml run --rm -v /abs/path/haruspex:/haruspex:ro \
  -e HARUSPEX_CLI=/haruspex/scripts/haruspex.py workspace cargo test --workspace
```

The workspace has three crates: `wg` (the binary), `wg-factory` (the `no_std`
factory, built for `thumbv7em-none-eabihf` in CI so it can never name `std`) and `xtask`.

The pre-push hook (`.githooks/pre-push`) refuses pushes to `main` and runs fmt, clippy and the
tests in the stack. Enable it with `git config core.hooksPath .githooks`, and set `HARUSPEX_DIR`
to include the finalize tests.
