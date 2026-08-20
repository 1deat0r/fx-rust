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
| Layered config (`~/.fx/settings.json`, workspace `.fx.json`, env) | ✅ | `src/config.rs` |
| Permission gates ask/auto/yolo + glob rules + session grants | 🟡 | `src/permissions.rs`; missing sandbox, auto-classifier, approval flows |
| Providers: gateway/OpenAI-compatible + Anthropic + SSE | 🟡 | `src/providers/`; missing model catalog/capabilities, context limits |
| Agent streaming loop, steps, tool-choice, text-tool-call fallback | 🟡 | `src/agent.rs`; missing worker runtime, execution memory, question/presentation subsystems |
| Sessions (JSON, resume, per-session grants) | 🟡 | `src/sessions.rs`; missing store, migration, usage, history sidecars |
| Hooks (4 lifecycle events) | 🟡 | `src/hooks.rs`; upstream has full definitions/prompt/runtime/tool |
| MCP stdio JSON-RPC | 🟡 | `src/mcp.rs`; stdio only |
| Toolkit | 🟡 | 21 tools; see tool matrix |
| CLI (interactive/ask/resume/sessions/session/status/permissions/setup/models/help/version/hooks/mcp/upgrade) | 🟡 | missing doctor/usage/replay/auth/gh/review/capture/one-off/terminal/mcp_lookup |
| Upgrade (`fxrs upgrade --install`) | 🟡 | `src/upgrade.rs` |
| Vision (`view_image`) | ✅ | |
| Minimal subagent | 🟡 | depth-capped single tool; upstream is a 21-file subsystem |

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
| Sessions full | `core/session/*` (40 files) | store types/paths, codec, catalog, discovery, replay, migration, usage sidecars, prompt history, result store, artifacts |
| Config catalog | `core/config/settings_catalog.zig`, `settings_store.zig`, `context_limits.zig`, `input_appearance.zig`, `presentation_mode.zig` | settings schema + catalog |
| Permissions full | `core/permissions/*` (13 files) | sandbox, command admission, auto-classifier, approval flow, direct command |
| Slash commands | `core/slash_commands/*`, `builtins/commands.zig` | router + command catalog |
| Modes & mods | `core/modes/*`, `core/mods/*`, `builtins/modes.zig` | mode contract + registry |
| Hooks full | `core/hooks/*` (6 files) | definitions, common, prompt, runtime, tool |
| Skills full | `core/skills/*`, `ui/skills_screen.zig` | contract, invocation, runtime, commands |
| Images commands | `core/images/*` (2 files) | attachment + commands |
| Output/transcript presentation | `core/output/*` | diff, activity, transcript presentation/release, worker status |
| Workspace runtime | `core/workspace/*` (21 files) | file index, change tracker, grep, glob, path completion, access, menus, diagnostics, metrics, record tape |
| Agent runtime full | `core/agent/*` | worker runtime, execution memory, question prompt/answer, tool preparation, presentation |
| GitHub | `core/github/*` (3 files) | git context, publish (pr/issue), workflows |
| Usage reporting | `core/session/profile_usage*`, `usage*.zig`, `core/cli/usage_cli_runtime.zig` | usage.jsonl, recovery, reports, usage menu |
| Feedback | `core/feedback/runtime.zig` | |
| Notifications + sounds | `core/notifications/*` | contract + bundled sounds |
| Tasks | `core/tasks/task_helpers.zig` | |
| Devbox | `builtins/devbox.zig`, `core/execution/devbox_executor.zig` | |
| Browser workspace tools | `builtins/browser_workspace_tools.zig` | |
| Shell command parsing | `core/shell_command/*` (3 files) | classification, effect, lex |
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
- **Phase 1 — Core backend parity.** Full session store (codec, catalog, discovery, migration, sidecars), config catalog/context limits, permissions sandbox + auto-classifier + approval flows, slash-command catalog, full agent runtime (worker, execution memory, tool preparation), shell-command lex/classification, web tooling full set, full hooks definitions, `doctor`, `usage`, `replay` CLI.
- **Phase 2 — Protocols & auth.** Full MCP (streamable HTTP, legacy SSE, elicitation, auth, negotiation, json-schema), auth/login (OAuth + keychain), gateway model catalog/capabilities, context limits, prompt history.
- **Phase 3 — Execution & terminal.** Background store/supervisor, process tree, local + devbox executors, terminal integration (native/tmux/browser), terminal + background_process tools.
- **Phase 4 — Subagent & modes.** Full subagent subsystem + UI, modes/mods registries.
- **Phase 5 — TUI.** Render engine, transcript runtime, footer + input composer, all screens, theme detection, resize, activity, notifications/sounds.
- **Phase 6 — GitHub & advanced CLI.** git context, pr/issue publish, workflows; capture/one-off/gh/review/mcp_lookup commands.
- **Phase 7 — ACP + SDK/bindings.** ACP server/runner; NAPI + WASM cores; sdk/ JS bindings, term-demo, xterm adapter.
- **Phase 8 — Hardening.** e2e suite, benchmarks, json-schema corpus, AB parity harness (run upstream fixtures against fxrs).

Definition of done: run the upstream manual + automated surface checklists against fxrs with no gaps; every upstream CLI command, tool, config key, and screen has a Rust equivalent.
