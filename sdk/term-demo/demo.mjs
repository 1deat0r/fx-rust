#!/usr/bin/env node
/**
 * term-demo — a terminal demo for libfx (stand-in for upstream's sdk/term-demo
 * and xterm adapter). Runs a one-shot prompt through the SDK and renders the
 * transcript with ANSI styling.
 *
 * Usage:
 *   node demo.mjs "<prompt>"
 *   node demo.mjs --bin /abs/path/to/fxrs "<prompt>"
 *   echo "..." | node demo.mjs --bin ./target/debug/fxrs
 */

import { promisify } from "node:util";
import { readFileSync } from "node:fs";

import { prompt } from "../libfx.mjs";

const BOLD = "\x1b[1m";
const DIM = "\x1b[2m";
const RESET = "\x1b[0m";
const RED = "\x1b[31m";
const GREEN = "\x1b[32m";
const BLUE = "\x1b[34m";
const CYAN = "\x1b[36m";

function parseArgs(argv) {
  const opts = { bin: process.env.FXRS_BIN, prompt: "" };
  const rest = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--bin") {
      opts.bin = argv[++i];
    } else if (a === "--help" || a === "-h") {
      opts.help = true;
    } else {
      rest.push(a);
    }
  }
  let text = rest.join(" ");
  if (!text && !process.stdin.isTTY) {
    try {
      text = readFileSync(0, "utf8").trim();
    } catch {
      text = "";
    }
  }
  opts.prompt = text;
  return opts;
}

function renderEvent(ev) {
  const update = ev.sessionUpdate;
  switch (update) {
    case "agent_message_chunk": {
      const text = ev.content?.text ?? "";
      process.stdout.write(text);
      return "";
    }
    case "user_message_chunk": {
      const text = ev.content?.text ?? "";
      return `${BOLD}${BLUE}> ${text}${RESET}\n`;
    }
    case "tool_call": {
      return `${DIM}⎿ ${ev.title ?? ev.toolCallId}${RESET}\n`;
    }
    case "tool_call_update": {
      if (ev.content) {
        return `${DIM}⎿ ${ev.toolCallId}: ${JSON.stringify(ev.content).slice(0, 160)}${RESET}\n`;
      }
      return "";
    }
    case "available_commands_update":
      return "";
    default:
      return "";
  }
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.help || !opts.prompt) {
    console.log(`Usage: node demo.mjs [--bin /path/to/fxrs] "<prompt>"
  A terminal demo of the libfx SDK. Streams a one-shot agent reply.`);
    process.exit(opts.help ? 0 : 1);
  }
  console.log(`${DIM}libfx term-demo — calling ${opts.bin || "fxrs"} acp...${RESET}\n`);
  try {
    const result = await prompt({
      text: opts.prompt,
      bin: opts.bin,
      onEvent: (ev) => process.stdout.write(renderEvent(ev)),
    });
    process.stdout.write("\n");
    console.log(
      `${RESET}${GREEN}[done]${RESET} session ${result.sessionId} · stopReason ${result.stopReason}`,
    );
    process.exit(0);
  } catch (err) {
    console.error(`${RED}libfx demo failed: ${err.message}${RESET}`);
    process.exit(1);
  }
}

main();
