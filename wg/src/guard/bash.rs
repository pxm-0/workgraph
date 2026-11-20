//! Bash rules without a shell parser (design §5.4): an exact allowed string, or an allowed prefix
//! plus arguments free of shell metacharacters; Haruspex calls follow a per-position grammar.
//! Incidents are matched on subcommand position after tokenizing, never as substrings.

use serde::{Deserialize, Serialize};

const METACHARS: &[char] = &[
    ';', '&', '|', '$', '`', '<', '>', '(', ')', '{', '}', '\\', '\n', '\r',
];

/// What one role's Bash may run.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BashRules {
    pub exact: Vec<String>,
    pub prefixes: Vec<String>,
    /// The pinned Haruspex CLI path, when the role may call it.
    pub haruspex_cli: Option<String>,
    /// Allowed Haruspex subcommands: `init`, `task create`, `task activate`, `gate`, `status`.
    pub haruspex: Vec<String>,
    /// The only task id `task activate` may name.
    pub task_id: Option<String>,
    /// `git switch -c work/<slug>` is allowed (in place, Reader only).
    pub switch_branch: bool,
}

#[derive(Debug, PartialEq)]
pub enum Bash {
    Allow,
    Deny(String),
    Incident(String),
}

fn is_wg(token: &str) -> bool {
    token == "wg" || token.ends_with("/wg")
}

fn is_python(token: &str) -> bool {
    token == "python3" || token.ends_with("/python3") || token == "python"
}

/// Approve, record or reopen through any Haruspex CLI, or `wg finalize|approve|self|auth`.
fn incident(tokens: &[&str]) -> Option<String> {
    for w in tokens.windows(2) {
        let (a, b) = (w[0], w[1]);
        let hx = a.ends_with("haruspex.py") || a == "haruspex";
        if hx && matches!(b, "approve" | "record" | "reopen") {
            return Some(format!("haruspex {b} is the owner's alone"));
        }
        if is_wg(a) && matches!(b, "finalize" | "approve" | "self" | "auth") {
            return Some(format!("wg {b} is the owner's alone"));
        }
    }
    None
}

fn haruspex(rules: &BashRules, args: &[&str]) -> Bash {
    let allowed = |s: &str| rules.haruspex.iter().any(|h| h == s);
    let deny = |why: &str| Bash::Deny(format!("haruspex: {why}"));
    match args {
        ["init", "."] if allowed("init") => Bash::Allow,
        ["init", ..] => deny("only `init .` is allowed"),
        ["task", "create", rest @ ..] if allowed("task create") => {
            let Some((last, body)) = rest.split_last() else {
                return deny("task create needs a title and `.`");
            };
            if *last != "." || body.is_empty() {
                return deny("task create must end with the path `.`");
            }
            let mut i = 0;
            let mut title = 0;
            while i < body.len() {
                match body[i] {
                    "--id" => {
                        let ok = body
                            .get(i + 1)
                            .is_some_and(|id| crate::ids::task_id(id).is_ok());
                        if !ok {
                            return deny("--id needs a valid task id");
                        }
                        i += 2;
                    }
                    t if t.starts_with('-') => return deny("task create takes no other flags"),
                    _ => {
                        title += 1;
                        i += 1;
                    }
                }
            }
            if title == 0 {
                return deny("task create needs a title");
            }
            Bash::Allow
        }
        ["task", "activate", id, "."]
            if allowed("task activate") && rules.task_id.as_deref() == Some(*id) =>
        {
            Bash::Allow
        }
        ["gate", "ready-to-build", ".", "--json"] if allowed("gate") => Bash::Allow,
        ["status", "."] | ["status", ".", "--json"] if allowed("status") => Bash::Allow,
        _ => deny("this call is outside the role's grammar"),
    }
}

pub fn check(rules: &BashRules, cmd: &str) -> Bash {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    if let Some(why) = incident(&tokens) {
        return Bash::Incident(why);
    }
    if rules.exact.iter().any(|e| e == cmd) {
        return Bash::Allow;
    }
    if let Some(c) = cmd.chars().find(|c| METACHARS.contains(c)) {
        return Bash::Deny(format!("shell metacharacter {c:?} is not allowed"));
    }
    if let (Some(cli), [py, path, rest @ ..]) = (&rules.haruspex_cli, tokens.as_slice()) {
        if is_python(py) && path == cli {
            return haruspex(rules, rest);
        }
    }
    if rules.switch_branch {
        if let ["git", "switch", "-c", branch] = tokens.as_slice() {
            return if crate::ids::work_branch(branch) {
                Bash::Allow
            } else {
                Bash::Deny("only work/<slug> branches may be created".into())
            };
        }
    }
    let normalized = tokens.join(" ");
    let prefixed = rules
        .prefixes
        .iter()
        .any(|p| normalized == *p || normalized.starts_with(&format!("{p} ")));
    if prefixed {
        Bash::Allow
    } else {
        Bash::Deny(
            "this command is not on the role's allowlist; run approved commands exactly as given"
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLI: &str = "/opt/haruspex/scripts/haruspex.py";

    fn rules() -> BashRules {
        BashRules {
            exact: vec!["git status".into(), "npm run x && y".into()],
            prefixes: vec!["npm test".into(), "echo wg sandbox exec".into()],
            haruspex_cli: Some(CLI.into()),
            haruspex: ["init", "task create", "task activate", "gate", "status"]
                .map(String::from)
                .to_vec(),
            task_id: Some("T-001".into()),
            switch_branch: true,
        }
    }

    fn hx(args: &str) -> String {
        format!("python3 {CLI} {args}")
    }

    #[test]
    fn allowed_commands() {
        let ok = [
            "git status".to_string(),
            "npm run x && y".into(),
            "npm test".into(),
            "npm test -- --grep 'it s'".into(),
            "echo wg sandbox exec -- ok".into(),
            hx("init ."),
            hx("task create \"Fix add\" ."),
            hx("task create --id T-009 Fix ."),
            hx("task activate T-001 ."),
            hx("gate ready-to-build . --json"),
            hx("status ."),
            hx("status . --json"),
            "git switch -c work/add-x".into(),
        ];
        for c in ok {
            assert_eq!(check(&rules(), &c), Bash::Allow, "{c}");
        }
    }

    #[test]
    fn denied_commands_are_blocks_not_incidents() {
        let denied = [
            "npm test; touch x".to_string(),
            "npm test && touch x".into(),
            "npm test | tee x".into(),
            "npm test $(touch x)".into(),
            "npm test `touch x`".into(),
            "npm test > x".into(),
            "npm test < x".into(),
            "npm test\ntouch x".into(),
            "npm test & touch x".into(),
            "npm test (x)".into(),
            "npm test {x}".into(),
            "npm test \\x".into(),
            "echo wg sandbox exec -- x; touch y".into(),
            "npm testing".into(),
            "git status --porcelain".into(),
            "gh pr create".into(),
            "git push origin main".into(),
            "curl example.com".into(),
            hx("init /other --force"),
            hx("init . --force"),
            hx("task create --force T ."),
            hx("task create T /elsewhere"),
            hx("task create --id ../x T ."),
            hx("task activate T-999 ."),
            hx("gate ready-to-build . --record"),
            hx("gate verification . --json"),
            hx("upgrade ."),
            "python3 /other/haruspex2.py gate ready-to-build . --json".into(),
            "git switch -c main".into(),
            "git switch -c work/x extra".into(),
        ];
        for c in denied {
            assert!(matches!(check(&rules(), &c), Bash::Deny(_)), "{c}");
        }
    }

    #[test]
    fn approve_record_reopen_and_owner_commands_are_incidents() {
        let incidents = [
            hx("approve ready-to-build . --by me"),
            hx("record deployment succeeded prod"),
            hx("reopen ."),
            "python3 .haruspex/bin/haruspex.py approve ready-to-build .".to_string(),
            "x; python3 /a/haruspex.py approve ready-to-build .".into(),
            "/Users/me/.workgraph/bin/wg finalize --run r".into(),
            "wg approve --repo a --pr 1".into(),
            "wg self gate".into(),
            "wg auth key anthropic-api".into(),
        ];
        for c in incidents {
            assert!(matches!(check(&rules(), &c), Bash::Incident(_)), "{c}");
        }
        // `--record` is a flag, not a subcommand: a plain block, not an incident.
        assert!(matches!(
            check(&rules(), &hx("gate ready-to-build . --record")),
            Bash::Deny(_)
        ));
    }

    #[test]
    fn without_haruspex_or_switch_rights_those_are_denied() {
        let r = BashRules {
            prefixes: vec!["npm test".into()],
            ..BashRules::default()
        };
        assert!(matches!(
            check(&r, &hx("gate ready-to-build . --json")),
            Bash::Deny(_)
        ));
        assert!(matches!(check(&r, "git switch -c work/x"), Bash::Deny(_)));
    }
}
