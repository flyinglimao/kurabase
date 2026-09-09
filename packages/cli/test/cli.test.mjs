import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { CliError, loadMigrations, migrate, run, sql } from "../dist/index.js";

function json(value, status = 200) { return new Response(JSON.stringify(value), { status, headers: { "content-type": "application/json" } }); }
async function directory(files) {
  const path = await mkdtemp(join(tmpdir(), "kurabase-cli-"));
  await Promise.all(Object.entries(files).map(([name, content]) => writeFile(join(path, name), content)));
  return path;
}

test("migrate sorts, skips applied versions, and sends exact JSON bodies", async () => {
  const dir = await directory({ "202609090002_second.sql": "select 2;", "202609090001_first.sql": "select 1;" });
  const calls = [];
  const fetch = async (url, init) => {
    calls.push([new URL(String(url)), init]);
    return init.method === "GET" ? json({ migrations: ["202609090001"] }) : json({ migrations: [JSON.parse(init.body).version] }, 201);
  };
  try {
    const result = await migrate("http://gateway.test/", dir, "secret-value", { fetch });
    assert.deepEqual(result, { applied: ["202609090002"], skipped: ["202609090001"] });
    assert.equal(calls[0][0].pathname, "/admin/v1/migrations");
    assert.equal(new Headers(calls[0][1].headers).get("Authorization"), "Bearer secret-value");
    assert.deepEqual(JSON.parse(calls[1][1].body), { version: "202609090002", sql: "select 2;" });
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test("migration discovery rejects duplicate versions and invalid filenames", async () => {
  const duplicates = await directory({ "1_first.sql": "select 1", "1_second.sql": "select 2" });
  const invalid = await directory({ "not-a-migration.txt": "nope" });
  try {
    await assert.rejects(() => loadMigrations(duplicates), CliError);
    await assert.rejects(() => loadMigrations(invalid), /Invalid migration filename/);
  } finally { await Promise.all([rm(duplicates, { recursive: true, force: true }), rm(invalid, { recursive: true, force: true })]); }
});

test("sql accepts a supplied query and CLI validates command/file options", async () => {
  const calls = [];
  const fetch = async (url, init) => { calls.push([new URL(String(url)), init]); return json({ rows: [{ value: 1 }], affected: 0, block_number: 2, revision: 3, transaction_hash: null }); };
  assert.deepEqual(await sql("http://gateway.test", "select 1", "secret", { fetch }), { rows: [{ value: 1 }], affected: 0, block_number: 2, revision: 3, transaction_hash: null });
  assert.equal(calls[0][0].pathname, "/admin/v1/sql");
  assert.deepEqual(JSON.parse(calls[0][1].body), { sql: "select 1" });
  await assert.rejects(() => run(["sql", "--url", "http://gateway.test"], { env: { KURA_SECRET_KEY: "secret" }, fetch }), /exactly one/);
});
