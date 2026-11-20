# workgraph

A Claude Code plugin that runs a coding task as a graph under a
[Haruspex](https://github.com/pxm-0/haruspex) Reading, and gets better per repo.

```
/workgraph:work add rate limiting to the login endpoint
```

| Step | Model | What happens |
|---|---|---|
| Recall | `wg recall`, no model | Load the repo's rules and model stats; note your uncommitted work so the run never touches or commits it |
| Read | opus, high effort | Init Haruspex if the repo has none. Write the Reading: outcome, scope, acceptance criteria, assumptions, failure modes, test plan, core reviews. Split the work into up to 5 units, each owning criteria |
| Challenge | sonnet, high effort | Adversarial pass: assume the Reading is incomplete and add what it missed |
| Escalate | none (code) | Anything critical, or high and not mitigable in code and tests, stops the run here before any edit; you're asked only those |
| Build | learned per unit size | Test-first workers (`mattpocock-skills:tdd`), independent units in parallel, on the cheapest model proven in this repo |
| Check / Fix | `wg check` / one tier up per round | `wg` runs the commands and exit codes decide. The workflow sandbox can't run commands, so a haiku agent relays `wg`'s output, which must echo a per-call nonce or it is rejected. Only the blamed unit goes back (`mattpocock-skills:diagnosing-bugs`), max 3 rounds |
| Review | opus | Independent Spec and Standards review (`mattpocock-skills:code-review`) against the Haruspex criteria; writes verdicts into the task. One fix and re-review round |
| Learn | sonnet, then `wg save` | Score the rules in play, turn fixed failures into rules, and propose guardrails in the `mattpocock-skills:retro` style (never applied unasked). `wg save` validates and writes the memory |
| You | `wg finalize`, no model | Approve the build gate and commit. One command commits, records the gates, runs Haruspex `verify` and the verification gate; a PR opens only if it passes |

**Risk policy.** Everything below critical is mitigated without asking: assumptions are validated
in code or given a reversible default, and high-severity failure modes get a criterion and a test.
Agents never accept high or critical risk; those are escalated. Risky units (migrations, deletes,
deploys, auth, payments, secrets, CI) never run.

**Haruspex gates.** Agents never approve a human gate. Because the build runs before you approve
it, your final approval is recorded as approved after the build, under the auto-proceed policy.
Verification evidence is tied to the commit, so it runs after the commit, and the PR only opens
if the verification gate passes.

**The Rust path (Phase 1a).** `/workgraph:work --experimental <task>` runs the same pipeline
through `wg run`: one `claude -p` child per stage with a cleared environment, a per-role tool
allowlist and the fail-closed `wg guard` hook; the kernel runs the checks itself and keeps their
results, and `wg finalize --run` refuses unless they and the independent review hold at the
current tree. Every call is in `~/.workgraph/cost.jsonl` with its provider and model id. Setup
and the layout under `~/.workgraph/` are in `docs/design/factory.md` (§5, §7.1, §9) and
`docs/design/adr-001-claude-p-contract.md`. run.js stays the default until Phase 1b.

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
stops at your human gate with the issue labelled `wg:release-ready`. It never records a
deployment and never deploys: that stays yours. A stage of an existing kind is TOML plus a prompt
file (both compiled in by `wg/build.rs`); a new kind is a Rust change.

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

- `wg`, the Rust binary in `wg/` (a crate of the root workspace) that does every mechanical step
  (`recall`, `check`, `save`, `finalize`) and prints one JSON line. Each `v*` release attaches builds for macOS (arm64, x86_64)
  and Linux x86_64 with `.sha256` checksums; the skill downloads the matching one on first use after
  asking. With Rust installed you can build it instead: `cargo build --release -p wg`, which
  writes `target/release/wg`.

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
docker compose -f compose.dev.yml run --rm workspace sh -c 'node --test tests/*.test.mjs'
```

The finalize tests need the Haruspex CLI; without it they skip. Mount a checkout read-only:

```
docker compose -f compose.dev.yml run --rm -v /abs/path/haruspex:/haruspex:ro \
  -e HARUSPEX_CLI=/haruspex/scripts/haruspex.py workspace cargo test --workspace
```

`node --test` runs the real workflow script against fake agents, with no dependencies.

The workspace has three crates: `wg` (the binary), `wg-factory` (an empty `no_std` crate for the
factory, built for `thumbv7em-none-eabihf` in CI so it can never name `std`) and `xtask`.

The pre-push hook (`.githooks/pre-push`) refuses pushes to `main` and runs fmt, clippy and the
tests in the stack. Enable it with `git config core.hooksPath .githooks`, and set `HARUSPEX_DIR`
to include the finalize tests.
