use super::*;
use serde_json::json;
use std::fs;

struct Fixture {
    root: PathBuf,
    m: Manifest,
}

fn fixture(name: &str) -> Fixture {
    let base = std::env::temp_dir().join(format!("wg-guard-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    for d in [
        "wt/src",
        "wt/.haruspex/tasks",
        "tmp",
        "home/.workgraph",
        "home/.ssh",
        "outside",
    ] {
        fs::create_dir_all(base.join(d)).unwrap();
    }
    fs::write(base.join("wt/src/a.js"), "a").unwrap();
    fs::write(base.join("outside/secret.txt"), "s").unwrap();
    std::os::unix::fs::symlink(base.join("outside"), base.join("wt/link")).unwrap();
    let root = base.canonicalize().unwrap();
    let p = |s: &str| root.join(s).to_string_lossy().into_owned();
    let m = Manifest {
        label: "work:a".into(),
        log: p("guard.jsonl"),
        worktree: p("wt"),
        read_roots: vec![p("wt"), p("tmp")],
        write_roots: vec![
            p("wt/src/a.js"),
            p("wt/src/new.js"),
            format!("{}/", p("tmp")),
        ],
        secret_roots: vec![p("home/.workgraph"), p("home/.ssh")],
        tools: ["Read", "Glob", "Grep", "Edit", "Write", "Bash", "Skill"]
            .map(String::from)
            .to_vec(),
        skills: vec!["mattpocock-skills:tdd".into()],
        bash: BashRules {
            prefixes: vec!["npm test".into()],
            ..BashRules::default()
        },
        sandbox: None,
    };
    Fixture { root, m }
}

fn hook(tool: &str, input: Value) -> Value {
    json!({ "tool_name": tool, "tool_input": input, "cwd": "/" })
}

fn allow(v: &Verdict) -> bool {
    matches!(v, Verdict::Allow { .. })
}

#[test]
fn reads_in_the_worktree_and_run_tmp_are_allowed_elsewhere_denied() {
    let f = fixture("reads");
    let r = |p: String| decide(&f.m, &hook("Read", json!({ "file_path": p })));
    assert!(allow(&r(f.root.join("wt/src/a.js").display().to_string())));
    assert!(allow(&r("src/a.js".into())), "relative to the worktree");
    assert!(allow(&r(f.root.join("tmp/x.txt").display().to_string())));
    assert!(matches!(r("/etc/passwd".into()), Verdict::Deny(_)));
    assert!(matches!(
        r("src/../../outside/secret.txt".into()),
        Verdict::Deny(_)
    ));
    assert!(
        matches!(r("link/secret.txt".into()), Verdict::Deny(_)),
        "symlink escape"
    );
    assert!(matches!(
        r(f.root.join("home/.workgraph/key").display().to_string()),
        Verdict::Incident(_)
    ));
    assert!(matches!(
        r(f.root.join("home/.ssh/id_ed25519").display().to_string()),
        Verdict::Incident(_)
    ));
    let glob = decide(&f.m, &hook("Glob", json!({ "pattern": "**/*.js" })));
    assert!(allow(&glob), "Glob without a path searches the worktree");
    let grep = decide(&f.m, &hook("Grep", json!({ "pattern": "x", "path": "/" })));
    assert!(matches!(grep, Verdict::Deny(_)));
}

#[test]
fn writes_only_to_the_unit_files_and_protected_paths_are_incidents() {
    let f = fixture("writes");
    let w =
        |tool: &str, p: &str| decide(&f.m, &hook(tool, json!({ "file_path": p, "content": "x" })));
    assert_eq!(w("Edit", "src/a.js"), Verdict::Allow { effect: true });
    assert!(
        allow(&w("Write", "src/new.js")),
        "a planned file that does not exist yet"
    );
    assert!(allow(&w(
        "Write",
        &f.root.join("tmp/scratch/t.py").display().to_string()
    )));
    assert!(matches!(w("Edit", "src/b.js"), Verdict::Deny(_)));
    assert!(matches!(w("Write", "link/secret.txt"), Verdict::Deny(_)));
    assert!(matches!(
        w("Write", "src/../../outside/x"),
        Verdict::Deny(_)
    ));
    for p in [
        ".haruspex/state.json",
        ".haruspex/evidence/checks/x.json",
        ".haruspex/bin/haruspex.py",
        ".haruspex/templates/task.json",
        ".haruspex/managed/gates.json",
        ".git/config",
        ".git/hooks/pre-commit",
    ] {
        assert!(matches!(w("Write", p), Verdict::Incident(_)), "{p}");
        assert!(matches!(w("Edit", p), Verdict::Incident(_)), "{p}");
    }
    assert!(matches!(
        w(
            "Write",
            &f.root
                .join("home/.workgraph/factory.toml")
                .display()
                .to_string()
        ),
        Verdict::Incident(_)
    ));
    let no_path = decide(&f.m, &hook("Write", json!({ "content": "x" })));
    assert!(matches!(no_path, Verdict::Deny(_)));
    let nb = decide(
        &f.m,
        &hook("NotebookEdit", json!({ "notebook_path": "src/a.js" })),
    );
    assert!(
        matches!(nb, Verdict::Deny(_)),
        "a tool the role does not list"
    );
}

#[test]
fn tools_skills_and_bash() {
    let f = fixture("tools");
    let d = |tool: &str, input: Value| decide(&f.m, &hook(tool, input));
    assert!(allow(&d("StructuredOutput", json!({}))));
    assert!(allow(&d("TodoWrite", json!({}))));
    for t in ["WebFetch", "WebSearch", "Task", "Agent", "ToolSearch", ""] {
        assert!(matches!(d(t, json!({})), Verdict::Deny(_)), "{t}");
    }
    assert!(allow(&d(
        "Skill",
        json!({ "skill": "mattpocock-skills:tdd" })
    )));
    assert!(matches!(
        d("Skill", json!({ "skill": "deploy-oreochi" })),
        Verdict::Deny(_)
    ));
    assert!(matches!(d("Skill", json!({})), Verdict::Deny(_)));
    assert_eq!(
        d("Bash", json!({ "command": "npm test -- x" })),
        Verdict::Allow { effect: true }
    );
    assert!(matches!(
        d("Bash", json!({ "command": "npm test; rm -rf ~" })),
        Verdict::Deny(_)
    ));
    assert!(matches!(d("Bash", json!({})), Verdict::Deny(_)));
    assert!(matches!(
        d("Bash", json!({ "command": "wg finalize --run r" })),
        Verdict::Incident(_)
    ));
}

#[test]
fn canonical_resolves_missing_tails_and_dotdot() {
    let f = fixture("canon");
    let wt = f.root.join("wt");
    assert_eq!(canonical("src/new/x.js", &wt), wt.join("src/new/x.js"));
    assert_eq!(canonical("src/../src/a.js", &wt), wt.join("src/a.js"));
    assert_eq!(canonical("missing/../src/a.js", &wt), wt.join("src/a.js"));
    assert_eq!(
        canonical("link/secret.txt", &wt),
        f.root.join("outside/secret.txt")
    );
}
