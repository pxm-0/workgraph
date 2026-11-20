//! Shifts (design §7): nightly runs that take owner-queued GitHub issues, work each in its own
//! worktree through the dev pipeline, and leave draft PRs and a report.

pub mod admin;
pub mod config;
pub mod intake;
pub mod lock;
pub mod owner;
pub mod record;
pub mod report;
pub mod run;
pub mod sandbox;
pub mod schedule;
pub mod task;
