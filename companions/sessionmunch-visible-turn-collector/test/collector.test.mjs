// Synthetic-fixture tests for the visible-turn collector (MUNCH-022).
// Requires CHROME_HEADLESS_SHELL (a local headless Chromium). Optional
// SESSIONMUNCH_BIN runs the emitted NDJSON through the real importer in
// --dry-run mode with an isolated HOME/data dir.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { after, before, test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

import { drain, installCollector, launch, loadProfile, openPage, toNdjson } from "../run.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const FIXTURE = pathToFileURL(join(HERE, "fixtures", "conversation.html")).href;
const CONVERSATION = "c0c0c0c0-0000-4000-8000-00000000c0c0";
const executable = process.env.CHROME_HEADLESS_SHELL;
const profile = loadProfile("fixture");
const work = mkdtempSync(join(tmpdir(), "sm-collector-test-"));

let browser;
before(async () => {
  assert.ok(executable, "set CHROME_HEADLESS_SHELL to a local headless Chromium");
  browser = await launch(executable);
});
after(async () => {
  await browser?.close();
  rmSync(work, { recursive: true, force: true });
});

async function runScenario(url, { steps = true, using = browser } = {}) {
  const page = await openPage(using, url);
  await installCollector(page, profile, "claude");
  const records = await drain(page);
  if (steps) {
    const count = await page.evaluate("window.__fixtureSteps");
    for (let step = 1; step <= count; step += 1) {
      await page.evaluate(`window.__fixtureStep(${step})`);
      records.push(...(await drain(page)));
    }
  }
  return { records, counts: await page.evaluate("globalThis.__smCollector.counts()"), page };
}

const withIds = `${FIXTURE}#c=${CONVERSATION}`;

test("finalized visible turns only, with native ids, edits and regeneration", async () => {
  const { records, counts } = await runScenario(withIds);
  const view = records.map((r) => [r.message_id.slice(-1), r.role, r.text]);
  assert.deepEqual(view, [
    ["1", "user", "fixture question"],
    ["2", "assistant", "fixture answer"],
    ["1", "user", "fixture question (edited)"],
    ["3", "assistant", "fixture answer"],
  ]);
  for (const record of records) {
    assert.deepEqual(Object.keys(record).sort(), [
      "conversation_id", "final", "message_id", "observed_at", "occurred_at",
      "provider", "role", "schema", "surface", "text",
    ]);
    assert.equal(record.schema, "sessionmunch.observed-visible-turn.v1");
    assert.equal(record.conversation_id, CONVERSATION);
    assert.equal(record.final, true);
    assert.equal(record.occurred_at, "2026-11-26T12:00:00.000Z");
  }
  const all = JSON.stringify(records);
  for (const forbidden of ["partial", "hidden reasoning", "tool output"]) {
    assert.ok(!all.includes(forbidden), forbidden);
  }
  assert.equal(counts.emitted, 4);
  assert.equal(counts.non_final, 2, "first answer and regeneration streamed");
  assert.equal(counts.unsupported_role, 1, "tool block");
  assert.equal(counts.no_message_id, 0);
  writeFileSync(join(work, "run1.ndjson"), toNdjson(records));
});

test("browser restart replays the same native identities", async () => {
  const first = await runScenario(withIds);
  // A brand-new browser process and profile: no collector state survives.
  const fresh = await launch(executable);
  let second;
  try {
    second = await runScenario(withIds, { using: fresh });
  } finally {
    await fresh.close();
  }
  const key = (r) => `${r.message_id}|${r.role}|${r.text}`;
  assert.deepEqual(second.records.map(key), first.records.map(key));
  writeFileSync(join(work, "restart.ndjson"), toNdjson([...first.records, ...second.records]));
});

test("fails closed without native message ids", async () => {
  const { records, counts } = await runScenario(`${FIXTURE}?ids=0#c=${CONVERSATION}`);
  assert.equal(records.length, 0);
  assert.equal(counts.emitted, 0);
  assert.ok(counts.no_message_id >= 3, JSON.stringify(counts));
});

test("fails closed without a native conversation id", async () => {
  const { records, counts } = await runScenario(FIXTURE);
  assert.equal(records.length, 0);
  assert.ok(counts.no_conversation_id >= 3, JSON.stringify(counts));
});

test("isolated profile has no outside network", async () => {
  const page = await openPage(browser, withIds);
  const outcome = await page.evaluate(
    "fetch('http://example.com/').then(() => 'reached', (e) => 'blocked: ' + e.name)",
  );
  assert.match(outcome, /^blocked/);
});

test("emitted NDJSON imports with stable identity (dry run)", { skip: !process.env.SESSIONMUNCH_BIN }, () => {
  const home = join(work, "home");
  const env = Object.fromEntries(
    Object.entries(process.env).filter(([k]) => !k.startsWith("SESSIONMUNCH_") && !k.startsWith("AI_MEMORY_")),
  );
  Object.assign(env, {
    HOME: home,
    SESSIONMUNCH_DATA_DIR: join(work, "data"),
    SESSIONMUNCH_IGNORE_MARKER: "1",
  });
  const importFile = (file) => {
    const result = spawnSync(process.env.SESSIONMUNCH_BIN, [
      "import", "chats", "--format", "observed", "--provider", "claude",
      "--account", "fixture", "--workspace", "fixture-history", "--project", "hosted",
      "--dry-run", "--json", join(work, file),
    ], { env, encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr + result.stdout);
    return JSON.parse(result.stdout);
  };
  const once = importFile("run1.ndjson");
  assert.equal(once.harnesses[0].events_queued, 4, JSON.stringify(once));
  assert.deepEqual(once.harnesses[0].losses, []);
  // Two full runs (restart) collapse to the same four revisions.
  const replay = importFile("restart.ndjson");
  assert.equal(replay.harnesses[0].events_queued, 4, JSON.stringify(replay));
  assert.ok(replay.harnesses[0].losses.some((l) => l.includes("4 repeated observations")), JSON.stringify(replay));
});
