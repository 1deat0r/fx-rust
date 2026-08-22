/**
 * libfx — a minimal, dependency-free JS binding over the `fxrs acp` server.
 *
 * The ACP (Agent Client Protocol) server is a newline-delimited JSON-RPC
 * stdio service started with `fxrs acp`. This module spawns it, correlates
 * requests by id, and stream `session/update` notifications to callbacks.
 *
 * Wire protocol (v1):
 *   request:  {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
 *   response: {"jsonrpc":"2.0","id":1,"result":{...}}
 *   error:    {"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"..."}}
 *   notify:   {"jsonrpc":"2.0","method":"session/update","params":{...}}
 *
 * Session updates carry `sessionUpdate`: agent_message_chunk,
 * user_message_chunk, tool_call, tool_call_update, available_commands_update.
 *
 * @module libfx
 */

import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";

const DEFAULT_BIN = process.env.FXRS_BIN || "fxrs";
const PROTOCOL_VERSION = 1;

/** Resolve the fxrs binary path, honoring FXRS_BIN and a fallback to the
 * repository's debug build when neither exists on PATH. */
function resolveBin() {
  if (process.env.FXRS_BIN) return process.env.FXRS_BIN;
  return DEFAULT_BIN;
}

/**
 * A line-delimited JSON reader. Buffers partial lines and splits on "\n",
 * tolerant of CRLF. Emits parsed objects via onLine.
 */
class LineReader {
  constructor(onLine) {
    this.buffer = "";
    this.onLine = onLine;
  }

  push(chunk) {
    this.buffer += chunk;
    let idx;
    while ((idx = this.buffer.indexOf("\n")) !== -1) {
      let line = this.buffer.slice(0, idx);
      this.buffer = this.buffer.slice(idx + 1);
      if (line.endsWith("\r")) line = line.slice(0, -1);
      if (!line.trim()) continue;
      try {
        const msg = JSON.parse(line);
        this.onLine(msg);
      } catch (err) {
        // Ignore malformed frames; a robust client keeps going.
      }
    }
    // Cap the buffer to avoid unbounded growth on a broken stream.
    if (this.buffer.length > 16 * 1024 * 1024) this.buffer = "";
  }

  end() {
    if (this.buffer.trim().length > 0) {
      try {
        this.onLine(JSON.parse(this.buffer));
      } catch {
        /* ignore */
      }
    }
    this.buffer = "";
  }
}

/**
 * A JSON-RPC client over a spawned `fxrs acp` process.
 */
export class FxClient extends EventEmitter {
  /**
   * @param {object} [opts]
   * @param {string} [opts.bin] binary path (default FXRS_BIN || "fxrs")
   * @param {string[]} [opts.args] extra argv (default ["acp"])
   * @param {string} [opts.cwd] working directory for the child process
   * @param {number} [opts.timeoutMs] default request timeout (default 30000)
   */
  constructor(opts = {}) {
    super();
    this.bin = opts.bin || resolveBin();
    this.args = opts.args || ["acp"];
    this.cwd = opts.cwd || process.cwd();
    this.timeoutMs = opts.timeoutMs ?? 30_000;
    this.nextId = 1;
    this.pending = new Map(); // id -> {resolve, reject, timer}
    this.initialized = false;
    this.stopped = false;
    this.child = null;
    this.reader = null;
  }

  /** Spawn the child process and start reading. Idempotent. */
  async start() {
    if (this.child && this.child.exitCode === null) return;
    this.child = spawn(this.bin, this.args, {
      cwd: this.cwd,
      stdio: ["pipe", "pipe", "inherit"],
    });
    this.reader = new LineReader((msg) => this._onMessage(msg));
    this.child.stdout.setEncoding("utf8");
    this.child.stdout.on("data", (chunk) => this.reader.push(chunk));
    this.child.stdout.on("end", () => this.reader.end());
    this.child.on("error", (err) => {
      this._failAll(new Error(`libfx: cannot spawn ${this.bin}: ${err.message}`));
      this.emit("error", err);
    });
    this.child.on("exit", (code) => {
      this._failAll(new Error(`libfx: ${this.bin} exited with code ${code}`));
      this.emit("exit", code);
    });
  }

  _nextId() {
    return this.nextId++;
  }

  _onMessage(msg) {
    if (msg && msg.method === "session/update" && msg.params) {
      this.emit("update", msg.params);
      const update = msg.params.update;
      if (update && update.sessionUpdate) {
        this.emit(`update:${update.sessionUpdate}`, msg.params.sessionId, update);
      }
      return;
    }
    if (msg && msg.id !== undefined) {
      const entry = this.pending.get(msg.id);
      if (entry) {
        this.pending.delete(msg.id);
        clearTimeout(entry.timer);
        if (msg.error) {
          entry.reject(new Error(`libfx: RPC error ${msg.error.code}: ${msg.error.message}`));
        } else {
          entry.resolve(msg.result);
        }
      }
      return;
    }
    // Unknown server notification.
    if (msg && msg.method) this.emit(`notification:${msg.method}`, msg.params);
  }

  _request(method, params = {}, { timeoutMs } = {}) {
    return new Promise((resolve, reject) => {
      if (!this.child || this.child.exitCode !== null) {
        reject(new Error("libfx: client not started"));
        return;
      }
      const id = this._nextId();
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`libfx: request timed out after ${timeoutMs ?? this.timeoutMs}ms (${method})`));
      }, timeoutMs ?? this.timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      const frame = JSON.stringify({ jsonrpc: "2.0", id, method, params });
      this.child.stdin.write(frame + "\n");
    });
  }

  _failAll(err) {
    for (const [, entry] of this.pending) {
      clearTimeout(entry.timer);
      entry.reject(err);
    }
    this.pending.clear();
  }

  /**
   * Announce protocol support and return agent capabilities.
   * @returns {Promise<object>} initialize result (protocolVersion, capabilities)
   */
  async initialize() {
    const result = await this._request("initialize", { protocolVersion: PROTOCOL_VERSION });
    this.initialized = true;
    return result;
  }

  /**
   * Create a fresh session.
   * @param {object} [opts] ignored for now (configOptions are server-side)
   * @returns {Promise<{sessionId: string, configOptions: object[], modes: object}>}
   */
  async sessionNew(opts = {}) {
    return this._request("session/new", opts);
  }

  /** Load a saved session; history replays as session/update notifications. */
  async sessionLoad(sessionId) {
    return this._request("session/load", { sessionId });
  }

  /** Resume a saved session (alias of load). */
  async sessionResume(sessionId) {
    return this._request("session/resume", { sessionId });
  }

  /** List sessions for the current workspace. */
  async sessionList() {
    return this._request("session/list", {});
  }

  /** Set the current mode (e.g. "code" | "ask"). */
  async sessionSetMode(modeId) {
    return this._request("session/set_mode", { modeId });
  }

  /** Set a config option value. */
  async sessionSetConfigOption(key, value) {
    return this._request("session/set_config_option", { key, value });
  }

  /**
   * Run a prompt in a session, streaming events.
   * @param {string} sessionId target session
   * @param {string} text the user message
   * @param {object} [opts]
   * @param {(ev: object) => void} [opts.onEvent] called with each session/update
   * @param {AbortSignal} [opts.signal] abort -> session/cancel + reject
   * @param {number} [opts.timeoutMs] prompt timeout (default 120000)
   * @returns {Promise<{stopReason: string}>}
   */
  async sessionPrompt(sessionId, text, { onEvent, signal, timeoutMs } = {}) {
    const handler = (params) => {
      if (params.sessionId !== sessionId) return;
      if (onEvent) onEvent(params.update);
    };
    this.on("update", handler);

    let abortHandler = null;
    let abortPromise = null;
    if (signal) {
      abortPromise = new Promise((_, reject) => {
        abortHandler = () => {
          this.cancel().catch(() => {});
          reject(new Error("libfx: prompt aborted"));
        };
        if (signal.aborted) abortHandler();
        else signal.addEventListener("abort", abortHandler, { once: true });
      });
    }

    try {
      const result = await Promise.race([
        this._request(
          "session/prompt",
          { sessionId, prompt_text: text },
          { timeoutMs: timeoutMs ?? 120_000 },
        ),
        ...(abortPromise ? [abortPromise] : []),
      ]);
      return result;
    } finally {
      this.off("update", handler);
      if (signal && abortHandler) signal.removeEventListener?.("abort", abortHandler);
    }
  }

  /** Cancel the in-flight prompt (server cancels the active agent run). */
  async cancel() {
    try {
      return await this._request("session/cancel", {});
    } catch {
      return { cancelled: true };
    }
  }

  /** Close a session (server keeps it for future resume). */
  async sessionClose(sessionId) {
    return this._request("session/close", { sessionId });
  }

  /** Remove a session entirely. */
  async sessionRemove(sessionId) {
    return this._request("session/remove", { sessionId });
  }

  /**
   * Shut down the server and wait for the child to exit.
   * @param {number} [graceMs] (default 500)
   */
  async close(graceMs = 500) {
    if (this.stopped) return;
    this.stopped = true;
    try {
      if (this.child && this.child.exitCode === null) {
        try {
          await this._request("shutdown", {}, { timeoutMs: 2000 });
        } catch {
          /* ignore */
        }
        const proc = this.child;
        const exited = new Promise((resolve) => proc.once("exit", resolve));
        proc.stdin.end();
        const timer = setTimeout(() => {
          try {
            proc.kill("SIGTERM");
          } catch {
            /* ignore */
          }
        }, graceMs);
        await exited;
        clearTimeout(timer);
      }
    } finally {
      this._failAll(new Error("libfx: client closed"));
    }
  }

  /** Force-kill the child without a graceful shutdown. */
  kill() {
    this.stopped = true;
    if (this.child) {
      try {
        this.child.kill("SIGKILL");
      } catch {
        /* ignore */
      }
    }
  }
}

/**
 * Convenience one-shot prompt helper.
 *
 * ```js
 * const { stopReason } = await prompt({
 *   text: "hello",
 *   onEvent: (ev) => { if (ev.sessionUpdate === "agent_message_chunk") process.stdout.write(ev.content.text); },
 * });
 * ```
 *
 * @param {object} opts
 * @param {string} opts.text the prompt
 * @param {(ev: object) => void} [opts.onEvent] update callback
 * @param {string} [opts.sessionId] load an existing session instead of creating one
 * @param {string} [opts.mode]
 * @param {string} [opts.cwd]
 * @param {string} [opts.bin]
 * @returns {Promise<{sessionId: string, stopReason: string}>}
 */
export async function prompt(opts) {
  const client = new FxClient({ cwd: opts.cwd, bin: opts.bin });
  let sessionId = opts.sessionId;
  try {
    await client.start();
    await client.initialize();
    if (sessionId) {
      const loaded = await client.sessionResume(sessionId);
      sessionId = loaded.sessionId;
    } else {
      const created = await client.sessionNew({});
      sessionId = created.sessionId;
    }
    if (opts.mode) await client.sessionSetMode(opts.mode);
    const result = await client.sessionPrompt(sessionId, opts.text, {
      onEvent: opts.onEvent,
      signal: opts.signal,
    });
    return { sessionId, stopReason: result.stopReason };
  } finally {
    await client.close();
  }
}
