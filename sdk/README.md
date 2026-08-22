# libfx — JavaScript SDK for fxrs (the Rust port of fx)

`libfx` is a small, dependency-free JavaScript SDK for
[`fxrs`](https://github.com/vercel/fx) — the Rust 1:1 rewrite of
[vercel-labs/fx](https://github.com/vercel-labs/fx). It speaks the **Agent
Client Protocol (ACP)** to the `fxrs acp` server over newline-delimited
JSON-RPC on stdio, and exposes a promise-based client plus a streaming event
API.

Everything here runs on Node ≥ 20 with **zero npm dependencies** — only
`node:child_process` and `node:events`.

---

## What it is

`fxrs acp` is a small, long-lived JSON-RPC 2.0 service over standard input and
output:

```
your app  ──{"jsonrpc":"2.0","id":1,"method":"initialize","params":{...}}\n──▶  fxrs acp
          ◀──{"jsonrpc":"2.0","id":1,"result":{...}}\n────────────────────────  (responses)
          ◀──{"jsonrpc":"2.0","method":"session/update","params":{...}}\n────  (streaming)
```

- Each request is one JSON object on one line (`\n`-terminated); each response
  or notification is one JSON object on one line.
- Requests carry an increasing numeric `id`; responses echo that `id`.
- During a prompt, the server pushes `session/update` **notifications** (not
  responses) so the client can stream tokens, tool calls, and status changes
  as they happen.
- The client correlates responses by id, routes notifications to event
  callbacks, and manages the child process lifecycle.

`libfx` wraps that protocol so callers never touch the wire format.

---

## Quickstart

### 1. Install / import

The package is consumed from source (no build step):

```js
import { FxClient, prompt } from "./libfx.mjs";          // local checkout
import { FxClient, prompt } from "libfx";                // npm name
import { xtermAdapter } from "libfx/terminal";           // xterm.js wiring
```

The npm package is published under the name **`libfx`** (same name as the
upstream SDK), version 0.1.0, Apache-2.0, Node ≥ 20.

### 2. Find the binary

`libfx` resolves the `fxrs` binary in this order:

1. the `bin` option passed to `FxClient` / `prompt`,
2. `FXRS_BIN` environment variable,
3. `fxrs` on `PATH`.

```js
const client = new FxClient({ bin: "./target/debug/fxrs" });
```

### 3. Spawn, initialize, create a session, prompt

```js
import { FxClient } from "./libfx.mjs";

const client = new FxClient();
await client.start();               // spawn `fxrs acp`
const init = await client.initialize();       // { protocolVersion, agentInfo, agentCapabilities, authMethods }
const { sessionId } = await client.sessionNew({});

// Stream the reply to the console:
const result = await client.sessionPrompt(sessionId, "say hello in one line", {
  onEvent: (ev) => {
    if (ev.sessionUpdate === "agent_message_chunk") process.stdout.write(ev.content?.text ?? "");
    if (ev.sessionUpdate === "tool_call") console.log(`\n[tool] ${ev.title}`);
  },
});
console.log("\nstopReason:", result.stopReason);

await client.close();               // graceful shutdown (sends `shutdown`, then exits)
```

### 4. One-shot helper

For simple "ask one question, get one answer" flows there is a convenience
helper that creates (or resumes) a session, runs the prompt, and shuts down:

```js
import { prompt } from "./libfx.mjs";

const { sessionId, stopReason } = await prompt({
  text: "what is the airspeed velocity of an unladen swallow?",
  onEvent: (ev) => {
    if (ev.sessionUpdate === "agent_message_chunk") process.stdout.write(ev.content?.text ?? "");
  },
});
```

---

## API reference

### `class FxClient`

Create with `new FxClient(opts)`:

| option | default | description |
| --- | --- | --- |
| `bin` | `FXRS_BIN \|\| "fxrs"` | path to the `fxrs` executable |
| `args` | `["acp"]` | argv appended after the binary |
| `cwd` | `process.cwd()` | working directory for the child (also the session workspace) |
| `timeoutMs` | `30_000` | default per-request timeout |

`FxClient` extends `EventEmitter`; it emits:

| event | payload | when |
| --- | --- | --- |
| `"update"` | `{ sessionId, update }` | on every `session/update` notification |
| `"update:<type>"` | `(sessionId, update)` | typed events — see *Streaming / events* below |
| `"notification:<method>"` | `params` | any other server notification |
| `"error"` | `Error` | child spawn/IO errors |
| `"exit"` | `code` | child exited |

Methods (all return Promises):

| method | RPC | returns |
| --- | --- | --- |
| `start()` | — | spawns the child and begins reading (idempotent) |
| `initialize()` | `initialize` | `{ protocolVersion, agentInfo, agentCapabilities, authMethods }` |
| `sessionNew(opts)` | `session/new` | `{ sessionId, configOptions, modes }` |
| `sessionLoad(id)` | `session/load` | `{ sessionId }` (replays history as updates) |
| `sessionResume(id)` | `session/resume` | `{ sessionId }` (alias of load) |
| `sessionList()` | `session/list` | `{ sessions: [...] }` |
| `sessionSetMode(modeId)` | `session/set_mode` | `{ currentModeId }` |
| `sessionSetConfigOption(key, value)` | `session/set_config_option` | `{ ok }` |
| `sessionPrompt(sessionId, text, opts)` | `session/prompt` | `{ stopReason, sessionId? }` |
| `cancel()` | `session/cancel` | `{ cancelled: true }` |
| `sessionClose(id)` | `session/close` | `{ sessionId }` |
| `sessionRemove(id)` | `session/remove` | `{ deleted }` |
| `close(graceMs?)` | `shutdown` | graceful shutdown + child exit |
| `kill()` | — | force SIGKILL the child |

`sessionPrompt` options:

| option | description |
| --- | --- |
| `onEvent` | callback for each `session/update` payload (the `update` object) |
| `signal` | `AbortSignal`; aborting cancels the prompt server-side and rejects |
| `timeoutMs` | prompt timeout (default 120_000) |

### `prompt(opts)`

One-shot convenience function. Options: `text` (required), `onEvent`,
`sessionId` (resume instead of create), `mode`, `cwd`, `bin`, `signal`.
Returns `{ sessionId, stopReason }`.

---

## Streaming / event reference

`session/update` notifications carry `params = { sessionId, update }`. The
`update.sessionUpdate` discriminates the type:

| `sessionUpdate` | update payload | meaning |
| --- | --- | --- |
| `agent_message_chunk` | `{ content: { text, type } }` | streaming assistant text |
| `user_message_chunk` | `{ content: { text, type } }` | echoed user text |
| `tool_call` | `{ title, toolCallId, kind, status }` | a tool started |
| `tool_call_update` | `{ toolCallId, status, content }` | tool finished / progressed |
| `available_commands_update` | `{ commands }` | slash-command list changed |
| `step_started` | `{ step }` | a reasoning step began |

Subscribe to everything with `client.on("update", handler)`, or to a specific
kind with `client.on("update:agent_message_chunk", (sessionId, update) => …)`.

`session/load` replays the past transcript as the same
`agent_message_chunk` / `user_message_chunk` updates, so renderers written for
live streaming work unchanged for resumes.

---

## Cancellation

To abort an in-flight prompt:

```js
const ac = new AbortController();
client.sessionPrompt(sid, "long task", { signal: ac.signal });  // later:
ac.abort();  // libfx fires session/cancel, then rejects with "prompt aborted"
```

`client.cancel()` also sends `session/cancel` directly. The server stops the
active agent run and the prompt resolves with `stopReason: "cancelled"` (or
rejects if the request is still pending).

---

## Session lifecycle

```
session/new ──▶ sessionId ──▶ session/prompt ──▶ session/close (keep for resume) ──▶ session/load / session/resume
                      │                                                                    │
                      ▼                                                                    ▼
              session/remove (delete entirely)                                   replay history as updates
```

- `sessionClose` keeps the session on disk for later `sessionLoad`/`sessionResume`.
- `sessionRemove` deletes it (`{ deleted: true }` on success, `false` if absent).
- `sessionList` returns persisted sessions for the workspace, each with
  `sessionId`, `workspace`, `updated_ms`, `tokens`, `tool_calls`.

---

## Terminal (xterm.js) adapter

`sdk/xterm-adapter.mjs` renders live session activity into an
[xterm.js](https://xtermjs.org/) terminal without depending on the package —
pass any terminal object with a `write()` method:

```js
import { Terminal } from "@xterm/xterm";
import { FxClient } from "../libfx.mjs";
import { xtermAdapter, sendPrompt } from "../xterm-adapter.mjs";

const term = new Terminal();
term.open(document.getElementById("terminal"));

const client = new FxClient({ bin: "./target/debug/fxrs" });
await client.start();
await client.initialize();
const { sessionId } = await client.sessionNew({});

const adapter = xtermAdapter(term, client, { sessionId });
// later, on Enter:
await sendPrompt(term, client, sessionId, input);
```

Agent chunks stream in green, tool calls render as dim `⎿` lines, user messages
as bold `> ` lines. `xtermAdapter` returns `{ dispose }` to stop rendering.

---

## Examples

- `examples/basic.mjs` — end-to-end spawn → initialize → session/new → prompt,
  printing the streamed transcript. Runnable as
  `node examples/basic.mjs --bin ./target/debug/fxrs "say hello in one line"`.
  Without a configured gateway the prompt step reports a clear error and exits
  1 (the session machinery still demonstrates the full flow).
- `term-demo/demo.mjs` — a one-shot terminal demo using the `prompt()` helper
  with ANSI-built transcript output.
- `index.html` — a standalone (no build) page that documents libfx + ACP and
  shows protocol frames side by side.

---

## Tests

Run against the real `fxrs` binary (all tests spawn the actual server and are
isolated from `~/.fx` via a temp `FX_HOME`):

```sh
# from the repository root
FXRS_BIN=$PWD/target/debug/fxrs node --test 'sdk/tests/*.mjs'

# or from sdk/ using the package scripts
cd sdk && FXRS_BIN=../target/debug/fxrs npm test   # runs tests/test-*.mjs
cd sdk && FXRS_BIN=../target/debug/fxrs npm run test:smoke
```

Coverage:

- `test-libfx.mjs` (12 tests, node:test): initialize shape; session/new shape;
  session/list; session/set_mode roundtrip; session/load unknown reject;
  session/close + session/remove; session/cancel; prompt-against-unpersisted
  session; `prompt()` fast rejection for a bad sessionId; stopped-client
  rejection; LineReader split-chunk/CRLF/garbage/EOF handling; and a full
  persistence roundtrip (raw no-sessionId prompt persists a session file that
  `session/list`, `session/load`, `session/close` and `session/remove` then see).
- `test-smoke.mjs` — spawn → initialize → session/new → close; prints `PASS`.

Each test keeps the child under a temp `FX_HOME` (never touches `~/.fx`) and
kills the child in cleanup. Tests assume `./target/debug/fxrs` exists (use
`FXRS_BIN` to override; missing binaries fail with a clear message).

---

## Parity notes vs upstream `fx` SDK

The upstream Node SDK (`node.js/fx-sdk.js` in vercel-labs/fx) is the reference
for the ACP method surface; `libfx` mirrors it 1:1 at the protocol level:

| surface | upstream fx-sdk | libfx |
| --- | --- | --- |
| transport | spawn `fx acp`, stdio JSON-RPC | spawn `fxrs acp`, stdio JSON-RPC |
| initialize | `initialize()` | `initialize()` — same result shape |
| sessions | session/new, load, resume, list, close, remove | identical methods (`sessionNew`, `sessionLoad`, …) |
| prompt | streaming `session/update` events | identical events, plus typed `update:<kind>` |
| errors | RPC errors surfaced | RPC errors surfaced as `Error` with code/message |
| dependencies | depends on upstream runtime pieces | **none** (node builtins only) |

Differences from upstream:

- `libfx` targets `fxrs` (the Rust port) and is dependency-free; upstream links
  against the `fx` product runtime.
- `libfx` adds `cancel()`, `AbortSignal` support, and typed update events.
- Cancellation and session persistence semantics follow the current `fxrs`
  server; if the server changes behavior, the SDK keeps the same call surface
  and only the payload expectations may move.

---

## Roadmap note

The current `fxrs` core is a faithful C-rust port of upstream fx. A
NAPI/WASM core for `fxrs` — letting libfx embed the agent runtime in-process
instead of over stdio — is future work and intentionally out of scope for this
package. The ACP-over-stdio boundary above is the stable contract we build on.

## License

Apache-2.0.
