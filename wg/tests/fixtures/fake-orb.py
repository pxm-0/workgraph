#!/usr/bin/env python3
"""A stand-in for OrbStack's `orb` and `orbctl` (by the name it is called as) for the shift tests.

`orbctl list|config get` report the machine running and LAN exposure off. `orb run -m M -u U
/usr/local/lib/wgsbx/wg sandbox-agent <sub>` plays the agent: `submit` unpacks the job's tar into a
scratch dir and runs each step with `sh -c` there (a test double: no isolation), `poll` answers the
result, `doctor` and `reap` answer like a healthy agent. `orb run -m M systemctl ...` prints a fixed
argus fingerprint. A file `orb-down` next to this script makes every agent call fail, as when the
agent is killed or the sim is gone. Every call is logged to `orb-log.jsonl`."""
import json
import os
import subprocess
import sys
import tarfile
import io

here = os.path.dirname(os.path.abspath(__file__))
state = os.path.join(here, "orb-state")
os.makedirs(state, exist_ok=True)
name = os.path.basename(sys.argv[0])
args = sys.argv[1:]
with open(os.path.join(here, "orb-log.jsonl"), "a") as f:
    f.write(json.dumps([name] + args) + "\n")


def out(v, code=0):
    print(json.dumps(v))
    sys.exit(code)


if name == "orbctl":
    if args[:1] == ["list"]:
        out([{"name": "oreochiserver-sim", "state": "running"}, {"name": "oreochiserver-sim-golden", "state": "stopped"}])
    if args[:2] == ["config", "get"]:
        print("false")
        sys.exit(0)
    sys.exit(2)

assert args[:2] == ["run", "-m"], args
rest = args[3:]
if rest[:1] == ["systemctl"]:
    if "--state=failed" in rest:
        sys.exit(0)
    print("argus-a.service enabled enabled\nargus-b.timer enabled enabled")
    sys.exit(0)
assert rest[:2] == ["-u", "wgsbx"] and rest[2] == "/usr/local/lib/wgsbx/wg" and rest[3] == "sandbox-agent", rest
sub = rest[4]
if os.path.exists(os.path.join(here, "orb-down")):
    out({"error": "the agent is not running"}, 2)
if sub == "doctor":
    out({"ok": True, "protocol": 1, "failed": None, "checks": []})
if sub == "reap":
    out({"removed": {"pods": 0, "containers": 0, "volumes": 0}, "failed": []})
if sub == "poll":
    job = rest[rest.index("--job") + 1]
    with open(os.path.join(state, job + ".json")) as f:
        print(f.read())
    sys.exit(0)
if sub == "submit":
    data = sys.stdin.buffer.read()
    line, tar = data.split(b"\n", 1)
    job = json.loads(line)
    work = os.path.join(state, job["job"])
    os.makedirs(work)
    with tarfile.open(fileobj=io.BytesIO(tar)) as t:
        t.extractall(work, filter="data")
    steps = []
    for s in job["steps"]:
        p = subprocess.run(["sh", "-c", s["cmd"]], cwd=work, capture_output=True, text=True, timeout=s["timeout_secs"])
        tail = "\n".join((p.stdout + p.stderr).splitlines()[-40:])
        steps.append({"name": s["name"], "exit": p.returncode, "timed_out": False, "oom": False, "tail": tail})
    result = {"job": job["job"], "nonce": job["nonce"], "steps": steps,
              "cleanup": {"containers_left": 0, "volumes_left": 0}}
    with open(os.path.join(state, job["job"] + ".json"), "w") as f:
        json.dump(result, f)
    out({"job": job["job"], "submitted": True})
out({"error": f"unknown {sub}"}, 2)
