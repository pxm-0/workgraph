//! `lint-editable` (design §12.2): defence in depth beside the no-std build, for the compile-time
//! routes the compiler allows in editable Rust: `#[path]`, `include!`, `include_str!`,
//! `include_bytes!`, `env!`, `option_env!`, linkage attributes, `extern`, `unsafe`, any `std` name
//! and any mention of `unsafe_code` (an `allow` of the forbidden lint). It runs on a token stream
//! with comments and literal contents removed and raw identifiers normalized, never on raw text,
//! so `extern /**/ crate std` and `r#std` are caught as what they are.

/// One token and the line it starts on.
#[derive(Clone, Debug, PartialEq)]
pub struct Tok {
    pub text: String,
    pub line: usize,
}

const MACROS: [&str; 8] = [
    "include",
    "include_str",
    "include_bytes",
    "env",
    "option_env",
    "asm",
    "global_asm",
    "concat_idents",
];
const ATTRS: [&str; 6] = [
    "path",
    "no_mangle",
    "export_name",
    "link_section",
    "link",
    "used",
];
const WORDS: [&str; 4] = ["std", "unsafe", "unsafe_code", "extern"];

/// Skips a block comment starting at `i` (after `/*`), nested; returns the index after it.
fn block_comment(b: &[char], mut i: usize, line: &mut usize) -> usize {
    let mut depth = 1;
    while i < b.len() && depth > 0 {
        match (b[i], b.get(i + 1)) {
            ('/', Some('*')) => {
                depth += 1;
                i += 2;
            }
            ('*', Some('/')) => {
                depth -= 1;
                i += 2;
            }
            (c, _) => {
                if c == '\n' {
                    *line += 1;
                }
                i += 1;
            }
        }
    }
    i
}

/// Skips a quoted literal whose opening quote is at `i`; escapes honoured unless `raw`.
fn quoted(b: &[char], mut i: usize, raw_hashes: Option<usize>, line: &mut usize) -> usize {
    i += 1;
    while i < b.len() {
        let c = b[i];
        if c == '\n' {
            *line += 1;
        }
        match raw_hashes {
            None if c == '\\' => i += 2,
            None if c == '"' => return i + 1,
            Some(n)
                if c == '"' && b[i + 1..].iter().take(n).filter(|&&h| h == '#').count() == n =>
            {
                return i + 1 + n
            }
            _ => i += 1,
        }
    }
    i
}

fn ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The token stream of `src`: identifiers (with `r#` stripped), `::`, single punctuation, and a
/// `""` for every string, byte-string or char literal. Comments vanish.
pub fn tokens(src: &str) -> Vec<Tok> {
    let b: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut line = 1;
    let mut i = 0;
    let push = |out: &mut Vec<Tok>, t: &str, line: usize| {
        out.push(Tok {
            text: t.to_string(),
            line,
        })
    };
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        match c {
            '\n' => {
                line += 1;
                i += 1;
            }
            c if c.is_whitespace() => i += 1,
            '/' if next == Some('/') => {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
            }
            '/' if next == Some('*') => i = block_comment(&b, i + 2, &mut line),
            '"' => {
                let at = line;
                i = quoted(&b, i, None, &mut line);
                push(&mut out, "\"\"", at);
            }
            'r' | 'b' | 'c' if matches!(next, Some('"') | Some('#') | Some('r')) => {
                // r"..", r#".."#, b"..", br"..", c"..", or a raw identifier r#name.
                let mut j = i + 1;
                if b[i] != 'r' && next == Some('r') {
                    j += 1;
                }
                let raw = b[i] == 'r' || (j > i + 1);
                let hashes = b[j..].iter().take_while(|&&h| h == '#').count();
                let at = line;
                if b.get(j + hashes) == Some(&'"') {
                    i = quoted(&b, j + hashes, raw.then_some(hashes), &mut line);
                    push(&mut out, "\"\"", at);
                } else if b[i] == 'r'
                    && hashes == 1
                    && b.get(j + 1).is_some_and(|&c| ident_start(c))
                {
                    let start = j + 1;
                    let mut k = start;
                    while k < b.len() && ident_char(b[k]) {
                        k += 1;
                    }
                    push(&mut out, &b[start..k].iter().collect::<String>(), at);
                    i = k;
                } else {
                    let mut k = i;
                    while k < b.len() && ident_char(b[k]) {
                        k += 1;
                    }
                    push(&mut out, &b[i..k].iter().collect::<String>(), at);
                    i = k;
                }
            }
            '\'' => {
                // A char literal ('x', '\n', '\u{..}') or a lifetime ('a).
                let close = if next == Some('\\') {
                    b.get(i + 3..)
                        .and_then(|r| r.iter().position(|&c| c == '\''))
                        .map(|p| i + 3 + p)
                } else {
                    (b.get(i + 2) == Some(&'\'')).then_some(i + 2)
                };
                match close {
                    Some(e) => {
                        push(&mut out, "\"\"", line);
                        i = e + 1;
                    }
                    None => {
                        push(&mut out, "'", line);
                        i += 1;
                    }
                }
            }
            ':' if next == Some(':') => {
                push(&mut out, "::", line);
                i += 2;
            }
            c if ident_start(c) => {
                let mut k = i;
                while k < b.len() && ident_char(b[k]) {
                    k += 1;
                }
                push(&mut out, &b[i..k].iter().collect::<String>(), line);
                i = k;
            }
            c => {
                push(&mut out, &c.to_string(), line);
                i += 1;
            }
        }
    }
    out
}

/// Every forbidden route in `src`, as `line N: why`.
pub fn findings(src: &str) -> Vec<String> {
    let t = tokens(src);
    let mut out = Vec::new();
    for (i, tok) in t.iter().enumerate() {
        let next = t.get(i + 1).map(|n| n.text.as_str());
        let w = tok.text.as_str();
        let why = if WORDS.contains(&w) {
            Some(format!("`{w}` is not allowed in editable Rust"))
        } else if MACROS.contains(&w) && next == Some("!") {
            Some(format!("`{w}!` reaches outside the crate at compile time"))
        } else if w == "["
            && i > 0
            && (t[i - 1].text == "#" || (t[i - 1].text == "!" && i > 1 && t[i - 2].text == "#"))
            && next.is_some_and(|n| ATTRS.contains(&n))
        {
            Some(format!("`#[{}]` is not allowed", next.unwrap_or("")))
        } else {
            None
        };
        if let Some(w) = why {
            out.push(format!("line {}: {w}", tok.line));
        }
    }
    out
}

/// Is `path` editable Rust the lint covers: every `.rs` under `wg-factory/` except `lib.rs`
/// (protected, and the one place `extern crate alloc` lives).
pub fn covers(path: &str) -> bool {
    path.starts_with("wg-factory/src/") && path.ends_with(".rs") && path != "wg-factory/src/lib.rs"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bad(src: &str) -> String {
        let f = findings(src);
        assert!(!f.is_empty(), "not caught: {src}");
        f.join("; ")
    }

    #[test]
    fn each_route_is_caught_however_it_is_spelled() {
        for (src, want) in [
            ("use std::fs;", "`std`"),
            ("fn f() { ::std::process::exit(1) }", "`std`"),
            ("use r#std::fs;", "`std`"),
            ("extern /**/ crate std;", "`extern`"),
            ("extern\ncrate alloc as a;", "`extern`"),
            ("extern \"C\" { fn f(); }", "`extern`"),
            (
                "const X: &str = include_str!(\"../../wg/src/gh.rs\");",
                "include_str!",
            ),
            (
                "const X: &[u8] = core::include_bytes!(\"k\");",
                "include_bytes!",
            ),
            ("include!(\"x.rs\");", "include!"),
            ("const H: &str = env!(\"HOME\");", "env!"),
            (
                "const H: Option<&str> = option_env ! (\"HOME\");",
                "option_env!",
            ),
            ("#[path = \"../../wg/src/gh.rs\"] mod gh;", "#[path]"),
            ("#[no_mangle] pub fn f() {}", "#[no_mangle]"),
            ("#[export_name = \"main\"] pub fn f() {}", "#[export_name]"),
            (
                "#[link_section = \".x\"] static S: u8 = 0;",
                "#[link_section]",
            ),
            ("#![allow(unsafe_code)]", "`unsafe_code`"),
            (
                "#[cfg_attr(all(), allow(unsafe_code))] fn f() {}",
                "`unsafe_code`",
            ),
            ("fn f() { unsafe { } }", "`unsafe`"),
        ] {
            assert!(bad(src).contains(want), "{src}: {}", bad(src));
        }
    }

    #[test]
    fn comments_strings_and_chars_are_not_code() {
        for ok in [
            "// use std::fs;\nfn f() {}",
            "/* include_str!(\"x\") /* nested std */ */ fn f() {}",
            "const S: &str = \"std::fs and env!(X) and #[path]\";",
            "const R: &str = r#\"unsafe \" std\"#;",
            "const B: &[u8] = b\"extern crate std\";",
            "const C: char = '\"'; const D: char = '\\''; fn f<'a>(x: &'a str) -> &'a str { x }",
            "use alloc::vec::Vec; use core::fmt;",
            "#[derive(Clone)] struct S { environment: u8, standard: u8 }",
            "let path = 1; let env = 2;",
        ] {
            assert_eq!(findings(ok), Vec::<String>::new(), "{ok}");
        }
        assert_eq!(
            findings("fn f() {}\n\n/* x */ use std::fs;")[0],
            "line 3: `std` is not allowed in editable Rust"
        );
    }

    #[test]
    fn the_shipped_editable_rust_is_clean() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../wg-factory/src");
        let mut pending = vec![root.clone()];
        let mut seen = 0;
        while let Some(d) = pending.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    pending.push(p);
                    continue;
                }
                let rel = format!(
                    "wg-factory/src/{}",
                    p.strip_prefix(&root).unwrap().to_string_lossy()
                );
                if covers(&rel) {
                    seen += 1;
                    let f = findings(&std::fs::read_to_string(&p).unwrap());
                    assert!(f.is_empty(), "{rel}: {f:?}");
                }
            }
        }
        assert!(seen > 10);
        assert!(!covers("wg-factory/src/lib.rs") && !covers("wg/src/gh.rs"));
    }
}
