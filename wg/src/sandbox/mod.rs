//! The sandbox for risky units (design §8): rootless podman as `wgsbx` on the existing OrbStack
//! machine `oreochiserver-sim`. The Mac side builds jobs, talks to the agent through one fixed
//! `orb run` argv and checks what comes back; `agent` is the Linux side of the same binary.

pub mod agent;
pub mod bundle;
pub mod client;
pub mod config;
pub mod doctor;
pub mod job;
pub mod patch;
pub mod setup;

/// The agent, root-owned on the sim (design §8.3).
pub const AGENT: &str = "/usr/local/lib/wgsbx/wg";
