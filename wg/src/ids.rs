//! Validated identifiers: every string from outside the kernel that reaches a path, an argv or a
//! label is checked here first. No regex: each rule is a small character-class check.

fn all(s: &str, ok: impl Fn(char) -> bool) -> bool {
    s.chars().all(ok)
}

fn id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// A unit id: `^[A-Za-z0-9-]{1,32}$`.
pub fn unit_id(s: &str) -> Result<(), String> {
    if (1..=32).contains(&s.len()) && all(s, id_char) {
        Ok(())
    } else {
        Err(format!(
            "unit id {:?} must be 1-32 letters, digits or dashes",
            truncate(s, 40)
        ))
    }
}

/// A Haruspex task id, as Haruspex itself allows: letters, digits, dot, underscore, dash; ≤ 64.
pub fn task_id(s: &str) -> Result<(), String> {
    let ok = (1..=64).contains(&s.len())
        && all(s, |c| c.is_ascii_alphanumeric() || "._-".contains(c))
        && !s.starts_with('.');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "task id {:?} is not a valid Haruspex task id",
            truncate(s, 40)
        ))
    }
}

/// A run id: `^[A-Za-z0-9-]{6,64}$`.
pub fn run_id(s: &str) -> Result<(), String> {
    if (6..=64).contains(&s.len()) && all(s, id_char) {
        Ok(())
    } else {
        Err("runId must be 6+ letters, digits or dashes".into())
    }
}

/// A check name: `^[a-z0-9_-]{1,32}$`.
pub fn check_name(s: &str) -> Result<(), String> {
    let ok = (1..=32).contains(&s.len())
        && all(s, |c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || "_-".contains(c)
        });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "check name {:?} must be 1-32 of a-z 0-9 _ -",
            truncate(s, 40)
        ))
    }
}

/// A command label: `^[a-z]+(:[A-Za-z0-9-]{1,32})?(#r?[0-9]{1,2})?(!retry|!gate)?$` (design §4.2).
pub fn label(s: &str) -> Result<(), String> {
    let err = || {
        Err(format!(
            "label {:?} is not a valid command label",
            truncate(s, 40)
        ))
    };
    let rest = match s.rsplit_once('!') {
        Some((r, "retry" | "gate")) => r,
        Some(_) => return err(),
        None => s,
    };
    let (rest, round) = match rest.split_once('#') {
        Some((r, n)) => (r, Some(n)),
        None => (rest, None),
    };
    if let Some(n) = round {
        let digits = n.strip_prefix('r').unwrap_or(n);
        if !(1..=2).contains(&digits.len()) || !all(digits, |c| c.is_ascii_digit()) {
            return err();
        }
    }
    let (word, arg) = match rest.split_once(':') {
        Some((w, a)) => (w, Some(a)),
        None => (rest, None),
    };
    // The design's `[a-z]+` is unbounded; 16 keeps a 200-character label out.
    if !(1..=16).contains(&word.len()) || !all(word, |c| c.is_ascii_lowercase()) {
        return err();
    }
    if let Some(a) = arg {
        if !(1..=32).contains(&a.len()) || !all(a, id_char) {
            return err();
        }
    }
    Ok(())
}

/// A branch the Reader may create in place: `^work/[a-z0-9-]{1,48}$`.
pub fn work_branch(s: &str) -> bool {
    s.strip_prefix("work/").is_some_and(|slug| {
        (1..=48).contains(&slug.len())
            && all(slug, |c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
            })
    })
}

pub fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// One line, control characters replaced by spaces, at most `max` characters.
pub fn one_line(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_follow_the_grammar() {
        for ok in [
            "read",
            "challenge",
            "work:U2",
            "fix:U2#3",
            "fix:U2#r1",
            "review#2",
            "learn",
            "check:baseline",
            "check:L1",
            "check:review1",
            "read!gate",
            "challenge!retry",
        ] {
            assert_eq!(label(ok), Ok(()), "{ok}");
        }
        let long = "a".repeat(200);
        for bad in [
            "",
            "../../../../Library/LaunchAgents/x.plist",
            "a/b",
            long.as_str(),
            "Read",
            "work:",
            "work:a#",
            "work:a#123",
            "work:a!oops",
            "fix:a#x",
            "x:a b",
        ] {
            assert!(label(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ids_names_and_branches() {
        assert!(unit_id("U-1").is_ok() && unit_id("a/b").is_err() && unit_id("").is_err());
        assert!(task_id("T-001").is_ok() && task_id("../x").is_err() && task_id(".x").is_err());
        assert!(run_id("run-abc123").is_ok() && run_id("x;y").is_err());
        assert!(check_name("unit_tests").is_ok() && check_name("Test").is_err());
        assert!(work_branch("work/add-x") && !work_branch("main") && !work_branch("work/A"));
        assert_eq!(one_line("a\nb\u{7}c", 3), "a b");
    }
}
