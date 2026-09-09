import { copyFile, readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { build } from "vite";

await build();
const llmsFull = await readFile(resolve("llms-full.txt"), "utf8");
const llmsSummary = `${llmsFull.split("## Local setup")[0].trim()}\n\nFor setup, SDK usage, REST/admin endpoints, authority, and SQL limitations, read /llms-full.txt.\n`;
await Promise.all([
  writeFile(resolve("dist/llms.txt"), llmsSummary),
  copyFile(resolve("llms-full.txt"), resolve("dist/llms-full.txt")),
]);
