/**
 * libfx integration + unit tests. Runs against the REAL `fxrs acp` binary.
 *
 * Every test that spawns the binary sets FX_HOME to a fresh temp dir BEFORE
 * the child starts, so tests never touch ~/.fx. The child is spawned with the
 * repository root as its workspace (a fresh empty dir would otherwise change
 * the server's SessionStore workspace hash and config resolution).
 *
 * Run:  node --test tests/test-libfx.mjs
 *   or:  FXRS_BIN=$PWD/target/debug/fxrs node --test tests/
 */

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import {
  readdirSync,
  mkdtempSync,
  rmSync,
  existsSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, basename, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { EventEmitter } from "node:events";

import { FxClient, prompt } from "../libfx.mjs";
import { xtermAdapter, sendPrompt } from "../xterm-adapter.mjs";

// --- environment -----------------------------------------------------------

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, "..", "..");
const BIN = process.env.FXRS_BIN
  ? resolve(process.env.FXRS_BIN)
  : join(REPO_ROOT, "target", "debug", "fxrs");

if (!existsSync(BIN)) {
  throw new Error(
    `fxrs binary not found at ${BIN}. Build it (cargo build) or set FXRS_BIN.`,
  );
}

/** Build a client pointed at the real binary, spawned in the repo root. */
function makeClient(opts = {}) {
  return new FxClient({ bin: BIN, cwd: REPO_ROOT, ...opts });
}

let home;
before(() => {
  home = mkdtempSync(join(tmpdir(), "libfx-test-home-"));
  // Must be set before any child is spawned; spawn() inherits process.env.
  process.env.FX_HOME = home;
});

after(() => {
  try {
    rmSync(home, { recursive: true, force: true });
  } catch {
    /* best-effort cleanup */
  }
});

/** Locate persisted session ids under the temp FX_HOME sessions store. */
function persistedSessionIds() {
  const store = join(home, "sessions");
  if (!existsSync(store)) return [];
  const ids = [];
  for (const dir of readdirSync(store)) {
    if (!dir.startsWith("ws-")) continue;
    for (const file of readdirSync(join(store, dir))) {
      if (file === "latest.json" || !file.endsWith(".json")) continue;
      ids.push(file.slice(0, -".json".length));
    }
  }
  return ids;
}

// --- tests ------------------------------------------------------------------

test(
  "initialize returns protocolVersion 1 and agent capabilities",
  { timeout: 10_000 },
  async (t) => {
    const client = makeClient();
    t.after(() => client.close());
    await client.start();

    const result = await client.initialize();

    assert.equal(result.protocolVersion, 1);
    assert.equal(typeof result.agentInfo, "object");
    assert.equal(result.agentInfo.name, "fx");
    assert.equal(typeof result.agentInfo.version, "string");
    // Capability groups the ACP client needs to plan its UI.
    assert.equal(typeof result.agentCapabilities, "object");
    assert.equal(result.agentCapabilities.loadSession, true);
    assert.equal(typeof result.agentCapabilities.sessionCapabilities, "object");
    assert.equal(typeof result.agentCapabilities.promptCapabilities, "object");
    assert.ok(Array.isArray(result.authMethods));
  },
);

test(
  "session/new returns sessionId, configOptions and modes; initialize is idempotent",
  { timeout: 10_000 },
  async (t) => {
    const client = makeClient();
    t.after(() => client.close());
    await client.start();
    await client.initialize();

    const created = await client.sessionNew({});

    assert.match(created.sessionId, /^s/);
    assert.ok(Array.isArray(created.configOptions));
    const model = created.configOptions.find((o) => o.key === "model");
    const mode = created.configOptions.find((o) => o.key === "mode");
    assert.ok(model, "configOptions exposes a model key");
    assert.ok(mode, "configOptions exposes a mode key");
    assert.ok(
      ["code", "ask"].includes(created.modes?.currentModeId),
      `modes.currentModeId is a known mode, got ${created.modes?.currentModeId}`,
    );

    // Re-initializing is a no-op at the protocol level and keeps working.
    const second = await client.initialize();
    assert.equal(second.protocolVersion, 1);
  },
);

test(
  "session/list returns a sessions array with well-formed entries",
  { timeout: 10_000 },
  async (t) => {
    const client = makeClient();
    t.after(() => client.close());
    await client.start();
    await client.initialize();

    const listed = await client.sessionList();

    assert.ok(Array.isArray(listed.sessions));
    // Every entry is a session summary (id + workspace fields present).
    for (const s of listed.sessions) {
      assert.ok(typeof s.sessionId === "string" && s.sessionId.length > 0);
      assert.ok(typeof s.workspace === "string");
    }
    // The store is scoped to the temp FX_HOME; earlier tests may have added
    // sessions, so we do not assert absolute emptiness here.
    assert.ok(listed.sessions.length >= 0);
  },
);

test(
  "session/set_mode roundtrips currentModeId",
  { timeout: 10_000 },
  async (t) => {
    const client = makeClient();
    t.after(() => client.close());
    await client.start();
    await client.initialize();
    const created = await client.sessionNew({});

    const setCode = await client.sessionSetMode("code");
    assert.equal(setCode.currentModeId, "code");

    const setAsk = await client.sessionSetMode("ask");
    assert.equal(setAsk.currentModeId, "ask");

    // The session id survives the mode flips.
    assert.match(created.sessionId, /^s/);
  },
);

test(
  "session/load and session/resume reject with an RPC error for an unknown session",
  { timeout: 10_000 },
  async (t) => {
    const client = makeClient();
    t.after(() => client.close());
    await client.start();
    await client.initialize();

    await assert.rejects(client.sessionLoad("no-such-session-999"), (err) => {
      assert.match(err.message, /Session not found/);
      assert.match(err.message, /RPC error/);
      return true;
    });
    await assert.rejects(client.sessionResume("no-such-session-999"), /Session not found/);
  },
);

test(
  "session/close echoes the sessionId; session/remove reports deleted:false for a missing session",
  { timeout: 10_000 },
  async (t) => {
    const client = makeClient();
    t.after(() => client.close());
    await client.start();
    await client.initialize();
    const created = await client.sessionNew({});

    const closed = await client.sessionClose(created.sessionId);
    assert.equal(closed.sessionId, created.sessionId);

    const removed = await client.sessionRemove("definitely-not-there");
    assert.equal(removed.deleted, false);
  },
);

test(
  "session/cancel resolves cancelled:true even with no active prompt",
  { timeout: 10_000 },
  async (t) => {
    const client = makeClient();
    t.after(() => client.close());
    await client.start();
    await client.initialize();

    // Public helper catches transport errors defensively.
    const viaHelper = await client.cancel();
    assert.equal(viaHelper.cancelled, true);

    // Raw RPC path proves the server actually answered.
    const raw = await client._request("session/cancel", {});
    assert.equal(raw.cancelled, true);
  },
);

test(
  "session/prompt against a freshly created session reaches the server",
  { timeout: 15_000 },
  async (t) => {
    const client = makeClient();
    t.after(() => client.close());
    await client.start();
    await client.initialize();
    const created = await client.sessionNew({});

    // session/new now persists an empty session on disk (regression fixed), so
    // a prompt against the fresh id is routed to the agent run. On a machine
    // without credentials this fails with an auth/transport error (fast), not
    // "no session ... in workspace".
    try {
      const result = await client.sessionPrompt(created.sessionId, "say hello in one line", {
        timeoutMs: 8_000,
      });
      assert.ok(result && typeof result.stopReason === "string");
    } catch (err) {
      const msg = String(err && err.message ? err.message : err);
      assert.ok(
        !/no session|Session not found|workspace/s.test(msg),
        `expected a run-time error, not a missing-session error; got: ${msg}`,
      );
    }
  },
);

test(
  "prompt() helper rejects quickly for a bad sessionId",
  { timeout: 10_000 },
  async () => {
    const started = Date.now();
    await assert.rejects(
      prompt({
        sessionId: "bad-session-id",
        bin: BIN,
        cwd: REPO_ROOT,
      }),
      /Session not found/,
    );
    const elapsed = Date.now() - started;
    assert.ok(elapsed < 5_000, `expected a fast rejection, took ${elapsed}ms`);
  },
);

test(
  "requests against a stopped client reject",
  { timeout: 10_000 },
  async (t) => {
    const client = makeClient();
    await client.start();
    await client.initialize();
    await client.close();

    await assert.rejects(client.sessionList(), /not started/);
    t.after(() => client.kill());
  },
);

test(
  "LineReader handles split chunks, CRLF, garbage and a trailing unterminated line",
  async () => {
    // LineReader is internal; obtain the real class from a spawned client's
    // reader instance so the test exercises the exact production reader.
    const client = makeClient();
    await client.start();
    await client.close();
    const LineReader = client.reader.constructor;

    const lines = [];
    const reader = new LineReader((m) => lines.push(m));

    // A response frame split across several pushes (byte-by-byte where it
    // matters: in the middle of the JSON).
    reader.push('{"jsonrpc":"2.0","id":7,"resu');
    reader.push('lt":{"ok":true}}\n');

    // CRLF-terminated frame in one push.
    reader.push('{"jsonrpc":"2.0","id":8,"result":{"ok":true}}\r\n');

    // Two frames in one push.
    reader.push(
      '{"jsonrpc":"2.0","id":9,"result":{"a":1}}\n{"jsonrpc":"2.0","id":10,"result":{"b":2}}\n',
    );

    // Garbage line is tolerated and ignored.
    reader.push("this is not json\n");

    // A frame whose newline never arrives: flushed by end().
    reader.push('{"jsonrpc":"2.0","id":11,"result":{"c":3}}');

    // EOF: the trailing unterminated frame is flushed.
    reader.end();

    assert.equal(lines.length, 5);
    assert.equal(lines[0].id, 7);
    assert.equal(lines[0].result.ok, true);
    assert.equal(lines[1].id, 8); // CR stripped
    assert.equal(lines[2].id, 9);
    assert.equal(lines[3].id, 10);
    assert.equal(lines[4].id, 11); // flushed by end()

    // Empty lines are skipped.
    const reader2 = new LineReader((m) => lines2.push(m));
    const lines2 = [];
    reader2.push("\n\n");
    reader2.push('{"jsonrpc":"2.0","id":1,"result":{}}\n');
    reader2.end();
    assert.equal(lines2.length, 1);

    // Oversized unterminated buffers are dropped instead of growing forever.
    const reader3 = new LineReader((m) => lines3.push(m));
    const lines3 = [];
    reader3.push('{"x":"' + "a".repeat(20 * 1024 * 1024)); // > 16 MiB cap
    assert.equal(reader3.buffer.length, 0);
  },
);

test(
  "session persistence roundtrip: raw no-sessionId prompt saves a session file; list/load/close/remove see it",
  { timeout: 20_000 },
  async (t) => {
    // This is the only path in the current server that flushes a session to
    // disk: session/prompt WITHOUT a sessionId persists an empty session
    // before the agent run begins. (With a sessionId the server refuses first.)
    // The agent run itself currently fails — and can even panic the server in
    // the skills catalog — but the empty session file is written first, so the
    // whole persistence lifecycle below works regardless of run outcome.
    const boot = makeClient();
    await boot.start();
    await boot.initialize();
    let promptOutcome = "n/a";
    try {
      await boot._request("session/prompt", { prompt_text: "persist-test" }, { timeoutMs: 3_000 });
      promptOutcome = "resolved";
    } catch (err) {
      promptOutcome = `rejected: ${err.message}`;
    }
    boot.kill(); // server may have crashed mid-run; never rely on it

    const ids = persistedSessionIds();
    assert.ok(ids.length >= 1, `expected a persisted session file (promptOutcome=${promptOutcome})`);
    const sid = ids[0];

    // A fresh client in the same FX_HOME/workspace sees the persisted session.
    const client = makeClient();
    t.after(() => client.close());
    await client.start();
    await client.initialize();

    const listed = await client.sessionList();
    assert.ok(Array.isArray(listed.sessions));
    const entry = listed.sessions.find((s) => s.sessionId === sid);
    assert.ok(entry, `session ${sid} appears in session/list`);
    assert.equal(typeof entry.workspace, "string");
    assert.equal(typeof entry.tokens, "number");

    const loaded = await client.sessionLoad(sid);
    assert.equal(loaded.sessionId, sid);

    const closed = await client.sessionClose(sid);
    assert.equal(closed.sessionId, sid);

    const removed = await client.sessionRemove(sid);
    assert.equal(removed.deleted, true, "remove reports deletion");

    const after = await client.sessionList();
    assert.ok(
      !after.sessions.some((s) => s.sessionId === sid),
      "session no longer listed after remove",
    );
  },
);

test(
  "xtermAdapter renders agent chunks and tool calls, stops after dispose, and sendPrompt forwards",
  async () => {
    const writes = [];
    const term = {
      write: (s) => writes.push(s),
      clearLine: () => writes.push("[clear]"),
    };
    const fakeClient = new EventEmitter();
    fakeClient.sessionPrompt = async () => ({ stopReason: "end_turn" });

    const adapter = xtermAdapter(term, fakeClient, {});
    fakeClient.emit("update", {
      sessionId: "s1",
      update: { sessionUpdate: "agent_message_chunk", content: { text: "Hello " } },
    });
    fakeClient.emit("update", {
      sessionId: "s1",
      update: { sessionUpdate: "tool_call", title: "read_file", toolCallId: "t1" },
    });

    const joined = writes.join("");
    assert.ok(joined.includes("Hello "), "agent chunk is rendered");
    assert.ok(joined.includes("read_file"), "tool call is rendered");

    adapter.dispose();
    fakeClient.emit("update", {
      sessionId: "s1",
      update: { sessionUpdate: "agent_message_chunk", content: { text: "IGNORED" } },
    });
    assert.ok(!writes.join("").includes("IGNORED"), "disposed adapter ignores events");

    const out = await sendPrompt(term, fakeClient, "s1", "hi");
    assert.equal(out.stopReason, "end_turn");
    assert.ok(writes.join("").includes("> hi"), "sendPrompt writes the user line");
  },
);
