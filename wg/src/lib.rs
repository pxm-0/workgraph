//! The wg kernel: every effect of the factory lives here. `main.rs` maps each subcommand's
//! `Result` to one JSON line.

pub mod assets;
pub mod budget;
pub mod checks;
pub mod claude;
pub mod cli;
pub mod config;
pub mod engine;
pub mod exec;
pub mod gh;
pub mod git;
pub mod guard;
pub mod haruspex;
pub mod ids;
pub mod journal;
pub mod memory;
pub mod memory_store;
pub mod paths;
pub mod pipelines;
pub mod ports;
pub mod render;
pub mod report;
pub mod risk;
pub mod sandbox;
pub mod selfimprove;
pub mod shift;
pub mod state_dirs;
pub mod tokens;
pub mod tokens_store;
pub mod untrusted;
