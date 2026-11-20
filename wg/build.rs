//! Embeds every prompt (`prompts/*.md`) and pipeline (`pipelines/*.toml`) in the binary, so a run
//! uses exactly what was reviewed, and a new stage needs a TOML stage and a prompt file but never a
//! Rust change (design §6.1, §16 Phase 4). Names starting with `_` (the preamble) are not prompts.

use std::fs;
use std::path::Path;

fn table(root: &Path, dir: &str, ext: &str, name: &str) -> String {
    let d = root.join(dir);
    println!("cargo:rerun-if-changed={}", d.display());
    let mut files: Vec<_> = fs::read_dir(&d)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", d.display()))
        .map(|e| e.expect("a directory entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == ext))
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('_'))
        })
        .collect();
    files.sort();
    let mut out = format!("pub const {name}: &[(&str, &str)] = &[\n");
    for p in files {
        let stem = p.file_stem().expect("a file name").to_string_lossy();
        out.push_str(&format!(
            "    ({stem:?}, include_str!({:?})),\n",
            p.display().to_string()
        ));
    }
    out.push_str("];\n");
    out
}

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let root = Path::new(&manifest)
        .parent()
        .expect("wg lives below the repo root");
    let out = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let text =
        table(root, "prompts", "md", "PROMPTS") + &table(root, "pipelines", "toml", "PIPELINES");
    fs::write(Path::new(&out).join("assets.rs"), text).expect("OUT_DIR is writable");
}
