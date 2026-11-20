#!/usr/bin/env python3
# Spike stand-in for wg guard's Skill allowlist: only karpathy-guidelines may be invoked. Fails closed.
import json, sys
try:
    ev = json.load(sys.stdin)
    if ev.get("tool_name") == "Skill" and ev.get("tool_input", {}).get("skill") != "karpathy-guidelines":
        print("wg-guard-spike: skill not allowlisted", file=sys.stderr); sys.exit(2)
except Exception as e:
    print(f"wg-guard-spike: {e}", file=sys.stderr); sys.exit(2)
