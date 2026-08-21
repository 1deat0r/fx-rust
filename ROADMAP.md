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
| Hooks (4 lifecycle events + full input builders) | 🟡 | `src/hooks.rs`; upstream has full definitions/prompt/runtime/tool |
| MCP stdio JSON-RPC | 🟡 | `src/mcp.rs`; stdio only |
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
| Auth/login | `core/auth/*`, `core/hosts/native_keychain.zig` | OAuth flow, credentials/secret, keychain, api-key validation |
| Gateway model catalog | `core/gateway/*` | model catalog + metadata, failure diagnostics |
| Full MCP stack | `core/mcp/*` (30 files) | streamable HTTP, legacy SSE/http-SSE, elicitation, MCP auth, protocol negotiation, stdio dispatcher, MRTR, tool subscription, json-schema resolver |
| Full subagent system | `core/subagent/*` (21 files) + `ui/subagent/*` | domain, execution, manager, authority, approvals, communication, tool host, ui projection |
| Background execution | `core/background/*`, `core/execution/*`, `tools/shell/background_process.zig` | background store/launch/restore/supervisor, process tree, local + devbox executors, command environment |
| Terminal integration | `core/terminal/*` (16 files), `tools/terminal/*`, `app_terminal*` | native/tmux/browser sessions, shell resolver, recovery, takeover |
| Sessions full | `core/session/*` (40 files) | codec v2 + migration + latest pointer + usage sidecar-in-session landed (P1); replay tape (record_tape analog) + display metadata + `--search` landed (P1); prompt history, result store, artifacts pending |
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
| mcp (stdio) | 🟡 |
| semantic_search | ❌ |
| read_tool_result (session) | ❌ |
| background_process (shell) | ❌ |
| terminal · browser_terminal | ❌ |
| web content / html_to_markdown / http_fetch / url_policy (as tools) | ❌ (internal subset) |

## Phased plan

- **Phase 0 — Mandate + scaffolding (this commit).** ROADMAP, parity matrix, README correction, memory.
- **Phase 1 — Core backend parity.** ✅ shell-command lex/classification, ✅ sandbox + auto-classifier, ✅ usage.jsonl + `fxrs usage`, ✅ slash-command catalog, ✅ `doctor`/`replay` CLI, ✅ hooks input builders, ✅ session codec v2 + migration + latest pointer + per-session usage + delete, ✅ settings catalog + context limits (+ context guard in agent loop). Remaining: full session catalog/discovery (store-paths parity), full agent runtime (worker runtime, question prompt/answer), full hooks definitions.
- **Phase 2 — Protocols & auth.** Full MCP (streamable HTTP, legacy SSE, elicitation, auth, negotiation, json-schema), auth/login (OAuth + keychain), gateway model catalog/capabilities, context limits, prompt history.
- **Phase 3 — Execution & terminal.** Background store/supervisor, process tree, local + devbox executors, terminal integration (native/tmux/browser), terminal + background_process tools.
- **Phase 4 — Subagent & modes.** Full subagent subsystem + UI, modes/mods registries.
- **Phase 5 — TUI.** Render engine, transcript runtime, footer + input composer, all screens, theme detection, resize, activity, notifications/sounds.
- **Phase 6 — GitHub & advanced CLI.** git context, pr/issue publish, workflows; capture/one-off/gh/review/mcp_lookup commands.
- **Phase 7 — ACP + SDK/bindings.** ACP server/runner; NAPI + WASM cores; sdk/ JS bindings, term-demo, xterm adapter.
- **Phase 8 — Hardening.** e2e suite, benchmarks, json-schema corpus, AB parity harness (run upstream fixtures against fxrs).

Definition of done: run the upstream manual + automated surface checklists against fxrs with no gaps; every upstream CLI command, tool, config key, and screen has a Rust equivalent.
