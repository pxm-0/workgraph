#!/usr/bin/env python3
"""A stand-in for `gh` (design §15.3): serves issues, label timelines, body edits and PRs from a JSON
state file named by FAKE_GH_STATE, applies label edits, comments and PRs to it, and logs every argv.
Label edits are made by FAKE_GH_ACTOR (the owner's token), PR heads are read from the local bare
origin named in the state."""
import json
import os
import subprocess
import sys
import time

path = os.environ["FAKE_GH_STATE"]
with open(path) as f:
    state = json.load(f)
args = sys.argv[1:]
state.setdefault("log", []).append(args)
actor = os.environ.get("FAKE_GH_ACTOR", "pxm-0")
now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def opt(name, default=None):
    return args[args.index(name) + 1] if name in args else default


def opts(name):
    return [args[i + 1] for i, a in enumerate(args) if a == name]


def save_and_exit(out=None, code=0):
    with open(path, "w") as f:
        json.dump(state, f, indent=1)
    if out is not None:
        print(out if isinstance(out, str) else json.dumps(out))
    sys.exit(code)


def issue(repo, n):
    return state["issues"][repo][str(n)]


def public(i):
    return {k: i[k] for k in ["number", "title", "body", "author", "labels", "url", "createdAt"]} | {"updatedAt": i["createdAt"]}


def head(repo, branch):
    origin = state["origins"][repo]
    r = subprocess.run(["git", "--git-dir", origin, "rev-parse", f"refs/heads/{branch}"], capture_output=True, text=True)
    return r.stdout.strip()


def pr_view(p):
    p["headRefOid"] = head(p["repo"], p["headRefName"])
    return {k: p[k] for k in ["number", "url", "state", "headRefOid"]}


if args[:2] == ["auth", "status"]:
    save_and_exit("Logged in")
if args[:2] == ["issue", "list"]:
    repo, label = opt("-R"), opt("--label")
    found = [public(i) for i in state["issues"].get(repo, {}).values()
             if i.get("state", "OPEN") == "OPEN" and any(l["name"] == label for l in i["labels"])]
    save_and_exit(found)
if args[:2] == ["issue", "view"]:
    save_and_exit(public(issue(opt("-R"), args[2])))
if args[:2] == ["issue", "edit"]:
    i = issue(opt("-R"), args[2])
    for l in opts("--remove-label"):
        i["labels"] = [x for x in i["labels"] if x["name"] != l]
    for l in opts("--add-label"):
        if not any(x["name"] == l for x in i["labels"]):
            i["labels"].append({"name": l})
        i.setdefault("events", []).append({"label": l, "actor": actor, "at": now})
    save_and_exit("")
if args[:2] == ["issue", "comment"]:
    with open(opt("--body-file")) as f:
        issue(opt("-R"), args[2]).setdefault("comments", []).append(f.read())
    save_and_exit("")
if args[0] == "api" and args[1] == "--paginate":
    _, owner, name, _, n, _ = args[2].split("/")
    lines = [json.dumps(e) for e in issue(f"{owner}/{name}", n).get("events", [])]
    save_and_exit("\n".join(lines))
if args[:2] == ["api", "graphql"]:
    fields = dict(a.split("=", 1) for a in opts("-F"))
    i = issue(f"{fields['owner']}/{fields['name']}", fields["number"])
    save_and_exit("\n".join(json.dumps(e) for e in i.get("edits", [])))
if args[:2] == ["pr", "list"]:
    repo, branch = opt("-R"), opt("--head")
    save_and_exit([pr_view(p) for p in state.setdefault("prs", []) if p["repo"] == repo and p["headRefName"] == branch])
if args[:2] == ["pr", "create"]:
    repo, branch = opt("-R"), opt("--head")
    if not head(repo, branch):
        save_and_exit(None, 1)
    n = 100 + len(state.setdefault("prs", []))
    with open(opt("--body-file")) as f:
        body = f.read()
    p = {"repo": repo, "number": n, "url": f"https://github.com/{repo}/pull/{n}", "state": "OPEN",
         "headRefName": branch, "base": opt("--base"), "title": opt("--title"), "body": body,
         "draft": "--draft" in args, "labels": opts("--label")}
    state["prs"].append(p)
    pr_view(p)
    save_and_exit(f"Creating pull request\n{p['url']}")
if args[:2] == ["pr", "view"]:
    p = next(p for p in state["prs"] if p["repo"] == opt("-R") and p["number"] == int(args[2]))
    save_and_exit(pr_view(p))
if args[:2] == ["pr", "ready"]:
    p = next(p for p in state["prs"] if p["repo"] == opt("-R") and p["number"] == int(args[2]))
    p["draft"] = False
    save_and_exit("")
sys.stderr.write(f"fake gh: unknown command {args}\n")
save_and_exit(None, 1)
