/**
 * A deliberately small client for Kurabase's PostgREST-compatible surface.
 * It has no dependency on supabase-js and works in browsers and Node 18+.
 */

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json | undefined };
export type Row = Record<string, unknown>;

export interface KurabaseError {
  message: string;
  details: string | null;
  hint: string | null;
  /** Stable, gateway-defined error identifier when supplied by the server. */
  code: string | null;
}

export interface KurabaseResponse<T> {
  data: T | null;
  error: KurabaseError | null;
  count: number | null;
  status: number;
  statusText: string;
}

export interface TableDefinition<R extends Row = Row, I = Partial<R>, U = Partial<R>> {
  Row: R;
  Insert: I;
  Update: U;
}

export type DatabaseDefinition = Record<string, TableDefinition>;
type RowFor<Db extends DatabaseDefinition, Name extends keyof Db & string> = Db[Name]["Row"];
type InsertFor<Db extends DatabaseDefinition, Name extends keyof Db & string> = Db[Name]["Insert"];
type UpdateFor<Db extends DatabaseDefinition, Name extends keyof Db & string> = Db[Name]["Update"];

export interface ClientOptions {
  /** Override fetch; useful for SSR, testing, or a custom transport. */
  fetch?: typeof fetch;
  headers?: HeadersInit;
  schema?: string;
}

/** A wallet-signed envelope. Signing is deliberately outside this SDK. */
export interface SignedSessionEnvelope {
  session: {
    gateway: string;
    user: string;
    uid: string;
    claimsHash: string;
    expiresAt: string;
    nonce: string;
    gatewayEpoch: string;
  };
  signature: string;
  claims: Record<string, unknown> | null;
}

export interface SessionToken {
  access_token: string;
  token_type: "bearer" | string;
  user: { id: string };
  expires_at: string;
}

export interface SessionExchangeOptions { signal?: AbortSignal }

export interface SelectOptions { count?: "exact" | "planned" | "estimated"; head?: boolean }
export interface MutationOptions { count?: "exact" | "planned" | "estimated" }
export interface InsertOptions extends MutationOptions { defaultToNull?: boolean }
export interface UpsertOptions extends InsertOptions { onConflict?: string; ignoreDuplicates?: boolean }
export interface OrderOptions { ascending?: boolean; nullsFirst?: boolean; referencedTable?: string }
export interface RelationOptions { referencedTable?: string }
export interface RpcOptions extends SelectOptions { get?: boolean }

type Method = "GET" | "HEAD" | "POST" | "PATCH" | "DELETE";
type Cardinality = "many" | "single" | "maybeSingle";
interface RequestState {
  method: Method;
  path: string;
  params: URLSearchParams;
  headers: Headers;
  body?: unknown;
  cardinality: Cardinality;
  count?: SelectOptions["count"];
  signal?: AbortSignal;
}

const JSON_HEADERS = { Accept: "application/json", "Content-Type": "application/json" };
const CLIENT_CARDINALITY_CODE = "KURA_CARDINALITY";
const CLIENT_ABORT_CODE = "KURA_ABORT";
const CLIENT_SERIALIZE_CODE = "KURA_SERIALIZE";

function copyState(state: RequestState): RequestState {
  return {
    ...state,
    params: new URLSearchParams(state.params),
    headers: new Headers(state.headers),
  };
}

function appendPrefer(headers: Headers, value: string): void {
  const existing = headers.get("Prefer");
  const parts = existing ? existing.split(",").map((part) => part.trim()).filter(Boolean) : [];
  if (!parts.includes(value)) parts.push(value);
  headers.set("Prefer", parts.join(","));
}

function encodeFilterValue(value: unknown): string {
  if (value === null) return "null";
  if (typeof value === "boolean" || typeof value === "number" || typeof value === "bigint") return String(value);
  if (typeof value === "string") return value;
  return stringifyJson(value);
}

/** JSON has no bigint type; Kurabase sends bigint inputs as base-10 strings. */
function stringifyJson(value: unknown): string {
  return JSON.stringify(value, (_key, nested) => typeof nested === "bigint" ? nested.toString(10) : nested);
}

function inValue(values: readonly unknown[]): string {
  return `(${values.map((value) => {
    const text = encodeFilterValue(value);
    return /[(),\s]/.test(text) ? `"${text.replace(/"/g, '\\"')}"` : text;
  }).join(",")})`;
}

function parseCount(response: Response): number | null {
  const range = response.headers.get("content-range");
  if (!range) return null;
  const total = range.match(/\/(\d+)$/)?.[1];
  return total === undefined ? null : Number(total);
}

function errorFromBody(body: unknown, fallback: string): KurabaseError {
  if (body && typeof body === "object") {
    const object = body as Record<string, unknown>;
    return {
      message: typeof object.message === "string" ? object.message : fallback,
      details: typeof object.details === "string" ? object.details : null,
      hint: typeof object.hint === "string" ? object.hint : null,
      code: typeof object.code === "string" ? object.code : null,
    };
  }
  return { message: typeof body === "string" && body ? body : fallback, details: null, hint: null, code: null };
}

async function parseBody(response: Response): Promise<unknown> {
  if (response.status === 204 || response.headers.get("content-length") === "0") return null;
  const text = await response.text();
  if (!text) return null;
  try { return JSON.parse(text); } catch { return text; }
}

function cardinalityError(kind: Cardinality, rows: unknown[]): KurabaseError {
  const expected = kind === "single" ? "exactly one row" : "zero or one row";
  return {
    message: `Expected ${expected}, received ${rows.length} rows`,
    details: null,
    hint: null,
    code: CLIENT_CARDINALITY_CODE,
  };
}

export class QueryBuilder<T extends Row = Row, Result = T[]> implements PromiseLike<KurabaseResponse<Result>> {
  constructor(protected readonly client: KurabaseClient<any>, protected readonly state: RequestState) {}

  next(change: (state: RequestState) => void): QueryBuilder<T, Result> {
    const state = copyState(this.state);
    change(state);
    return new QueryBuilder<T, Result>(this.client, state);
  }

  /** Request columns or a PostgREST relation projection. Mutations use this as RETURNING. */
  select<Selected extends Row = T>(columns = "*", options: SelectOptions = {}): QueryBuilder<T, Selected[]> {
    const state = copyState(this.state);
    state.params.set("select", columns);
    state.count = options.count ?? state.count;
    if (state.method === "POST" || state.method === "PATCH" || state.method === "DELETE") {
      appendPrefer(state.headers, "return=representation");
    } else {
      state.method = options.head ? "HEAD" : "GET";
    }
    return new QueryBuilder<T, Selected[]>(this.client, state);
  }

  eq(column: string, value: unknown): QueryBuilder<T, Result> { return this.filter(column, "eq", value); }
  neq(column: string, value: unknown): QueryBuilder<T, Result> { return this.filter(column, "neq", value); }
  gt(column: string, value: unknown): QueryBuilder<T, Result> { return this.filter(column, "gt", value); }
  gte(column: string, value: unknown): QueryBuilder<T, Result> { return this.filter(column, "gte", value); }
  lt(column: string, value: unknown): QueryBuilder<T, Result> { return this.filter(column, "lt", value); }
  lte(column: string, value: unknown): QueryBuilder<T, Result> { return this.filter(column, "lte", value); }
  is(column: string, value: null | boolean): QueryBuilder<T, Result> { return this.filter(column, "is", value); }
  like(column: string, pattern: string): QueryBuilder<T, Result> { return this.filter(column, "like", pattern); }
  ilike(column: string, pattern: string): QueryBuilder<T, Result> { return this.filter(column, "ilike", pattern); }
  contains(column: string, value: unknown): QueryBuilder<T, Result> { return this.filter(column, "cs", value); }
  containedBy(column: string, value: unknown): QueryBuilder<T, Result> { return this.filter(column, "cd", value); }
  overlaps(column: string, value: unknown): QueryBuilder<T, Result> { return this.filter(column, "ov", value); }

  filter(column: string, operator: string, value: unknown): QueryBuilder<T, Result> {
    return this.next((state) => state.params.append(column, `${operator}.${encodeFilterValue(value)}`));
  }

  not(column: string, operator: string, value: unknown): QueryBuilder<T, Result> {
    return this.next((state) => state.params.append(column, `not.${operator}.${encodeFilterValue(value)}`));
  }

  in(column: string, values: readonly unknown[]): QueryBuilder<T, Result> {
    return this.next((state) => state.params.append(column, `in.${inValue(values)}`));
  }

  match(values: Record<string, unknown>): QueryBuilder<T, Result> {
    let query: QueryBuilder<T, Result> = this;
    for (const [column, value] of Object.entries(values)) query = query.eq(column, value);
    return query;
  }

  or(filters: string, options: RelationOptions = {}): QueryBuilder<T, Result> {
    const key = options.referencedTable ? `${options.referencedTable}.or` : "or";
    return this.next((state) => state.params.append(key, `(${filters})`));
  }

  order(column: string, options: OrderOptions = {}): QueryBuilder<T, Result> {
    const key = options.referencedTable ? `${options.referencedTable}.order` : "order";
    const direction = options.ascending === false ? "desc" : "asc";
    const nulls = options.nullsFirst === undefined ? "" : options.nullsFirst ? ".nullsfirst" : ".nullslast";
    return this.next((state) => state.params.append(key, `${column}.${direction}${nulls}`));
  }

  limit(count: number, options: RelationOptions = {}): QueryBuilder<T, Result> {
    const key = options.referencedTable ? `${options.referencedTable}.limit` : "limit";
    return this.next((state) => state.params.set(key, String(count)));
  }

  /** 0-based and inclusive at both ends. */
  range(from: number, to: number, options: RelationOptions = {}): QueryBuilder<T, Result> {
    if (!Number.isInteger(from) || !Number.isInteger(to) || from < 0 || to < from) {
      throw new RangeError("range expects non-negative inclusive integer bounds");
    }
    return this.next((state) => {
      if (options.referencedTable) {
        state.params.set(`${options.referencedTable}.offset`, String(from));
        state.params.set(`${options.referencedTable}.limit`, String(to - from + 1));
      } else {
        state.headers.set("Range-Unit", "items");
        state.headers.set("Range", `${from}-${to}`);
      }
    });
  }

  /** Attach an AbortSignal without mutating the query this builder was derived from. */
  abortSignal(signal: AbortSignal): QueryBuilder<T, Result> {
    return this.next((state) => { state.signal = signal; });
  }

  single(): QueryBuilder<T, T> { return this.withCardinality("single"); }
  maybeSingle(): QueryBuilder<T, T | null> { return this.withCardinality("maybeSingle"); }

  private withCardinality(kind: Cardinality): QueryBuilder<T, any> {
    const state = copyState(this.state);
    state.cardinality = kind;
    // Kurabase understands the PostgREST object media type. Client-side validation
    // below also covers gateways that return an array for this media type.
    state.headers.set("Accept", "application/vnd.pgrst.object+json");
    return new QueryBuilder<T, any>(this.client, state);
  }

  then<TResult1 = KurabaseResponse<Result>, TResult2 = never>(
    onfulfilled?: ((value: KurabaseResponse<Result>) => TResult1 | PromiseLike<TResult1>) | null,
    onrejected?: ((reason: unknown) => TResult2 | PromiseLike<TResult2>) | null,
  ): Promise<TResult1 | TResult2> {
    return this.execute().then(onfulfilled ?? undefined, onrejected ?? undefined);
  }

  async execute(): Promise<KurabaseResponse<Result>> { return this.client.execute<Result>(this.state); }
}

export class KurabaseClient<Db extends DatabaseDefinition = DatabaseDefinition> {
  readonly admin: KurabaseAdmin;
  readonly auth: KurabaseAuth;
  private readonly baseUrl: string;
  private readonly apiKey: string;
  private readonly fetcher: typeof fetch | undefined;
  private readonly headers: Headers;
  private readonly schema?: string;
  private accessToken: string | null = null;

  constructor(url: string, key: string, options: ClientOptions = {}) {
    this.baseUrl = url.replace(/\/+$/, "");
    this.apiKey = key;
    // Browser fetch is a Web IDL method: extracting it and later invoking it
    // as `this.fetcher(...)` can throw "Illegal invocation". Keep the supplied
    // transport untouched, but bind the runtime default to its global receiver.
    this.fetcher = options.fetch ?? (typeof globalThis.fetch === "function" ? globalThis.fetch.bind(globalThis) : undefined);
    this.headers = new Headers(options.headers);
    this.schema = options.schema;
    this.admin = new KurabaseAdmin(this);
    this.auth = new KurabaseAuth(this);
  }

  from<Name extends keyof Db & string>(table: Name): TableQueryBuilder<RowFor<Db, Name>, InsertFor<Db, Name>, UpdateFor<Db, Name>>;
  from(table: string): TableQueryBuilder<Row, Partial<Row>, Partial<Row>>;
  from(table: string): TableQueryBuilder<any, any, any> {
    return new TableQueryBuilder(this, this.initialState("GET", `/rest/v1/${encodeURIComponent(table)}`));
  }

  rpc<T extends Row = Row>(fn: string, args: Record<string, unknown> = {}, options: RpcOptions = {}): QueryBuilder<T, T[]> {
    const state = this.initialState(options.get ? "GET" : "POST", `/rest/v1/rpc/${encodeURIComponent(fn)}`);
    state.count = options.count;
    if (options.get) Object.entries(args).forEach(([key, value]) => state.params.set(key, encodeFilterValue(value)));
    else state.body = args;
    if (options.head) state.method = "HEAD";
    return new QueryBuilder<T, T[]>(this, state);
  }

  private initialState(method: Method, path: string): RequestState {
    const headers = new Headers(this.headers);
    headers.set("apikey", this.apiKey);
    if (this.accessToken) headers.set("Authorization", `Bearer ${this.accessToken}`);
    else if (!headers.has("Authorization")) headers.set("Authorization", `Bearer ${this.apiKey}`);
    headers.set("Accept", headers.get("Accept") ?? JSON_HEADERS.Accept);
    if (this.schema) {
      headers.set(method === "GET" || method === "HEAD" ? "Accept-Profile" : "Content-Profile", this.schema);
    }
    return { method, path, params: new URLSearchParams(), headers, cardinality: "many" };
  }

  async execute<T>(state: RequestState): Promise<KurabaseResponse<T>> {
    if (!this.fetcher) return this.fetchError<T>("No fetch implementation is available; pass options.fetch in this runtime.");
    try {
      const url = new URL(`${this.baseUrl}${state.path}`);
      state.params.forEach((value, key) => url.searchParams.append(key, value));
      const headers = new Headers(state.headers);
      if (state.count) appendPrefer(headers, `count=${state.count}`);
      const init: RequestInit = { method: state.method, headers, signal: state.signal };
      if (state.body !== undefined) {
        headers.set("Content-Type", JSON_HEADERS["Content-Type"]);
        try { init.body = stringifyJson(state.body); }
        catch (error) { return this.requestError<T>(error instanceof Error ? error.message : String(error), CLIENT_SERIALIZE_CODE, "SerializeError"); }
      }
      const response = await this.fetcher(url, init);
      const count = parseCount(response);
      const body = await parseBody(response);
      if (!response.ok) {
        return { data: null, error: errorFromBody(body, response.statusText || `HTTP ${response.status}`), count, status: response.status, statusText: response.statusText };
      }
      if (state.method === "HEAD") return { data: null, error: null, count, status: response.status, statusText: response.statusText };
      if (state.cardinality !== "many" && Array.isArray(body)) {
        if (body.length === 1) return { data: body[0] as T, error: null, count, status: response.status, statusText: response.statusText };
        if (state.cardinality === "maybeSingle" && body.length === 0) return { data: null, error: null, count, status: response.status, statusText: response.statusText };
        return { data: null, error: cardinalityError(state.cardinality, body), count, status: response.status, statusText: response.statusText };
      }
      return { data: body as T, error: null, count, status: response.status, statusText: response.statusText };
    } catch (error) {
      if (state.signal?.aborted || (error instanceof Error && error.name === "AbortError")) {
        return this.requestError<T>("The request was aborted.", CLIENT_ABORT_CODE, "AbortError");
      }
      return this.fetchError<T>(error instanceof Error ? error.message : String(error));
    }
  }

  private fetchError<T>(message: string): KurabaseResponse<T> {
    return { data: null, error: { message, details: null, hint: null, code: "KURA_FETCH" }, count: null, status: 0, statusText: "FetchError" };
  }

  private requestError<T>(message: string, code: string, statusText: string): KurabaseResponse<T> {
    return { data: null, error: { message, details: null, hint: null, code }, count: null, status: 0, statusText };
  }

  /** Set or clear the token used by subsequent requests from this client instance. */
  setAccessToken(token: string | null): void { this.accessToken = token; }

  /** @internal Used by KurabaseAdmin. */
  async requestAdmin<T>(path: string, method: Method, body?: unknown): Promise<KurabaseResponse<T>> {
    const state = this.initialState(method, path);
    state.body = body;
    return this.execute<T>(state);
  }

  /** @internal Used by KurabaseAuth. */
  async requestAuth<T>(path: string, body: unknown, signal?: AbortSignal): Promise<KurabaseResponse<T>> {
    const state = this.initialState("POST", path);
    // Session exchange is authenticated by the publishable key and envelope, not
    // an earlier bearer token that might be expired or belong to another wallet.
    state.headers.delete("Authorization");
    state.body = body;
    state.signal = signal;
    return this.execute<T>(state);
  }
}

export class TableQueryBuilder<T extends Row, Insert, Update> extends QueryBuilder<T, T[]> {
  constructor(client: KurabaseClient<any>, state: RequestState) { super(client, state); }

  insert(values: Insert | readonly Insert[], options: InsertOptions = {}): QueryBuilder<T, T[]> {
    return this.mutate("POST", values, options, options.defaultToNull === false ? "missing=default" : undefined);
  }

  update(values: Update, options: MutationOptions = {}): QueryBuilder<T, T[]> {
    return this.mutate("PATCH", values, options);
  }

  delete(options: MutationOptions = {}): QueryBuilder<T, T[]> {
    return this.mutate("DELETE", undefined, options);
  }

  upsert(values: Insert | readonly Insert[], options: UpsertOptions = {}): QueryBuilder<T, T[]> {
    const query = this.mutate("POST", values, options, options.ignoreDuplicates ? "resolution=ignore-duplicates" : "resolution=merge-duplicates");
    return options.onConflict ? query.next((state) => state.params.set("on_conflict", options.onConflict!)) : query;
  }

  private mutate(method: Method, body: unknown, options: MutationOptions, prefer?: string): QueryBuilder<T, T[]> {
    const state = copyState(this.state);
    state.method = method;
    state.body = body;
    state.count = options.count;
    const readSchema = state.headers.get("Accept-Profile");
    if (readSchema) {
      state.headers.delete("Accept-Profile");
      state.headers.set("Content-Profile", readSchema);
    }
    // Default mutation output is minimal, as required by the SDK contract.
    appendPrefer(state.headers, "return=minimal");
    if (prefer) appendPrefer(state.headers, prefer);
    return new QueryBuilder<T, T[]>(this.client, state);
  }
}

export interface Migration { version: string; name?: string; sql?: string }
export interface MigrationEnvelope { migrations: Array<string | Migration> }
export interface SqlResult {
  rows: unknown[];
  affected: number;
  block_number: number | string;
  revision: number | string;
  transaction_hash: string | null;
  [key: string]: unknown;
}
export interface InstanceMetadata { [key: string]: unknown }
export interface CatalogColumn { id?: string | number; name: string; data_type?: string; nullable?: boolean; [key: string]: unknown }
export interface CatalogTable {
  id?: string | number;
  name: string;
  schema?: string;
  columns?: CatalogColumn[];
  primary_key?: unknown;
  foreign_keys?: unknown[];
  policies?: CatalogPolicy[];
  rls_enabled?: boolean;
  [key: string]: unknown;
}
export interface CatalogPolicy { name?: string; table?: string; [key: string]: unknown }
/** Gateway catalog response. `catalog` is keyed by table name. Legacy flat fields are accepted for interoperability. */
export interface Catalog {
  catalog?: Record<string, CatalogTable>;
  migrations?: Migration[];
  tables?: CatalogTable[];
  policies?: CatalogPolicy[];
  [key: string]: unknown;
}

/**
 * Session convenience methods. They never sign a message or accept a private key:
 * obtain a wallet signature in the host application, then exchange its envelope.
 */
export class KurabaseAuth {
  constructor(private readonly client: KurabaseClient<any>) {}

  /** Use an existing bearer token for subsequent requests from this client. */
  setAccessToken(accessToken: string | null): void {
    this.client.setAccessToken(accessToken);
  }

  /** Exchange a pre-signed wallet session at POST /auth/v1/session. */
  async exchangeSession(
    envelope: SignedSessionEnvelope,
    options: SessionExchangeOptions = {},
  ): Promise<KurabaseResponse<SessionToken>> {
    const response = await this.client.requestAuth<SessionToken>("/auth/v1/session", envelope, options.signal);
    if (response.data?.access_token && !response.error) this.client.setAccessToken(response.data.access_token);
    return response;
  }
}

/** Instance/catalog introspection accepts project access; SQL and migrations require an admin secret. */
export class KurabaseAdmin {
  constructor(private readonly client: KurabaseClient<any>) {}

  instance(): Promise<KurabaseResponse<InstanceMetadata>> {
    return this.client.requestAdmin<InstanceMetadata>("/admin/v1/instance", "GET");
  }

  catalog(): Promise<KurabaseResponse<Catalog>> {
    return this.client.requestAdmin<Catalog>("/admin/v1/catalog", "GET");
  }

  sql<T = SqlResult>(sql: string, params?: readonly unknown[]): Promise<KurabaseResponse<T>> {
    return this.client.requestAdmin<T>("/admin/v1/sql", "POST", { sql, ...(params ? { params } : {}) });
  }

  migrations = {
    /** Normalizes the gateway's `{ migrations: [version] }` envelope to migration records. */
    list: async (): Promise<KurabaseResponse<Migration[]>> => {
      const response = await this.client.requestAdmin<MigrationEnvelope>("/admin/v1/migrations", "GET");
      if (response.error || !response.data) {
        return {
          data: null,
          error: response.error,
          count: response.count,
          status: response.status,
          statusText: response.statusText,
        };
      }
      if (!Array.isArray(response.data.migrations)) {
        return {
          data: null,
          error: { message: "Invalid migrations response", details: null, hint: null, code: "KURA_RESPONSE" },
          count: response.count,
          status: response.status,
          statusText: response.statusText,
        };
      }
      return {
        ...response,
        data: response.data.migrations.map((migration) => typeof migration === "string" ? { version: migration } : migration),
      };
    },
    apply: (migration: Migration): Promise<KurabaseResponse<Migration>> => this.client.requestAdmin<Migration>("/admin/v1/migrations", "POST", migration),
  };
}

export function createClient<Db extends DatabaseDefinition = DatabaseDefinition>(url: string, key: string, options: ClientOptions = {}): KurabaseClient<Db> {
  return new KurabaseClient<Db>(url, key, options);
}
