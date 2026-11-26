//! Runtime verification prototype (design §9.6): the kernel runs a repo's harness against the
//! fixture CLI, then validates the evidence it left. A broken app, a harness that skips a mapped
//! feature, and a repo with no harness must each fail or be reported, never pass.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use wg::exec::run::run_shell_in;
use wg::runtime_verify::{harness, validate, Harness, HARNESS_DIR, UNAVAILABLE_EXIT};

fn fixture_copy(name: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/verify-cli");
    let dst = std::env::temp_dir().join(format!("wg-rv-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dst);
    let ok = Command::new("cp")
        .arg("-R")
        .arg(&src)
        .arg(&dst)
        .status()
        .unwrap()
        .success();
    assert!(ok, "copy fixture");
    dst
}

/// What the kernel would do: a fresh evidence dir, the harness with exactly PATH and
/// WG_EVIDENCE_DIR, then validation against the map unless the doctor said the environment is
/// unusable.
fn verify(repo: &Path) -> Result<wg::runtime_verify::Evidence, String> {
    verify_with_path(repo, "/usr/local/bin:/usr/bin:/bin")
}

fn verify_with_path(repo: &Path, path: &str) -> Result<wg::runtime_verify::Evidence, String> {
    let Harness::Present { features } = harness(repo)? else {
        return Err("not configured".into());
    };
    let ev = repo.with_extension("evidence");
    let _ = std::fs::remove_dir_all(&ev);
    std::fs::create_dir_all(&ev).unwrap();
    let env = vec![
        ("PATH".into(), path.into()),
        ("WG_EVIDENCE_DIR".into(), ev.to_string_lossy().into_owned()),
    ];
    let (exit, tail) = run_shell_in(
        &format!("{HARNESS_DIR}/run"),
        repo,
        &env,
        Duration::from_secs(60),
    );
    if exit == UNAVAILABLE_EXIT {
        return Err(format!("unavailable\n{tail}"));
    }
    validate(&features, &ev, exit, "tree-1").map_err(|e| format!("{e}\n{tail}"))
}

fn edit(path: &Path, from: &str, to: &str) {
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains(from), "{from:?} not in {}", path.display());
    std::fs::write(path, text.replace(from, to)).unwrap();
}

#[test]
fn a_working_cli_yields_evidence_for_every_mapped_feature() {
    let repo = fixture_copy("ok");
    let e = verify(&repo).unwrap();
    assert!(e.passed && e.harness_exit == 0 && e.tree == "tree-1");
    let ids: Vec<&str> = e.features.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, ["add", "list"]);
    assert!(e.features.iter().all(|f| f.artifacts[0].bytes > 0));
}

#[test]
fn a_broken_cli_is_recorded_as_a_failed_feature() {
    let repo = fixture_copy("broken");
    edit(&repo.join("todo"), "%d. %s", "%d) %s");
    let e = verify(&repo).unwrap();
    assert!(!e.passed && e.harness_exit == 1);
    let failed: Vec<&str> = e
        .features
        .iter()
        .filter(|f| !f.passed)
        .map(|f| f.id.as_str())
        .collect();
    assert_eq!(failed, ["list"]);
}

#[test]
fn a_mapped_feature_the_harness_skips_fails_closed() {
    let repo = fixture_copy("skip");
    let feature = repo.join(HARNESS_DIR).join("features/delete.md");
    std::fs::write(feature, "# Delete an item\n").unwrap();
    let err = verify(&repo).unwrap_err();
    assert!(err.contains("delete has no evidence"), "{err}");
}

#[test]
fn a_missing_app_fails_its_features_and_is_not_unavailable() {
    let repo = fixture_copy("noapp");
    std::fs::remove_file(repo.join("todo")).unwrap();
    let e = verify(&repo).unwrap();
    assert!(!e.passed && e.harness_exit == 1);
    assert!(e.features.iter().all(|f| !f.passed));
}

#[test]
fn a_doctor_that_finds_a_tool_missing_reports_unavailable_without_a_manifest() {
    let repo = fixture_copy("doctor");
    let bin = repo.with_extension("bin");
    let _ = std::fs::remove_dir_all(&bin);
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/bin/sh", bin.join("sh")).unwrap();
    let err = verify_with_path(&repo, bin.to_str().unwrap()).unwrap_err();
    assert!(
        err.starts_with("unavailable") && err.contains("doctor: dirname is missing"),
        "{err}"
    );
    assert!(!repo
        .with_extension("evidence")
        .join("manifest.json")
        .exists());
}

#[test]
fn a_repo_without_a_harness_is_not_configured() {
    let repo = fixture_copy("none");
    std::fs::remove_dir_all(repo.join(".workgraph")).unwrap();
    assert_eq!(harness(&repo).unwrap(), Harness::NotConfigured);
}
