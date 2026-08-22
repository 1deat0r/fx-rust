#!/usr/bin/env node
/**
 * basic — end-to-end libfx example.
 *
 * Spawns `fxrs acp`, initializes, creates a session, sends one prompt, and
 * prints the streamed transcript to stdout.
 *
 * Usage:
 *   node examples/basic.mjs                        # FXRS_BIN or "fxrs" from PATH
 *   node examples/basic.mjs --bin ./target/debug/fxrs "say hello in one line"
 *   node examples/basic.mjs --bin /abs/path/fxrs
 *
 * Exit codes:
 *   0  spawn/initialize/session + prompt all succeeded
 *   1  anything failed (prompt can fail without a configured gateway; the
 *      failure is printed gracefully instead of a stack trace)
 */

import { FxClient } from "../libfx.mjs";

const BOLD = "\x1b[1m";
const DIM = "\x1b[2m";
const RESET = "\x1b[0m";
const GREEN = "\x1b[32m";
const RED = "\x1b[31m";
const BLUE = "\x1b[34m";

const DEFAULT_PROMPT = "say hello in one line";

function parseArgs(argv) {
  const opts = { bin: process.env.FXRS_BIN, prompt: DEFAULT_PROMPT };
  const rest = [];
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--bin") {
      opts.bin = argv[++i];
    } else if (argv[i] === "--help" || argv[i] === "-h") {
      opts.help = true;
    } else if (argv[i] === "--version" || argv[i] === "-v") {
      opts.version = true;
    } else {
      rest.push(argv[i]);
    }
  }
  if (rest.length > 0) opts.prompt = rest.join(" ");
  return opts;
}

function renderEvent(ev) {
  switch (ev.sessionUpdate) {
    case "agent_message_chunk":
      return ev.content?.text ?? "";
    case "user_message_chunk":
      return `${BOLD}${BLUE}> ${ev.content?.text ?? ""}${RESET}\n`;
    case "tool_call":
      return `${DIM}⎿ ${ev.title ?? ev.toolCallId}${RESET}\n`;
    case "tool_call_update":
      return ev.content
        ? `${DIM}⎿ ${ev.toolCallId}: ${JSON.stringify(ev.content).slice(0, 160)}${RESET}\n`
        : "";
    case "available_commands_update":
    case "step_started":
    default:
      return "";
  }
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.help) {
    console.log(`Usage: node examples/basic.mjs [--bin PATH] ["<prompt>"]`);
    return 0;
  }
  if (opts.version) {
    console.log("libfx example (basic) 0.1.0");
    return 0;
  }

  const client = new FxClient({ bin: opts.bin });
  let sessionId = null;
  try {
    process.stdout.write(`${DIM}libfx · spawning ${opts.bin || "fxrs"} acp ...${RESET}\n`);
    await client.start();
    const init = await client.initialize();
    process.stdout.write(
      `${DIM}initialized · ${init.agentInfo?.name} v${init.agentInfo?.version} · protocol v${init.protocolVersion}${RESET}\n`,
    );

    const created = await client.sessionNew({});
    sessionId = created.sessionId;
    process.stdout.write(`${DIM}session ${sessionId} · mode ${created.modes?.currentModeId}${RESET}\n\n`);

    process.stdout.write(`${BOLD}${BLUE}> ${opts.prompt}${RESET}\n`);
    const result = await client.sessionPrompt(sessionId, opts.prompt, {
      onEvent: (ev) => process.stdout.write(renderEvent(ev)),
      timeoutMs: 120_000,
    });
    process.stdout.write("\n");
    process.stdout.write(`${GREEN}[done]${RESET} session ${sessionId} · stopReason ${result.stopReason}\n`);
    return 0;
  } catch (err) {
    process.stderr.write(
      `${RED}[basic failed]${RESET} ${err.message}\n` +
        `  session machinery (spawn → initialize → session/new) succeeded; the prompt itself ` +
        `needs a working gateway/auth, and on the current fxrs build a fresh session must be ` +
        `persisted server-side before session/prompt will accept it.\n`,
    );
    return 1;
  } finally {
    await client.close().catch(() => {});
    if (sessionId) {
      // Best-effort: leave the on-disk store tidy if the session persisted.
      try {
        await client.sessionRemove(sessionId);
      } catch {
        /* ignore */
      }
    }
  }
}

process.exitCode = await main();
