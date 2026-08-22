# fxrs — Roadmap to 1:1 Full Parity

> **Mandate (2026-08-21):** fx-rust must be a **1:1 full Rust re-write** of
> [vercel-labs/fx](https://github.com/vercel-labs/fx) — every subsystem,
> command, tool, protocol, screen, and binding upstream ships, re-built in
> Rust with behavioral equivalence. This supersedes the earlier "faithful
> core port" scope.

## Reference points

| | |
|---|---|
| Upstream | vercel-labs/fx @ v0.0.4 (`cbd5c2e`) |
| Upstream size | 549 Zig files, ~688K LOC |
| Our size today | ~5.8K Rust LOC, 25 source files, 2 test files |
| Parity definition | Behavioral + surface 1:1: same CLI, tools, config, sessions, hooks, MCP, ACP, TUI, auth, usage reporting, SDK/bindings |

## Parity matrix

Legend: ✅ done · 🟡 partial · ❌ missing

### Already ported (core, good fidelity)
| Surface | Status | Notes |
|---|---|---|
| Layered config (`~/.fx/settings.json`, workspace `.fx.json`, env (+`FX_ADDITIONAL_DIRECTORIES`)) | ✅ | `src/config.rs` |
| Permission gates ask/auto/yolo + glob rules + session grants | 🟡 | `src/permissions.rs`; sandbox + deterministic auto-classifier landed (P1); approval flows pending |
| Providers: gateway/OpenAI-compatible + Anthropic + SSE | 🟡 | `src/providers/`; missing model catalog/capabilities, context limits |
| Agent streaming loop, steps, tool-choice, text-tool-call fallback | 🟡 | `src/agent.rs`; auto mode now runs sandbox+classifier first, model reviewer on undetermined (P1) |
| Sessions (JSON, resume, per-session grants) | 🟡 | `src/sessions.rs`; usage.jsonl sidecar landed (P1); **codec v2 + migration, latest pointer, per-session usage, delete, JSON output landed (P1)** |
| Hooks (4 lifecycle events) | ✅ | `src/hooks.rs` — full definitions contract: HookDefinition catalog (loop point + purpose), Limits, AttentionKind, Scope/Invocation, call_id/step_index in PreToolUse, enriched `fxrs hooks` |
| MCP stdio JSON-RPC | ✅ | `src/mcp.rs` — LSP-framed stdio client, per-call processes |
| MCP remote transports | ✅ | **Phase 2 — `src/mcp_transport.rs`** — streamable HTTP (`transport: "http"`), legacy HTTP+SSE (`"sse"`), endpoint validation (https/loopback-only), protocol-version negotiation with `-32022` fallback, `Mcp-Session-Id` round-trip, `${VAR}` env/header expansion, bearer-token auth |
| MCP tool schema validation | ✅ | **Phase 2 — `src/mcp_schema.rs`** — `$ref`-resolving JSON-Schema validator (types, required, additionalProperties, items, ranges, patterns, enums, anyOf/oneOf/not); invalid remote args fail fast with precise paths |
| Model catalog | ✅ | **Phase 2 — `src/model_catalog.rs`** — availability classification (ready/disabled/failed/auth_required), bounded `<mcp_servers>` prompt section with truncation, `fxrs models` table + `--json` |
| Auth store | 🟡 | **Phase 2 — `src/auth.rs`** — `~/.fx/auth.json` (0600), `fxrs auth add/list/remove` + `fxrs login`; env vars still win; OAuth + keychain pending |
| Toolkit | 🟡 | 21+ tools; see tool matrix |
| CLI (interactive/ask/resume/sessions/session/status/permissions/setup/models/help/version/hooks/mcp/upgrade/doctor/usage/replay) | 🟡 | doctor/usage/replay landed (P1); auth/gh/review/capture/one-off/terminal/mcp_lookup pending |
| Upgrade (`fxrs upgrade --install`) | 🟡 | `src/upgrade.rs` |
| Vision (`view_image`) | ✅ | |
| Minimal subagent | 🟡 | depth-capped single tool; upstream is a 21-file subsystem |
| **Shell-command lexer + classifier/effect** | ✅ | **P1 — `src/shell_command.rs`** (quotes/heredocs/assignments, class + writes/network/destructive) |
| **Tool preparation** | ✅ | **P1 — `src/tool_prep.rs`** (path absolutization, required-field validation, wired pre-execution) |
| **Execution memory** | ✅ | **P1 — `src/exec_memory.rs`** (bounded dedup-aware tool-call record replayed into the system prompt each turn) |
| **HTML→Markdown converter** | ✅ | **P1 — `src/tools/html.rs`** (self-contained state machine, script/style stripping, entities) |
| **`semantic_search` tool (BM25-lite)** | ✅ | **P1 — `src/tools/search.rs`** (workspace keyword ranking + snippets; lexical stand-in for embeddings) |
| **URL policy (SSRF guard)** | ✅ | **P1 — `src/tools/web.rs`** (http/https only; loopback + RFC1918/link-local blocked unless `FX_ALLOW_LOCAL_URLS=1`) |
| **Permission sandbox + auto-classifier** | ✅ | **P1 — `src/permissions.rs`** (`Sandbox`, `auto_classify`; deterministic fast path in auto mode) |
| **Usage store (`~/.fx/usage.jsonl`) + CLI** | ✅ | **P1 — `src/usage.rs`** (append-only records, `--period`, `fxrs usage [--json]`) |
| **Slash-command router + catalog** | ✅ | **P1 — `src/slash_commands.rs`** (upstream tokens routed; wired into shell; `/settings` added) |

### Not yet ported (upstream surface → fxrs gap)
| Subsystem | Upstream refs | Notes |
|---|---|---|
| Full TUI | `src/ui/**` (~150 files) | render engine, transcript runtime, footer, input composer (26 files), help/full-transcript/settings/resume/models/skills screens, theme detection, resize |
| Input composer | `core/input/*` | kill ring, undo, paste, selection, unicode text bounds, editor state |
| ACP (Agent Client Protocol) | `acp/*`, `core/cli/acp_runner.zig` | jsonrpc, server, sessions, prompt, test controls |
| SDK + NAPI/WASM bindings | `napi_core_main.zig`, `wasm_core_main.zig`, `wasm_term_main.zig`, `sdk/**` | libfx JS, node bindings, term-demo, xterm adapter |
| Auth/login | `core/auth/*`, `core/hosts/native_keychain.zig` | 🟡 API-key store + `fxrs auth`/`login` landed (P2); OAuth flow + keychain pending |
| Gateway model catalog | `core/gateway/*`, `builtins/gateway.zig` | ✅ **P2 — `src/gateway.rs`**: fetch + parse `GET {base}/coding-agent/v1/models` (public endpoint, anonymous fallback on 401/403), full `ModelCatalogEntry` capability metadata (tool-use/vision/reasoning/caching/context/max-tokens), upstream sort (tool-use → tier → provider → release), failure classification (auth/rate-limited/unavailable/transport/malformed), loopback-only base overrides, lazy cache + `context_limits_for`; wired into `fxrs models [--json|--offline|--limit N]` |
| Full MCP stack (rest) | `core/mcp/*` (30 files) | ✅ streamable HTTP + legacy SSE/http-SSE + protocol negotiation + json-schema resolver + stdio dispatcher landed (P2); elicitation, MCP OAuth (DCR), MRTR, tool subscription pending |
| Full subagent system | `core/subagent/*` (21 files) + `ui/subagent/*` | domain, execution, manager, authority, approvals, communication, tool host, ui projection |
| Background execution | `core/background/*`, `core/execution/*`, `tools/shell/background_process.zig` | ✅ **P3 — `src/background.rs` + `background_process` tool + supervisor/tree**: detached launch (double-fork / setsid, own session + log), JSON store with reconcile-on-load (liveness + `__FX_EXIT_CODE__` marker), start/list/get_output/log/stop, SIGTERM→SIGKILL grace, session-tagged records, `supervise` (live ps: RSS/elapsed/CPU/children), `tree` (descendant tree), `stop_tree` (group + descendant kill), restore-on-resume banner, `fxrs background` + `/background`; devbox executor pending |
| Terminal integration | `core/terminal/*` (16 files), `tools/terminal/*`, `app_terminal*` | 🟡 **P3 — `src/terminal.rs` + `terminal`/`browser_terminal` tools**: ✅ native PTY sessions (`backend` native|tmux, default native, `portable-pty`, reader-thread output ring, resize via winsize), ✅ `terminal exec` (`return_when` exit|started, `wait_ceiling_ms`), ✅ `browser_terminal` (strict `{action:"exec",command}` contract), ✅ terminal-recovery decision model (`src/terminal_recovery.rs`, host+process evidence, lost/corrupt dispositions, `fxrs doctor` checks both backends), ✅ tmux slice (durable across processes); ⏳ pending: app_terminal browser runtime, takeover, screen checkpoints |
| Sessions full | `core/session/*` (40 files) | codec/migration/pointer/usage/delete/tape/metadata landed (P1); prompt history (upstream record shape + compaction) landed (P1); result store, artifacts pending |
| Config catalog | `core/config/settings_catalog.zig`, `settings_store.zig`, `context_limits.zig`, `input_appearance.zig`, `presentation_mode.zig` | ✅ settings catalog + context limits landed (P1): `src/settings_catalog.rs`, `src/context.rs`, resolved in config + `fxrs settings`/`/settings` |
| Permissions full | `core/permissions/*` (13 files) | sandbox + deterministic auto-classifier landed (P1); ApprovalRequest/ApprovalDecision + structured prompt + AttentionRequired on interactive approval landed (P1); command admission, direct command pending |
| Slash commands | `core/slash_commands/*`, `builtins/commands.zig` | router + catalog landed (P1): help/exit/clear/version/status/model/permissions/sessions/session/resume/usage/doctor/setup/trace/feedback/workspace; compact/login/logout routed as not-ready |
| Modes & mods | `core/modes/*`, `core/mods/*`, `builtins/modes.zig` | mode contract + registry |
| Hooks full | `core/hooks/*` (6 files) | definitions, common, prompt, runtime, tool |
| Skills full | `core/skills/*`, `ui/skills_screen.zig` | contract, invocation, runtime, commands |
| Images commands | `core/images/*` (2 files) | attachment + commands |
| Output/transcript presentation | `core/output/*` | diff, activity, transcript presentation/release, worker status |
| Workspace runtime | `core/workspace/*` (21 files) | file index, change tracker, grep, glob, path completion, access, menus, diagnostics, metrics, record tape |
| Agent runtime full | `core/agent/*` | worker runtime, execution memory, question prompt/answer, tool preparation, presentation |
| GitHub | `core/github/*` (3 files) | git context, publish (pr/issue), workflows |
| Usage reporting | `core/session/profile_usage*`, `usage*.zig`, `core/cli/usage_cli_runtime.zig` | usage.jsonl + `fxrs usage` landed (P1); recovery, reports, usage menu pending |
| Feedback | `core/feedback/runtime.zig` | |
| Notifications + sounds | `core/notifications/*` | contract + bundled sounds |
| Tasks | `core/tasks/task_helpers.zig` | |
| Devbox | `builtins/devbox.zig`, `core/execution/devbox_executor.zig` | |
| Browser workspace tools | `builtins/browser_workspace_tools.zig` | |
| Shell command parsing | `core/shell_command/*` (3 files) | ✅ landed (P1) — lexer + classification + effect |
| Web tooling full | `core/tooling/*` (30 files) | web fetch/search runtimes, tool specs/admission/dispatch, file mutations, result limits |
| Shared/utilities | `core/shared/*` (18 files) | types, io, collections, display width, unicode data, token estimate, context encoding |
| Testing infra | `tests/**`, `benchmarks/**`, `sdk/tests/**` | e2e, json-schema corpus, benchmark exports |

### Tool matrix (upstream tool → fxrs status)
| Tool | fxrs |
|---|---|
| bash/run_command | ✅ |
| read_file · write_file · edit_file · delete_file · rename_file · copy_file · create_folder · file_info · glob_files · grep_files · list_files · open_file | ✅ |
| memory | ✅ |
| web_search · web_fetch | ✅ (subset of upstream runtimes) |
| ask_user_question | ✅ |
| skill · install_skill | ✅ |
| view_image (vision) | ✅ |
| subagent | 🟡 minimal |
| mcp (stdio / streamable-http / http-sse) | ✅ |
| semantic_search | ✅ (BM25-lite, P1) |
| read_tool_result (session) | ❌ |
| background_process (shell) | ✅ (supervise/tree/stop-tree) |
| terminal · browser_terminal | ✅ (native PTY + tmux; create/list/exec/send/read/resize/stop + browser_terminal exec) |
| web content / html_to_markdown / http_fetch / url_policy (as tools) | ❌ (internal subset) |

## Phased plan

- **Phase 0 — Mandate + scaffolding (this commit).** ROADMAP, parity matrix, README correction, memory.
- **Phase 1 — Core backend parity.** ✅ shell-command lex/classification, ✅ sandbox + auto-classifier, ✅ usage.jsonl + `fxrs usage`, ✅ slash-command catalog, ✅ `doctor`/`replay` CLI, ✅ hooks input builders, ✅ session codec v2 + migration + latest pointer + per-session usage + delete, ✅ settings catalog + context limits (+ context guard in agent loop). ✅ Phase 1 core backend parity is complete (session store/codec/catalog, config catalog + context limits, shell parsing, sandbox + auto-classifier, approval flow, usage, prompt history, replay tape, slash commands, tool-ready web tooling, full hooks contract, doctor/usage/replay/history/settings CLI). Phase 2 (protocols & auth) is next: full MCP stack, OAuth/login, gateway model catalog.
- **Phase 2 — Protocols & auth.** ✅ MCP streamable HTTP + legacy SSE/http-SSE transports, ✅ endpoint validation, ✅ protocol negotiation (-32022 fallback), ✅ MCP json-schema resolver, ✅ gateway model catalog + availability, ✅ auth store + `fxrs auth`/`login` (API key). Remaining: MCP elicitation, MCP OAuth (DCR + keychain), prompts/history compaction parity.
- **Phase 3 — Execution & terminal.** ✅ background store + detached launch + `background_process` tool (start/list/get_output/log/supervise/tree/stop_tree) + session-tagged records + restore-on-resume banner + `fxrs background` + `/background`. ✅ tmux terminal sessions (`terminal` tool create/list/send/read/resize/stop, `fxrs terminal` + `/terminal`, tmux-liveness reconcile; `fxrs doctor` checks tmux/background/terminal stores). ✅ native PTY sessions (default backend) + `terminal exec` + `browser_terminal` exec + terminal-recovery decision model landed. ⏳ Remaining (operator priority): browser/app terminal runtime, recovery/takeover (app_terminal takeover), devbox executors.
- **Phase 4 — Subagent & modes.** Full subagent subsystem + UI, modes/mods registries.
- **Phase 5 — TUI.** Render engine, transcript runtime, footer + input composer, all screens, theme detection, resize, activity, notifications/sounds.
- **Phase 6 — GitHub & advanced CLI.** git context, pr/issue publish, workflows; capture/one-off/gh/review/mcp_lookup commands.
- **Phase 7 — ACP + SDK/bindings.** ACP server/runner; NAPI + WASM cores; sdk/ JS bindings, term-demo, xterm adapter.
- **Phase 8 — Hardening.** e2e suite, benchmarks, json-schema corpus, AB parity harness (run upstream fixtures against fxrs).

Definition of done: run the upstream manual + automated surface checklists against fxrs with no gaps; every upstream CLI command, tool, config key, and screen has a Rust equivalent.
