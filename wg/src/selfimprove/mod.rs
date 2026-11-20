//! Self-improvement, shadow first (design §12, §16 Phase 5): the protected-path allowlist and the
//! editable-Rust lint, the self-ledger and the lane's switches, the self-merge gate run by the
//! installed base binary, the replay and live eval, fixture capture, and install, canary and
//! rollback of CI-built binaries.

pub mod canary;
pub mod capture;
pub mod config;
pub mod diff;
pub mod eval_live;
pub mod eval_store;
pub mod gate;
pub mod github;
pub mod install;
pub mod ledger;
pub mod lint;
pub mod merge;
pub mod protected;
pub mod rate;
pub mod replay;
pub mod replay_check;
pub mod rollback;
pub mod state;
