# fxrs

**fxrs** is a Rust port of [fx](https://github.com/vercel-labs/fx) — Vercel Labs'
tiny terminal coding agent (originally written in Zig) — rebuilt as a working
Rust crate. It mirrors fx's behavioral contract: a Unix-shell form factor, a
layered configuration system, workspace-scoped sessions you can resume, and a
four-gate permission runtime (`ask` / `auto` / `yolo`) that lets the agent run
code with a single keystroke or fully hands-free.

## Implemented surface

- **Toolkit (21 built-ins + MCP)**: bash/shell, filesystem (read, write, edit,
  delete, rename, copy, mkdir, list, glob, grep, info), web_search / web_fetch,
  memory, skills, install_skill, ask_user_question, run in-workspace,
  **view_image** (vision: attaches base64 image blocks for vision-capable
  models), **subagent** (nested agent runs, depth-capped at 3, own session).
- **Lifecycle hooks**: `PreToolUse` (allow / block / rewrite), `Stop`,
  `PostTurnEnd`, `AttentionRequired` — file-script shims at
  `~/.fx/hooks/<Event>` and `<workspace>/.fx/hooks/<Event>` speaking
  Claude-Code-style stdin-JSON / stdout `{"decision": ...}`; hook failures
  never abort the agent. Inspect with `fxrs hooks`.
- **MCP stdio clients**: `mcpServers` array in `~/.fx/settings.json`,
  workspace settings, or repo `.fx.json` (`command` / `args` / `env` /
  `transport: stdio`). Servers are discovered once per run and their tools
  published as `mcp__<server>__<tool>`; call them like any built-in. A broken
  server never wedges the agent (per-call process + 90s timeout). Inspect
  with `fxrs mcp`.
- **Upgrade**: `fxrs upgrade --install` checks the latest GitHub release and
  re-installs from source (no-op in this dev build unless `--install`).

## Deferred (per scope)

ACP, WASM/NAPI bindings, the full TUI render engine, GitHub `pr` / `issue`
flows, the full MCP streamable-http / elicitation / auth stack (stdio JSON-RPC
only), and upstream's 26-file subagent system (minimal single-tool subset).

## Status

Faithful core port of the agent core + CLI surface. Not a 1:1 line-for-line
port (upstream is ~685K LOC across 549 Zig files); deferred to roadmap:
ACP, WASM/NAPI bindings, the full TUI render engine, and `pr`/`issue`
GitHub flows.

Implemented so far:

- **Layered config** (`src/config.rs`) — `~/.fx/settings.json`, per-workspace
  `.fx.json`, `AGENTS.md` context loading, env overrides
  (`FX_MODEL`, `FX_PERMISSION_MODE`, `FX_MAX_AGENT_STEPS`, …).
- **Permission runtime** (`src/permissions.rs`) — ask / auto / yolo gates,
  rule patterns (whole-rule or glob patterns), most-specific-pattern-wins
  matching, session grants, human approval for unresolved calls.
- **Providers** (`src/providers/`) — OpenAI-compatible streaming client for
  AI Gateway and any OpenAI-compatible endpoint, plus native Anthropic
  Messages API streaming. Model-agnostic by design, like upstream.
- **Toolkit** (`src/tools/`) — 19 tools: bash, filesystem (read/write/edit/
  delete/rename/copy/folder/search), memory, question, skills, and web
  (search + fetch). Permission-agnostic tools; gating lives in the agent.
  Failures return structured JSON errors, never abort the agent.
- **Agent loop** (`src/agent.rs`) — streaming text + tool calls, step
  accounting, `max_agent_steps` (0 = unlimited), `first_call_tool_choice`
  support, auto-review fallback for unresolved calls in non-interactive
  mode, plus a text `<invoke>` fallback for endpoints without structured
  tool calling.
- **Sessions** (`src/sessions.rs`) — JSON sessions under
  `~/.fx/sessions/<workspace-hash>/`, resume, per-session grants.
- **CLI** (`src/cli.rs`, `src/main.rs`) — bare interactive shell, `ask`,
  `resume`, `sessions`, `session`, `status`, `permissions`, `setup`,
  `models`, `help`, `version`.

## Usage

```sh
fxrs                  # interactive shell in the current workspace
fxrs ask "explain this repo"
fxrs resume last      # continue the most recent session
fxrs sessions         # list sessions
fxrs permissions      # show current permission rules
fxrs models           # show the resolved model/provider
```

## Configuration

```sh
# AI Gateway (default)
export AI_GATEWAY_API_KEY=...

# Any OpenAI-compatible endpoint (local models, OpenRouter, etc.)
export FX_PROVIDER=openai FX_BASE_URL=http://localhost:11434/v1

# Native Anthropic
export ANTHROPIC_API_KEY=...
export FX_MODEL=claude-sonnet-4-6
```

Permission mode: `FX_PERMISSION_MODE=ask|auto|yolo` (default `ask`).
`yolo` allows all tool calls; `auto` uses an internal reviewer for
sensitive calls; `ask` prompts for every unresolved call.

## License

Apache-2.0. This is a from-scratch Rust port of
[fx](https://github.com/vercel-labs/fx) (Apache-2.0, © Vercel Labs); see
NOTICE. Upstream concepts, tool set, and configuration surface are faithful
to the original.
