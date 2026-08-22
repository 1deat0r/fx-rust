#!/usr/bin/env node
/**
 * Smoke test: spawn the real binary, initialize, create a session, close it.
 * Prints PASS and exits 0 on success; exits 1 with a message on failure.
 *
 * Use:  node tests/test-smoke.mjs
 *       FXRS_BIN=$PWD/target/debug/fxrs node tests/test-smoke.mjs
 */

import { mkdtempSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { FxClient } from "../libfx.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, "..", "..");
const BIN = process.env.FXRS_BIN
  ? resolve(process.env.FXRS_BIN)
  : join(REPO_ROOT, "target", "debug", "fxrs");

if (!existsSync(BIN)) {
  console.error(`[smoke FAIL] fxrs binary not found at ${BIN}`);
  process.exit(1);
}

const home = mkdtempSync(join(tmpdir(), "libfx-smoke-home-"));
process.env.FX_HOME = home;

const client = new FxClient({ bin: BIN, cwd: REPO_ROOT });
try {
  await client.start();
  const init = await client.initialize();
  if (init.protocolVersion !== 1 || init.agentInfo?.name !== "fx") {
    throw new Error(`unexpected initialize result: ${JSON.stringify(init)}`);
  }
  const created = await client.sessionNew({});
  if (!created.sessionId) throw new Error("session/new returned no sessionId");
  const closed = await client.sessionClose(created.sessionId);
  if (closed.sessionId !== created.sessionId) {
    throw new Error(`session/close echoed wrong id: ${JSON.stringify(closed)}`);
  }
  await client.close();
  console.log(`PASS (fxrs ${BIN} · initialize v${init.protocolVersion} · session ${created.sessionId} · close ok)`);
  process.exitCode = 0;
} catch (err) {
  console.error(`[smoke FAIL] ${err.message}`);
  try {
    client.kill();
  } catch {
    /* ignore */
  }
  process.exitCode = 1;
} finally {
  try {
    rmSync(home, { recursive: true, force: true });
  } catch {
    /* ignore */
  }
}
