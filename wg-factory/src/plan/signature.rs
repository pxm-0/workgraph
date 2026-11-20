//! A failure's identity across runs: the command plus its first error line with paths and numbers
//! stripped. No regex (design §10.2); equivalent to run.js's `\S*[/\\]\S*` → PATH, `\d+` → N.

use alloc::string::String;
use alloc::vec::Vec;

const MARKERS: [&str; 8] = [
    "error",
    "fail",
    "expected",
    "assert",
    "exception",
    "cannot",
    "undefined",
    "not found",
];

pub fn signature(cmd: &str, output_tail: &str) -> String {
    let lines: Vec<&str> = output_tail
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .collect();
    let line = lines
        .iter()
        .find(|l| {
            let lower = l.to_lowercase();
            MARKERS.iter().any(|m| lower.contains(m))
        })
        .or(lines.last())
        .copied()
        .unwrap_or("");
    let mut out = String::new();
    out.push_str(cmd);
    out.push_str(" | ");
    let body: String = normalize(line).chars().take(120).collect();
    out.push_str(&body);
    out
}

fn normalize(line: &str) -> String {
    let mut pathless = String::new();
    // Tokens split on whitespace keep the whitespace between them, as the regex replacement does.
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut String| {
        if token.contains('/') || token.contains('\\') {
            out.push_str("PATH");
        } else {
            out.push_str(token);
        }
        token.clear();
    };
    for c in line.chars() {
        if c.is_whitespace() {
            flush(&mut token, &mut pathless);
            pathless.push(c);
        } else {
            token.push(c);
        }
    }
    flush(&mut token, &mut pathless);
    let mut numbered = String::new();
    let mut in_digits = false;
    for c in pathless.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                numbered.push('N');
            }
            in_digits = true;
        } else {
            in_digits = false;
            numbered.push(c);
        }
    }
    let words: Vec<&str> = numbered.split_whitespace().collect();
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::signature;

    #[test]
    fn golden_signatures() {
        assert_eq!(
            signature("npm test", "FAIL src/b.js:3 expected 302, got 200"),
            "npm test | FAIL PATH expected N, got N"
        );
        assert_eq!(
            signature(
                "cargo test",
                "ok line\n\n  thread panicked at C:\\x\\y.rs:12:5\nlast"
            ),
            "cargo test | last"
        );
        assert_eq!(
            signature("t", "compiling\nError:   cannot  find  x42y7\nmore"),
            "t | Error: cannot find xNyN"
        );
        assert_eq!(signature("t", ""), "t | ");
        let long = "error ".repeat(40);
        assert_eq!(signature("t", &long).len(), "t | ".len() + 120);
    }
}
