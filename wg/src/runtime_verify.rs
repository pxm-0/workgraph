//! Runtime verification evidence (design §9.6, prototype): a repo's harness drives the built app
//! and writes `manifest.json` plus artifacts into an empty evidence dir the kernel made. The
//! manifest is untrusted. The kernel accepts it only if it covers exactly the repo's feature map,
//! every artifact is a regular file inside the dir whose sha256 matches, and the harness exit code
//! agrees with the per-feature results. Anything else fails closed; nothing here can turn a
//! missing harness into a pass.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Component, Path};

/// Where a repo keeps its harness, relative to the repo root.
pub const HARNESS_DIR: &str = ".workgraph/verify";
/// The manifest's name inside the evidence dir.
pub const MANIFEST: &str = "manifest.json";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// Total artifact bytes per run; screenshots count. Matches the sandbox's patch cap (§8.5).
pub const MAX_EVIDENCE_BYTES: u64 = 2 * 1024 * 1024;

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
}

/// Whether a repo has a harness at all. `NotConfigured` is reported as such, never as a pass.
#[derive(Debug, PartialEq)]
pub enum Harness {
    NotConfigured,
    Present { features: BTreeSet<String> },
}

/// The repo's harness and feature map. A harness dir with no runnable `run` or no feature file is
/// an error, not "not configured": a half-built harness must not read as an absent one.
pub fn harness(repo: &Path) -> Result<Harness, String> {
    let dir = repo.join(HARNESS_DIR);
    if !dir.exists() {
        return Ok(Harness::NotConfigured);
    }
    let run = dir.join("run");
    let meta =
        std::fs::symlink_metadata(&run).map_err(|_| format!("{HARNESS_DIR}/run is missing"))?;
    if !meta.is_file() {
        return Err(format!("{HARNESS_DIR}/run is not a regular file"));
    }
    let features = features(&dir.join("features"))?;
    Ok(Harness::Present { features })
}

/// Feature ids: `features/<id>.md`, README excluded; ids follow the check-name grammar.
fn features(dir: &Path) -> Result<BTreeSet<String>, String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("{HARNESS_DIR}/features is not readable: {e}"))?;
    let mut ids = BTreeSet::new();
    for entry in entries {
        let name = entry.map_err(|e| e.to_string())?.file_name();
        let name = name.to_string_lossy();
        let Some(id) = name.strip_suffix(".md") else {
            continue;
        };
        if id == "README" {
            continue;
        }
        crate::ids::check_name(id).map_err(|e| format!("feature file {name}: {e}"))?;
        ids.insert(id.to_string());
    }
    if ids.is_empty() {
        return Err(format!(
            "{HARNESS_DIR}/features maps no feature; a harness with no map proves nothing"
        ));
    }
    Ok(ids)
}

/// Validates what the harness left in `evidence_dir` against the feature map.
pub fn validate(
    features: &BTreeSet<String>,
    evidence_dir: &Path,
    harness_exit: i64,
    tree: &str,
) -> Result<Evidence, String> {
    let manifest_path = evidence_dir.join(MANIFEST);
    let meta = std::fs::symlink_metadata(&manifest_path)
        .map_err(|_| format!("the harness wrote no {MANIFEST} (exit {harness_exit})"))?;
    if !meta.is_file() || meta.len() > MAX_MANIFEST_BYTES {
        return Err(format!("{MANIFEST} is not a regular file of at most 64 KB"));
    }
    let text = std::fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?;
    let m: Manifest =
        serde_json::from_str(&text).map_err(|e| format!("{MANIFEST} is malformed: {e}"))?;
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
            crate::ids::truncate(id, 32)
        ));
    }
    let root = evidence_dir
        .canonicalize()
        .map_err(|e| format!("evidence dir: {e}"))?;
    let mut total = 0u64;
    let mut out = Vec::new();
    for f in m.features {
        if f.artifacts.is_empty() {
            return Err(format!("feature {} has no artifact", f.id));
        }
        let mut artifacts = Vec::new();
        for a in f.artifacts {
            let bytes = artifact(&root, &a)?;
            total += bytes;
            if total > MAX_EVIDENCE_BYTES {
                return Err("evidence exceeds 2 MB".into());
            }
            artifacts.push(Artifact {
                path: a.path,
                sha256: a.sha256,
                bytes,
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
/// whose sha256 is the one claimed. Returns its size.
fn artifact(root: &Path, a: &ManifestArtifact) -> Result<u64, String> {
    let rel = Path::new(&a.path);
    let plain = !a.path.is_empty() && rel.components().all(|c| matches!(c, Component::Normal(_)));
    if !plain {
        return Err(format!(
            "artifact path {:?} must be relative with no `..`",
            crate::ids::truncate(&a.path, 64)
        ));
    }
    let mut at = root.to_path_buf();
    for c in rel.components() {
        at.push(c);
        let meta = std::fs::symlink_metadata(&at)
            .map_err(|_| format!("artifact {} is missing", a.path))?;
        if meta.file_type().is_symlink() {
            return Err(format!("artifact {} goes through a symlink", a.path));
        }
    }
    let meta = std::fs::metadata(&at).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err(format!("artifact {} is not a regular file", a.path));
    }
    if meta.len() > MAX_EVIDENCE_BYTES {
        return Err("evidence exceeds 2 MB".into());
    }
    let bytes = std::fs::read(&at).map_err(|e| e.to_string())?;
    let got = format!("{:x}", Sha256::digest(&bytes));
    if got != a.sha256 {
        return Err(format!("artifact {} does not match its sha256", a.path));
    }
    Ok(meta.len())
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

    #[test]
    fn accepts_a_matching_manifest() {
        let d = tmp("ok");
        fs::write(d.join("t.txt"), b"hi").unwrap();
        one_feature(&d, "t.txt", &sha(b"hi"), true);
        let e = validate(&map(&["add"]), &d, 0, "abc").unwrap();
        assert!(e.passed && e.tree == "abc" && e.features[0].artifacts[0].bytes == 2);
    }

    #[test]
    fn refuses_what_a_faker_would_try() {
        let d = tmp("bad");
        fs::write(d.join("t.txt"), b"hi").unwrap();
        let cases: Vec<(&str, String, bool, i64, &str)> = vec![
            ("t.txt", sha(b"other"), true, 0, "sha256"),
            ("../t.txt", sha(b"hi"), true, 0, "relative"),
            ("/etc/passwd", sha(b"hi"), true, 0, "relative"),
            ("gone.txt", sha(b"hi"), true, 0, "missing"),
            ("t.txt", sha(b"hi"), true, 1, "disagrees"),
            ("t.txt", sha(b"hi"), false, 0, "disagrees"),
        ];
        for (path, hash, passed, exit, want) in cases {
            one_feature(&d, path, &hash, passed);
            let err = validate(&map(&["add"]), &d, exit, "t").unwrap_err();
            assert!(err.contains(want), "{path} {exit}: {err}");
        }
        std::os::unix::fs::symlink("/etc/hosts", d.join("link")).unwrap();
        one_feature(&d, "link", &sha(b"x"), true);
        assert!(validate(&map(&["add"]), &d, 0, "t")
            .unwrap_err()
            .contains("symlink"));
    }

    #[test]
    fn coverage_must_equal_the_map() {
        let d = tmp("cov");
        fs::write(d.join("t.txt"), b"hi").unwrap();
        one_feature(&d, "t.txt", &sha(b"hi"), true);
        assert!(validate(&map(&["add", "list"]), &d, 0, "t")
            .unwrap_err()
            .contains("list has no evidence"));
        assert!(validate(&map(&["list"]), &d, 0, "t").is_err());
        fs::remove_file(d.join(MANIFEST)).unwrap();
        assert!(validate(&map(&["add"]), &d, 0, "t")
            .unwrap_err()
            .contains("no manifest.json"));
    }

    #[test]
    fn harness_absent_is_not_configured_but_half_built_is_an_error() {
        let repo = tmp("h");
        assert_eq!(harness(&repo).unwrap(), Harness::NotConfigured);
        let dir = repo.join(HARNESS_DIR);
        fs::create_dir_all(dir.join("features")).unwrap();
        assert!(harness(&repo).unwrap_err().contains("run is missing"));
        fs::write(dir.join("run"), "#!/bin/sh\n").unwrap();
        assert!(harness(&repo).unwrap_err().contains("maps no feature"));
        fs::write(dir.join("features/README.md"), "").unwrap();
        fs::write(dir.join("features/add.md"), "").unwrap();
        assert_eq!(
            harness(&repo).unwrap(),
            Harness::Present {
                features: map(&["add"])
            }
        );
    }
}
