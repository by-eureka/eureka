//! Lean theorem-proving harness: an agent loop over the OpenAI Responses API,
//! Lean Beam over MCP, worktrees isolation, and bundled Lean verification.
//!
//! The call path is main -> search -> agent -> tools. [`search::Harness`] runs
//! the attempts: leads, plus any helpers a lead spawns. Each attempt gets its own
//! project copy, Docker container and conversation.
//!
//! When a lead submits, the harness stops its container, takes the target file,
//! puts it into a fresh copy of the trusted template and runs `lake comparator` there
//! ([`verify`]). Only a proof that passes is published. Whatever else the agent
//! changed in its workspace is thrown away.
//!
//! The unit tests need neither Docker nor a model:
//! `cargo test --locked --manifest-path harness/Cargo.toml --lib`.
//! The tests in `harness/tests/` say what else they need.

pub mod agent;
pub mod beam;
mod compaction;
pub mod lean_text;
pub mod prompt;
pub mod responses;
pub mod runtime;
pub mod sandbox;
pub mod search;
pub mod tools;
pub mod trace;
pub mod verify;
