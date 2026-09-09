#!/usr/bin/env node
import { readFile, readdir } from "node:fs/promises";
import { resolve } from "node:path";

type Fetch = typeof fetch;
export interface MigrationFile { version: string; filename: string; sql: string }
export interface CliDependencies {
  fetch?: Fetch;
  env?: Record<string, string | undefined>;
  readFile?: typeof readFile;
  readdir?: typeof readdir;
}

export class CliError extends Error {}

function requireValue(value: string | undefined, label: string): string {
  if (!value) throw new CliError(`${label} is required`);
  return value;
}

function endpoint(baseUrl: string, path: string): URL {
  try { return new URL(`${baseUrl.replace(/\/+$/, "")}${path}`); }
  catch { throw new CliError(`Invalid --url: ${baseUrl}`); }
}

function options(args: string[], supported: readonly string[]): Map<string, string> {
  const result = new Map<string, string>();
  for (let index = 0; index < args.length; index += 2) {
    const flag = args[index]; const value = args[index + 1];
    if (!flag?.startsWith("--") || !supported.includes(flag)) throw new CliError(`Unknown option: ${flag ?? ""}`);
    if (value === undefined || value.startsWith("--")) throw new CliError(`Option ${flag} requires a value`);
    if (result.has(flag)) throw new CliError(`Option ${flag} may only be provided once`);
    result.set(flag, value);
  }
  return result;
}

function headers(secret: string): HeadersInit {
  return { apikey: secret, Authorization: `Bearer ${secret}`, Accept: "application/json", "Content-Type": "application/json" };
}

async function responseJson<T>(fetcher: Fetch, url: URL, init: RequestInit, secret: string): Promise<T> {
  let response: Response;
  try { response = await fetcher(url, { ...init, headers: { ...headers(secret), ...(init.headers ?? {}) } }); }
  catch (cause) { throw new CliError(`Request to ${url.pathname} failed: ${cause instanceof Error ? cause.message : String(cause)}`); }
  const text = await response.text();
  let payload: unknown = null;
  if (text) { try { payload = JSON.parse(text); } catch { payload = text; } }
  if (!response.ok) {
    const message = payload && typeof payload === "object" && typeof (payload as { message?: unknown }).message === "string"
      ? (payload as { message: string }).message : response.statusText || `HTTP ${response.status}`;
    throw new CliError(`${url.pathname} failed (${response.status}): ${message}`);
  }
  return payload as T;
}

export async function loadMigrations(directory: string, read: typeof readFile = readFile, list: typeof readdir = readdir): Promise<MigrationFile[]> {
  let entries: Awaited<ReturnType<typeof readdir>>;
  try { entries = await list(directory, { withFileTypes: true }) as Awaited<ReturnType<typeof readdir>>; }
  catch (cause) { throw new CliError(`Unable to read migrations directory ${directory}: ${cause instanceof Error ? cause.message : String(cause)}`); }
  const migrations: MigrationFile[] = [];
  const seen = new Set<string>();
  for (const entry of entries as unknown as { name: string; isFile(): boolean }[]) {
    if (!entry.isFile()) continue;
    const match = /^(\d+)_([A-Za-z0-9][A-Za-z0-9_-]*)\.sql$/.exec(entry.name);
    if (!match) throw new CliError(`Invalid migration filename: ${entry.name}; expected <timestamp>_<name>.sql`);
    const version = match[1]!;
    if (seen.has(version)) throw new CliError(`Duplicate migration version: ${version}`);
    seen.add(version);
    let sql: string;
    try { sql = await read(resolve(directory, entry.name), "utf8"); }
    catch (cause) { throw new CliError(`Unable to read migration ${entry.name}: ${cause instanceof Error ? cause.message : String(cause)}`); }
    migrations.push({ version, filename: entry.name, sql });
  }
  return migrations.sort((left, right) => left.version.localeCompare(right.version) || left.filename.localeCompare(right.filename));
}

function appliedVersions(payload: unknown): Set<string> {
  if (!payload || typeof payload !== "object" || !Array.isArray((payload as { migrations?: unknown }).migrations)) {
    throw new CliError("Invalid GET /admin/v1/migrations response");
  }
  const versions = new Set<string>();
  for (const migration of (payload as { migrations: unknown[] }).migrations) {
    const version = typeof migration === "string" ? migration : migration && typeof migration === "object" && typeof (migration as { version?: unknown }).version === "string" ? (migration as { version: string }).version : null;
    if (!version) throw new CliError("Invalid migration version in GET /admin/v1/migrations response");
    versions.add(version);
  }
  return versions;
}

export async function migrate(url: string, directory: string, secret: string, dependencies: CliDependencies = {}): Promise<{ applied: string[]; skipped: string[] }> {
  const fetcher = dependencies.fetch ?? globalThis.fetch;
  if (!fetcher) throw new CliError("No fetch implementation is available");
  const migrations = await loadMigrations(directory, dependencies.readFile ?? readFile, dependencies.readdir ?? readdir);
  const applied = appliedVersions(await responseJson<unknown>(fetcher, endpoint(url, "/admin/v1/migrations"), { method: "GET" }, secret));
  const result = { applied: [] as string[], skipped: [] as string[] };
  for (const migration of migrations) {
    if (applied.has(migration.version)) { result.skipped.push(migration.version); continue; }
    await responseJson(fetcher, endpoint(url, "/admin/v1/migrations"), { method: "POST", body: JSON.stringify({ version: migration.version, sql: migration.sql }) }, secret);
    result.applied.push(migration.version);
  }
  return result;
}

export async function sql(url: string, query: string, secret: string, dependencies: CliDependencies = {}): Promise<unknown> {
  const fetcher = dependencies.fetch ?? globalThis.fetch;
  if (!fetcher) throw new CliError("No fetch implementation is available");
  if (!query.trim()) throw new CliError("SQL query must not be empty");
  return responseJson(fetcher, endpoint(url, "/admin/v1/sql"), { method: "POST", body: JSON.stringify({ sql: query }) }, secret);
}

export async function run(argv: string[], dependencies: CliDependencies = {}): Promise<string> {
  const [command, ...args] = argv;
  const env = dependencies.env ?? process.env;
  const secret = requireValue(env.KURA_SECRET_KEY, "KURA_SECRET_KEY environment variable");
  if (command === "migrate") {
    const flags = options(args, ["--url", "--dir"]);
    const result = await migrate(requireValue(flags.get("--url"), "--url"), requireValue(flags.get("--dir"), "--dir"), secret, dependencies);
    return JSON.stringify(result, null, 2);
  }
  if (command === "sql") {
    const flags = options(args, ["--url", "--file", "--query"]);
    const url = requireValue(flags.get("--url"), "--url");
    const file = flags.get("--file"); const query = flags.get("--query");
    if (Boolean(file) === Boolean(query)) throw new CliError("Provide exactly one of --file or --query");
    let source = query;
    if (file) {
      try { source = await (dependencies.readFile ?? readFile)(file, "utf8"); }
      catch (cause) { throw new CliError(`Unable to read SQL file ${file}: ${cause instanceof Error ? cause.message : String(cause)}`); }
    }
    return JSON.stringify(await sql(url, source!, secret, dependencies), null, 2);
  }
  throw new CliError(`Unknown command: ${command ?? ""}. Use migrate or sql.`);
}

async function main(): Promise<void> {
  try { console.log(await run(process.argv.slice(2))); }
  catch (error) { console.error(`kura: ${error instanceof Error ? error.message : String(error)}`); process.exitCode = 1; }
}

if (import.meta.url === `file://${process.argv[1]}`) void main();
