# fxrs

**fxrs** is a Rust port of [fx](https://github.com/vercel-labs/fx) — Vercel Labs'
tiny terminal coding agent (originally written in Zig) — rebuilt as a working
Rust crate. It mirrors fx's behavioral contract: a Unix-shell form factor, a
layered configuration system, workspace-scoped sessions you can resume, and a
four-gate permission runtime (`ask` / `auto` / `yolo`) that lets the agent run
code with a single keystroke or fully hands-free.

## Implemented surface

- **Toolkit (22 built-ins + MCP)**: bash/shell, filesystem (read, write, edit,
  delete, rename, copy, mkdir, list, glob, grep, info), web_search / web_fetch
  (SSRF-guarded URL policy, HTML→Markdown extraction), **semantic_search**
  (BM25-lite ranked workspace search), memory, skills, install_skill,
  ask_user_question, run in-workspace, **view_image** (vision: attaches base64
  image blocks for vision-capable models), **subagent** (nested agent runs,
  depth-capped at 3, own session).
- **Lifecycle hooks**: `PreToolUse` (allow / block / rewrite), `Stop`,
  `PostTurnEnd`, `AttentionRequired` — file-script shims at
  `~/.fx/hooks/<Event>` and `<workspace>/.fx/hooks/<Event>` speaking
  Claude-Code-style stdin-JSON / stdout `{"decision": ...}`; hook failures
  never abort the agent. Inspect with `fxrs hooks`.
- **MCP clients (stdio + remote)**: `mcpServers` array in `~/.fx/settings.json`,
  workspace settings, or repo `.fx.json`. Three transports: `stdio` (default,
  `command`/`args`/`env`), `http` / `streamable-http` (modern streamable HTTP),
  and `sse` / `http-sse` (legacy HTTP+SSE). Remote endpoints are validated
  (https always; http only loopback), protocol versions negotiate with
  `-32022` fallback, and `Mcp-Session-Id` round-trips. Remote tool arguments
  are JSON-schema validated before the call with precise error paths. Env
  values expand `${VAR}`; `header_env` pulls headers from the environment;
  `bearer_token_env` adds `Authorization: Bearer <env>`. A broken server
  never wedges the agent (per-call connection + timeout). Inspect with
  `fxrs mcp`.
- **Upgrade**: `fxrs upgrade --install` checks the latest GitHub release and
  re-installs from source (no-op in this dev build unless `--install`).
- **Auth store + model catalog**: `fxrs auth add <provider> [--key K] [--base-url U]`
  stores API keys in `~/.fx/auth.json` (mode 0600; `fxrs login` is a shortcut);
  env vars always win as a fallback order. `fxrs models` shows the resolved
  provider plus the MCP model catalog (per-server availability + tool counts),
  and the catalog is injected into the agent system prompt as a bounded
  `<mcp_servers>` section.

## Pending (targeted — full 1:1 mandate)

fxrs is a **1:1 full Rust re-write** of upstream fx: every subsystem, command,
tool, protocol, screen, and binding ships upstream is being rebuilt in Rust
with behavioral equivalence. Still pending: the full TUI render engine +
input composer, ACP server, NAPI/WASM bindings + JS SDK, OAuth/auth stack,
MCP elicitation / OAuth + keychain, per-model gateway capabilities,
the full subagent subsystem, background/terminal execution, usage reporting,
GitHub `pr`/`issue` flows, and the `gh`/`review`/`capture`/`one-off` CLI commands.
See [ROADMAP.md](ROADMAP.md) for the parity matrix and phased plan.

## Status

Working Rust port of the fx agent core + CLI surface (~5.8K LOC so far,
upstream is ~688K LOC across 549 Zig files). **Project mandate: a 1:1 full
Rust re-write** — see [ROADMAP.md](ROADMAP.md) for the complete upstream
surface inventory, the parity matrix, and the phased plan to full parity.

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
Models without native tool-calling (many local/OpenAI-compatible endpoints,
e.g. DeepSeek streaming DSML markup) emit their tool calls as plain text.
fxrs parses three text dialects — Claude-style `<invoke name="...">`, DeepSeek
DSML (`<│DSML│invoke ...>`), and wrapped `<tool_calls>...<invoke ...>` blocks —
and hides that markup from the terminal and the saved transcript.
- **Agent loop** (`src/agent.rs`) — streaming text + tool calls, step
  accounting, `max_agent_steps` (0 = unlimited), `first_call_tool_choice`
  support, auto-review fallback for unresolved calls in non-interactive
  mode, plus a text `<invoke>` fallback for endpoints without structured
  tool calling.
- **Sessions** (`src/sessions.rs`) — JSON sessions under
  `~/.fx/sessions/<workspace-hash>/`, resume, per-session grants.
- **CLI** (`src/cli.rs`, `src/main.rs`) — bare interactive shell, `ask`,
  `resume`, `sessions`, `session`, `status`, `permissions`, `setup`,
  `models`, `mcp`, `auth`, `login`, `help`, `version`.

## Usage

```sh
fxrs                  # interactive shell in the current workspace
fxrs ask "explain this repo"
fxrs resume last      # continue the most recent session
fxrs sessions         # list sessions
fxrs session <id>     # show a session
fxrs usage [24h|7d|30d|all]  # token usage / cost from ~/.fx/usage.jsonl
fxrs settings         # settings catalog + effective values
fxrs doctor           # environment + config diagnostics
fxrs replay <id>      # replay a session transcript
fxrs replay tape <id> # replay the JSONL tool-call tape of a session
fxrs history [--search t] [--limit n] [--json]  # prompt history from ~/.fx/history.jsonl
fxrs session <id|last> [--json|--delete]  # session details / JSON / delete
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
