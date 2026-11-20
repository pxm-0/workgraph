use super::*;
use std::cell::RefCell;

fn fx(name: &str, expected: &str) -> Fixture {
    Fixture {
        name: name.into(),
        dir: PathBuf::new(),
        repo: "o/r".into(),
        base_commit: "a".repeat(40),
        expected_status: expected.into(),
        image: String::new(),
        checks: BTreeMap::new(),
    }
}

fn ok(cost: u64) -> ArmResult {
    ArmResult {
        status: "done".into(),
        escalations: vec![],
        gates: vec!["ready_to_build".into()],
        schema_ok: true,
        passed: true,
        cost_micros: cost,
        incidents: 0,
        by_provider: BTreeMap::new(),
    }
}

type Answer = Box<dyn Fn(&str, Arm, usize) -> ArmResult>;

/// Answers by (fixture, arm); records every call.
struct Script {
    f: Answer,
    calls: RefCell<Vec<(String, Arm)>>,
}

impl Arms for Script {
    fn run(&self, fixture: &Fixture, arm: Arm) -> Result<ArmResult, String> {
        let n = self
            .calls
            .borrow()
            .iter()
            .filter(|(f, a)| f == &fixture.name && *a == arm)
            .count();
        self.calls.borrow_mut().push((fixture.name.clone(), arm));
        Ok((self.f)(&fixture.name, arm, n))
    }
}

fn script(f: impl Fn(&str, Arm, usize) -> ArmResult + 'static) -> Script {
    Script {
        f: Box::new(f),
        calls: RefCell::new(vec![]),
    }
}

fn ten() -> Vec<Fixture> {
    (0..10)
        .map(|i| {
            fx(
                &format!("f{i}"),
                if i < 3 { "needs_decision" } else { "done" },
            )
        })
        .collect()
}

#[test]
fn a_clean_candidate_passes_with_bounded_calls() {
    let s = script(|_, _, _| ok(100));
    let o = evaluate(&ten(), &s, 7).unwrap();
    assert!(o.passed, "{}", o.reason);
    // Stage 1: 10 fixtures x 2 arms; stage 2: 4 fixtures x (1 base + 1 candidate).
    assert_eq!(s.calls.borrow().len(), 20 + 8);
    assert_eq!(o.spend.as_array().unwrap().len(), 28);
}

#[test]
fn each_regression_fails_the_pr() {
    let cases: Vec<(Answer, &str)> = vec![
        (
            Box::new(|f, a, _| ArmResult {
                escalations: if a == Arm::Candidate && f == "f0" {
                    vec![]
                } else {
                    vec!["FM-1".into()]
                },
                ..ok(1)
            }),
            "escalations",
        ),
        (
            Box::new(|_, a, _| ArmResult {
                gates: if a == Arm::Candidate {
                    vec![]
                } else {
                    vec!["ready_to_build".into()]
                },
                ..ok(1)
            }),
            "gate sequence",
        ),
        (
            Box::new(|_, a, _| ArmResult {
                schema_ok: a == Arm::Base,
                ..ok(1)
            }),
            "schema-invalid",
        ),
        (
            Box::new(|f, a, _| ArmResult {
                passed: !(a == Arm::Candidate && f == "f4"),
                ..ok(1)
            }),
            "stage 1, fixture f4",
        ),
        (
            Box::new(|_, a, _| ArmResult {
                incidents: u32::from(a == Arm::Candidate),
                ..ok(1)
            }),
            "guard incident",
        ),
        (
            Box::new(|_, a, _| ok(if a == Arm::Candidate { 116 } else { 100 })),
            "over 1.15x",
        ),
    ];
    for (f, want) in cases {
        let s = Script {
            f,
            calls: RefCell::new(vec![]),
        };
        let o = evaluate(&ten(), &s, 7).unwrap();
        assert!(!o.passed && o.reason.contains(want), "{want}: {}", o.reason);
    }
    let at_limit = script(|_, a, _| ok(if a == Arm::Candidate { 115 } else { 100 }));
    assert!(evaluate(&ten(), &at_limit, 7).unwrap().passed);
}

#[test]
fn stage_two_stops_when_the_candidate_fails_twice_but_forgives_a_flake() {
    // Stage 1 compares single runs; in stage 2 the candidate's first try flakes everywhere.
    let flaky = script(|_, a, n| ArmResult {
        passed: !(a == Arm::Candidate && n == 1),
        ..ok(1)
    });
    assert!(evaluate(&ten(), &flaky, 3).unwrap().passed);
    let broken = script(|_, a, n| ArmResult {
        passed: !(a == Arm::Candidate && n >= 1),
        ..ok(1)
    });
    let o = evaluate(&ten(), &broken, 3).unwrap();
    assert!(o.reason.contains("fails twice"), "{}", o.reason);
    let stage2: Vec<_> = broken.calls.borrow()[20..].to_vec();
    assert_eq!(stage2.len(), 3, "base once, the candidate twice, then stop");
}

#[test]
fn the_order_is_a_fixed_permutation_and_the_store_needs_enough_fixtures() {
    let o = order(10, 42);
    assert_eq!(o, order(10, 42));
    let mut sorted = o.clone();
    sorted.sort();
    assert_eq!(sorted, (0..10).collect::<Vec<_>>());
    assert_ne!(order(10, 42), order(10, 43));
    assert!(enough(&ten(), 10, 3).is_ok());
    assert!(enough(&ten()[..9], 10, 3)
        .unwrap_err()
        .contains("9 fixtures"));
    assert!(enough(&ten()[3..], 7, 3)
        .unwrap_err()
        .contains("0 expecting"));
}

#[test]
fn arm_results_come_from_the_result_journal_and_ledger() {
    let out = json!({ "status": "needs_decision", "escalations": [{ "id": "FM-2" }, { "id": "CHECKS" }] });
    let journal = vec![
        json!({ "kind": "gate_evaluated", "gate": "ready_to_build" }),
        json!({ "kind": "llm_retry" }),
        json!({ "kind": "incident" }),
    ];
    let ledger = vec![
        json!({ "run": "r-1", "provider": "subscription", "cost_micros": 40 }),
        json!({ "run": "r-1", "provider": "anthropic-api", "cost_micros": 2 }),
        json!({ "run": "r-2", "provider": "subscription", "cost_micros": 999 }),
    ];
    let r = arm_result(&out, &journal, &ledger, "r-1", "needs_decision", true);
    assert_eq!(r.escalations, ["CHECKS", "FM-2"]);
    assert_eq!(r.gates, ["ready_to_build"]);
    assert!(!r.schema_ok && r.passed && r.incidents == 1 && r.cost_micros == 42);
    assert_eq!(r.by_provider["anthropic-api"], 2);
    assert!(!arm_result(&out, &journal, &ledger, "r-1", "done", true).passed);
    assert!(!arm_result(&out, &journal, &ledger, "r-1", "needs_decision", false).passed);
}

#[test]
fn fixtures_load_from_the_store_and_bad_ones_are_refused() {
    let store = std::env::temp_dir().join(format!("wg-fixtures-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store);
    let img = format!("docker.io/library/python@sha256:{}", "c".repeat(64));
    for (n, commit) in [("b-two", "b".repeat(40)), ("a-one", "a".repeat(40))] {
        std::fs::create_dir_all(store.join("fixtures").join(n)).unwrap();
        std::fs::write(
            store.join("fixtures").join(n).join("fixture.toml"),
            format!("repo = \"o/r\"\nbase_commit = \"{commit}\"\nexpected_status = \"done\"\nimage = \"{img}\"\nchecks = {{ test = \"python3 check.py\" }}\n"),
        )
        .unwrap();
    }
    std::fs::create_dir_all(store.join("fixtures/notes")).unwrap();
    let f = fixtures(&store).unwrap();
    assert_eq!(
        f.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
        ["a-one", "b-two"]
    );
    std::fs::write(store.join("fixtures/a-one/fixture.toml"), "repo = \"o/r\"\nbase_commit = \"x\"\nexpected_status = \"done\"\nimage = \"i\"\nchecks = {}\n").unwrap();
    assert!(fixtures(&store).unwrap_err().contains("commit id"));
    assert!(fixtures(&store.join("missing")).is_err());
}
