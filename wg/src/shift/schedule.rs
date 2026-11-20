//! `wg shift schedule-print` (design §7.2, §7.1): prints the launchd job, the two commands that
//! load and unload it, the GitHub labels a shift uses, and each repo's factory compose override.
//! It installs nothing: launchd, labels and override files are the owner's to create.

use super::config::RepoCfg;
use crate::config::Config;
use crate::state_dirs::Dirs;
use serde_json::{json, Value};
use std::path::Path;

pub const JOB: &str = "dev.pxm0.workgraph.shift";
pub const LABELS: [&str; 10] = [
    "wg:queued",
    "wg:p1",
    "wg:in-shift",
    "wg:pr-open",
    "wg:needs-decision",
    "wg:stuck",
    "wg:incident",
    "wg:verified",
    "wg:awaiting-approval",
    "wg:release-ready",
];

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn plist(dirs: &Dirs, start: u32) -> String {
    let home = dirs.home.to_string_lossy();
    let wg = format!("{home}/.workgraph/bin/current/wg");
    let log = format!("{home}/.workgraph/shifts/launchd.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{JOB}</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/bin/caffeinate</string><string>-i</string>
    <string>{}</string><string>shift</string><string>run</string>
  </array>
  <key>StartCalendarInterval</key>
  <dict><key>Hour</key><integer>{}</integer><key>Minute</key><integer>{}</integer></dict>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string></dict>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        xml(&wg),
        start / 60,
        start % 60,
        xml(&log),
        xml(&log)
    )
}

/// Top-level service names of a compose file: two-space keys under `services:`.
/// shortcut: a line scan, not a YAML parser; enough for the dev-env protocol's compose files,
/// upgrade if a repo writes services in flow style.
pub fn services(compose: &str) -> Vec<String> {
    let mut inside = false;
    let mut out = Vec::new();
    for line in compose.lines() {
        if !line.starts_with(' ') && !line.trim().is_empty() && !line.starts_with('#') {
            inside = line.trim_end() == "services:";
            continue;
        }
        let Some(rest) = line.strip_prefix("  ") else {
            continue;
        };
        if inside && !rest.starts_with(' ') && !rest.starts_with('#') {
            if let Some(name) = rest.trim_end().strip_suffix(':') {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// The override for factory runs: no published ports on any service (dev-env rules 2 and 4).
pub fn override_file(services: &[String]) -> String {
    let mut s = String::from("# Factory runs of this repo: no published ports, so they never collide with your stack.\nservices:\n");
    for name in services {
        s.push_str(&format!("  {name}:\n    ports: !reset []\n"));
    }
    s
}

fn overrides(repo: &RepoCfg) -> Option<Value> {
    let compose = repo.factory_compose.as_deref()?;
    let path = compose
        .split_whitespace()
        .find(|t| t.starts_with('/') && t.ends_with(".yml"))?;
    let text = crate::git::git(
        Path::new(&repo.path),
        &[
            "show",
            &format!("origin/{}:compose.dev.yml", repo.default_branch),
        ],
        &[],
    )
    .unwrap_or_default();
    Some(json!({ "repo": repo.name, "path": path, "content": override_file(&services(&text)) }))
}

pub fn print(dirs: &Dirs, cfg: &Config) -> Value {
    let plist_path = dirs
        .home
        .join("Library/LaunchAgents")
        .join(format!("{JOB}.plist"));
    let p = plist_path.to_string_lossy();
    let s = &cfg.shift;
    json!({
        "plan": format!(
            "A nightly shift at {:02}:{:02}, taking no new task after {:02}:{:02}. Write the plist, then load it yourself; nothing here is installed.",
            s.start / 60, s.start % 60, s.until / 60, s.until % 60
        ),
        "plistPath": p,
        "plist": plist(dirs, s.start),
        "load": format!("launchctl bootstrap gui/$(id -u) {p}"),
        "unload": format!("launchctl bootout gui/$(id -u) {p}"),
        "labels": cfg.repos.iter().flat_map(|r| LABELS.iter().map(move |l| format!("gh label create {l} -R {} --force", r.name))).collect::<Vec<_>>(),
        "composeOverrides": cfg.repos.iter().filter_map(overrides).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_services_and_override() {
        let dirs = Dirs {
            home: "/Users/o".into(),
        };
        let p = plist(&dirs, 90);
        assert!(p.contains("<integer>1</integer><key>Minute</key><integer>30</integer>"));
        assert!(p.contains("<string>/usr/bin/caffeinate</string><string>-i</string>"));
        assert!(p.contains(
            "/Users/o/.workgraph/bin/current/wg</string><string>shift</string><string>run"
        ));
        let compose = "name: kadath\nservices:\n  workspace:\n    image: x\n    ports:\n      - \"3000:3000\"\n  # db:\n  db:\n    image: y\nvolumes:\n  data:\n";
        assert_eq!(services(compose), ["workspace", "db"]);
        let o = override_file(&services(compose));
        assert!(o.contains("  workspace:\n    ports: !reset []\n  db:\n    ports: !reset []\n"));
        let cfg = crate::config::parse(
            "[[repo]]\nname = \"o/r\"\npath = \"/nonexistent\"\nfactory_compose = \"docker compose -p r-wg -f compose.dev.yml -f /Users/o/.workgraph/compose/r.factory.yml\"\n",
        )
        .unwrap();
        let out = print(&dirs, &cfg);
        assert_eq!(out["labels"].as_array().unwrap().len(), LABELS.len());
        assert_eq!(
            out["composeOverrides"][0]["path"],
            "/Users/o/.workgraph/compose/r.factory.yml"
        );
        assert!(out["load"]
            .as_str()
            .unwrap()
            .starts_with("launchctl bootstrap gui/"));
    }
}
