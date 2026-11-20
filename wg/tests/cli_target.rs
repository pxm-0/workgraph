//! Test target for `cli.rs`. That file must stay byte for byte as it was before the workspace
//! (Phase 0 exit criterion) and was never rustfmt-clean, so `cargo fmt --check` skips it here.
#[rustfmt::skip]
mod cli;
