//! The chain invariants (design §6.3), one rejection each, and the shipped qa and release lines.

use super::*;
use crate::assets::Assets;
use wg_factory::api::{OnFail, Role, SchemaId};

const QA: &str = include_str!("../../../pipelines/qa.toml");
const RELEASE: &str = include_str!("../../../pipelines/release.toml");

fn view() -> RepoView {
    RepoView {
        qa_checks: vec!["e2e".into()],
        rehearsal: true,
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// The compiled assets with these pipelines and prompts on top.
fn with(files: &[(&str, &str)]) -> Assets {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("wg-chain-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (path, text) in files {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    Assets::compiled().overlay(&dir).unwrap()
}

#[test]
fn the_shipped_qa_chain_publishes_after_its_re_review_within_the_raised_cap() {
    let c = chain(&Assets::compiled(), &s(&["dev", "qa"]), &view(), 60).unwrap();
    let ids: Vec<&str> = c.stages.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["regression", "explore", "re-review", "publish"]);
    let labels: Vec<&str> = c.stages.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(labels, ["regression", "explore", "rereview", "publish"]);
    assert!(
        matches!(&c.stages[0].kind, StageKind::Checks { names, on_fail: OnFail::Fix, max_fix_rounds: 1 } if names == &["e2e"])
    );
    assert!(
        matches!(&c.stages[1].kind, StageKind::Agent { role: Role::QaExplorer, schema: SchemaId::Verdict, gate: Some(g), on_fail: OnFail::Fix, llm }
        if g.field == "passed" && g.equals && llm.tier == Tier::Sonnet && llm.max_turns == 60)
    );
    assert!(
        matches!(&c.stages[2].kind, StageKind::Review { llm, max_fix_rounds: 1 } if llm.tier == Tier::Opus)
    );
    let publish = c.publish().unwrap();
    assert!(publish.draft && publish.labels == ["wg:awaiting-approval"]);
    // dev 31 + regression 5 + explore (2 runs, 5 fixes) 7 + re-review (2 reviews, 5 fixes) 7.
    assert_eq!(worst_case(&c), 50);
    assert!(
        worst_case(&c) <= crate::config::MAX_CALLS_PER_TASK,
        "the compiled cap fits the chain"
    );
    assert!(chain(&Assets::compiled(), &s(&["dev", "qa"]), &view(), 49)
        .unwrap_err()
        .contains("worst case is 50 calls"));
    let dev = chain(&Assets::compiled(), &s(&["dev"]), &view(), 40).unwrap();
    assert!(dev.stages.is_empty() && dev.publish().is_none());
}

#[test]
fn the_shipped_release_line_prepares_and_ends_at_the_human_stage() {
    let c = chain(&Assets::compiled(), &s(&["release"]), &view(), 60).unwrap();
    assert!(c.dev.is_none() && c.publish().is_none());
    let kinds: Vec<&StageKind> = c.stages.iter().map(|s| &s.kind).collect();
    assert!(
        matches!(kinds[0], StageKind::Agent { role: Role::ReleaseWriter, schema: SchemaId::ReleaseNotes, llm, .. } if llm.tier == Tier::Haiku)
    );
    assert!(
        matches!(kinds[1], StageKind::Agent { schema: SchemaId::ReleasePlan, gate: Some(_), on_fail: OnFail::Stop, llm, .. } if llm.tier == Tier::Opus)
    );
    assert!(
        matches!(kinds[2], StageKind::Sandbox { steps_from, always: true } if steps_from == "release_rehearsal")
    );
    assert!(
        matches!(kinds[3], StageKind::HaruspexGate { gate, expect_only } if gate == "ready-to-release"
        && expect_only == &["Human approval for ready_to_release is pending."])
    );
    assert!(matches!(kinds[4], StageKind::Human { gate, .. } if gate == "ready-to-release"));
    assert_eq!(worst_case(&c), 2);
    let no_rehearsal = RepoView::default();
    assert!(
        chain(&Assets::compiled(), &s(&["release"]), &no_rehearsal, 60)
            .unwrap_err()
            .contains("release_rehearsal")
    );
}

#[test]
fn each_chain_invariant_rejects_one_bad_chain() {
    let dev_qa = |qa: String| with(&[("pipelines/qa.toml", &qa)]);
    let release = |r: String| with(&[("pipelines/release.toml", &r)]);
    let cases: Vec<(Assets, Vec<String>, &str)> = vec![
        // §6.3 rule 3: a QA fix must be re-reviewed before publish.
        (
            dev_qa(QA.replace("kind = \"review\"", "kind = \"human\"\ngate = \"ready-to-release\"").replace("model = \"opus\"\nprompt = \"review\"\nmax_fix_rounds = 1\nmax_turns = 60\ntimeout_secs = 1800\n", "")),
            s(&["dev", "qa"]),
            "must follow a review",
        ),
        (
            dev_qa(format!("{QA}\n[[stage]]\nid = \"again\"\nkind = \"publish\"\n")),
            s(&["dev", "qa"]),
            "at most one publish",
        ),
        (
            release(RELEASE.replace("[[stage]]\nid = \"owner\"", "[[stage]]\nid = \"publish\"\nkind = \"publish\"\n\n[[stage]]\nid = \"owner\"")),
            s(&["release"]),
            "needs the dev build",
        ),
        // §6.3 rule 4.
        (
            release(RELEASE.replace("kind = \"human\"", "kind = \"haruspex_gate\"").replace("ask = \"Review the release plan and record ready-to-release yourself; deploy by hand.\"", "")),
            s(&["release"]),
            "end with a human stage",
        ),
        (dev_qa(format!("{QA}\n[[stage]]\nid = \"ship\"\nkind = \"deploy\"\n")), s(&["dev", "qa"]), "unknown kind"),
        (dev_qa(format!("{QA}\n[[stage]]\nid = \"again\"\nkind = \"baseline\"\n")), s(&["dev", "qa"]), "only in the dev pipeline"),
        (dev_qa(QA.replace("id = \"regression\"", "id = \"fix\"")), s(&["dev", "qa"]), "reuse the dev labels"),
        (dev_qa(QA.replace("id = \"regression\"", "id = \"read\"")), s(&["dev", "qa"]), "appears twice"),
        (dev_qa(QA.replace("id = \"regression\"", "id = \"Re_gression\"")), s(&["dev", "qa"]), "lowercase"),
        (dev_qa(QA.replace("set = \"qa\"", "set = \"prod\"")), s(&["dev", "qa"]), "set must be"),
        (dev_qa(QA.replace("max_fix_rounds = 1\n\n[[stage]]\nid = \"explore\"", "max_fix_rounds = 4\n\n[[stage]]\nid = \"explore\"")), s(&["dev", "qa"]), "1-3"),
        (dev_qa(QA.replace("on_fail = \"fix\"              # blamed units", "on_fail = \"retry\"              # blamed units")), s(&["dev", "qa"]), "fix or stop"),
        (dev_qa(QA.replace("schema = \"verdict\"", "schema = \"release-plan\"")), s(&["dev", "qa"]), "role and schema"),
        (dev_qa(QA.replace("field = \"passed\"", "field = \"deployed\"")), s(&["dev", "qa"]), "gate must be"),
        (dev_qa(QA.replace("gate = { field = \"passed\", equals = true }", "gate = \"ready-to-release\"")), s(&["dev", "qa"]), "gate must be"),
        (dev_qa(QA.replace("prompt = \"qa-explore\"", "prompt = \"learn\"")), s(&["dev", "qa"]), "does not provide"),
        (dev_qa(QA.replace("prompt = \"qa-explore\"", "prompt = \"nope\"")), s(&["dev", "qa"]), "no prompt named"),
        (dev_qa(QA.replace("model = \"opus\"\nprompt = \"review\"", "model = \"sonnet\"\nprompt = \"review\"")), s(&["dev", "qa"]), "uses opus"),
        (dev_qa(QA.replace("labels = [\"wg:awaiting-approval\"]", "labels = [\"wg:verified\"]")), s(&["dev", "qa"]), "may carry only"),
        (dev_qa(QA.replace("labels = [\"wg:awaiting-approval\"]", "labels = [\"a b\"]")), s(&["dev", "qa"]), "label"),
        (dev_qa(QA.replace("draft = true", "draft = false")), s(&["dev", "qa"]), "always a draft"),
        (dev_qa(QA.replace("draft = true", "draft = true\nprompt = \"read\"")), s(&["dev", "qa"]), "does not apply"),
        (dev_qa(QA.replace("name = \"qa\"", "name = \"qa\"\nharuspex = false")), s(&["dev", "qa"]), "haruspex must be true"),
        // A release writer has no built units to fix.
        (release(RELEASE.replace("schema = \"release-plan\"\n", "schema = \"release-plan\"\non_fail = \"fix\"\n")), s(&["release"]), "needs built units"),
        (release(RELEASE.replace("steps_from = \"release_rehearsal\"", "steps_from = \"deploy\"")), s(&["release"]), "steps_from"),
        (release(RELEASE.replace("when = \"always\"", "when = \"sometimes\"")), s(&["release"]), "always or risky"),
        (release(RELEASE.replace("gate = \"ready-to-release\"\nexpect_only", "gate = \"verification\"\nexpect_only")), s(&["release"]), "gate must be one of"),
        (release(RELEASE.replace("gate = \"ready-to-release\"    # parks", "gate = \"deployment\"    # parks")), s(&["release"]), "gate must be one of"),
        (release(RELEASE.replace("ask = \"Review", "ask = \"\\nReview")), s(&["release"]), "one line"),
        (Assets::compiled(), s(&["qa", "dev"]), "dev must come first"),
        (Assets::compiled(), s(&["dev", "self"]), "Phase 5"),
        (Assets::compiled(), s(&["dev", "nope"]), "no pipeline named"),
    ];
    for (a, names, want) in cases {
        let err = chain(&a, &names, &view(), 60).unwrap_err();
        assert!(err.contains(want), "{want}: {err}");
    }
}
