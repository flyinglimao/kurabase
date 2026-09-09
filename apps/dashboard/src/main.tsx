import { createClient, type Catalog, type CatalogPolicy, type CatalogTable, type InstanceMetadata, type KurabaseResponse, type Migration } from "@kurabase/js";
import { useEffect, useMemo, useState, type FormEvent } from "react";
import { createRoot } from "react-dom/client";
import "./styles.css";

type Connection = { url: string; publishableKey: string; sessionToken: string; secretKey: string };
type ConnectionResult = { instance: InstanceMetadata; catalog: Catalog };

const SAVED_CONNECTION = "kurabase.console.connection.v1";
const emptyCatalog: Catalog = { catalog: {}, migrations: [] };

function catalogTables(catalog: Catalog): CatalogTable[] {
  return catalog.tables ?? Object.values(catalog.catalog ?? {});
}

function catalogPolicies(catalog: Catalog): CatalogPolicy[] {
  return catalog.policies ?? catalogTables(catalog).flatMap((table) =>
    (table.policies ?? []).map((policy) => ({ ...policy, table: policy.table ?? table.name })),
  );
}

function persistedConnection(): Partial<Pick<Connection, "url" | "publishableKey">> {
  try {
    return JSON.parse(localStorage.getItem(SAVED_CONNECTION) ?? "{}") as Partial<Pick<Connection, "url" | "publishableKey">>;
  } catch { return {}; }
}

function clientFor(connection: Connection, useSecret = false) {
  const key = useSecret && connection.secretKey ? connection.secretKey : connection.publishableKey;
  const client = createClient(connection.url, key);
  // A gateway-validated bearer token comes from a pre-signed wallet or configured
  // identity authority session. It remains in this in-memory client only.
  if (connection.sessionToken && !(useSecret && connection.secretKey)) client.auth.setAccessToken(connection.sessionToken);
  return client;
}

function responseError(response: KurabaseResponse<unknown>): string {
  return response.error?.message ?? `${response.status} ${response.statusText}`;
}

function App() {
  const saved = persistedConnection();
  const [connection, setConnection] = useState<Connection>({
    url: saved.url ?? "http://127.0.0.1:54322",
    publishableKey: saved.publishableKey ?? "",
    sessionToken: "",
    secretKey: "",
  });
  const [result, setResult] = useState<ConnectionResult | null>(null);
  const [connectionError, setConnectionError] = useState<string | null>(null);
  const [connecting, setConnecting] = useState(false);
  const [active, setActive] = useState("overview");
  const [selectedTable, setSelectedTable] = useState<string | null>(null);
  const [sql, setSql] = useState("select * from posts limit 25;");
  const [sqlResult, setSqlResult] = useState<KurabaseResponse<unknown> | null>(null);
  const [migration, setMigration] = useState({ version: "", sql: "" });
  const [migrationResult, setMigrationResult] = useState<KurabaseResponse<Migration> | null>(null);

  const connected = result !== null;
  const tables = result ? catalogTables(result.catalog) : catalogTables(emptyCatalog);
  const policies = result ? catalogPolicies(result.catalog) : catalogPolicies(emptyCatalog);

  useEffect(() => {
    if (selectedTable || tables.length === 0) return;
    setSelectedTable(String(tables[0].name));
  }, [tables, selectedTable]);

  async function connect(event: FormEvent) {
    event.preventDefault();
    setConnecting(true);
    setConnectionError(null);
    setResult(null);
    const client = clientFor(connection, Boolean(connection.secretKey));
    const [instance, catalog] = await Promise.all([client.admin.instance(), client.admin.catalog()]);
    setConnecting(false);
    if (instance.error || catalog.error || !instance.data || !catalog.data) {
      setConnectionError(responseError(instance.error ? instance : catalog));
      return;
    }
    localStorage.setItem(SAVED_CONNECTION, JSON.stringify({
      url: connection.url, publishableKey: connection.publishableKey,
    }));
    setResult({ instance: instance.data, catalog: catalog.data });
  }

  const nav = ["overview", "tables", "sql", "migrations", "policies", "delegation"];
  return <main className="shell">
    <aside className="sidebar">
      <a className="brand" href="#overview" onClick={() => setActive("overview")}><span className="brand-mark">K</span> Kurabase</a>
      <p className="eyebrow">Developer console</p>
      <nav>{nav.map((item) => <button key={item} className={active === item ? "nav-active" : ""} onClick={() => setActive(item)}>{item}</button>)}</nav>
      <div className="side-note"><span className={connected ? "dot online" : "dot"}></span>{connected ? "Gateway connected" : "Not connected"}</div>
    </aside>
    <section className="workspace">
      <header><div><p className="eyebrow">CHAIN-BACKED RELATIONAL DATA</p><h1>{active === "overview" ? "Instance" : active[0].toUpperCase() + active.slice(1)}</h1></div><span className="status">{connected ? "Live session" : "Local setup"}</span>{connected && <button onClick={() => { setResult(null); setSelectedTable(null); setSqlResult(null); }}>Change instance</button>}</header>

      {!connected && <section className="panel connect-panel" id="overview">
        <div><p className="kicker">Connect an instance</p><h2>Your database, ready to build on.</h2><p>Browse tables, run SQL, and manage migrations. Add a secret key for administrative operations; it stays in this tab until refresh.</p></div>
        <form onSubmit={connect}>
          <Field label="Gateway URL" value={connection.url} onChange={(url) => setConnection({ ...connection, url })} placeholder="http://127.0.0.1:54322" required />
          <Field label="Publishable key" value={connection.publishableKey} onChange={(publishableKey) => setConnection({ ...connection, publishableKey })} required />
          <Field label="Wallet or identity session token (optional, memory only)" value={connection.sessionToken} onChange={(sessionToken) => setConnection({ ...connection, sessionToken })} type="password" />
          <Field label="Secret server key (memory only)" value={connection.secretKey} onChange={(secretKey) => setConnection({ ...connection, secretKey })} type="password" />
          {connectionError && <p className="error">Could not connect: {connectionError}</p>}
          <button className="primary" disabled={connecting}>{connecting ? "Checking gateway…" : "Connect gateway"}</button>
        </form>
      </section>}

      {connected && active === "overview" && <Overview instance={result.instance} catalog={result.catalog} />}
      {connected && active === "tables" && <TableBrowser tables={tables} selected={selectedTable} setSelected={setSelectedTable} connection={connection} />}
      {connected && active === "sql" && <section className="panel editor"><p className="kicker">Privileged SQL endpoint</p><textarea value={sql} onChange={(event) => setSql(event.target.value)} spellCheck={false} />
        <div className="editor-actions"><span>Changes commit atomically to your instance.</span><button className="primary" onClick={async () => setSqlResult(await clientFor(connection, true).admin.sql(sql))}>Run SQL</button></div>
        {sqlResult && <ResponseView response={sqlResult} />}</section>}
      {connected && active === "migrations" && <Migrations connection={connection} migration={migration} setMigration={setMigration} result={migrationResult} setResult={setMigrationResult} />}
      {connected && active === "policies" && <Policies policies={policies} />}
      {connected && active === "delegation" && <Delegation instance={result.instance} hasSecret={Boolean(connection.secretKey)} />}
    </section>
  </main>;
}

function Field({ label, value, onChange, type = "text", placeholder, required = false }: { label: string; value: string; onChange: (value: string) => void; type?: string; placeholder?: string; required?: boolean }) {
  return <label>{label}<input type={type} value={value} placeholder={placeholder} required={required} autoComplete="off" onChange={(event) => onChange(event.target.value)} /></label>;
}

function Overview({ instance, catalog }: ConnectionResult) {
  return <><section className="metrics"><Metric label="Tables" value={String(catalogTables(catalog).length)} /><Metric label="Policies" value={String(catalogPolicies(catalog).length)} /><Metric label="Chain revision" value={String(instance.revision ?? instance.block_number ?? "reported by gateway")} /></section>
    <section className="panel split"><div><p className="kicker">Gateway metadata</p><h2>Current instance configuration</h2><p>The console displays the gateway’s reported metadata. A connected indicator is not a guarantee of chain finality.</p></div><pre>{JSON.stringify(instance, null, 2)}</pre></section></>;
}
function Metric({ label, value }: { label: string; value: string }) { return <article className="metric"><p>{label}</p><strong>{value}</strong></article>; }

function TableBrowser({ tables, selected, setSelected, connection }: { tables: CatalogTable[]; selected: string | null; setSelected: (name: string) => void; connection: Connection }) {
  const [response, setResponse] = useState<KurabaseResponse<unknown> | null>(null);
  const selectedInfo = tables.find((table) => String(table.name) === selected);
  return <section className="table-layout"><aside className="panel table-list"><p className="kicker">Catalog tables</p>{tables.map((table) => <button className={selected === String(table.name) ? "selected" : ""} key={String(table.name)} onClick={() => { setSelected(String(table.name)); setResponse(null); }}>{String(table.schema ?? "public")}.{String(table.name)}</button>)}{tables.length === 0 && <p>No tables reported by this gateway.</p>}</aside><div className="panel"><p className="kicker">Data browser</p><h2>{selectedInfo ? `${selectedInfo.schema ?? "public"}.${selectedInfo.name}` : "Select a table"}</h2><p>Reads use the independent <code>@kurabase/js</code> client and your publishable key/session token.</p>{selected && <button className="primary" onClick={async () => setResponse(await clientFor(connection).from(selected).select("*").range(0, 49))}>Load first 50 rows</button>}{response && <ResponseView response={response} />}</div></section>;
}

function Migrations({ connection, migration, setMigration, result, setResult }: { connection: Connection; migration: { version: string; sql: string }; setMigration: (value: { version: string; sql: string }) => void; result: KurabaseResponse<Migration> | null; setResult: (value: KurabaseResponse<Migration>) => void }) {
  const [list, setList] = useState<KurabaseResponse<Migration[]> | null>(null);
  return <section className="panel migration"><p className="kicker">Atomic migration unit</p><h2>Apply a versioned SQL migration</h2><p>The gateway decides whether a migration can run and records it only after success.</p><Field label="Version" value={migration.version} onChange={(version) => setMigration({ ...migration, version })} placeholder="202609090001" required /><label>SQL<textarea value={migration.sql} onChange={(event) => setMigration({ ...migration, sql: event.target.value })} spellCheck={false} /></label><div className="editor-actions"><button onClick={async () => setList(await clientFor(connection, true).admin.migrations.list())}>Refresh history</button><button className="primary" disabled={!migration.version || !migration.sql} onClick={async () => setResult(await clientFor(connection, true).admin.migrations.apply(migration))}>Apply migration</button></div>{result && <ResponseView response={result} />}{list && <ResponseView response={list} />}</section>;
}

function Policies({ policies }: { policies: CatalogPolicy[] }) { return <section className="panel"><p className="kicker">Row-level security</p><h2>Policies reported by the catalog</h2>{policies.length ? <div className="policy-grid">{policies.map((policy, index) => <pre key={index}>{JSON.stringify(policy, null, 2)}</pre>)}</div> : <p>No policies were reported. This is not proof that a table is publicly writable; check the gateway catalog and migration history.</p>}</section>; }
function Delegation({ instance, hasSecret }: { instance: InstanceMetadata; hasSecret: boolean }) { return <section className="panel delegation"><p className="kicker">Trusted executor policy</p><h2>Signed sessions identify callers; the gateway executes within its delegation.</h2><p>A self-signed wallet session or configured identity-authority session identifies the caller. The contract validates delegation, binding and expiry; RLS and SQL semantics run in the delegated gateway and are not cryptographically proven on-chain.</p><dl><dt>Secret key in memory</dt><dd>{hasSecret ? "Present for this tab only" : "Not provided"}</dd><dt>Configured authority</dt><dd>{String(instance.authority_model ?? "Reported by gateway metadata")}</dd><dt>Confidentiality</dt><dd>Public-chain state is not a privacy boundary.</dd></dl></section>; }
function ResponseView({ response }: { response: KurabaseResponse<unknown> }) {
  const payload = response.data;
  const rows = Array.isArray(payload) ? payload : payload && typeof payload === "object" && "rows" in payload && Array.isArray(payload.rows) ? payload.rows : null;
  const tabular = rows?.every((row) => row && typeof row === "object" && !Array.isArray(row));
  const columns = tabular ? [...new Set(rows!.flatMap((row) => Object.keys(row)))] : [];
  const value = (cell: unknown) => cell === null ? "NULL" : typeof cell === "object" ? JSON.stringify(cell) : String(cell ?? "");
  return <div className={response.error ? "response failed" : "response"}><p>{response.error ? `Request failed · ${response.status}` : `Response · ${response.status}`} {response.count !== null && `· ${response.count} rows`}</p>
    {!response.error && tabular ? rows!.length ? <div className="result-table"><table><thead><tr>{columns.map((column) => <th key={column}>{column}</th>)}</tr></thead><tbody>{rows!.map((row, index) => <tr key={index}>{columns.map((column) => <td key={column} className={row[column] === null ? "null-value" : ""}>{value(row[column])}</td>)}</tr>)}</tbody></table></div> : <p>No rows returned.</p> : <pre>{JSON.stringify(response.error ?? payload, null, 2)}</pre>}
    {!response.error && payload !== null && !Array.isArray(payload) && typeof payload === "object" && "transaction_hash" in payload && typeof payload.transaction_hash === "string" && <p className="transaction-id">Transaction {payload.transaction_hash}</p>}
  </div>;
}

createRoot(document.getElementById("root")!).render(<App />);
