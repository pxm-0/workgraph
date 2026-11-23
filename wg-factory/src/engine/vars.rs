//! Template variables: blocks of text the kernel renders into the prompt files. The kernel fences
//! the untrusted ones (`rules`, `problem`, `evidence`) and adds its own (`hx`).

use crate::api::{CheckOutcome, Finding, Review, Rule, Scope};
use crate::learn::rules::for_prompt;
use crate::model::{add, RunState, Unit};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub type Vars = Vec<(String, String)>;

pub fn var(name: &str, value: String) -> (String, String) {
    (name.to_string(), value)
}

/// The rule lines for a role's prompt; the rules used are marked in play.
pub fn rules_var(state: &mut RunState, role: Scope) -> (String, String) {
    let rs: Vec<Rule> = for_prompt(&state.memory.rules, role);
    for r in &rs {
        add(&mut state.in_play, &r.id);
    }
    let lines: Vec<String> = rs.iter().map(|r| format!("- {}", r.text)).collect();
    var("rules", lines.join("\n"))
}

pub fn read_vars(state: &mut RunState) -> Vars {
    let decisions = if state.decisions.is_empty() {
        String::new()
    } else {
        let lines: Vec<String> = state
            .decisions
            .iter()
            .map(|d| format!("- {}: {}", d.id, d.answer))
            .collect();
        format!(
            "\nThe user answered these escalations; record each answer on its item (status \"validated\" or \"answered\", or \"accepted_risk\" with the user's words as the resolution):\n{}\n",
            lines.join("\n")
        )
    };
    let user_changes = if state.user_changes_before.is_empty() {
        String::new()
    } else {
        format!(
            "\nThese paths hold the user's uncommitted work. Plan no edits to them unless the task truly needs it: {}\n",
            state.user_changes_before.join(", ")
        )
    };
    let prior = if state.memory.checks.is_empty() {
        String::new()
    } else {
        format!(
            "\nCheck commands that worked here before: {}. Reuse them if they still fit.\n",
            state.memory.checks.join(" | ")
        )
    };
    let task_step = match &state.resume_task {
        Some(t) => format!(
            "continue task {t} (the Haruspex CLI's `task activate {t} .`) and apply the answers above"
        ),
        None => String::from(
            "create and activate it with the Haruspex CLI's `task create \"<short title>\" .`",
        ),
    };
    let rules = rules_var(state, Scope::Planner);
    alloc::vec![
        var("task", state.task.clone()),
        var("decisions", decisions),
        var("user_changes", user_changes),
        rules,
        var("prior_checks", prior),
        var("task_step", task_step),
        var("max_units", format!("{}", state.pipeline().max_units)),
    ]
}

pub fn challenge_vars(state: &RunState, task_id: &str) -> Vars {
    let units: Vec<String> = state
        .units
        .iter()
        .map(|u| format!("{} ({})", u.id, u.files.join(", ")))
        .collect();
    alloc::vec![
        var("task", state.task.clone()),
        var("task_id", task_id.to_string()),
        var("units", units.join("; ")),
    ]
}

fn unit_vars(state: &RunState, u: &Unit) -> Vars {
    alloc::vec![
        var("task", state.task.clone()),
        var("task_id", task_id(state)),
        var("unit_id", u.id.clone()),
        var("goal", u.goal.clone()),
        var("files", u.files.join(", ")),
    ]
}

pub fn work_vars(state: &mut RunState, u: &Unit) -> Vars {
    let mut v = unit_vars(state, u);
    let criteria = if u.criteria.is_empty() {
        String::from("its goal")
    } else {
        u.criteria.join(", ")
    };
    v.push(var("criteria", criteria));
    v.push(rules_var(state, Scope::Worker));
    v
}

pub fn fix_vars(state: &mut RunState, u: &Unit, problem: &str, round: u32, max: u32) -> Vars {
    let mut v = unit_vars(state, u);
    v.push(var("problem", problem.to_string()));
    v.push(var("round", format!("{round}")));
    v.push(var("max_rounds", format!("{max}")));
    v.push(rules_var(state, Scope::Worker));
    v
}

pub fn review_vars(state: &RunState, round: u32) -> Vars {
    let again = if round > 1 || state.stage_at.is_some() {
        " This is a re-review after fixes: check the earlier findings again and mark the fixed ones \"resolved\"."
    } else {
        ""
    };
    alloc::vec![
        var("task", state.task.clone()),
        var("task_id", task_id(state)),
        var("rereview", again.to_string()),
    ]
}

/// The Reading's task id, or the task a release (no Reading) works on.
pub fn task_id(state: &RunState) -> String {
    state
        .reading
        .as_ref()
        .map(|r| r.task_id.clone())
        .or_else(|| state.resume_task.clone())
        .unwrap_or_default()
}

/// The variables every `agent` stage gets; its prompt may use any of them (the kernel checks a
/// prompt against this set at load, so a new agent stage needs only TOML and a prompt).
pub const AGENT_VARS: &[&str] = &[
    "task",
    "task_id",
    "units",
    "criteria",
    "problem",
    "round",
    "max_rounds",
];

pub fn agent_vars(state: &RunState, problem: &str, run: u32) -> Vars {
    let units: Vec<String> = state
        .built
        .iter()
        .map(|u| format!("{} ({})", u.id, u.files.join(", ")))
        .collect();
    let mut criteria: Vec<String> = Vec::new();
    for u in &state.built {
        for c in &u.criteria {
            add(&mut criteria, c);
        }
    }
    alloc::vec![
        var("task", state.task.clone()),
        var("task_id", task_id(state)),
        var("units", units.join("; ")),
        var("criteria", criteria.join(", ")),
        var("problem", problem.to_string()),
        var("round", format!("{run}")),
        var("max_rounds", String::from("2")),
    ]
}

/// What a QA agent found, for the fixers and the agent's second run.
pub fn verdict_problem(findings: &[Finding]) -> String {
    let lines: Vec<String> = findings
        .iter()
        .map(|f| {
            let repro = f.repro.as_ref().map_or(String::new(), |r| {
                format!(
                    "\n  repro: $ {}\n  printed: {}\n  expected: {}",
                    r.command, r.output, r.expected
                )
            });
            format!(
                "- {} [{}] {}: {}{repro}",
                f.id,
                f.severity.name(),
                f.file.as_deref().unwrap_or(""),
                f.summary
            )
        })
        .collect();
    format!("QA found these problems:\n{}", lines.join("\n"))
}

pub fn check_problem(failed: &[CheckOutcome]) -> String {
    let parts: Vec<String> = failed
        .iter()
        .map(|f| {
            format!(
                "$ {}   (exit {})\n{}",
                f.cmd,
                f.exit_code,
                last_chars(&f.output_tail, 3000)
            )
        })
        .collect();
    format!(
        "After the build, these check commands failed:\n\n{}",
        parts.join("\n\n")
    )
}

pub fn review_problem(review: &Review) -> String {
    let mut lines: Vec<String> = review
        .criteria
        .iter()
        .filter(|c| !c.passed)
        .map(|c| format!("- criterion {} failed", c.id))
        .collect();
    lines.extend(
        review
            .findings
            .iter()
            .filter(|f| f.severity.serious())
            .map(|f| {
                format!(
                    "- {} [{}] {}: {}",
                    f.id,
                    f.severity.name(),
                    f.file.as_deref().unwrap_or(""),
                    f.summary
                )
            }),
    );
    format!(
        "The independent reviewer rejected the change:\n{}",
        lines.join("\n")
    )
}

/// The last `n` characters, as JS's `slice(-n)` (by char here; equal on ASCII).
pub fn last_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

/// A JSON string literal.
pub fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn json_strs(v: &[String]) -> String {
    let items: Vec<String> = v.iter().map(|s| json_str(s)).collect();
    format!("[{}]", items.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_strings_escape_quotes_controls_and_newlines() {
        assert_eq!(json_str("a\"b\\c\nd\u{1}"), "\"a\\\"b\\\\c\\nd\\u0001\"");
        assert_eq!(
            json_strs(&[String::from("x"), String::from("y")]),
            "[\"x\",\"y\"]"
        );
        assert_eq!(json_strs(&[]), "[]");
    }

    #[test]
    fn last_chars_keeps_the_tail() {
        assert_eq!(last_chars("abcdef", 3), "def");
        assert_eq!(last_chars("ab", 3), "ab");
    }
}
