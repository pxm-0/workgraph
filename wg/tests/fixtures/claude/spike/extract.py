# Copies each raw stream (raw/<case>.json, written by run.sh) into the fixtures dir, redacted:
#   <case>.json       the `type: "result"` envelope
#   <case>.init.json  the whole stream-json `init` event, for the cases listed in INIT
#   rate_limit_event.json  the first `rate_limit_event` of subscription_no_helper
# Redaction only replaces values, never reshapes: spike paths become <spike>, home ~, account ids,
# emails and sk-ant- strings <redacted>; memory_paths and messaging_socket_path values become
# <redacted>; the owner's private name lists (tools, slash_commands, skills, agents) become one
# string "<N names redacted>" each, so the key and the array-of-strings type stay.
# usage: python3 extract.py <spike>/raw <repo>/wg/tests/fixtures/claude
import json, os, re, sys

raw, out = sys.argv[1], sys.argv[2]
spike = os.path.dirname(os.path.abspath(raw))
INIT = ("subscription_no_helper", "api_key_helper_invalid", "gateway_loopback_dry_run",
        "project_config_both_flags", "project_config_no_flags",
        "project_config_setting_sources_only", "project_config_strict_mcp_only",
        "plugins_default", "plugins_profile_off")
NAME_LISTS = ("tools", "slash_commands", "skills", "agents")
secrets = [os.environ.get(k) for k in ("CLAUDE_CODE_ACCOUNT_UUID", "CLAUDE_CODE_ORGANIZATION_UUID",
                                       "CLAUDE_CODE_USER_EMAIL")]
secrets = [s for s in secrets if s]


def redact(text):
    for s in secrets:
        text = text.replace(s, "<redacted>")
    text = text.replace(spike, "<spike>").replace(os.path.realpath(spike), "<spike>")
    text = text.replace(os.path.expanduser("~"), "~")
    text = re.sub(r"sk-ant-[A-Za-z0-9_-]+", "sk-ant-<redacted>", text)
    return re.sub(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}", "<email>", text)


def redact_init(e):
    e = dict(e)
    for k in NAME_LISTS:
        if isinstance(e.get(k), list):
            e[k] = [f"<{len(e[k])} names redacted>"]
    if isinstance(e.get("memory_paths"), dict):
        e["memory_paths"] = {k: "<redacted>" for k in e["memory_paths"]}
    if "messaging_socket_path" in e:
        e["messaging_socket_path"] = "<redacted>"
    return e


def redact_rate_limit(e):
    info = dict(e["rate_limit_info"])
    if "overageDisabledReason" in info:
        info["overageDisabledReason"] = "<redacted>"
    return {**e, "rate_limit_info": info}


def write(name, value):
    with open(os.path.join(out, name), "w") as f:
        f.write(redact(json.dumps(value, indent=2)) + "\n")


total = 0.0
skip = ("probe", "stages_t1", "stages_t2")
for name in sorted(f[:-5] for f in os.listdir(raw) if f.endswith(".json") and f[:-5] not in skip):
    events = [json.loads(l) for l in open(os.path.join(raw, name + ".json")) if l.strip()]
    result = [e for e in events if e.get("type") == "result"]
    assert len(result) == 1, (name, len(result))
    total += result[0].get("total_cost_usd") or 0
    write(name + ".json", result[0])
    init = [e for e in events if e.get("subtype") == "init"]
    if name in INIT:
        assert len(init) == 1, (name, "init")
        write(name + ".init.json", redact_init(init[0]))
    if name == "subscription_no_helper":
        limits = [e for e in events if e.get("type") == "rate_limit_event"]
        assert limits, "no rate_limit_event"
        write("rate_limit_event.json", redact_rate_limit(limits[0]))
print(f"total_cost_usd over fixtures: {total:.4f}")
