//! The kernel's deterministic risk classifier (design §8.2): path rules on planned and changed
//! files, and content rules on the lines a unit added. The factory cannot lower it, and
//! over-approximating is the safe direction. A reason reads `<category>: <why> (<where>)`; the
//! category names the PR label `risky:<category>`.

/// Files whose name alone pins a dependency set.
const LOCKFILES: [&str; 11] = [
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "Cargo.lock",
    "Gemfile.lock",
    "poetry.lock",
    "Pipfile.lock",
    "uv.lock",
    "composer.lock",
    "go.sum",
];

fn reason(category: &str, why: &str, at: &str) -> String {
    format!("{category}: {why} ({at})")
}

/// The category of a reason made here; `flagged` for a reason the Reading wrote itself.
pub fn category(reason: &str) -> &str {
    match reason.split_once(": ") {
        Some((c, _)) if !c.is_empty() && c.chars().all(|ch| ch.is_ascii_lowercase()) => c,
        _ => "flagged",
    }
}

/// One repo-relative path against the path rules.
pub fn path(p: &str) -> Option<String> {
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    let (name, dirs) = parts.split_last()?;
    let lower = p.to_lowercase();
    let first = parts.first().copied().unwrap_or("");
    let hit = |c: &str, why: &str| Some(reason(c, why, p));
    if dirs.contains(&"migrations") || lower.contains("migration") || name.ends_with(".sql") {
        return hit("migration", "a database migration");
    }
    if first == ".github" {
        return hit("ci", "CI configuration");
    }
    if name.starts_with("Dockerfile")
        || (name.starts_with("compose") && (name.ends_with(".yml") || name.ends_with(".yaml")))
        || first == ".devcontainer"
    {
        return hit("container", "a container build or dev stack");
    }
    if first == "deploy" || lower.contains("deploy") {
        return hit("deploy", "a deploy");
    }
    if first == "infra" || name.ends_with(".tf") || lower.contains("terraform") {
        return hit("infra", "infrastructure");
    }
    if lower.contains("auth") {
        return hit("auth", "authentication or authorization");
    }
    if name.starts_with(".env") || lower.contains("secret") {
        return hit("secrets", "secrets");
    }
    if lower.contains("payment") {
        return hit("payments", "payments");
    }
    if LOCKFILES.contains(name) {
        return hit("deps", "a dependency lockfile");
    }
    None
}

/// The first risky path among `files`.
pub fn paths(files: &[String]) -> Option<String> {
    files.iter().find_map(|f| path(f))
}

/// Content rules on one added line (design §8.2's diff rules, without a regex engine).
fn line(l: &str) -> Option<(&'static str, &'static str)> {
    let lower = l.to_lowercase();
    if ["DROP ", "TRUNCATE", "DELETE FROM"]
        .iter()
        .any(|k| l.contains(k))
        || ["drop table", "truncate table", "delete from"]
            .iter()
            .any(|k| lower.contains(k))
    {
        return Some(("migration", "destructive SQL"));
    }
    if lower.contains("rm -rf")
        || ["unlink(", "rmtree", "fs.rm"]
            .iter()
            .any(|k| lower.contains(k))
    {
        return Some(("deletes", "deletes files"));
    }
    if ["stripe", "paypal", "braintree"]
        .iter()
        .any(|k| lower.contains(k))
    {
        return Some(("payments", "a payment provider"));
    }
    if ["jwt", "bcrypt", "session", "oauth"]
        .iter()
        .any(|k| lower.contains(k))
    {
        return Some(("auth", "authentication or sessions"));
    }
    if let Some((_, after)) = l.split_once("process.env") {
        if ["KEY", "TOKEN", "SECRET"].iter().any(|k| after.contains(k)) {
            return Some(("secrets", "reads a secret from the environment"));
        }
    }
    None
}

/// The first risky added line of a unified diff (lines starting `+`, not the `+++` header).
pub fn diff(text: &str, at: &str) -> Option<String> {
    text.lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .find_map(|l| line(&l[1..]))
        .map(|(c, why)| reason(c, why, at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_rules_one_per_category() {
        let cat = |p: &str| path(p).map(|r| category(&r).to_string());
        let table = [
            ("db/migrations/001_init.js", Some("migration")),
            ("schema.sql", Some("migration")),
            (".github/workflows/ci.yml", Some("ci")),
            ("Dockerfile.dev", Some("container")),
            ("compose.dev.yml", Some("container")),
            (".devcontainer/devcontainer.json", Some("container")),
            ("deploy/run.sh", Some("deploy")),
            ("infra/main.tf", Some("infra")),
            ("modules/vpc.tf", Some("infra")),
            ("src/auth/login.ts", Some("auth")),
            ("src/oauth_client.py", Some("auth")),
            ("config/secrets.yml", Some("secrets")),
            (".env.production", Some("secrets")),
            ("src/payments/charge.ts", Some("payments")),
            ("package-lock.json", Some("deps")),
            ("sub/Cargo.lock", Some("deps")),
            ("src/calc.py", None),
            ("README.md", None),
        ];
        for (p, want) in table {
            assert_eq!(cat(p).as_deref(), want, "{p}");
        }
        assert!(path("db/migrations/1.sql")
            .unwrap()
            .contains("(db/migrations/1.sql)"));
        assert_eq!(
            paths(&["a.py".into(), ".env".into()]).as_deref(),
            Some("secrets: secrets (.env)")
        );
        assert_eq!(category("the Reading said so"), "flagged");
    }

    #[test]
    fn diff_rules_look_only_at_added_lines() {
        let d = |body: &str| diff(body, "src/x.js").map(|r| category(&r).to_string());
        let table = [
            ("+db.exec('DROP TABLE users')", Some("migration")),
            ("+  await knex.raw('delete from t')", Some("migration")),
            ("+execSync('rm -rf build')", Some("deletes")),
            ("+shutil.rmtree(path)", Some("deletes")),
            ("+await fs.rm(dir, { recursive: true })", Some("deletes")),
            ("+const s = require('stripe')", Some("payments")),
            ("+import jwt from 'jsonwebtoken'", Some("auth")),
            ("+req.session.user = u", Some("auth")),
            ("+const k = process.env.API_KEY", Some("secrets")),
            ("+const p = process.env.PORT", None),
            ("-db.exec('DROP TABLE users')", None),
            ("+++ b/src/session.js", None),
            ("+return a + b", None),
            (" context line with rm -rf", None),
        ];
        for (text, want) in table {
            assert_eq!(d(text).as_deref(), want, "{text}");
        }
        assert!(diff("+DROP TABLE x", "m.sql").unwrap().ends_with("(m.sql)"));
    }
}
