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
| Upstream size | 549 Zig files, ~688K LOC (devbox executor removed upstream before v0.0.5) |
| Our size today | ~33.7K Rust LOC, 83 source files (incl. tests), 317 tests |
| Parity definition | Behavioral + surface 1:1: same CLI, tools, config, sessions, hooks, MCP, ACP, TUI, auth, usage reporting, SDK/bindings |
| Latest | 2026-08-22: skills subsystem landed (`src/skills/`, 3 commits: contract/runtime/commands + invocation); background `stop` group-kill fix |

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
| Input composer | `core/input/*` | 🟡 **`src/input_composer.rs`** — **core ported**: text boundaries (char/word/paragraph/line edges, combining-mark display units), editor state (cursor/selection/insert/backspace/delete-forward/cut), undo/redo history (invalid-transition reset, insert coalescing), bounded kill ring (consecutive-kill coalescing + finish_kill + rotate); TUI paste/entity/picker coupling deferred to Phase 5 |
| ACP (Agent Client Protocol) | `acp/*`, `core/cli/acp_runner.zig` | ✅ **`src/acp/` + `fxrs acp`** — newline-delimited JSON-RPC stdio server: `initialize` (protocol v1 + capabilities), `session/new` (configOptions/modes), `session/load`/`resume` (history replay notifications), `session/list`/`close`/`remove`, `session/prompt` (streams `session/update` agent_message_chunk/tool_call/tool_call_update via Human sink, returns stopReason; cooperative cancel via oneshot abort), `session/set_mode`, `session/set_config_option`, `session/cancel`; upstream ErrorCode table + event shapes; prompt/session test controls pending |
| SDK + NAPI/WASM bindings | `napi_core_main.zig`, `wasm_core_main.zig`, `wasm_term_main.zig`, `sdk/**` | libfx JS, node bindings, term-demo, xterm adapter |
| Auth/login | `core/auth/*`, `core/hosts/native_keychain.zig` | 🟡 API-key store + `fxrs auth`/`login` landed (P2); OAuth flow + keychain pending |
| Gateway model catalog | `core/gateway/*`, `builtins/gateway.zig` | ✅ **P2 — `src/gateway.rs`**: fetch + parse `GET {base}/coding-agent/v1/models` (public endpoint, anonymous fallback on 401/403), full `ModelCatalogEntry` capability metadata (tool-use/vision/reasoning/caching/context/max-tokens), upstream sort (tool-use → tier → provider → release), failure classification (auth/rate-limited/unavailable/transport/malformed), loopback-only base overrides, lazy cache + `context_limits_for`; wired into `fxrs models [--json|--offline|--limit N]` |
| Full MCP stack (rest) | `core/mcp/*` (30 files) | ✅ streamable HTTP + legacy SSE/http-SSE + protocol negotiation + json-schema resolver + stdio dispatcher landed (P2); elicitation, MCP OAuth (DCR), MRTR, tool subscription pending |
| Full subagent system | `core/subagent/*` (21 files) + `ui/subagent/*` | 🟡 **P4 — substantial**: `src/subagent_domain.rs` (52f7ae1, full type + validation layer), `src/subagent_control.rs` (61f5fea, control-record store + manager state machine: append_revision/awaitApproval/resumeApproval, next_lifecycle_state, apply_command, communication Delivery envelopes), `src/operation_id.rs` (9bf8b50, fxop bound ids + outcome/failure JSON), `src/subagent_authority.rs` (9bf8b50, captureAdmission), `src/subagent_executor.rs` (2419eb0, run work items through the nested agent with state transitions), authority applied at runtime (04d3fea: child model/permission/tool-filter honored; agent tool_filter + admission denial), `src/subagent_relationship.rs` (6d31fb5, parent/child index queries). `fxrs subagent` CLI (create/inspect/message/configure/relationship/lifecycle/run/list --tree/delete). Remaining: approval registry/persistence, communication store/manager, parent delivery projection, resume admission, modes UI |
| Background execution | `core/background/*`, `core/execution/*`, `tools/shell/background_process.zig` | ✅ **P3 — `src/background.rs` + `background_process` tool + supervisor/tree**: detached launch (double-fork / setsid, own session + log), JSON store with reconcile-on-load (liveness + `__FX_EXIT_CODE__` marker), start/list/get_output/log/stop, SIGTERM→SIGKILL grace, session-tagged records, `supervise` (live ps: RSS/elapsed/CPU/children), `tree` (descendant tree), `stop_tree` (group + descendant kill), restore-on-resume banner, `fxrs background` + `/background`; devbox executor pending |
| Terminal integration | `core/terminal/*` (16 files), `tools/terminal/*`, `app_terminal*` | ✅ **P3 — `src/terminal.rs` + `terminal`/`browser_terminal` tools**: native PTY sessions (default backend, `portable-pty`, reader-thread output ring), `terminal exec` (`return_when` exit|started, `wait_ceiling_ms`), `browser_terminal` (strict `{action:"exec",command}` contract), terminal-recovery decision model (`src/terminal_recovery.rs`, host+process evidence, lost/corrupt dispositions), tmux slice (durable across processes); ✅ **takeover decision layer (`src/terminal_takeover.rs`)** — Ctrl-] prefix parser (detach/help/literal + bracketed paste passthrough), Phase/ReturnReason/SurfaceReturnAction state machine, bounded input retention, alternate-screen ownership oracle; ⏳ deferred to Phase 5 TUI: app_terminal render wiring, screen checkpoints/lease correlations |
| Sessions full | `core/session/*` (40 files) | codec/migration/pointer/usage/delete/tape/metadata landed (P1); prompt history (upstream record shape + compaction) landed (P1); result store, artifacts pending |
| Config catalog | `core/config/settings_catalog.zig`, `settings_store.zig`, `context_limits.zig`, `input_appearance.zig`, `presentation_mode.zig` | ✅ settings catalog + context limits landed (P1): `src/settings_catalog.rs`, `src/context.rs`, resolved in config + `fxrs settings`/`/settings` |
| Permissions full | `core/permissions/*` (13 files) | sandbox + deterministic auto-classifier landed (P1); ApprovalRequest/ApprovalDecision + structured prompt + AttentionRequired on interactive approval landed (P1); command admission, direct command pending |
| Slash commands | `core/slash_commands/*`, `builtins/commands.zig` | router + catalog landed (P1): help/exit/clear/version/status/model/permissions/sessions/session/resume/usage/doctor/setup/trace/feedback/workspace; compact/login/logout routed as not-ready |
| Modes & mods | `core/modes/*`, `core/mods/*`, `builtins/modes.zig` | ✅ **P4 — `src/modes.rs` + `src/mods.rs`** (efbb450): ModeSpec/ToolPolicy/Registry + builtin ask/code modes (upstream order), toolAllowed + toolPolicyDeniedJson, `ToolRegistry`/`CommandRegistry` (alias + prefix matching), `mode` config key + `effective_permission_mode`, read-only tool-policy enforcement in the agent, `fxrs modes` CLI |
| Hooks full | `core/hooks/*` (6 files) | definitions, common, prompt, runtime, tool |
| Skills full | `core/skills/*` (4 files), `ui/skills_screen.zig` | ✅ **`src/skills/`** — contract (SKILL.md frontmatter parser, block descriptions, byte bounds), runtime (multi-root discovery: 7 workspace roots at every ancestor below home + managed `~/.fx/skills` + 5 compat roots; catalog diagnostics; bounded `<available_skills>` prompt section), invocation (resolve_skill, load_by_identity, sigil + natural-language prompt matching, explicit prompt section), commands (`/skills` list/show/add/install/create/remove/path; local + GitHub install with filters; transactional stage+rename). Wired: `fxrs skills [--json]`, `/skills` slash, agent system prompt advertises + auto-loads matched skills, `skill`/`install_skill` tools rebuilt. screen/menu UI deferred to Phase 5 TUI |
| Images commands | `core/images/*` (2 files) | attachment + commands |
| Output/transcript presentation | `core/output/*` | diff, activity, transcript presentation/release, worker status |
| Workspace runtime | `core/workspace/*` (21 files) | file index, change tracker, grep, glob, path completion, access, menus, diagnostics, metrics, record tape |
| Agent runtime full | `core/agent/*` | worker runtime, execution memory, question prompt/answer, tool preparation, presentation |
| GitHub | `core/github/*` (3 files) | ✅ **`src/github.rs` + `fxrs gh`** — git snapshot (`branch/status/log/diff stats`, upstream text shape), draft parse (title = first line, body = rest), `gh` CLI publish (`pr create`/`issue create`, `--no-publish` dry-run), PR/issue prompt contracts (upstream section mandates, NotGitRepository guard), `fxrs gh feedback` (fx.sh endpoint) |
| Usage reporting | `core/session/profile_usage*`, `usage*.zig`, `core/cli/usage_cli_runtime.zig` | usage.jsonl + `fxrs usage` landed (P1); ✅ **usage recovery** landed (P3): `~/.fx/usage_recovery/` marker registry (`v1 <ts>\n`, 0700/0600, validation, 512 cap), `collect_from_home_conservative` (facts/incidents/pending/unknown_pending), agent checkpoint seam (mark on save, clear when ledger covers claims), `fxrs doctor` + `fxrs usage` surfacing; reports, usage menu pending |
| Feedback | `core/feedback/runtime.zig` | |
| Notifications + sounds | `core/notifications/*` | contract + bundled sounds |
| Tasks | `core/tasks/task_helpers.zig` | |
| Devbox | ~~`builtins/devbox.zig`, `core/execution/devbox_executor.zig`~~ | **Removed upstream before v0.0.5** — no longer a parity gap. ✅ **P3 — `src/executor.rs`** (local executor): `PreparedCommand` direct-read-only / approved-shell routing, `DirectOutputProjector` (UTF-8-safe control escaping, chunk-boundary resume, 65,536-byte cap), `foregroundResultComparisonLimit`, command-contract output envelope (`exit_code`/`signal` line, `<stdout>`/`<stderr>` sections, `(no output)`, middle truncation) |
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
| skill · install_skill | ✅ | contract parse + catalog + confined resource reads + managed install |
| view_image (vision) | ✅ |
| subagent | 🟡 minimal |
| mcp (stdio / streamable-http / http-sse) | ✅ |
| semantic_search | ✅ (BM25-lite, P1) |
| read_tool_result (session) | ✅ | **`src/result_store.rs` + `src/tools/read_tool_result.rs`**: durable tool-result store (`~/.fx/results/tool_results/result-{tool}-{call8}-{content8}.txt` handles, deterministic SHA-256), large results (>16 KB) stored with preview envelope + `Use read_tool_result` hint, byte-range (1-based, UTF-8 boundary-safe, ≤64 KB) + literal query reads, agent loop wired (large tool outputs auto-store); tool is read-only policy |
| background_process (shell) | ✅ (supervise/tree/stop-tree) |
| terminal · browser_terminal | ✅ (native PTY + tmux; create/list/exec/send/read/resize/stop + browser_terminal exec) |
| web content / html_to_markdown / http_fetch / url_policy (as tools) | ❌ (internal subset) |

## Phased plan

- **Phase 0 — Mandate + scaffolding (this commit).** ROADMAP, parity matrix, README correction, memory.
- **Phase 1 — Core backend parity.** ✅ shell-command lex/classification, ✅ sandbox + auto-classifier, ✅ usage.jsonl + `fxrs usage`, ✅ slash-command catalog, ✅ `doctor`/`replay` CLI, ✅ hooks input builders, ✅ session codec v2 + migration + latest pointer + per-session usage + delete, ✅ settings catalog + context limits (+ context guard in agent loop). ✅ Phase 1 core backend parity is complete (session store/codec/catalog, config catalog + context limits, shell parsing, sandbox + auto-classifier, approval flow, usage, prompt history, replay tape, slash commands, tool-ready web tooling, full hooks contract, doctor/usage/replay/history/settings CLI). Phase 2 (protocols & auth) is next: full MCP stack, OAuth/login, gateway model catalog.
- **Phase 2 — Protocols & auth.** ✅ MCP streamable HTTP + legacy SSE/http-SSE transports, ✅ endpoint validation, ✅ protocol negotiation (-32022 fallback), ✅ MCP json-schema resolver, ✅ gateway model catalog + availability, ✅ auth store + `fxrs auth`/`login` (API key). Remaining: MCP elicitation, MCP OAuth (DCR + keychain), prompts/history compaction parity.
- **Phase 3 — Execution & terminal.** ✅ COMPLETE (2026-08-22): background store + detached launch + `background_process` tool (start/list/get_output/log/supervise/tree/stop_tree) + session-tagged records + restore-on-resume banner + `fxrs background` + `/background`. ✅ tmux terminal sessions + native PTY sessions (default backend) + `terminal exec` + `browser_terminal` exec + terminal-recovery decision model. ✅ **Recovery/takeover**: model-response recovery decision model (`src/model_response_recovery.rs`, wired into agent retry loop, commit 847214c), app_terminal takeover decision layer (`src/terminal_takeover.rs`, 70f138a), usage recovery marker registry + collector + agent checkpoint seam + doctor/usage wiring (`src/usage_recovery.rs`, eaa51a4). ✅ **Executors**: upstream removed devbox before v0.0.5; local executor ported (`src/executor.rs`, 9370502).
- **Phase 4 — Subagent & modes.** ✅ Modes/mods registries (efbb450). ✅ Subagent domain + control store + operation ids + authority + executor + relationship helpers (52f7ae1, 61f5fea, 9bf8b50, 2419eb0, 04d3fea, 6d31fb5): `fxrs subagent` can create/inspect/message/configure/relate/lifecycle/run/delete and renders the parent/child tree; subagents run under their own authority (model, permission mode, admission tool filter). ✅ **Communication store/manager + parent delivery projection + resume admission** (pending commit): `src/subagent_communication.rs` (bounded per-child delivery ledger `~/.fx/subagents/communication/<child>.json`, `deliver`/`read_page`/cursor accounting/`project_child_deliveries`/`project_parent_deliveries` parent-turn fold, work notifications), result delivery wired into the executor (child→parent Message on success / Milestone on failure, work+operation ids attached), `fxrs subagent deliveries <id>` + `fxrs subagent parent-turn <parent-id>` CLI, and `resume_admission` in `src/subagent_authority.rs` (re-derive authority from live config on resumed runs, tool restriction preserved). ✅ **Approval registry/persistence** (this commit): `src/subagent_approval.rs` — durable `Approval` records on the communication ledger (bounded 64), canonical SHA-256 identity fingerprint, `register_approval` (tool/relationship, replay-vs-conflict), pure `decide_approval_response` (upstream exact-once model), durable `resolve_approval` (once/always/deny commits status + resolved revision), `invalidate_child_approvals` (cancelled/stale), paged `snapshot_pending_routes`; Approval deliveries appended on registration; CLI `fxrs subagent approval register|resolve`, `fxrs subagent approvals [--pending] [--json]`; invalidation wired into lifecycle cancel/close and delete. Remaining: subagent UI, modes UI.
- **Phase 5 — TUI.** Render engine, transcript runtime, footer + input composer (core landed), all screens, theme detection, resize, activity, notifications/sounds. ✅ skills backend (contract/runtime/invocation/commands) landed before the TUI; remaining: skills screen/menu drawn in the TUI.
- **Phase 6 — GitHub & advanced CLI.** git context, pr/issue publish, workflows; capture/one-off/gh/review/mcp_lookup commands.
- **Phase 7 — ACP + SDK/bindings.** ACP server/runner; NAPI + WASM cores; sdk/ JS bindings, term-demo, xterm adapter.
- **Phase 8 — Hardening.** e2e suite, benchmarks, json-schema corpus, AB parity harness (run upstream fixtures against fxrs).

Definition of done: run the upstream manual + automated surface checklists against fxrs with no gaps; every upstream CLI command, tool, config key, and screen has a Rust equivalent.
