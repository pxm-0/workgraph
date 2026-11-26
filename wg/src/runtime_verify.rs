//! Runtime verification evidence (design §9.6, prototype): a repo's harness drives the built app
//! and writes `manifest.json` plus artifacts into an empty evidence dir the kernel made. The
//! manifest is untrusted. The kernel accepts it only if it covers exactly the repo's feature map,
//! every artifact is a regular file inside the dir whose sha256 matches, and the harness exit code
//! agrees with the per-feature results. Anything else fails closed; nothing here can turn a
//! missing harness into a pass.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{File, Metadata};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path};

/// Where a repo keeps its harness, relative to the repo root.
pub const HARNESS_DIR: &str = ".workgraph/verify";
/// The manifest's name inside the evidence dir.
pub const MANIFEST: &str = "manifest.json";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// Total artifact bytes per run; screenshots count.
pub const MAX_EVIDENCE_BYTES: u64 = 2 * 1024 * 1024;
/// `EX_TEMPFAIL`: the harness's doctor found the environment unusable. The kernel parks the task
/// instead of blaming units, and never validates.
pub const UNAVAILABLE_EXIT: i64 = 75;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    v: u32,
    features: Vec<ManifestFeature>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFeature {
    id: String,
    passed: bool,
    artifacts: Vec<ManifestArtifact>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestArtifact {
    path: String,
    sha256: String,
}

/// What the kernel records, tied to the tree the check ran against.
#[derive(Debug, PartialEq, Serialize)]
pub struct Evidence {
    pub tree: String,
    pub harness_exit: i64,
    pub passed: bool,
    pub features: Vec<FeatureEvidence>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct FeatureEvidence {
    pub id: String,
    pub passed: bool,
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Artifact {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    /// The bytes that were hashed; finalize writes these, never a fresh read of the dir.
    #[serde(skip)]
    pub data: Vec<u8>,
}

/// Whether a repo has a harness at all. `NotConfigured` is reported as such, never as a pass.
#[derive(Debug, PartialEq)]
pub enum Harness {
    NotConfigured,
    Present { features: BTreeSet<String> },
}

/// The repo's harness and feature map. Only a missing `.workgraph` or `.workgraph/verify` is "not
/// configured". A symlink on the way, a `run` that is not an executable regular file, or a map
/// with no feature is an error: a half-built harness must not read as an absent one.
pub fn harness(repo: &Path) -> Result<Harness, String> {
    let mut dir = repo.to_path_buf();
    for part in HARNESS_DIR.split('/') {
        dir.push(part);
        match std::fs::symlink_metadata(&dir) {
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Harness::NotConfigured),
            Err(e) => return Err(format!("{HARNESS_DIR} is not readable: {e}")),
            Ok(m) if !m.is_dir() => return Err(format!("{HARNESS_DIR} is not a plain directory")),
            Ok(_) => {}
        }
    }
    let meta = std::fs::symlink_metadata(dir.join("run")).map_err(|e| match e.kind() {
        ErrorKind::NotFound => format!("{HARNESS_DIR}/run is missing"),
        _ => format!("{HARNESS_DIR}/run is not readable: {e}"),
    })?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "{HARNESS_DIR}/run is not an executable regular file"
        ));
    }
    let features = features(&dir.join("features"))?;
    Ok(Harness::Present { features })
}

/// Feature ids: `features/<id>.md`, README excluded; ids follow the check-name grammar.
fn features(dir: &Path) -> Result<BTreeSet<String>, String> {
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("{HARNESS_DIR}/features is not readable: {e}"))?;
    if !meta.is_dir() {
        return Err(format!("{HARNESS_DIR}/features is not a plain directory"));
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("{HARNESS_DIR}/features is not readable: {e}"))?;
    let mut ids = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(id) = name.strip_suffix(".md") else {
            continue;
        };
        let shown = crate::ids::one_line(&name, 64);
        if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            return Err(format!("feature file {shown} is not a regular file"));
        }
        if id == "README" {
            continue;
        }
        crate::ids::check_name(id).map_err(|e| format!("feature file {shown}: {e}"))?;
        ids.insert(id.to_string());
    }
    if ids.is_empty() {
        return Err(format!(
            "{HARNESS_DIR}/features maps no feature; a harness with no map proves nothing"
        ));
    }
    Ok(ids)
}

/// Validates what the harness left in `evidence_dir` against the feature map. The kernel calls it
/// only after the harness's process group is gone, and never for `UNAVAILABLE_EXIT`.
pub fn validate(
    features: &BTreeSet<String>,
    evidence_dir: &Path,
    harness_exit: i64,
    tree: &str,
) -> Result<Evidence, String> {
    if harness_exit == UNAVAILABLE_EXIT {
        return Err("the harness reported the environment unusable (exit 75); that parks, it is never validated".into());
    }
    if features.is_empty() {
        return Err("the feature map is empty; nothing can be proven".into());
    }
    let root = evidence_dir
        .canonicalize()
        .map_err(|e| format!("evidence dir: {e}"))?;
    let seen = std::fs::symlink_metadata(root.join(MANIFEST)).map_err(|e| match e.kind() {
        ErrorKind::NotFound => format!("the harness wrote no {MANIFEST} (exit {harness_exit})"),
        _ => format!("{MANIFEST} is not readable: {e}"),
    })?;
    let text = read_once(&root.join(MANIFEST), &seen, MAX_MANIFEST_BYTES)
        .map_err(|why| format!("{MANIFEST} {why}; it must be a regular file of at most 64 KB"))?;
    let m: Manifest =
        serde_json::from_slice(&text).map_err(|e| format!("{MANIFEST} is malformed: {e}"))?;
    if m.v != 1 {
        return Err(format!("{MANIFEST} version {} is not 1", m.v));
    }
    let listed: Vec<&str> = m.features.iter().map(|f| f.id.as_str()).collect();
    let listed_set: BTreeSet<&str> = listed.iter().copied().collect();
    if listed_set.len() != listed.len() {
        return Err(format!("{MANIFEST} lists a feature twice"));
    }
    if let Some(id) = features.iter().find(|id| !listed_set.contains(id.as_str())) {
        return Err(format!("mapped feature {id} has no evidence"));
    }
    if let Some(id) = listed.iter().find(|id| !features.contains(**id)) {
        return Err(format!(
            "{MANIFEST} names feature {:?}, which is not in the map",
            crate::ids::one_line(id, 32)
        ));
    }
    let mut total = 0u64;
    let mut out = Vec::new();
    for f in m.features {
        if f.artifacts.is_empty() {
            return Err(format!("feature {} has no artifact", f.id));
        }
        let mut artifacts = Vec::new();
        for a in f.artifacts {
            let data = artifact(&root, &a, MAX_EVIDENCE_BYTES - total)?;
            total += data.len() as u64;
            artifacts.push(Artifact {
                path: a.path,
                sha256: a.sha256,
                bytes: data.len() as u64,
                data,
            });
        }
        out.push(FeatureEvidence {
            id: f.id,
            passed: f.passed,
            artifacts,
        });
    }
    let all_passed = out.iter().all(|f| f.passed);
    if all_passed != (harness_exit == 0) {
        return Err(format!(
            "harness exit {harness_exit} disagrees with its manifest (all features passed: {all_passed})"
        ));
    }
    Ok(Evidence {
        tree: tree.into(),
        harness_exit,
        passed: all_passed,
        features: out,
    })
}

/// One artifact: a relative path to a regular file under `root` (no symlink anywhere on the way)
/// whose sha256 is the one claimed. Returns the bytes it hashed.
fn artifact(root: &Path, a: &ManifestArtifact, budget: u64) -> Result<Vec<u8>, String> {
    let shown = crate::ids::one_line(&a.path, 64);
    let rel = Path::new(&a.path);
    let plain = !a.path.is_empty() && rel.components().all(|c| matches!(c, Component::Normal(_)));
    if !plain {
        return Err(format!(
            "artifact path {shown:?} must be relative with no `..`"
        ));
    }
    let mut at = root.to_path_buf();
    let mut seen = None;
    for c in rel.components() {
        at.push(c);
        let meta =
            std::fs::symlink_metadata(&at).map_err(|_| format!("artifact {shown} is missing"))?;
        if meta.file_type().is_symlink() {
            return Err(format!("artifact {shown} goes through a symlink"));
        }
        seen = Some(meta);
    }
    let seen = seen.expect("a plain path has a component");
    let bytes = read_once(&at, &seen, budget).map_err(|why| {
        if why == "is too big" {
            "evidence exceeds 2 MB".to_string()
        } else {
            format!("artifact {shown} {why}")
        }
    })?;
    if format!("{:x}", Sha256::digest(&bytes)) != a.sha256 {
        return Err(format!("artifact {shown} does not match its sha256"));
    }
    Ok(bytes)
}

/// Opens `path` once and reads at most `max` bytes through that handle. The open file must be the
/// regular, singly linked file `seen` described, so a swap or a hard link to a file outside the
/// dir is refused.
// shortcut: a plain open follows a symlink or blocks on a FIFO swapped in after the lstat; safe
// only because the kernel kills the harness before validating (§9.6), open with O_NOFOLLOW |
// O_NONBLOCK if validation ever runs beside a live writer.
fn read_once(path: &Path, seen: &Metadata, max: u64) -> Result<Vec<u8>, &'static str> {
    if !seen.is_file() {
        return Err("is not a regular file");
    }
    let file = File::open(path).map_err(|_| "cannot be opened")?;
    let meta = file.metadata().map_err(|_| "cannot be read")?;
    if !meta.is_file() || meta.dev() != seen.dev() || meta.ino() != seen.ino() {
        return Err("changed while it was checked");
    }
    if meta.nlink() != 1 {
        return Err("has more than one link");
    }
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot be read")?;
    if bytes.len() as u64 > max {
        return Err("is too big");
    }
    Ok(bytes)
}

/// The repo's QA checks the owner approved with `--evidence`.
pub fn evidence_checks<'a>(
    cfg: &'a crate::config::Config,
    repo: &'a crate::shift::config::RepoCfg,
) -> impl Iterator<Item = &'a str> + 'a {
    repo.qa_checks
        .iter()
        .filter(|n| cfg.check(&repo.name, n).is_some_and(|c| c.evidence))
        .map(String::as_str)
}

/// `Ok` when one of the owner-local override files in `factory_compose` defines the `verify`
/// service, the only one with the evidence mount (§9.6).
pub fn verify_service(factory_compose: Option<&str>) -> Result<(), String> {
    let compose = factory_compose
        .ok_or("an evidence check needs factory_compose with an override that defines `verify`")?;
    for f in compose.split_whitespace().filter(|t| t.starts_with('/')) {
        let text =
            std::fs::read_to_string(f).map_err(|e| format!("cannot read override {f}: {e}"))?;
        if defines_verify(&text) {
            return Ok(());
        }
    }
    Err("no factory compose override defines a `verify` service".into())
}

// shortcut: a line scan, not a YAML parser; it misses flow style (`services: {verify: ...}`), so
// that fails closed. Upgrade to `<factory_compose> config --services` if overrides get fancier.
fn defines_verify(text: &str) -> bool {
    let (mut in_services, mut indent) = (false, None);
    for l in text.lines() {
        let body = l.split('#').next().unwrap_or("").trim_end();
        let key = body.trim_start();
        if key.is_empty() {
            continue;
        }
        let at = body.len() - key.len();
        if at == 0 {
            in_services = body == "services:";
            indent = None;
        } else if in_services && *indent.get_or_insert(at) == at && key == "verify:" {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wg-rv-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn sha(b: &[u8]) -> String {
        format!("{:x}", Sha256::digest(b))
    }

    fn map(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn write(dir: &Path, manifest: serde_json::Value) {
        fs::write(dir.join(MANIFEST), manifest.to_string()).unwrap();
    }

    fn one_feature(dir: &Path, path: &str, hash: &str, passed: bool) {
        write(
            dir,
            serde_json::json!({"v":1,"features":[{"id":"add","passed":passed,
                "artifacts":[{"path":path,"sha256":hash}]}]}),
        );
    }

    fn err(dir: &Path) -> String {
        validate(&map(&["add"]), dir, 0, "t").unwrap_err()
    }

    #[test]
    fn defines_verify_only_under_services() {
        assert!(defines_verify(
            "services:\n  workspace: {}\n  verify:  # evidence\n    extends: workspace\n"
        ));
        assert!(!defines_verify("services:\n  workspace:\n    verify: x\n"));
        assert!(!defines_verify(
            "services:\n  workspace:\n    verify:\n      a: 1\n"
        ));
        assert!(!defines_verify(
            "volumes:\n  verify:\nservices:\n  workspace:\n"
        ));
        assert!(verify_service(None).is_err());
        let d = std::env::temp_dir().join(format!("wg-verify-svc-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("o.yml");
        std::fs::write(&f, "services:\n  verify:\n    extends: workspace\n").unwrap();
        let ok = format!("docker compose -p r -f compose.dev.yml -f {}", f.display());
        assert!(verify_service(Some(&ok)).is_ok());
        std::fs::write(&f, "services:\n  workspace: {}\n").unwrap();
        assert!(verify_service(Some(&ok))
            .unwrap_err()
            .contains("no factory compose override"));
    }

    #[test]
    fn accepts_a_matching_manifest_and_keeps_the_bytes_it_hashed() {
        let d = tmp("ok");
        fs::create_dir(d.join("add")).unwrap();
        fs::write(d.join("add/t.txt"), b"hi").unwrap();
        one_feature(&d, "add/t.txt", &sha(b"hi"), true);
        let e = validate(&map(&["add"]), &d, 0, "abc").unwrap();
        let a = &e.features[0].artifacts[0];
        assert!(e.passed && e.tree == "abc" && a.bytes == 2 && a.data == b"hi");
    }

    #[test]
    fn refuses_a_bad_artifact() {
        let d = tmp("bad");
        fs::write(d.join("t.txt"), b"hi").unwrap();
        fs::create_dir(d.join("sub")).unwrap();
        let cases: Vec<(&str, String, bool, i64, &str)> = vec![
            ("t.txt", sha(b"other"), true, 0, "sha256"),
            ("../t.txt", sha(b"hi"), true, 0, "relative"),
            ("/etc/passwd", sha(b"hi"), true, 0, "relative"),
            ("", sha(b"hi"), true, 0, "relative"),
            ("gone.txt", sha(b"hi"), true, 0, "missing"),
            ("sub", sha(b"hi"), true, 0, "not a regular file"),
            ("t.txt", sha(b"hi"), true, 1, "disagrees"),
            ("t.txt", sha(b"hi"), false, 0, "disagrees"),
        ];
        for (path, hash, passed, exit, want) in cases {
            one_feature(&d, path, &hash, passed);
            let err = validate(&map(&["add"]), &d, exit, "t").unwrap_err();
            assert!(err.contains(want), "{path:?} {exit}: {err}");
        }
    }

    #[test]
    fn refuses_symlinks_anywhere_on_the_path_and_hard_links() {
        let d = tmp("links");
        let outside = tmp("links-outside");
        fs::write(outside.join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), d.join("leaf")).unwrap();
        one_feature(&d, "leaf", &sha(b"x"), true);
        assert!(err(&d).contains("symlink"), "leaf");
        std::os::unix::fs::symlink(&outside, d.join("mid")).unwrap();
        one_feature(&d, "mid/secret", &sha(b"x"), true);
        assert!(err(&d).contains("symlink"), "mid");
        fs::hard_link(outside.join("secret"), d.join("hard")).unwrap();
        one_feature(&d, "hard", &sha(b"x"), true);
        assert!(err(&d).contains("more than one link"), "hard");
    }

    #[test]
    fn refuses_artifacts_that_are_empty_or_over_the_total_cap() {
        let d = tmp("caps");
        write(
            &d,
            serde_json::json!({"v":1,"features":[{"id":"add","passed":true,"artifacts":[]}]}),
        );
        assert!(err(&d).contains("has no artifact"));
        let half = vec![b'a'; (MAX_EVIDENCE_BYTES / 2) as usize + 1];
        fs::write(d.join("a"), &half).unwrap();
        fs::write(d.join("b"), &half).unwrap();
        write(
            &d,
            serde_json::json!({"v":1,"features":[{"id":"add","passed":true,"artifacts":[
                {"path":"a","sha256":sha(&half)},{"path":"b","sha256":sha(&half)}]}]}),
        );
        assert!(err(&d).contains("exceeds 2 MB"));
    }

    #[test]
    fn never_validates_an_unavailable_run() {
        let d = tmp("unavail");
        fs::write(d.join("t.txt"), b"hi").unwrap();
        one_feature(&d, "t.txt", &sha(b"hi"), false);
        let err = validate(&map(&["add"]), &d, UNAVAILABLE_EXIT, "t").unwrap_err();
        assert!(err.contains("exit 75"), "{err}");
    }

    #[test]
    fn read_once_refuses_a_file_other_than_the_one_checked() {
        let d = tmp("swap");
        fs::write(d.join("a"), b"a").unwrap();
        fs::write(d.join("b"), b"b").unwrap();
        let seen = fs::symlink_metadata(d.join("a")).unwrap();
        assert_eq!(
            read_once(&d.join("b"), &seen, 10),
            Err("changed while it was checked")
        );
        assert_eq!(read_once(&d.join("a"), &seen, 10), Ok(b"a".to_vec()));
        assert_eq!(read_once(&d.join("a"), &seen, 0), Err("is too big"));
    }

    #[test]
    fn refuses_a_bad_manifest() {
        let d = tmp("manifest");
        assert!(err(&d).contains("no manifest.json (exit 0)"));
        fs::write(
            d.join(MANIFEST),
            vec![b' '; MAX_MANIFEST_BYTES as usize + 1],
        )
        .unwrap();
        assert!(err(&d).contains("at most 64 KB"));
        write(&d, serde_json::json!({"v":2,"features":[]}));
        assert!(err(&d).contains("version 2"));
        write(&d, serde_json::json!({"v":1,"features":[],"extra":1}));
        assert!(err(&d).contains("malformed"));
        let f = serde_json::json!({"id":"add","passed":true,"artifacts":[]});
        write(&d, serde_json::json!({"v":1,"features":[f, f]}));
        assert!(err(&d).contains("twice"));
        fs::remove_file(d.join(MANIFEST)).unwrap();
        fs::create_dir(d.join(MANIFEST)).unwrap();
        assert!(err(&d).contains("not a regular file"));
    }

    #[test]
    fn coverage_must_equal_a_non_empty_map() {
        let d = tmp("cov");
        fs::write(d.join("t.txt"), b"hi").unwrap();
        one_feature(&d, "t.txt", &sha(b"hi"), true);
        let v = |ids: &[&str]| validate(&map(ids), &d, 0, "t").unwrap_err();
        assert!(v(&["add", "list"]).contains("list has no evidence"));
        assert!(v(&[]).contains("map is empty"));
        write(
            &d,
            serde_json::json!({"v":1,"features":[
                {"id":"add","passed":true,"artifacts":[{"path":"t.txt","sha256":sha(b"hi")}]},
                {"id":"x\ny","passed":true,"artifacts":[{"path":"t.txt","sha256":sha(b"hi")}]}]}),
        );
        assert!(v(&["add"]).contains("\"x y\", which is not in the map"));
    }

    #[test]
    fn harness_absent_is_not_configured_but_half_built_is_an_error() {
        let repo = tmp("h");
        assert_eq!(harness(&repo).unwrap(), Harness::NotConfigured);
        fs::create_dir(repo.join(".workgraph")).unwrap();
        assert_eq!(harness(&repo).unwrap(), Harness::NotConfigured);
        let dir = repo.join(HARNESS_DIR);
        fs::create_dir_all(dir.join("features")).unwrap();
        assert!(harness(&repo).unwrap_err().contains("run is missing"));
        fs::write(dir.join("run"), "#!/bin/sh\n").unwrap();
        assert!(harness(&repo).unwrap_err().contains("not an executable"));
        fs::set_permissions(dir.join("run"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(harness(&repo).unwrap_err().contains("maps no feature"));
        fs::write(dir.join("features/Bad Name.md"), "").unwrap();
        assert!(harness(&repo)
            .unwrap_err()
            .contains("feature file Bad Name.md"));
        fs::remove_file(dir.join("features/Bad Name.md")).unwrap();
        fs::create_dir(dir.join("features/add.md")).unwrap();
        assert!(harness(&repo)
            .unwrap_err()
            .contains("add.md is not a regular file"));
        fs::remove_dir(dir.join("features/add.md")).unwrap();
        fs::write(dir.join("features/README.md"), "").unwrap();
        fs::write(dir.join("features/add.md"), "").unwrap();
        assert_eq!(
            harness(&repo).unwrap(),
            Harness::Present {
                features: map(&["add"])
            }
        );
    }

    #[test]
    fn a_symlinked_harness_or_map_is_an_error() {
        let real = tmp("h-real");
        fs::create_dir_all(real.join("features")).unwrap();
        let repo = tmp("h-link");
        fs::create_dir(repo.join(".workgraph")).unwrap();
        std::os::unix::fs::symlink(&real, repo.join(HARNESS_DIR)).unwrap();
        assert!(harness(&repo)
            .unwrap_err()
            .contains("not a plain directory"));
        let repo = tmp("h-root");
        std::os::unix::fs::symlink(&real, repo.join(".workgraph")).unwrap();
        assert!(harness(&repo)
            .unwrap_err()
            .contains("not a plain directory"));
        let repo = tmp("h-map");
        let dir = repo.join(HARNESS_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("run"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("run"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(real.join("features"), dir.join("features")).unwrap();
        assert!(harness(&repo)
            .unwrap_err()
            .contains("features is not a plain directory"));
    }
}
