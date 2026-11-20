#!/usr/bin/env python3
"""A stand-in for `gh` (design §15.3): serves issues, label timelines, body edits and PRs from a JSON
state file named by FAKE_GH_STATE, applies label edits, comments and PRs to it, and logs every argv.
Label edits are made by FAKE_GH_ACTOR (the owner's token), PR heads are read from the local bare
origin named in the state. `fail` maps a command ("pr ready") to how many more calls of it fail,
and `touch` maps one to a file it creates when it runs (a STOP file mid-publish, say). A PR the
test merged carries `state: MERGED` and its `mergeCommit` oid."""
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
    return {k: i[k] for k in ["number", "title", "body", "author", "labels", "url", "createdAt"]} | {
        "updatedAt": i["createdAt"], "state": i.get("state", "OPEN")}


def head(repo, branch):
    origin = state["origins"][repo]
    r = subprocess.run(["git", "--git-dir", origin, "rev-parse", f"refs/heads/{branch}"], capture_output=True, text=True)
    return r.stdout.strip()


def git(repo, *a):
    r = subprocess.run(["git", "--git-dir", state["origins"][repo], *a], capture_output=True, text=True)
    if r.returncode != 0:
        sys.stderr.write(r.stderr)
    return r


def commits(p):
    """A self PR's commits, as `gh pr view --json commits` lists them."""
    out = git(p["repo"], "log", "--format=%H%x1f%s%x1f%b%x1e", f"refs/heads/{p['base']}..{p['headRefOid']}").stdout
    rows = [c.strip("\n").split("\x1f") for c in out.split("\x1e") if c.strip()]
    return [{"oid": c[0], "messageHeadline": c[1], "messageBody": c[2]} for c in reversed(rows)]


def pr_view(p):
    p["headRefOid"] = head(p["repo"], p["headRefName"]) or p.get("headRefOid", "")
    view = {k: p[k] for k in ["number", "url", "state", "headRefOid"]}
    if "title" in args[-1]:
        view |= {"title": p.get("title", ""), "body": p.get("body", "")}
    if "commits" in args[-1]:  # the self gate's view (design §12.3 step 1)
        views = state.setdefault("views", {})
        views[str(p["number"])] = views.get(str(p["number"]), 0) + 1
        move = state.get("move_head")
        if move and move["number"] == p["number"] and views[str(p["number"])] > move["after"]:
            git(p["repo"], "update-ref", f"refs/heads/{p['headRefName']}", move["to"])
            p["headRefOid"] = view["headRefOid"] = move["to"]
        view |= {"baseRefName": p["base"], "isDraft": p["draft"], "mergeable": p.get("mergeable", "MERGEABLE"),
                 "labels": [{"name": l} for l in p["labels"]], "commits": commits(p)}
    if "mergeCommit" in args[-1]:  # `pr view --json ...,mergeCommit`: a merged PR names its merge
        view["mergeCommit"] = {"oid": p["mergeCommit"]} if p.get("mergeCommit") else None
    return view


command = " ".join(args[:2])
fails = state.get("fail", {})
if fails.get(command, 0) > 0:
    fails[command] -= 1
    sys.stderr.write(f"fake gh: {command} fails as scripted\n")
    save_and_exit(None, 1)
if command in state.get("touch", {}):
    open(state["touch"][command], "w").close()

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
if args[:2] == ["api", "--paginate"] and "/check-runs" in args[2]:
    sha = args[2].split("/")[-2]
    runs = state.get("check_runs", [])
    save_and_exit("\n".join(json.dumps({"name": r["name"], "head_sha": r.get("head_sha", sha), "app": r.get("app", "github-actions"),
                                         "status": r.get("status", "completed"), "conclusion": r.get("conclusion", "success"),
                                         "suite": i + 1}) for i, r in enumerate(runs)))
if args[:2] == ["api", "--paginate"] and "/actions/runs?head_sha=" in args[2]:
    sha = args[2].split("=")[-1]
    runs = state.get("check_runs", [])
    save_and_exit("\n".join(json.dumps({"suite": i + 1, "path": r["workflow"], "head_sha": sha}) for i, r in enumerate(runs)))
if args[0] == "api" and args[1] == "--paginate":
    _, owner, name, _, n, _ = args[2].split("/")
    lines = [json.dumps(e) for e in issue(f"{owner}/{name}", n).get("events", [])]
    save_and_exit("\n".join(lines))
if args[:2] == ["api", "graphql"]:
    fields = dict(a.split("=", 1) for a in opts("-F"))
    i = issue(f"{fields['owner']}/{fields['name']}", fields["number"])
    save_and_exit("\n".join(json.dumps(e) for e in i.get("edits", [])))
if args[:2] == ["label", "create"]:
    names = state.setdefault("labels", {}).setdefault(opt("-R"), [])
    if args[2] not in names:
        names.append(args[2])
    save_and_exit("")
if args[:2] == ["pr", "list"] and "--label" in args:
    repo, label = opt("-R"), opt("--label")
    save_and_exit([{"number": p["number"]} for p in state.setdefault("prs", [])
                   if p["repo"] == repo and p.get("state") == "OPEN" and label in p["labels"]])
if args[:2] == ["pr", "edit"]:
    p = next(p for p in state["prs"] if p["repo"] == opt("-R") and p["number"] == int(args[2]))
    for l in opts("--add-label"):
        if l not in p["labels"]:
            p["labels"].append(l)
    save_and_exit("")
if args[:2] == ["pr", "comment"]:
    p = next(p for p in state["prs"] if p["repo"] == opt("-R") and p["number"] == int(args[2]))
    with open(opt("--body-file")) as f:
        p.setdefault("comments", []).append(f.read())
    save_and_exit("")
if args[:2] == ["pr", "merge"]:
    p = next(p for p in state["prs"] if p["repo"] == opt("-R") and p["number"] == int(args[2]))
    h = head(p["repo"], p["headRefName"])
    if "--admin" in args or opt("--match-head-commit") != h or "--squash" not in args:
        sys.stderr.write("fake gh: merge refused: head moved, or not a pinned squash\n")
        save_and_exit(None, 1)
    tree = git(p["repo"], "rev-parse", f"{h}^{{tree}}").stdout.strip()
    main = git(p["repo"], "rev-parse", f"refs/heads/{p['base']}").stdout.strip()
    m = git(p["repo"], "-c", "user.name=gh", "-c", "user.email=gh@example.invalid", "commit-tree", tree, "-p", main,
            "-m", f"{p['title']} (#{p['number']})").stdout.strip()
    git(p["repo"], "update-ref", f"refs/heads/{p['base']}", m)
    if "--delete-branch" in args:
        git(p["repo"], "update-ref", "-d", f"refs/heads/{p['headRefName']}")
    p.update({"state": "MERGED", "mergeCommit": m, "headRefOid": h})
    save_and_exit("")
if args[:2] == ["issue", "create"]:
    repo = opt("-R")
    issues = state["issues"].setdefault(repo, {})
    n = 500 + len(issues)
    with open(opt("--body-file")) as f:
        body = f.read()
    issues[str(n)] = {"number": n, "title": opt("--title"), "body": body, "author": {"login": actor},
                      "labels": [{"name": l} for l in opts("--label")], "url": f"https://github.com/{repo}/issues/{n}",
                      "createdAt": now}
    save_and_exit(f"https://github.com/{repo}/issues/{n}")
if args[:2] == ["run", "list"]:
    save_and_exit([r for r in state.get("runs", []) if r["headSha"] == opt("--commit")])
if args[:2] == ["run", "download"]:
    run = next(r for r in state.get("runs", []) if str(r["databaseId"]) == args[2])
    name, dest = opt("-n"), opt("-D")
    src = run["artifacts"].get(name)
    if src is None:
        save_and_exit(None, 1)
    os.makedirs(dest, exist_ok=True)
    for f in os.listdir(src):
        with open(os.path.join(src, f), "rb") as a, open(os.path.join(dest, f), "wb") as b:
            b.write(a.read())
    save_and_exit("")
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
