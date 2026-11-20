//! The factory: pure functions over owned values, no I/O. `no_std` with only `alloc`, so it
//! cannot name `std`; CI proves it by building for a target that has no `std`
//! (`cargo build -p wg-factory --target thumbv7em-none-eabihf`).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod alloc_tiers;
pub mod api;
pub mod engine;
pub mod learn;
pub mod model;
pub mod plan;
