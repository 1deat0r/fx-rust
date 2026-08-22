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

pub mod acp;
pub mod agent;
pub mod approval;
pub mod auth;
pub mod background;
pub mod cli;
pub mod config;
pub mod context;
pub mod exec_memory;
pub mod executor;
pub mod gateway;
pub mod github;
pub mod history;
pub mod hooks;
pub mod mcp;
pub mod mcp_schema;
pub mod mcp_transport;
pub mod model_catalog;
pub mod model_response_recovery;
pub mod modes;
pub mod mods;
pub mod operation_id;
pub mod permissions;
pub mod providers;
pub mod result_store;
pub mod sessions;
pub mod settings_catalog;
pub mod shell_command;
pub mod slash_commands;
pub mod subagent_approval;
pub mod subagent_authority;
pub mod subagent_communication;
pub mod subagent_control;
pub mod subagent_domain;
pub mod subagent_executor;
pub mod subagent_relationship;
pub mod tape;
pub mod terminal;
pub mod terminal_recovery;
pub mod terminal_takeover;
pub mod tool_prep;
pub mod tools;
pub mod ui;
mod upgrade;
pub mod usage;
pub mod usage_recovery;
pub mod util;
pub mod version;

pub use version::VERSION;

/// Test-only helper: a process-wide mutex serializing tests that mutate
/// environment variables. Rust runs tests concurrently, and env vars are
/// process-global, so without this guard a reader test can observe a
/// writer test's temporary value (e.g. `FX_HOME` or `FX_ALLOW_LOCAL_URLS`).
#[cfg(test)]
pub mod test_env {
    use std::sync::{Mutex, OnceLock};

    pub fn lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Run `f` while holding the env guard. Recovers from a poisoned lock
    /// (a previous test's panic) so one failure cannot cascade.
    pub fn with<R>(f: impl FnOnce() -> R) -> R {
        let _guard = lock().lock().unwrap_or_else(|e| e.into_inner());
        f()
    }
}
