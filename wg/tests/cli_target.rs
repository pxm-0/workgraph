//! Test target for `cli.rs`. That file must stay byte for byte as it was before the workspace
//! (Phase 0 exit criterion) and was never rustfmt-clean, so `cargo fmt --check` skips it here.
#[rustfmt::skip]
mod cli;

/// `autotests = false` in wg/Cargo.toml (so cli.rs compiles only through this wrapper) means a new
/// `tests/*.rs` file without a `[[test]]` entry would never compile or run. Fail instead.
#[test]
fn every_test_file_is_a_declared_target() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("wg/Cargo.toml");
    let declared: std::collections::BTreeSet<String> = manifest
        .lines()
        .filter_map(|l| l.trim().strip_prefix("path = \"tests/"))
        .filter_map(|rest| rest.strip_suffix('"'))
        .map(str::to_owned)
        .collect();
    let on_disk: std::collections::BTreeSet<String> = std::fs::read_dir(root.join("tests"))
        .expect("wg/tests")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .into_string()
                .expect("utf-8 name")
        })
        .filter(|f| f.ends_with(".rs") && f != "cli.rs") // cli.rs is `mod cli` above
        .collect();
    assert_eq!(
        on_disk, declared,
        "every wg/tests/*.rs file needs a [[test]] entry in wg/Cargo.toml"
    );
}
