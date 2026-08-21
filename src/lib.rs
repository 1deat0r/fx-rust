//! fxrs — a Rust port of fx (Vercel Labs): a tiny, open, model-agnostic
//! terminal coding agent with a Unix-shell form factor.
//!
//! The port mirrors fx's behavioral contract: an agent tool loop, a layered
//! configuration system, a four-gate permission runtime (needs-approval set,
//! allow/deny rules, session grants, mode), streaming model providers, and
//! workspace-scoped session persistence.
//!
//! Ported behavior is informed by the public fx repository and docs
//! (https://github.com/vercel-labs/fx, Apache-2.0). This is an independent
//! Rust implementation, not a translation of the Zig sources.

pub mod agent;
pub mod approval;
pub mod cli;
pub mod context;
pub mod config;
pub mod exec_memory;
pub mod hooks;
pub mod mcp;
pub mod permissions;
pub mod providers;
pub mod sessions;
pub mod tape;
pub mod settings_catalog;
pub mod slash_commands;
pub mod shell_command;
pub mod tool_prep;
pub mod tools;
mod upgrade;
pub mod ui;
pub mod usage;
pub mod util;
pub mod version;

pub use version::VERSION;
