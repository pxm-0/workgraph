---
name: verify-todo
description: Drive the todo CLI the way a user does and capture terminal transcripts as evidence. Use to prove a change to `todo add` or `todo list` works in the built CLI.
---

# Verify the todo CLI

- **Launch:** nothing to build or keep alive. Each journey runs `./todo` with its own empty
  `TODO_FILE` in a scratch dir.
- **Doctor:** `./todo` exists and is executable. `run` checks this first and exits 3 if not.
- **Drive:** `TODO_FILE=<scratch>/x.txt ./todo add <text>` and `./todo list`. The prompts and
  outputs are the handles: `added: <text>` and `<n>. <text>`.
- **Evidence:** `WG_EVIDENCE_DIR/<feature>/transcript.txt` (each command, its output, `[exit N]`)
  and `WG_EVIDENCE_DIR/manifest.json` with a sha256 per artifact. The data file is checked as
  the side effect, alongside what the CLI prints.
- **Cleanup:** `run` removes its scratch dir on exit. Evidence stays in `WG_EVIDENCE_DIR`.
- **Helpers:** `.workgraph/verify/run` drives every feature in `features/`:
  `WG_EVIDENCE_DIR=$(mktemp -d) .workgraph/verify/run`.
