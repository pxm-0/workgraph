//! Gate step 1 (design §12.3, §15 "a table test per rule"): one row per rule.

use super::*;

const OID: &str = "0123456789abcdef0123456789abcdef01234567";

fn pr() -> SelfPr {
    SelfPr {
        number: 7,
        state: "OPEN".into(),
        head: OID.into(),
        base_ref: "main".into(),
        labels: vec!["wg:self".into()],
        draft: false,
        mergeable: "MERGEABLE".into(),
        commits: vec![(OID.into(), "feat: x\n\nWorkgraph-Run: r-00aa11".into())],
    }
}

#[test]
fn step1_has_one_row_per_rule() {
    let dir = std::env::temp_dir().join(format!("wg-step1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ledger = Ledger::at(&dir.join("self-ledger.jsonl"));
    ledger
        .append(
            "authored",
            json!({ "pr": 7, "head": OID, "run": "r-00aa11" }),
        )
        .unwrap();
    let with = |f: &dyn Fn(&mut SelfPr)| {
        let mut p = pr();
        f(&mut p);
        p
    };
    let rows: Vec<(SelfPr, usize, &str, &str)> = vec![
        (with(&|p| p.labels.clear()), 1, "refused", "not a self PR"),
        (
            with(&|p| p.base_ref = "dev".into()),
            1,
            "refused",
            "not a self PR",
        ),
        (
            with(&|p| p.draft = true),
            1,
            "refused",
            "not open and ready",
        ),
        (
            with(&|p| p.state = "CLOSED".into()),
            1,
            "refused",
            "not open and ready",
        ),
        (with(&|p| p.commits.clear()), 1, "refused", "no commits"),
        (
            with(&|p| p.commits[0].1 = "feat: x".into()),
            1,
            "held",
            "not factory-produced",
        ),
        (
            with(&|p| p.commits[0].1 = "feat: x\n\nWorkgraph-Run: r-00bb22".into()),
            1,
            "held",
            "not factory-produced",
        ),
        (
            with(&|p| p.mergeable = "CONFLICTING".into()),
            1,
            "held",
            "not mergeable (CONFLICTING)",
        ),
        (
            with(&|p| p.mergeable = "UNKNOWN".into()),
            1,
            "pending",
            "not computed mergeability",
        ),
        (
            with(&|p| p.mergeable = String::new()),
            1,
            "pending",
            "not computed mergeability",
        ),
        (pr(), 4, "held", "over the cap of 3"),
    ];
    for (p, open, name, why) in rows {
        let o = step1(&p, &ledger, open).unwrap().expect(why);
        assert_eq!(o.name(), name, "{why}: {o:?}");
        assert!(o.reason().contains(why), "{why}: {o:?}");
    }
    assert_eq!(step1(&pr(), &ledger, 3).unwrap(), None, "a clean PR passes");
    let _ = std::fs::remove_dir_all(&dir);
}
