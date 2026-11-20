#!/usr/bin/env python3
"""A scripted stand-in for `claude -p` (design §15.3): it acts by the label in its guard manifest,
uses the real Haruspex CLI the way a Reader and a Reviewer would, exercises the guard hook from its
--settings file, and prints a result envelope. It records what it saw (argv flags, env names, hook
exit codes) next to the manifest, never any value of a key.

Shift tests steer it through the task text: NEEDS-DECISION makes the Reading raise a critical
escalation E-CRIT, RATE-LIMIT makes a worker hit a 429, and SLOW makes the first worker wait until
its parent dies (the kill-and-resume test), then exit without touching anything. STOP-MID makes the
first worker create the STOP file as it works (and STOP-TWICE the first reviewer too), FAIL-CHECK
writes a test no fixer makes pass, WORKFLOW writes a test that drops a GitHub workflow when it runs,
INCIDENT makes the worker run `wg shift run` through the guard hook, WRONG-BRANCH makes it
switch branches, and RISKY makes the unit risky (it also touches auth_token.py) so the worker runs
its check through `wg sandbox exec`, the one Bash it is granted. Phase 4 stages: `explore` (the QA
agent) passes, `rereview` reviews as `review` does, every `<stage>:<unit>#<n>` label is a fixer
(a `regression:` one sets `E2E = True` in calc.py, which a test's e2e check asserts), `notes`
writes release notes and `plan` fills the Haruspex task's release plan (and, when the file
`$HOME/release-incident` exists, also approves ready-to-release itself: an incident). Phase 5:
SELF-DOCS plans and writes one editable file, docs/calc.md (a self-lane task), and when the file
`$HOME/factory-proposal` exists the learn agent proposes a `factory` change."""
import json
import os
import subprocess
import sys
import time


def flag(name):
    argv = sys.argv[1:]
    return argv[argv.index(name) + 1] if name in argv else None


def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


manifest_path = os.environ["WG_GUARD_MANIFEST"]
manifest = json.load(open(manifest_path))
settings = json.load(open(flag("--settings")))
label = manifest["label"]
model = flag("--model")
prompt = sys.stdin.read()
cli = (manifest["bash"] or {}).get("haruspex_cli")
record = {"label": label, "model": model, "env": sorted(os.environ), "cwd": os.getcwd()}


def hook(tool, tool_input):
    command = settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
    payload = json.dumps({"tool_name": tool, "tool_input": tool_input, "cwd": os.getcwd()})
    return subprocess.run(["/bin/sh", "-c", command], input=payload, capture_output=True, text=True).returncode


def task_path():
    return ".haruspex/tasks/T-001.json"


def complete_task():
    with open(task_path()) as f:
        task = json.load(f)
    task.update({
        "status": "ready",
        "risk": "low",
        "problem": {"summary": "add subtracts", "affected_actors": ["callers"], "current_behavior": "a - b",
                    "desired_outcome": "a + b", "success_metrics": ["check passes"]},
        "scope": {"included": ["calc.py"], "excluded": ["everything else"]},
        "acceptance_criteria": [{"id": "AC-001", "criterion": "add(2, 3) == 5", "status": "unverified", "evidence": []}],
        "failure_modes": [{"id": "FM-001", "scenario": "negative numbers", "severity": "medium", "status": "addressed"}],
        "test_plan": {"unit": ["test_calc.py asserts add(2, 3) == 5 at the public seam"]},
        "required_checks": ["test"],
    })
    for review in task["review_coverage"]["core"].values():
        review.update({"status": "complete", "rationale": "toy change"})
    with open(task_path(), "w") as f:
        json.dump(task, f, indent=2)
    with open(".haruspex/project.json") as f:
        project = json.load(f)
    project["project"]["risk"] = "low"
    project["characteristics"] = {k: False for k in project["characteristics"]}
    project["commands"] = {"test": "python3 check.py"}
    with open(".haruspex/project.json", "w") as f:
        json.dump(project, f, indent=2)


def envelope(reply):
    return json.dumps({"type": "result", "subtype": "success", "is_error": False, "structured_output": reply,
                       "total_cost_usd": 0.001, "num_turns": 1, "session_id": "fake",
                       "modelUsage": {f"claude-{model}-5-5": {"outputTokens": 5}}})


home = os.environ["HOME"]


def once(name):
    p = os.path.join(home, name)
    if os.path.exists(p):
        return False
    open(p, "w").close()
    return True


def stop_the_shift():
    open(os.path.join(home, ".workgraph", "STOP"), "w").close()


if label == "doctor":
    print(envelope({"ok": True}))
    sys.exit(0)
if label.startswith("work:") and "RATE-LIMIT" in prompt:
    print(json.dumps({"type": "result", "subtype": "success", "is_error": True, "api_error_status": 429,
                      "result": "rate limited", "session_id": "fake"}))
    sys.exit(0)
slow_marker = os.path.join(os.environ["HOME"], "slow-once")
if label.startswith("work:") and "SLOW" in prompt and not os.path.exists(slow_marker):
    open(slow_marker, "w").close()
    parent = os.getppid()
    for _ in range(600):
        if os.getppid() != parent:
            sys.exit(1)
        time.sleep(0.1)

if label.startswith("work:") and "STOP-MID" in prompt and once("stopped-at-work"):
    stop_the_shift()
    if "STOP-TWICE" in prompt:
        open(os.path.join(home, "stop-at-review"), "w").close()
if label.startswith("review") and os.path.exists(os.path.join(home, "stop-at-review")) and once("stopped-at-review"):
    stop_the_shift()

if label.startswith("read"):
    record["init"] = run(["python3", cli, "init", "."]).returncode
    record["create"] = run(["python3", cli, "task", "create", "Fix add", ".", "--id", "T-001"]).returncode
    complete_task()
    branch = run(["git", "branch", "--show-current"]).stdout.strip()
    reply = {"branch": branch, "taskId": "T-001", "initialized": True, "risk": "low",
             "checks": [{"name": "test", "command": "python3 check.py"}],
             "criteria": [{"id": "AC-001", "text": "add(2, 3) == 5"}],
             "units": [{"id": "fix", "goal": "make add add", "files": ["calc.py", "test_calc.py"], "dependsOn": [],
                        "criteria": ["AC-001"], "size": "small", "risky": False}],
             "escalations": []}
    if "SELF-DOCS" in prompt:
        reply["units"][0]["files"] = ["docs/calc.md"]
    if "RISKY" in prompt:
        reply["units"][0].update({"files": ["calc.py", "test_calc.py", "auth_token.py"], "risky": True,
                                  "riskReason": "touches the auth token"})
    if "NEEDS-DECISION" in prompt and "E-CRIT" not in prompt:  # a decided escalation is not raised again
        reply["escalations"] = [{"id": "E-CRIT", "severity": "critical", "question": "Drop the legacy table?",
                                 "recommendation": "Keep it", "options": ["Keep it", "Drop it"]}]
elif label.startswith("challenge"):
    reply = {"added": [], "assign": [], "escalations": []}
elif label.startswith("work:"):
    record["files_seen"] = sorted(os.listdir("."))  # an eval arm's hidden tests are never here yet
    record["hook_edit_unit_file"] = hook("Edit", {"file_path": "calc.py"})
    record["hook_edit_other_file"] = hook("Edit", {"file_path": "check.py"})
    record["hook_chained_bash"] = hook("Bash", {"command": "python3 check.py; touch escaped"})
    record["hook_approved_check"] = hook("Bash", {"command": "python3 check.py"})
    if "SELF-DOCS" in prompt:
        os.makedirs("docs", exist_ok=True)
        with open("docs/calc.md", "w") as f:
            f.write("# calc\n\nadd(a, b) returns a + b.\n")
        print(envelope({"summary": "documented add"}))
        sys.exit(0)
    test = "from calc import add\nassert add(2, 3) == 5, 'add is wrong'\n"
    if "FAIL-CHECK" in prompt:
        test = "from calc import add\nassert add(2, 3) == 6, 'add(2, 3) is not 6'\n"
    if "WORKFLOW" in prompt:
        test += "import os\nos.makedirs('.github/workflows', exist_ok=True)\nopen('.github/workflows/x.yml', 'w').write('on: push\\n')\n"
    with open("test_calc.py", "w") as f:
        f.write(test)
    with open("calc.py", "w") as f:
        f.write("def add(a, b):\n    return a + b\n")
    if "INCIDENT" in prompt:
        record["hook_owner_command"] = hook("Bash", {"command": "wg shift run"})
    if "RISKY" in prompt:
        with open("auth_token.py", "w") as f:
            f.write("TOKEN_TTL = 3600\n")
        exec_prefix = next(p for p in manifest["bash"]["prefixes"] if " sandbox exec --unit " in p)
        command = exec_prefix + " -- python3 check.py"
        record["hook_sandbox_exec"] = hook("Bash", {"command": command})
        done = run(["/bin/sh", "-c", command])
        record["sandbox_exec"] = {"code": done.returncode, "out": json.loads(done.stdout.splitlines()[-1])}
    if "WRONG-BRANCH" in prompt:
        run(["git", "checkout", "-q", "-b", "wg/99-elsewhere"])
    reply = {"summary": "add now adds"}
elif label.startswith("regression:") and "#" in label:
    with open("calc.py", "a") as f:
        f.write("\nE2E = True\n")
    reply = {"summary": "logout now ends the session"}
elif ":" in label and "#" in label:
    reply = {"summary": "tried, but nothing changed"}
elif label.startswith("explore"):
    reply = {"passed": True, "findings": []}
elif label == "notes":
    reply = {"summary": "add now adds", "changes": ["add(2, 3) returns 5"], "breaking": False}
elif label == "plan":
    with open(task_path()) as f:
        task = json.load(f)
    task["status"] = "ready_for_release"
    task["release_plan"] = {"deployment_steps": ["merge is deployed by the owner"],
                            "rollback_steps": ["revert the merge commit"],
                            "smoke_tests": ["python3 check.py"], "monitoring": ["none: a library"],
                            "ownership": ["pxm-0"]}
    with open(task_path(), "w") as f:
        json.dump(task, f, indent=2)
    if os.path.exists(os.path.join(home, "release-incident")):
        with open(".haruspex/state.json") as f:
            state = json.load(f)
        state.setdefault("approvals", {})["ready_to_release"] = {
            "status": "approved", "by": "the plan writer", "at": "2026-11-21T00:00:00Z"}
        with open(".haruspex/state.json", "w") as f:
            json.dump(state, f, indent=2)
    reply = {"ready": True, "summary": "revert to roll back; smoke with the check"}
elif label.startswith(("review", "rereview")):
    with open(task_path()) as f:
        task = json.load(f)
    task["acceptance_criteria"][0].update({"status": "passed", "evidence": ["check:test"]})
    task["independent_review"] = {"status": "passed", "reviewer": "workgraph reviewer (opus)",
                                  "at": "2026-11-20T00:00:00Z", "findings": [], "waiver_reason": None}
    with open(task_path(), "w") as f:
        json.dump(task, f, indent=2)
    reply = {"passed": True, "criteria": [{"id": "AC-001", "status": "passed", "evidence": ["check:test"]}], "findings": []}
elif os.path.exists(os.path.join(home, "factory-proposal")):
    reply = {"rules": [], "proposals": [{"category": "factory", "proposal": "The worker prompt should name the check command once.",
                                         "evidence": "work:fix ran the check three times"}]}
else:
    reply = {"rules": [], "proposals": []}

helper = settings.get("apiKeyHelper")
if helper:
    key = run(["/bin/sh", "-c", helper]).stdout.strip()
    record["helper_key_length"] = len(key)

with open(os.path.join(os.path.dirname(manifest_path), "fake-claude.jsonl"), "a") as f:
    f.write(json.dumps(record) + "\n")
print(envelope(reply))
