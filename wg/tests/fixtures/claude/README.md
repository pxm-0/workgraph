# `claude -p` envelopes (Phase 0 spike, ADR-001)

Recorded with Claude Code 2.1.311 on 2026-11-20 in a throwaway git repo, env cleared as in design
§5.1. Each `<case>.json` is the `type: "result"` envelope, re-indented and redacted (spike paths
`<spike>`, home `~`, account ids, emails and `sk-ant-` strings). `<case>.init.json` is an excerpt of
the stream-json `init` event. Argv per case: `docs/design/adr-001-claude-p-contract.md`.
`wg/tests/claude_fixtures.rs` checks every file here.

| Case | What it records |
|---|---|
| `success_structured` | haiku, `--json-schema`: `structured_output` object, `result` mirrors it as text |
| `schema_max_turns_1` | `--json-schema` with `--max-turns 1` still succeeds (`num_turns` 2) |
| `is_error_bad_model` | unknown `--model`: `is_error` true with `subtype` "success", `api_error_status` 404, cost 0, empty `modelUsage` |
| `budget_guard` | `--max-budget-usd 0.0001`: `subtype` "error_max_budget_usd" after the first turn |
| `max_turns` | `--max-turns 1` on a two-read task: `subtype` "error_max_turns" |
| `multi_model_usage` | sonnet call spawning a haiku subagent: two `modelUsage` keys, sonnet dominant |
| `perm_dontask` | `dontAsk` with only Read allowed: Bash and Write denied (one Write denial is GateGuard's) |
| `perm_edit_scope` | `Edit(notes.txt)`: matches `notes.txt` and `deep/notes.txt`, denies `calc.py` |
| `perm_edit_anchored_dot` | `Edit(./notes.txt)`: still matched `deep/notes.txt` too (not anchored) |
| `perm_edit_anchored_abs` | `Edit(//<abs>/notes.txt)`: only the root `notes.txt` was edited |
| `perm_bash_chain` | `Bash(git status:*)` refuses `git status && touch M` and `git status; touch M`; marker absent |
| `guard_metachar` | `--settings` PreToolUse guard refuses `echo wg sandbox exec -- x; touch M`; plain command runs |
| `hook_exit2` | `--settings` PreToolUse hook fires for Read and Bash; its exit 2 blocks Bash |
| `hook_abort` | hook that aborts (exit 134): the allowed command runs anyway; `touch` is stopped by the allowlist |
| `hook_ask_dontask` | hook answering `permissionDecision: "ask"` under `-p` `dontAsk`: the tool is denied |
| `hook_ask_default` | the same under `--permission-mode default`: denied |
| `skill_scope` | `Skill(karpathy-guidelines)` allowed: another skill still launches (no denial) |
| `skill_disallow` | `--disallowedTools 'Skill(ponytail:ponytail-help)'` blocks that skill |
| `skill_guard_hook` | a PreToolUse guard allowlisting one skill name blocks the other |
| `skill_frontmatter_widen` | a skill's `allowed-tools: Bash(touch:*)` does not widen `dontAsk`; Bash denied |
| `project_config_both_flags` | `--setting-sources user --strict-mcp-config`: project hook silent, no MCP servers at all |
| `project_config_no_flags` | neither flag: the project hook fired and the project `.mcp.json` server was spawned |
| `project_config_setting_sources_only` | project hook silent, project server absent, user and claude.ai servers start |
| `project_config_strict_mcp_only` | no MCP servers, but the project hook fired |
| `plugins_default` | `--setting-sources user`: the owner's full plugin list |
| `plugins_profile_off` | `--settings` `enabledPlugins` false for four plugins: they are gone from `init` |
| `subscription_no_helper` | no key variable, no helper: `apiKeySource` "none", call billed to the login |
| `api_key_helper_invalid` | `--settings` `apiKeyHelper` printing a dummy key: `apiKeySource` "apiKeyHelper", 401 after 10 retries |
| `gateway_loopback_dry_run` | `ANTHROPIC_BASE_URL` at a loopback stub, helper key, `--model sonnet`: stub saw `claude-sonnet-5-5` |
| `gateway_loopback_provider_model` | same with `--model openai/gpt-5.1`: the id reached the stub unchanged |
| `stage_t1_read` | toy task 1 (fix `add`), Reader-shaped call, haiku |
| `stage_t1_challenge` | toy task 1, Challenger-shaped call |
| `stage_t1_work` | toy task 1, Worker-shaped call (`Edit(//<abs>/calc.py)`, `Bash(python3 check.py)`) |
| `stage_t1_fix` | toy task 1, Fixer-shaped call |
| `stage_t1_review` | toy task 1, Reviewer-shaped call |
| `stage_t1_learn` | toy task 1, Learner-shaped call, no tools |
| `stage_t2_read` | toy task 2 (add `is_even`), Reader-shaped call |
| `stage_t2_challenge` | toy task 2, Challenger-shaped call |
| `stage_t2_work` | toy task 2, Worker-shaped call |
| `stage_t2_fix` | toy task 2, Fixer-shaped call |
| `stage_t2_review` | toy task 2, Reviewer-shaped call |
| `stage_t2_learn` | toy task 2, Learner-shaped call |
