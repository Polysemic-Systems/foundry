//! Foundry CLI command handlers. Each submodule owns one command domain;
//! `main.rs` remains a thin argument-parsing and dispatch shell.

pub mod ask;
pub mod common;
pub mod evolution;
pub mod iterate;
pub mod job;
pub mod plan;
pub mod propose;
pub mod review;
pub mod snapshot;
