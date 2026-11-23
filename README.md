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
| Learn | sonnet, then `wg` | Score the rules in play, turn fixed failures into rules, and propose guardrails in the `mattpocock-skills:retro` style (never applied unasked). A mechanical failure gets an automated-check proposal with a check name, and its rule is kept only as a stopgap, listed every run and retired once you approve a check under that name. The kernel validates and writes the memory |
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

**Shifts (Phase 2).** `wg shift run`, started at night by a launchd job you load yourself
(`/workgraph:shift setup` prints it), takes the GitHub issues you labelled `wg:queued` in the
repos listed in `~/.workgraph/factory.toml`. An issue is work only if a trusted actor wrote it,
labelled it last and is the only one who edited it since. Each runs in its own worktree under
`~/.workgraph-work/worktrees/` (your checkout is never touched) through the same dev pipeline,
which then commits, pushes a `wg/<n>-<slug>` branch and opens a draft PR once the kernel's checks
and an opus review hold at the tree. In the morning, `/workgraph:shift` walks the report:
incidents, decisions (`wg decide` stores your answers and queues the issue again), approvals
(`/workgraph:work approve <owner/repo>#<pr>` records Haruspex verification and makes the PR
ready), and parked work. `~/.workgraph/STOP` pauses shifts; `wg shift resume <id>` finishes an
interrupted one without paying again for the answers it already got.

**Sandbox (Phase 3).** Risky units (migrations, auth, deletes, CI, deploys, payments, secrets, by
the kernel's classifier on planned paths and on each layer's actual diff) proceed in a rootless
podman container run as `wgsbx` on the existing OrbStack machine `oreochiserver-sim`: no host
mounts, no network, read-only root, no capabilities, CPU, memory and pid limits, and a throwaway
tar copy of the work. A risky worker's only Bash is `wg sandbox exec --unit <U> -- <cmd>`; once one
is built the task is tainted, so every later check, review command and Haruspex verify runs there
too, and the draft PR carries `risky:<category>` labels and the `SandboxRun` evidence. If the
sandbox is unavailable the task parks; it never falls back to the Mac. `wg sandbox doctor` checks
the machine and proves the container's isolation (it fails closed), `wg sandbox setup-print`
prints the provisioning script you run once, `wg sandbox reap` removes stale jobs, and
`wg/tests/sim/drill.sh` is the real-sim drill. Configure it per repo with `[repo.sandbox]` in
`factory.toml` (image and service digests, in-container `checks`, and `verify`).

**QA and release (Phase 4).** A repo's assembly line is `[[repo]].pipelines`, `["dev", "qa"]` by
default. After dev's build and review, `pipelines/qa.toml` reruns the gate checks with the repo's
`qa_checks` (owner-approved `[[check]]` entries, e.g. an e2e suite), sends a QA explorer after
negative and recovery paths, and lets either one fix the blamed units for one round through the
build fix loop; any QA fix is re-reviewed by opus before `publish`, which now ends `qa.toml`. The
dev + qa chain's worst case is 50 calls, inside the compiled `max_calls_per_task` ceiling of 60:
an owner `factory.toml` that lowered `[limits].max_calls_per_task` below 50 makes the preflight
skip the repo (`wg doctor` shows each chain's worst case). With `[[repo]].release = true` and
`[repo.sandbox].release_rehearsal` steps, a shift also takes each task you verified with
`/workgraph:work approve` whose PR has merged: it recreates a worktree at the merge commit,
reruns Haruspex verify there with the pinned commands, writes release notes and the Haruspex
release plan, rehearses the steps in the sim sandbox (no network), evaluates ready-to-release and
stops at your human gate with the issue labelled `wg:release-ready`, only once the kernel holds
the rehearsal, the gate and the plan itself. It never records a deployment and never deploys:
that stays yours. A release that is stuck is listed in the shift report; after you fix its cause,
`wg shift retry-release --repo <owner/repo> --issue <n>` puts it back for the next shift. A stage of an existing kind is TOML plus a prompt
file (both compiled in by `wg/build.rs`); a new kind is a Rust change.

**Self-improvement, shadow first (Phase 5).** The factory may improve itself, but only through
the editable allowlist in `config/protected.toml` (prompts, the dev, qa and release pipelines,
docs, and the planning, allocation, learning and engine code of the `no_std`, zero-dependency
`wg-factory` crate); everything else, including any file that does not exist yet, is protected.
Label an issue in pxm-0/workgraph `wg:self` yourself and, with `[self_improve] enabled = true`
and a `[[repo]]` for it with `pipelines = ["self"]`, `always_tainted = true` and a
`[repo.sandbox]`, a shift takes at most one (within `[limits].max_self_prs_per_day` and
`max_self_prs_per_week`, and at most 3 open) through `pipelines/self.toml` on a `wg-self/*`
branch, every check in the sandbox, and opens a ready PR labelled `wg:self`. A plan or a change
that names a protected path never gets built or pushed. `wg self gate --pr N`, run by the
installed base binary after CI, then decides it: factory authorship from the hash-chained
self-ledger, a local rename-free diff checked on every side, the caps, `lint-editable`, the
head's prompts and pipelines loaded by the base kernel, the required GitHub Actions check-runs
(`ci.yml` test, coverage, no-std-build and `self-gate.yml` protected-paths, lint-editable,
replay-eval), the staged live eval when prompts or pipelines change, and the rate rules. Editable
Rust is held for you (D2). Merging ships off: the gate labels a PR it would merge
`wg:self-shadow`, you merge or close it by hand, and after 5 shadow decisions
`wg self resume --enable-merge` is your switch. A merge is a squash pinned with
`--match-head-commit`; any revert you make of one pauses the lane. `wg self install --sha S`
installs a main SHA only from its CI artifact (sha256 and head checked, memory snapshotted); the
first shift on it is the canary, and an unhealthy one rolls back (`current` flips back, memory is
restored, the SHA is denylisted, a revert PR opens, the lane pauses). `wg eval replay` replays
`eval/replay/` through the engine; `wg eval capture --pr <url>` turns a merged PR into a fixture
branch on the private pxm-0/workgraph-eval (never its main). `wg self status`, `pause` and
`rollback` are yours; there are no dollar caps, only run counts, and every eval's spend is in
the self-ledger and the shift report.

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
