//! ACP (Agent Client Protocol) — jsonrpc framing, event types, and the stdio
//! server (ports of upstream `src/acp/jsonrpc.zig`, `types.zig`,
//! `server.zig`, `sessions.zig`, `prompt.zig`).

pub mod jsonrpc;
pub mod server;
pub mod types;
