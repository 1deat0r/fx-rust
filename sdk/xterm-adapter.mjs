/**
 * xterm-adapter — wire libfx to an xterm.js Terminal instance.
 *
 * This module is intentionally dependency-free: it never imports `xterm`.
 * Pass any object that implements the tiny `xterm.js` subset used here:
 *
 *   terminal.write(text: string)                      — write ANSI-escaped text
 *   terminal.clearLine?()                             — optional
 *   terminal.write("\x1b[2K\r") / "\x1b[?25l"         — fallback escape codes
 *   terminal.reset?()                                 — optional
 *
 * Rendered events:
 *   agent_message_chunk   -> plain green text (written as it streams)
 *   user_message_chunk    -> bold "> " prompt line
 *   tool_call             -> dim line "⎿ <title>"
 *   tool_call_update      -> dim status line "⎿ <tool> <completed|failed>"
 *   available_commands_update -> ignored (UI chrome)
 *   step_started          -> dim "(step N)" line
 *
 * @module libfx/xterm
 */

const RESET = "\x1b[0m";
const BOLD = "\x1b[1m";
const DIM = "\x1b[2m";
const GREEN = "\x1b[32m";
const BLUE = "\x1b[34m";

/**
 * Return `true` when the event is one libfx emits for session activity.
 * (Kept internal; used to filter noise the adapter does not render.)
 */
function isRendered(update) {
  return ["agent_message_chunk", "user_message_chunk", "tool_call", "tool_call_update", "step_started"].includes(update?.sessionUpdate);
}

/**
 * Wire a libfx `FxClient` to an xterm.js-style terminal.
 *
 * @param {object} term - duck-typed xterm.js Terminal (write is required).
 * @param {import("./libfx.mjs").FxClient} client - started + initialized FxClient.
 * @param {object} [opts]
 * @param {string} [opts.sessionId] - if given, only update events for this session render.
 * @param {boolean} [opts.autoClear] - clear the current terminal line on each event (default true).
 * @param {string[]} [opts.ignore] - extra sessionUpdate kinds to not render.
 * @returns {{ dispose: () => void }} cleanup handle; call dispose() when the terminal is destroyed.
 */
export function xtermAdapter(term, client, opts = {}) {
  const { sessionId, autoClear = true, ignore = [] } = opts;
  const ignored = new Set(ignore);

  /**
   * Clear the current terminal line in place (used before writing an
   * event line so partial streaming output does not leave stray text).
   */
  function clearLine() {
    if (!autoClear) return;
    if (typeof term.clearLine === "function") {
      term.clearLine();
    } else {
      term.write("\x1b[2K\r");
    }
  }

  /**
   * @param {{ sessionId: string, update: object }} params - libfx "update" payload.
   */
  function onUpdate(params) {
    if (sessionId && params.sessionId !== sessionId) return;
    const update = params.update;
    if (!update || ignored.has(update.sessionUpdate)) return;
    clearLine();
    switch (update.sessionUpdate) {
      case "agent_message_chunk":
        term.write(`${GREEN}${update.content?.text ?? ""}${RESET}`);
        break;
      case "user_message_chunk": {
        const text = update.content?.text ?? "";
        term.write(`\r\n${BOLD}${BLUE}> ${text}${RESET}\r\n`);
        break;
      }
      case "tool_call":
        term.write(`${DIM}⎿ ${update.title ?? update.toolCallId ?? "tool"}${RESET}\r\n`);
        break;
      case "tool_call_update": {
        const status = update.status ?? update.content?.status ?? "";
        term.write(`${DIM}⎿ ${update.toolCallId ?? update.name ?? "tool"} ${status}${RESET}\r\n`);
        if (update.content?.text && status !== "completed") {
          term.write(`${DIM}  ${String(update.content.text).slice(0, 200)}${RESET}\r\n`);
        }
        break;
      }
      case "step_started":
        term.write(`${DIM}(step ${update.step ?? "?"})${RESET}\r\n`);
        break;
      default:
        // available_commands_update and any unseen kinds: no visual.
        break;
    }
  }

  client.on("update", onUpdate);
  return {
    /** Stop rendering events for this terminal. */
    dispose() {
      client.off("update", onUpdate);
    },
  };
}

/**
 * Render a user prompt into the terminal and send it to the session.
 *
 * @param {object} term - duck-typed xterm.js Terminal.
 * @param {import("./libfx.mjs").FxClient} client - started + initialized FxClient.
 * @param {string} sessionId - target session.
 * @param {string} text - the user text to send.
 * @param {object} [opts] - forwarded to `client.sessionPrompt` (onEvent, signal, timeoutMs).
 * @returns {Promise<{stopReason: string}>} the prompt result.
 */
export function sendPrompt(term, client, sessionId, text, opts = {}) {
  term.write(`\r\n${BOLD}${BLUE}> ${text}${RESET}\r\n`);
  return client.sessionPrompt(sessionId, text, {
    onEvent: opts.onEvent,
    signal: opts.signal,
    timeoutMs: opts.timeoutMs,
  });
}
