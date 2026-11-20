# ADR-001: the `claude -p` contract (Phase 0 spike)

Status: **accepted with open items**, 2026-11-20. Inputs to Phase 1a. Design: `docs/design/factory.md`
(§5, §16 Phase 0, §17 D5 and D6) on `feat/factory-design`.

Open items: §16 lists these as Phase 0 work, and this spike did not deliver them. Each is a tracked
issue that blocks the Phase 1a work that depends on it, and §16 needs the design owner's amendment
to match (listed under "Open items" at the end):

- no fixture for a subscription usage limit or an API rate limit (§5), so `RateLimited` is unpinned;
- the gateway dry-run test of the argv, env and settings `wg` builds (§9) moved to Phase 1a;
- "a guard built to panic still blocks" (§12), no key in the Bash env (8d), the helper re-run
  cadence on a valid key (8c) and the usage-limit signature per path (8e) are not observed;
- the permission layer was tested against `&&` and `;` only (§10c);
- child calls wrote into the owner's `~/.claude` (Method), so the 8d sentinel session must be rerun
  under isolation.

## Method

- Claude Code **2.1.311**, on the owner's Mac, subscription login. Every call ran in a throwaway
  git repo under the session scratchpad (written `<spike>` below), never in an owner repo.
- Environment cleared as §5.1 prescribes (`env -i HOME USER LANG=en_US.UTF-8 TERM=dumb
  PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin TMPDIR=<spike>/tmp WG_AGENT=1`), prompt on
  stdin, and every call carried `--no-session-persistence`. The parent session's environment held
  `ANTHROPIC_BASE_URL` and about 30 `CLAUDE_CODE_*` variables (account and org ids, a messaging
  token): `env_clear()` in the runner is required, not just tidy.
- Models: haiku for everything except `multi_model_usage` (sonnet, which spawned a haiku
  subagent). Writing `B` below for the base `claude -p --no-session-persistence --model haiku`.
- **Not isolated from the owner's `~/.claude`.** `run.sh` passed the real `HOME`, so every child
  loaded the owner's user-scope settings and plugins, and their hooks wrote into `~/.claude`: 36 of
  the 42 fixture sessions left `~/.claude/session-data/2026-11-20-<id8>-session.tmp` (ecc's session
  hook), the spike's tool calls are in `~/.claude/bash-commands.log`,
  `~/.claude/cost-tracker.log` and `~/.claude/metrics/tool-usage.jsonl`, and Claude Code created
  one `~/.claude/projects/<slug>-scratchpad-spike-<repo>` directory per spike repo (15, empty but
  for one subagent `.meta.json`) despite `--no-session-persistence`. The owner can find every line
  with `grep -rl scratchpad/spike ~/.claude` and remove them. `~/.claude/settings.json` was also
  modified mid-run (16:59:27) by something unknown; it holds no spike strings. Phase 1a must keep
  child calls out of the owner's `~/.claude` (an `enabledPlugins` profile that turns off logging
  plugins such as ecc's session and cost hooks, or a dedicated config dir), and the 8d sentinel
  session reruns under that isolation, as §16's "a spike-only session" requires.
- Fixtures: `wg/tests/fixtures/claude/<case>.json` (the result envelope, redacted) and, where the
  `stream-json` init event matters, `<case>.init.json` (the whole init event, redacted, not
  reshaped). One line per case in `wg/tests/fixtures/claude/README.md`.
  `wg/tests/claude_fixtures.rs` parses every one and pins the fields §5.3 relies on.
- Evidence: the spike scripts (`run.sh`, `stages.sh`, `extract.py`), hooks, project-config files,
  argv per case and the marker and stub logs are in `wg/tests/fixtures/claude/spike/`, redacted;
  the fixtures README lists which markers exist per case.
- **Spend:** 45 calls (5 more were rejected at argv parsing), `total_cost_usd` summed **$0.75** (notional, subscription). $0.56 is in the
  fixtures; the rest is one probe and two calls rerun after an argv mistake.

Verdicts: **confirmed** (observed as the design assumes), **refuted** (observed otherwise; the
fallback is named), **not observed** (could not be triggered; what would confirm it is named).

## 1. Envelope: where `structured_output` lands, cost and `modelUsage` under the subscription

Ran: `B --output-format json --max-turns 3 --json-schema '{"type":"object","properties":{"answer":
{"type":"string"},"sum":{"type":"integer"}},"required":["answer","sum"],"additionalProperties":false}'
--permission-mode dontAsk --setting-sources user --strict-mcp-config --mcp-config '{"mcpServers":{}}'`

Happened: exit 0; one JSON object; `type` "result", `subtype` "success", `is_error` false;
`structured_output` is a top-level object `{"answer":"Five","sum":5}`; `result` holds the same
object as text. `total_cost_usd` 0.0095 and `modelUsage` `{"claude-haiku-5-5": {... "costUSD",
"outputTokens", "provider": "firstParty", "costBasis": "list"}}` are present on the subscription.
Also present and useful: `terminal_reason` ("completed", "api_error", "max_turns",
"budget_exhausted"), `api_error_status`, `permission_denials`, `num_turns`, `session_id`.

Fixture: `success_structured.json`. Verdict: **confirmed**.

Side finding (`schema_max_turns_1.json`, same argv with `--max-turns 1`): it succeeds with
`num_turns` 2. Observed: `num_turns` can exceed `--max-turns` by **at least 2** with `subtype`
"success": both Challenger calls report 10 at `--max-turns 8` (§7), the t2 Reader 9 at 8 and the
Learners 4 at 3. The cause is not established (one extra structured-output turn would explain only
+1). Phase 1a: ledger `num_turns` as reported, do not treat `num_turns > max_turns` as an anomaly,
and never derive limits from `max_turns + 1`. Pinned by `num_turns_can_exceed_max_turns`.

## 2. `is_error`

Ran: `B --output-format json --model claude-spike-nonexistent-model --max-turns 1
--permission-mode dontAsk` (the `--model` overrides B's).

Happened: exit 1, `is_error` true, **`subtype` "success"**, `api_error_status` 404,
`terminal_reason` "api_error", `total_cost_usd` 0, `modelUsage` `{}`, `result` is a human message.

Fixture: `is_error_bad_model.json`. Verdict: **confirmed**, with a constraint for §5.3: `subtype`
alone says success on API errors, so step 1's conjunction (`subtype == "success"` **and**
`is_error == false`) is load-bearing and pinned by a test. A non-zero exit accompanies it.

## 3. Runaway guard (`error_max_budget_usd`)

Ran: `B --output-format json --max-turns 5 --max-budget-usd 0.0001 --allowedTools Read
--permission-mode dontAsk`

Happened: exit 1, `subtype` "error_max_budget_usd", `is_error` true, `terminal_reason`
"budget_exhausted", `errors: ["Reached maximum budget ($0.0001)"]`. The cap is checked after a
turn, so the call spent $0.0082, 80 times the cap: the guard bounds runaways, not single turns.

Fixture: `budget_guard.json`. Verdict: **confirmed**.

## 4. Turn limit (`error_max_turns`)

Ran: `B --output-format json --max-turns 1 --allowedTools Read --permission-mode dontAsk` with a
two-read prompt.

Happened: exit 1, `subtype` "error_max_turns", `is_error` true, `terminal_reason` "max_turns",
`errors: ["Reached maximum number of turns (1)"]`, cost and `modelUsage` present.

Fixture: `max_turns.json`. Verdict: **confirmed**.

## 5. Subscription usage limit and API rate limit

Not triggerable on purpose without exhausting the owner's plan or holding an API key (there is
none in the keychain). **No fixture was invented.** Verdict: **not observed** for both `is_error`
envelopes.

Signal recorded: every stream-json run (18 of 18) emitted a `rate_limit_event` before the result,
with `rate_limit_info.status` ("allowed"), `rateLimitType` ("five_hour"), `resetsAt`, overage
fields and `unifiedWindows.{five_hour,seven_day}.{utilization,status,resetsAt}` (utilization 0.34
and 0.18 here). Fixture: `rate_limit_event.json` (`overageDisabledReason` redacted). It is the
candidate detector for §5.3's subscription usage limit: a `status` other than "allowed", or a window
at utilization 1, should accompany the limited envelope. The first real limited call confirms or
refutes it; it is only in stream-json output, so the runner would need `stream-json` to read it.

What would confirm them: a call made while the subscription is limited (the owner's next natural
limit; record the envelope with the runner's argv), and an `anthropic-api` call answered 429. Until
then §5.3's `RateLimited` signature is unpinned: Phase 1a must match on `api_error_status` (429,
and whatever the limited subscription returns) rather than on `result` text, and classify an
unknown `is_error` as `Died`, which parks safely.

## 6. Multi-key `modelUsage`

Ran: `claude -p --no-session-persistence --output-format json --model sonnet --max-turns 4
--allowedTools Agent Task --permission-mode dontAsk`, asking for one haiku subagent.

Happened: `modelUsage` keys `claude-sonnet-5-5` (209 output tokens) and `claude-haiku-5-5` (75).
The dominant key by output tokens is the requested tier. A plain sonnet call with two Bash tool
uses (first attempt, not kept) had a single key: background haiku calls do not always happen.

Fixture: `multi_model_usage.json`. Verdict: **confirmed**; §5.3 step 4 (dominant key by output
tokens) is pinned by the test.

## 7. Per-stage cost fixtures

Ran (`spike/stages.sh`, haiku for every stage to keep spend small; the real tiers are opus, sonnet and the
allocated tier, so these size turns and shape, not dollars):

`B --output-format json --permission-mode dontAsk --setting-sources user --strict-mcp-config
--mcp-config '{"mcpServers":{}}' --json-schema <stage schema>` plus, per stage:

| Stage | Extra argv |
|---|---|
| read | `--max-turns 8 --allowedTools Read Glob Grep` |
| challenge | `--max-turns 8 --allowedTools Read Glob Grep` |
| work, fix | `--max-turns 12 --allowedTools Read Glob Grep 'Edit(//<spike>/tN/calc.py)' 'Bash(python3 check.py)'` |
| review | `--max-turns 10 --allowedTools Read Glob Grep 'Bash(git diff)' 'Bash(python3 check.py)'` |
| learn | `--max-turns 3` (no tools) |

Toy task 1: `add` returns `a - b`, fix it. Toy task 2: add `is_even(n)`.

| Stage | t1 turns / $ | t2 turns / $ | Denials t1, t2 |
|---|---|---|---|
| read | 8 / 0.0144 | 9 / 0.0148 | 3, 3 (the Reader tried to edit) |
| challenge | 10 / 0.0143 | 10 / 0.0147 | 1, 1 |
| work | 9 / 0.0146 | 9 / 0.0117 | 2, 1 (GateGuard first edit; `python3 check.py; echo "exit=$?"`) |
| fix | 8 / 0.0110 | 8 / 0.0111 | 1, 1 (GateGuard first edit) |
| review | 6 / 0.0126 | 7 / 0.0130 | 1, 1 (`... python3 check.py; echo "exit=$?"`) |
| learn | 4 / 0.0114 | 4 / 0.0093 | 0, 0 |

Every call returned a schema-valid `structured_output`. The fixed cost of a call is about
$0.007 on haiku: the owner's user-scope plugins put ~42k tokens of system prompt in cache creation.

Fixtures: `stage_t{1,2}_{read,challenge,work,fix,review,learn}.json`. Verdict: **confirmed** (the
first cost-ledger entries). Two findings for Phase 1a:

- Models append `; echo "exit=$?"` to commands; exact-string `Bash(...)` entries and the guard
  refuse them, so role prompts must say "run the command exactly as given; the exit status is
  reported to you". t1's Reviewer returned `fail` only because of this.
- GateGuard costs one denied call plus a retry per first edit of a file (§11 below).

## 8. Billing paths (D5)

**8a. Subscription, no key variable, no helper.** Ran: `B --output-format stream-json --verbose
--max-turns 3 --permission-mode dontAsk --setting-sources user --strict-mcp-config --mcp-config
'{"mcpServers":{}}'`. Init event `apiKeySource` "none"; call billed normally
(`total_cost_usd` 0.0067). Fixtures: `subscription_no_helper.json`, `.init.json`. Verdict:
**confirmed**.

**8b. `apiKeyHelper` from `--settings` under `-p`.** Ran the same argv plus `--allowedTools
'Bash(env)' --settings '{"apiKeyHelper":"<spike>/hooks/key_helper.sh"}'`; the helper prints a dummy
invalid key (`sk-ant-api03-SPIKE-DUMMY-...`) and logs each run.

Happened: init `apiKeySource` "apiKeyHelper": the helper is honoured under `-p` and **takes
precedence over the subscription login**. The API answered 401; Claude Code retried 10 times with
backoff (`system/api_retry` events, `error_status` 401) and ended after **262 s** with exit 1,
`is_error` true, `subtype` "success", `api_error_status` 401, `result` "Invalid API key · Fix
external API key", cost 0. Fixtures: `api_key_helper_invalid.json`, `.init.json` (the API-path
fixture). Verdict: **confirmed** (honoured, reported).

Consequences: a bad or revoked key costs four minutes per call before failing, so `wg doctor
--live` must run first on any key-bearing profile and a 401 is `Died` (not `RateLimited`): it must
not trigger fallback churn across the shift.

**8c. Helper re-run cadence.** The helper ran 11 times in 8b (once per attempt after a 401) and
once per request in the gateway runs (§9): 15 timestamps in `spike/markers/helper.log`. Verdict: **partly observed**: re-run after every failed
request; the cadence on a successful key (Claude Code caches helper output) is **not observed**,
and needs a valid key.

**8d. No key in a Bash tool's environment.** Verdict: **not observed**: with an invalid key the
call never reaches a tool. Confirm with a sentinel key in a test keychain item that the API
accepts (a real low-limit key), running `Bash(env)` and searching the output for the sentinel.

**8e. Usage-limit `is_error` signature per path.** **Not observed** (§5).

## 9. Gateway profiles (D6), dry run

No gateway key and no LiteLLM proxy exist, so no real gateway call ran. The `wg` builder
(`claude/auth.rs`, `claude/providers.rs`) is Phase 1a, so the "dry-run test pins the argv, env and
settings `wg` builds" moves there, with this spike's shape as its expected value:

- argv: `claude -p --output-format json --model <profile tier id> ... --settings <per-call file
  with "apiKeyHelper": "<wg> auth key <provider>">`
- env: the §5.1 cleared set plus `ANTHROPIC_BASE_URL=https://openrouter.ai/api` (openrouter) or
  `http://127.0.0.1:<port>` (litellm); no key variable.

Ran, with no network beyond loopback: a stub on `127.0.0.1:18977` that logs method, path, `model`
and which auth header arrived, and answers 404. `ANTHROPIC_BASE_URL=http://127.0.0.1:18977` in the
env, `--settings` with the dummy helper, `--setting-sources user --strict-mcp-config --mcp-config
'{"mcpServers":{}}' --permission-mode dontAsk --max-turns 2`:

| Case | `--model` | Stub saw | Envelope |
|---|---|---|---|
| `gateway_loopback_dry_run` | `sonnet` | requests not retained in the stub log (overwritten); the init event's `model` is `claude-sonnet-5-5` | `is_error` true, 404, cost 0, init `apiKeySource` "apiKeyHelper" |
| `gateway_loopback_provider_model` | `openai/gpt-5.1` | `HEAD /api/hello`, then 2 × `POST /v1/messages?beta=true` with `model` `openai/gpt-5.1` unchanged, key in `x-api-key` (`spike/markers/gateway.log`) | `is_error` true, 404, cost 0 |

Verdicts:
- `ANTHROPIC_BASE_URL` plus `apiKeyHelper` route the call and the helper's key to the gateway:
  **confirmed** (loopback).
- Tier aliases resolve **client-side** (`sonnet` is `claude-sonnet-5-5` in the init event before
  any request) and arbitrary ids pass through (stub log): **confirmed**. So a gateway profile must map tiers to the provider's own ids and pass
  them with `--model` (§7.1's tier map), or the gateway must alias Anthropic ids.
- Background haiku calls on a gateway: **not observed** (a failing one-turn call issued none).
- `structured_output` through `--json-schema`, tool use and skills on a gateway: **not observed**.
  Gateway profiles stay **unusable** until a spike with a real OpenRouter key or LiteLLM proxy runs
  §1, §6 and the skill case against them (as §16 prescribes).

## 10. Permissions

Precondition for every verdict here and in §11: the owner's `~/.claude/settings.json` had **no
`permissions` block** (no `allow`, `deny`, `ask` or `defaultMode`). The factory keeps
`--setting-sources user`, so a later user-scope `permissions.allow` entry would merge into every
role's allowlist, and a user `defaultMode` could change what `dontAsk` means. Phase 1a: `wg doctor`
and the runner refuse to start when user-scope settings carry `permissions.allow`, `permissions.ask`
or `permissions.defaultMode` (a spike case with a user allow entry would confirm the merge).

**10a. `--permission-mode dontAsk` denies what is not allowlisted.** Ran: `B --output-format json
--max-turns 6 --allowedTools Read --permission-mode dontAsk`, asking for a Bash `touch`, a Write
and a Read. Bash and Write denied ("Permission to use Bash has been denied because Claude Code is
running in don't ask mode"), Read ran, no file created. Fixture: `perm_dontask.json`. Verdict:
**confirmed**.

**10b. `Edit(<path>)` scopes edits.** Ran: `B --output-format stream-json --verbose --max-turns 14
--allowedTools Read <pattern> --permission-mode dontAsk`, asking to edit `notes.txt`,
`deep/notes.txt` and `calc.py`:

| Pattern | notes.txt | deep/notes.txt | calc.py | Fixture |
|---|---|---|---|---|
| `Edit(notes.txt)` | edited | **edited** | denied | `perm_edit_scope.json` |
| `Edit(./notes.txt)` | edited | **edited** | denied | `perm_edit_anchored_dot.json` |
| `Edit(//<spike>/r3/notes.txt)` | edited | denied | denied | `perm_edit_anchored_abs.json` |

Verdict: **refuted as written**: relative patterns use gitignore semantics and match the name at
any depth, so the §5.1 example `Edit(.haruspex/tasks/T-001.json)` would also allow
`x/.haruspex/tasks/T-001.json`. Fix (not a fallback): the kernel emits only absolute, anchored
patterns, `Edit(//<canonical worktree>/<path>)` and `Write(//...)`, and the guard's canonicalized
write roots (§5.4) stay the primary check.

**10c. `Bash(<cmd>:*)` refuses `<cmd> && rm -rf ~`.** Ran: `B --output-format stream-json
--verbose --max-turns 8 --permission-mode dontAsk --allowedTools 'Bash(git status:*)'`, asking
for `git status && touch <spike>/markers/escaped_and` and `git status; touch
<spike>/markers/escaped_semi` (a marker instead of `rm`). Both denied by the permission layer; no
marker exists. Fixture: `perm_bash_chain.json`. Verdict: **confirmed for `&&` and `;` only**. Not
tested: `|`, `$(...)`, backticks, a newline and `>`; and the run has no positive control (a plain
`git status` never ran under `Bash(git status:*)`), so it cannot tell "refuses chaining" from "the
rule matched nothing". Open: a run with a marker per metacharacter plus a plain allowed command in
the same session, or the real `wg guard` panic test passing in Phase 1a.

**10d. `Skill(<name>)` blocks other skills.** Ran: `B --output-format stream-json --verbose
--max-turns 6 --permission-mode dontAsk --allowedTools 'Skill(karpathy-guidelines)'`, invoking
`karpathy-guidelines` then `ponytail:ponytail-help`. **Both launched**, no denial: the Skill tool is
not permission-gated in `dontAsk`, so an allow entry restricts nothing. Fixture:
`skill_scope.json`. Verdict: **refuted**.

Follow-ups run:
- `--disallowedTools 'Skill(ponytail:ponytail-help)'` blocks that skill ("Skill execution blocked
  by permission rules"; `skill_disallow.json`): deny entries work, allow entries do not.
- A `--settings` PreToolUse hook that denies any `Skill` whose `tool_input.skill` is not on its list
  blocked the second skill (`skill_guard_hook.json`).
- A plugin skill with `allowed-tools: Bash(touch:*)` in its frontmatter, invoked under `dontAsk`
  with only Read and `Skill(spikeplug:widen)` allowed: its `touch` was **denied**; frontmatter does
  not widen the session's allowlist (`skill_frontmatter_widen.json`, marker absent).

Fallback for Phase 1a: the **guard is the skill boundary**. The manifest carries the role's skill
names and `wg guard` denies any other `Skill` call (it already sees every tool through
`"matcher":"*"`). Defence in depth: the factory settings profile (`enabledPlugins`, §12 below)
shrinks the loadable set, and `--disallowedTools` names the known-dangerous skills (for example
`deploy-oreochi`, `clasp-ship`). If the guard dies, a stray skill can inject instructions but
cannot add tools.

## 11. Project config isolation: `--setting-sources user`, `--strict-mcp-config`, `enabledPlugins`

Both flags exist in 2.1.311 (`claude --help`). Each repo had `.claude/settings.json` with
`"enableAllProjectMcpServers": true` and a `PreToolUse` hook (`"matcher":"*"`) touching
`<spike>/markers/project_hook_<variant>`, and `.mcp.json` with a server whose command touches
`<spike>/markers/project_mcp_<variant>` and exits. Ran: `B --output-format stream-json --verbose
--max-turns 3 --permission-mode dontAsk --allowedTools Read` plus:

| Variant (fixture `project_config_*`) | Extra argv | Project hook | Project MCP | Other MCP servers |
|---|---|---|---|---|
| `both_flags` | `--setting-sources user --strict-mcp-config --mcp-config '{"mcpServers":{}}'` | silent | not started | none |
| `no_flags` | none | **fired** | **started** | 28 plugin + 5 claude.ai |
| `setting_sources_only` | `--setting-sources user` | silent | not started | 28 plugin + 5 claude.ai |
| `strict_mcp_only` | `--strict-mcp-config --mcp-config '{"mcpServers":{}}'` | **fired** | not started | none |

Verdict: **confirmed**: with both flags a project hook does not fire and a project server does not
start; the control shows the threat is real. Each flag closes one hole, so both are required.
Finding: without `--strict-mcp-config` a child also gets the owner's **claude.ai connectors**
(Gmail, Google Drive, Notion were "connected") and plugin MCP servers: the flag is load-bearing
for more than `.mcp.json`. The §5.1 fallback (factory profile plus project-config refusal) is not
needed; the refusal stays as cheap defence in depth.

`enabledPlugins` per call: `B --output-format stream-json --verbose --max-turns 1
--permission-mode dontAsk --setting-sources user` with and without `--settings
'{"enabledPlugins":{"ponytail@ponytail":false,"superpowers@claude-plugins-official":false,
"i-have-adhd@i-have-adhd":false,"workgraph@workgraph":false}}'`. The four plugins left the init
`plugins` list, their skills left `skills` (472 → 448), and SessionStart hook responses fell from 8
to 5 (ponytail's mode banner gone); ecc (GateGuard), haruspex and mattpocock-skills stayed.
Fixtures: `plugins_default.json`, `plugins_profile_off.json` and their `.init.json`. Verdict:
**confirmed**. Note: Cowork and desktop plugins (`productivity`, `marketing`, `finance`,
`engineering`, `design`, `cc-plugin-*`) load too, though `~/.claude/settings.json` does not list
them; Phase 1a's profile must set every name in the live init `plugins` list, not just the user
file's.

## 12. A PreToolUse hook from `--settings` fires, and exit 2 blocks

Ran: `B --output-format stream-json --verbose --max-turns 8 --permission-mode dontAsk
--allowedTools Read 'Bash(echo spike:*)' --settings <spike>/hooks/exit2.settings.json` (hook
`"matcher":"*"` logs the tool name and exits 2 for Bash). The log shows `Read` then `Bash`; Read ran,
`echo spike hello` was blocked with the hook's stderr as the tool error. Fixture:
`hook_exit2.json`. Verdict: **confirmed**.

**A guard built to panic still blocks (`catch_unwind`, exit 2).** `wg guard` does not exist yet
(Phase 1a), so this is **not observed**; it becomes a Phase 1a integration test. The premise is
confirmed: a hook that dies with another code is non-blocking. Ran the same with
`hooks/abort.sh` (`kill -ABRT`, exit 134) and `--allowedTools 'Bash(echo spike:*)'`: `echo spike
hello` **ran** despite the hook dying, and `touch <spike>/markers/abort_escaped` was stopped by
the allowlist (marker absent). Fixture: `hook_abort.json`. Verdict for "a guard built to abort is
contained by the permission allowlist": **confirmed**.

## 13. Bash allowlist plus the guard's metacharacter rule refuse `wg sandbox exec -- x; rm -rf ~`

Ran: `B --output-format stream-json --verbose --max-turns 8 --permission-mode dontAsk
--allowedTools 'Bash(echo wg sandbox exec:*)' --settings <spike>/hooks/guard.settings.json`, where
the hook is a Python stand-in for §5.4's rule (deny any of ``; & | $ ` < > ( ) { } \`` or newline,
fail closed on any exception). Asked for `echo wg sandbox exec -- x; touch
<spike>/markers/escaped_guard` and `echo wg sandbox exec -- ok`. The guard log shows a deny for the
first and an allow for the second; the first never ran (marker absent), the second printed.
Fixture: `guard_metachar.json`. Verdict: **confirmed** (with the stand-in; the real guard's unit
tests replay this input in Phase 1a). The "no Bash at all" fallback is **not ruled out**: §10c
covers only `&&` and `;` without a positive control, and a guard that panics is not yet shown to
block (§12). It is ruled out once the permission layer refuses every metacharacter with a positive
control in the same run, or the real `wg guard` panic test passes.

## 14. What a GateGuard "ask" does under `-p`

The installed GateGuard (ecc 2.2.3, `scripts/hooks/gateguard-fact-force.js`) never answers
`ask`: it answers `deny` with a "Fact-Forcing Gate" message on the first edit of each file and on
destructive Bash. Under `-p` that deny is a tool error; the model states the facts and retries,
and the retry passes (seen in `perm_edit_scope` and every work and fix stage). The denial is listed
in `permission_denials`, indistinguishable there from a permission-layer denial, so the kernel must
take incidents from the guard log (§5.4), never from `permission_denials`.

A synthetic hook answering `{"hookSpecificOutput":{"hookEventName":"PreToolUse",
"permissionDecision":"ask","permissionDecisionReason":"spike ask"}}` was run with
`--allowedTools 'Bash(echo spike:*)'` under `--permission-mode dontAsk` and under `default`. In both
the allowed `echo spike asked` was **denied**, the reason returned as the tool result. Fixtures:
`hook_ask_dontask.json`, `hook_ask_default.json`. Verdict: **confirmed** (ask = deny).

## Summary for Phase 1a

| §16 item | Verdict | Phase 1a action |
|---|---|---|
| `structured_output` location, cost, `modelUsage` on the subscription | confirmed | parse as §5.3; ledger `num_turns` as reported (it can exceed `--max-turns` by 2+) |
| `is_error` | confirmed | test both `subtype` and `is_error` (API errors say "success") |
| `error_max_budget_usd` | confirmed | none |
| `error_max_turns` | confirmed | none |
| subscription usage limit, API rate limit | not observed (`rate_limit_event` recorded) | watch `rate_limit_info.status` and `unifiedWindows`, match `api_error_status`, default unknown errors to `Died`; record the first real envelope |
| multi-key `modelUsage` | confirmed | dominant key by output tokens |
| per-stage cost fixtures | confirmed (haiku) | "run exactly" in role prompts |
| subscription path | confirmed | none |
| `apiKeyHelper` under `-p`, `apiKeySource` | confirmed | doctor `--live` before use; 401 is `Died` |
| helper re-run cadence | partly observed | confirm with a valid key |
| no key in Bash env | not observed | confirm with a valid sentinel key |
| gateway argv/env (dry run, loopback) | confirmed | tier → provider id map required; builder test in 1a |
| gateway structured output, tools, skills, background calls | not observed | profiles unusable until a real gateway spike |
| `dontAsk` denies the rest | confirmed (no user `permissions` block) | doctor refuses user-scope `permissions.allow`/`ask`/`defaultMode` |
| `Edit(<path>)` scoping | refuted as written | emit `Edit(//<abs>)` / `Write(//<abs>)` only |
| `Bash(<cmd>:*)` refuses chaining | confirmed for `&&`, `;` only | test `\|`, `$()`, backticks, newline, `>` with a positive control |
| `Skill(<name>)` blocks other skills | refuted | guard enforces skill names; profile and deny list as depth |
| `--setting-sources user`, `--strict-mcp-config` | confirmed | keep both; repo refusal stays as depth |
| `enabledPlugins` profile | confirmed | profile covers every plugin in the live init list |
| GateGuard ask under `-p` | confirmed (deny) | incidents from guard log only |
| `--settings` hook fires, exit 2 blocks | confirmed | none |
| guard panics still block | not observed | integration test of `wg guard` |
| guard abort contained by allowlist | confirmed | none |
| allowlist plus metacharacter rule refuse `x; rm -rf ~` | confirmed (stand-in) | replay in guard unit tests; "no Bash" fallback stays open |
| child calls stay out of the owner's `~/.claude` | refuted (they wrote to it) | profile or config dir that isolates children; rerun 8d under it |

## Open items

Tracked as GitHub issues; each blocks the Phase 1a work named. The design owner amends factory.md
§16 Phase 0 to move them out of Phase 0.

| Item | Blocks |
|---|---|
| Record a subscription usage-limit and an API 429 envelope (with `rate_limit_event`) | `RateLimited` classification and fallback |
| Gateway builder dry-run test pins argv, env and settings | the gateway profile builder |
| `wg guard` built to panic still blocks (`catch_unwind`, exit 2) | the guard; the "no Bash" fallback decision |
| Permission layer vs `\|`, `$()`, backticks, newline, `>` with a positive control | the "no Bash" fallback decision |
| Child calls isolated from the owner's `~/.claude`; rerun 8d (no key in Bash env) and 8c (helper cadence on a valid key) there | any key-bearing profile |
| Doctor refuses user-scope `permissions.allow`/`ask`/`defaultMode` | the runner's first real call |
