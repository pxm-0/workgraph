//! Flags for the new subcommands: `--name value` pairs and bare boolean switches, accepting
//! hyphen-leading values, with every error ending up in the one JSON line.

#[derive(Debug)]
pub struct Args {
    pairs: Vec<(String, String)>,
    switches: Vec<String>,
}

/// Parses `args`; names in `switches` take no value.
pub fn parse(args: &[String], switches: &[&str]) -> Result<Args, String> {
    let mut pairs = Vec::new();
    let mut on = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let name = a
            .strip_prefix("--")
            .ok_or_else(|| format!("unexpected argument: {a}"))?;
        if switches.contains(&name) {
            on.push(name.to_string());
            continue;
        }
        let v = it.next().ok_or_else(|| format!("--{name} needs a value"))?;
        pairs.push((name.to_string(), v.clone()));
    }
    Ok(Args {
        pairs,
        switches: on,
    })
}

impl Args {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn need(&self, name: &str) -> Result<&str, String> {
        self.get(name)
            .ok_or_else(|| format!("--{name} is required"))
    }

    pub fn on(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }

    /// Fails on any flag not in `known`.
    pub fn only(&self, known: &[&str]) -> Result<(), String> {
        let bad = self
            .pairs
            .iter()
            .map(|(k, _)| k)
            .chain(self.switches.iter())
            .find(|k| !known.contains(&k.as_str()));
        match bad {
            Some(k) => Err(format!("unknown flag --{k}")),
            None => Ok(()),
        }
    }
}

/// Owner-only commands refuse inside an agent's environment. A tripwire, not a boundary: agents
/// cannot reach these commands through the guard either (design §9.4).
pub fn not_an_agent(what: &str) -> Result<(), String> {
    if std::env::var_os("WG_AGENT").is_some() {
        return Err(format!(
            "{what} refuses to run inside an agent (WG_AGENT is set)"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pairs_switches_and_errors() {
        let p = parse(
            &a(&[
                "--task-file",
                "-x",
                "--experimental",
                "--run",
                "r1",
                "--run",
                "r2",
            ]),
            &["experimental"],
        )
        .unwrap();
        assert_eq!(p.get("task-file"), Some("-x"));
        assert_eq!(p.get("run"), Some("r2"));
        assert!(p.on("experimental") && !p.on("live"));
        assert!(p.only(&["task-file", "experimental", "run"]).is_ok());
        assert!(p.only(&["task-file"]).unwrap_err().contains("unknown flag"));
        assert!(p.need("nope").unwrap_err().contains("--nope is required"));
        assert!(parse(&a(&["x"]), &[]).unwrap_err().contains("unexpected"));
        assert!(parse(&a(&["--by"]), &[])
            .unwrap_err()
            .contains("needs a value"));
    }
}
