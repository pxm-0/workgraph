//! Prompts and pipelines compiled into the binary (every `prompts/*.md` and `pipelines/*.toml`, by
//! `build.rs`), so a run uses exactly what was reviewed. An overlay directory can add or replace
//! them for one run (eval, and the proof that a new stage needs no Rust, design §16 Phase 4); the
//! self pipeline and the preamble never come from one.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

pub const PREAMBLE: &str = include_str!("../../prompts/_preamble.md");
pub const DEV_PIPELINE: &str = include_str!("../../pipelines/dev.toml");

/// The largest prompt or pipeline file an overlay may carry.
const MAX_FILE: u64 = 64 * 1024;

/// The prompt and pipeline set a run uses.
#[derive(Clone, Debug, PartialEq)]
pub struct Assets {
    prompts: BTreeMap<String, String>,
    pipelines: BTreeMap<String, String>,
}

fn table(t: &[(&str, &str)]) -> BTreeMap<String, String> {
    t.iter()
        .map(|(n, text)| (n.to_string(), text.to_string()))
        .collect()
}

fn name_ok(n: &str) -> bool {
    (1..=32).contains(&n.len())
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `<dir>/<sub>/*.<ext>` as name → text, each name and size checked.
fn read_dir(dir: &Path, sub: &str, ext: &str) -> Result<BTreeMap<String, String>, String> {
    let d = dir.join(sub);
    let entries = match std::fs::read_dir(&d) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", d.display())),
    };
    let mut out = BTreeMap::new();
    for e in entries {
        let p = e
            .map_err(|e| format!("cannot read {}: {e}", d.display()))?
            .path();
        if p.extension().is_none_or(|x| x != ext) {
            continue;
        }
        let name = p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !name_ok(&name) {
            return Err(format!("overlay {sub}/{name}.{ext}: not an asset name"));
        }
        let meta = std::fs::symlink_metadata(&p).map_err(|e| e.to_string())?;
        if !meta.is_file() || meta.len() > MAX_FILE {
            return Err(format!(
                "overlay {sub}/{name}.{ext}: not a regular file of at most 64 KB"
            ));
        }
        let text =
            std::fs::read_to_string(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
        out.insert(name, text);
    }
    Ok(out)
}

impl Assets {
    pub fn compiled() -> Assets {
        Assets {
            prompts: table(PROMPTS),
            pipelines: table(PIPELINES),
        }
    }

    /// These assets with `<dir>/prompts/*.md` and `<dir>/pipelines/*.toml` added or replaced.
    pub fn overlay(&self, dir: &Path) -> Result<Assets, String> {
        let pipelines = read_dir(dir, "pipelines", "toml")?;
        if pipelines.contains_key("self") {
            return Err("the self pipeline is compiled in and never overlaid".into());
        }
        let mut next = self.clone();
        next.prompts.extend(read_dir(dir, "prompts", "md")?);
        next.pipelines.extend(pipelines);
        Ok(next)
    }

    pub fn prompt(&self, name: &str) -> Option<&str> {
        self.prompts.get(name).map(String::as_str)
    }

    pub fn pipeline(&self, name: &str) -> Option<&str> {
        self.pipelines.get(name).map(String::as_str)
    }

    pub fn prompts(&self) -> impl Iterator<Item = (&str, &str)> {
        self.prompts.iter().map(|(n, t)| (n.as_str(), t.as_str()))
    }

    /// sha256 of every asset in use, recorded in the journal so outcomes map to exact versions.
    pub fn hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(PREAMBLE);
        for (kind, set) in [("p", &self.prompts), ("l", &self.pipelines)] {
            for (n, t) in set {
                h.update(kind);
                h.update(n);
                h.update(t);
            }
        }
        format!("{:x}", h.finalize())[..12].to_string()
    }
}

/// The compiled assets' hash.
pub fn hash() -> String {
    Assets::compiled().hash()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(dir: &str, ext: &str) -> Vec<String> {
        let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(dir);
        let mut v: Vec<String> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == ext))
            .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('_'))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn every_prompt_and_pipeline_file_is_compiled_in_without_a_rust_change() {
        let a = Assets::compiled();
        let prompts: Vec<String> = a.prompts().map(|(n, _)| n.to_string()).collect();
        assert_eq!(prompts, names("prompts", "md"));
        let pipelines: Vec<&String> = a.pipelines.keys().collect();
        assert_eq!(
            pipelines,
            names("pipelines", "toml").iter().collect::<Vec<_>>()
        );
        assert_eq!(a.pipeline("dev"), Some(DEV_PIPELINE));
        assert!(a.prompt("_preamble").is_none());
    }

    #[test]
    fn an_overlay_adds_and_replaces_but_never_the_self_pipeline() {
        let dir = std::env::temp_dir().join(format!("wg-assets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("prompts")).unwrap();
        std::fs::create_dir_all(dir.join("pipelines")).unwrap();
        std::fs::write(dir.join("prompts/new-stage.md"), "Look at {{task}}").unwrap();
        std::fs::write(dir.join("prompts/review.md"), "replaced").unwrap();
        std::fs::write(dir.join("prompts/notes.txt"), "ignored").unwrap();
        let base = Assets::compiled();
        let a = base.overlay(&dir).unwrap();
        assert_eq!(a.prompt("new-stage"), Some("Look at {{task}}"));
        assert_eq!(a.prompt("review"), Some("replaced"));
        assert_ne!(a.hash(), base.hash());
        assert_eq!(base.overlay(&dir.join("absent")).unwrap(), base);
        std::fs::write(dir.join("pipelines/self.toml"), "").unwrap();
        assert!(base.overlay(&dir).unwrap_err().contains("self"));
        std::fs::remove_file(dir.join("pipelines/self.toml")).unwrap();
        std::fs::write(dir.join("prompts/Bad Name.md"), "").unwrap();
        assert!(base.overlay(&dir).unwrap_err().contains("asset name"));
    }
}
