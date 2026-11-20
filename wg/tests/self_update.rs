//! Self-update at the start of a shift (design §7.3 step 2, §12.5), with the installed binary a
//! copy of this build: origin/main moved to a SHA whose CI and release build passed, so the shift
//! installs it and re-execs once; and when `current` already is main, a shift never re-execs, even
//! when the ledger's last install names another SHA (a drill rolled back).

mod self_support;

use self_support::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A release artifact whose binary is this build of wg.
fn artifact(base: &Path, sha: &str) -> PathBuf {
    let dir = base.join(format!("artifact-{}", &sha[..12]));
    fs::create_dir_all(&dir).unwrap();
    fs::copy(WG, dir.join("wg")).unwrap();
    let hash = wg::haruspex::cli::sha256_file(&dir.join("wg")).unwrap();
    fs::write(dir.join("wg.sha256"), format!("{hash}  wg\n")).unwrap();
    fs::write(dir.join("head_sha"), format!("{sha}\n")).unwrap();
    dir
}

fn shifts(f: &Fx) -> Vec<Vec<Value>> {
    let root = f.home.join(".workgraph/shifts");
    let mut ids: Vec<PathBuf> = fs::read_dir(&root)
        .map(|d| d.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    ids.sort();
    ids.iter()
        .map(|d| {
            fs::read_to_string(d.join("journal.jsonl"))
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        })
        .collect()
}

fn installed_shift(f: &Fx) -> Value {
    let out = Command::new(f.home.join(".workgraph/bin/current/wg"))
        .args(["shift", "run"])
        .env("HOME", &f.home)
        .env("FAKE_GH_STATE", &f.state)
        .env_remove("WG_AGENT")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(text.trim()).unwrap()
}

#[test]
fn a_shift_installs_moved_main_once_and_never_loops_on_an_already_current_install() {
    let f = fx("update", "");
    // The shift window is open from now for 23 h 59 min.
    let now = sh(&f.base, "date +%H:%M");
    let (h, m): (u32, u32) = (now[..2].parse().unwrap(), now[3..].parse().unwrap());
    let until = (h * 60 + m + 1439) % 1440;
    let toml = f.home.join(".workgraph/factory.toml");
    let text = fs::read_to_string(&toml).unwrap();
    fs::write(
        &toml,
        format!(
            "{text}\n[shift]\nstart = \"{now}\"\nuntil = \"{:02}:{:02}\"\n",
            until / 60,
            until % 60
        ),
    )
    .unwrap();
    let good = sh(&f.repo, "git rev-parse HEAD");
    let work = f.base.join("merger");
    sh(
        &f.base,
        &format!("git clone -q {} {}", f.origin.display(), work.display()),
    );
    sh(&work, "git config user.name t && git config user.email t@example.invalid && echo n >> docs/a.md && git commit -qam 'docs: n (#201)' && git push -q origin main");
    let next = sh(&work, "git rev-parse HEAD");
    let target = format!("wg-{}", wg::selfimprove::install::target());
    let (ag, an) = (artifact(&f.base, &good), artifact(&f.base, &next));
    f.set_gh(|s| {
        s["runs"] = json!([
            { "databaseId": 1, "headSha": good, "conclusion": "success", "event": "push", "artifacts": { &target: ag } },
            { "databaseId": 2, "headSha": next, "conclusion": "success", "event": "push", "artifacts": { &target: an } },
        ])
    });
    assert_eq!(f.wg(&["self", "install", "--sha", &good]).0, 0);

    // main moved: the shift installs it and re-execs the new binary, which runs the shift.
    installed_shift(&f);
    let bin = f.home.join(".workgraph/bin");
    assert_eq!(
        fs::read_link(bin.join("current")).unwrap(),
        PathBuf::from(format!("wg-{}", &next[..12]))
    );
    let s = shifts(&f);
    assert_eq!(
        s.len(),
        2,
        "the old binary's shift, then the new one's: {s:?}"
    );
    let update = s[0].iter().find(|e| e["kind"] == "self_update").unwrap();
    assert_eq!(update["installed"]["installed"], next.as_str(), "{update}");
    assert!(s[0].iter().any(|e| e["kind"] == "shift_finished"));
    assert!(
        !s[1].iter().any(|e| e["kind"] == "self_update"),
        "the new binary is main: {:?}",
        s[1]
    );
    assert!(
        s[1].iter().any(|e| e["kind"] == "shift_ended_early"),
        "it stops at preflight (no Haruspex pinned): {:?}",
        s[1]
    );

    // A drill: an older SHA installed, then rolled back. current is main again, but the ledger's
    // last install is the older SHA; the shift must not install-and-re-exec in a loop.
    assert_eq!(f.wg(&["self", "install", "--sha", &good]).0, 0);
    let (code, v) = f.wg(&["self", "rollback", "--reason", "drill"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["current"], format!("wg-{}", &next[..12]));
    installed_shift(&f);
    let s = shifts(&f);
    assert_eq!(s.len(), 3, "one shift, no re-exec: {s:?}");
    assert!(
        !s[2].iter().any(|e| e["kind"] == "self_update"),
        "{:?}",
        s[2]
    );
    let installs = f.ledger().of_kind("installed").unwrap();
    assert_eq!(installs.len(), 3, "good, next, good: never next again");
}
