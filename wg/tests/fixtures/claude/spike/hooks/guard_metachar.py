#!/usr/bin/env python3
# Spike stand-in for wg guard's Bash rule (§5.4): deny any command with a shell metacharacter. Fails closed.
import json, sys
try:
    ev = json.load(sys.stdin)
    cmd = ev.get("tool_input", {}).get("command", "") if ev.get("tool_name") == "Bash" else ""
    bad = [c for c in ';&|$`<>(){}\\\n' if c in cmd]
    with open("<spike>/markers/guard.log", "a") as f:
        f.write(json.dumps({"tool": ev.get("tool_name"), "cmd": cmd, "deny": bool(bad)}) + "\n")
    if bad:
        print("wg-guard-spike: metacharacter refused", file=sys.stderr); sys.exit(2)
except Exception as e:
    print(f"wg-guard-spike: {e}", file=sys.stderr); sys.exit(2)
