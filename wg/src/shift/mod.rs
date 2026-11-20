//! Shifts (design §7): nightly runs that take owner-queued GitHub issues, work each in its own
//! worktree through the dev pipeline, and leave draft PRs and a report.

pub mod config;
