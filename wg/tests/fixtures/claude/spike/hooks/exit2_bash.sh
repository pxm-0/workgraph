#!/bin/sh
in=$(cat); echo "$in" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin)["tool_name"])' >> <spike>/markers/exit2.log
case "$in" in *'"tool_name":"Bash"'*) echo "spike hook: Bash blocked" >&2; exit 2;; esac
exit 0
