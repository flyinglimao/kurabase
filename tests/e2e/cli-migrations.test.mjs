import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { migrate } from "../../packages/cli/dist/index.js";
import { secretKey, startLocal } from "../../scripts/local-stack.mjs";

test("real Anvil: CLI applies ordered migrations once", { timeout: 180000 }, async (t) => {
  const stack = await startLocal({ rpcPort: 38545, gatewayPort: 35432, adminPort: 35433 });
  const directory = await mkdtemp(join(tmpdir(), "kurabase-cli-"));
  t.after(async () => {
    await stack.stop();
    await rm(directory, { recursive: true, force: true });
  });

  await writeFile(join(directory, "202609090101_create_projects.sql"),
    "CREATE TABLE projects (id integer PRIMARY KEY, name text NOT NULL); INSERT INTO projects VALUES (1, 'Kurabase');");
  await writeFile(join(directory, "202609090102_add_status.sql"),
    "ALTER TABLE projects ADD COLUMN status text DEFAULT 'draft';");

  const first = await migrate(stack.adminUrl, directory, secretKey);
  assert.deepEqual(first, { applied: ["202609090101", "202609090102"], skipped: [] });

  const second = await migrate(stack.adminUrl, directory, secretKey);
  assert.deepEqual(second, { applied: [], skipped: ["202609090101", "202609090102"] });

  const response = await fetch(`${stack.adminUrl}/admin/v1/sql`, {
    method: "POST",
    headers: { apikey: secretKey, Authorization: `Bearer ${secretKey}`, "Content-Type": "application/json" },
    body: JSON.stringify({ sql: "SELECT id, name, status FROM projects" }),
  });
  assert.equal(response.status, 200);
  const payload = await response.json();
  assert.deepEqual(payload.rows, [{ id: 1, name: "Kurabase", status: "draft" }]);
});
