#!/usr/bin/env python3
"""A scripted stand-in for `claude -p` (design §15.3): it acts by the label in its guard manifest,
uses the real Haruspex CLI the way a Reader and a Reviewer would, exercises the guard hook from its
--settings file, and prints a result envelope. It records what it saw (argv flags, env names, hook
exit codes) next to the manifest, never any value of a key.

Shift tests steer it through the task text: NEEDS-DECISION makes the Reading raise a critical
escalation E-CRIT, RATE-LIMIT makes a worker hit a 429, and SLOW makes the first worker wait until
its parent dies (the kill-and-resume test), then exit without touching anything."""
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
    if "NEEDS-DECISION" in prompt and "E-CRIT" not in prompt:  # a decided escalation is not raised again
        reply["escalations"] = [{"id": "E-CRIT", "severity": "critical", "question": "Drop the legacy table?",
                                 "recommendation": "Keep it", "options": ["Keep it", "Drop it"]}]
elif label.startswith("challenge"):
    reply = {"added": [], "assign": [], "escalations": []}
elif label.startswith("work:"):
    record["hook_edit_unit_file"] = hook("Edit", {"file_path": "calc.py"})
    record["hook_edit_other_file"] = hook("Edit", {"file_path": "check.py"})
    record["hook_chained_bash"] = hook("Bash", {"command": "python3 check.py; touch escaped"})
    record["hook_approved_check"] = hook("Bash", {"command": "python3 check.py"})
    with open("test_calc.py", "w") as f:
        f.write("from calc import add\nassert add(2, 3) == 5, 'add is wrong'\n")
    with open("calc.py", "w") as f:
        f.write("def add(a, b):\n    return a + b\n")
    reply = {"summary": "add now adds"}
elif label.startswith("review"):
    with open(task_path()) as f:
        task = json.load(f)
    task["acceptance_criteria"][0].update({"status": "passed", "evidence": ["check:test"]})
    task["independent_review"] = {"status": "passed", "reviewer": "workgraph reviewer (opus)",
                                  "at": "2026-11-20T00:00:00Z", "findings": [], "waiver_reason": None}
    with open(task_path(), "w") as f:
        json.dump(task, f, indent=2)
    reply = {"passed": True, "criteria": [{"id": "AC-001", "status": "passed", "evidence": ["check:test"]}], "findings": []}
else:
    reply = {"rules": [], "proposals": []}

helper = settings.get("apiKeyHelper")
if helper:
    key = run(["/bin/sh", "-c", helper]).stdout.strip()
    record["helper_key_length"] = len(key)

with open(os.path.join(os.path.dirname(manifest_path), "fake-claude.jsonl"), "a") as f:
    f.write(json.dumps(record) + "\n")
print(envelope(reply))
