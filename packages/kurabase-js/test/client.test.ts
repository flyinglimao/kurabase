import { describe, expect, it, vi } from "vitest";
import { createClient } from "../src/index.js";

function json(body: unknown, init: ResponseInit = {}): Response {
  return new Response(JSON.stringify(body), {
    status: init.status ?? 200,
    statusText: init.statusText,
    headers: { "content-type": "application/json", ...(init.headers ?? {}) },
  });
}

describe("@kurabase/js", () => {
  it("builds an immutable, thenable select request with PostgREST filters", async () => {
    const fetcher = vi.fn<typeof fetch>().mockImplementation(async () => json([{ id: 7, title: "hello" }], {
      headers: { "content-range": "0-0/1" },
    }));
    const client = createClient("https://kura.example/", "anon-key", { fetch: fetcher });
    const original = client.from("posts").select("id,title");
    const query = original.eq("published", true).order("id", { ascending: false }).range(0, 9);

    expect(await query).toMatchObject({ data: [{ id: 7, title: "hello" }], count: 1, error: null });
    const [url, init] = fetcher.mock.calls[0]!;
    const parsed = new URL(String(url));
    expect(parsed.pathname).toBe("/rest/v1/posts");
    expect(parsed.searchParams.get("select")).toBe("id,title");
    expect(parsed.searchParams.get("published")).toBe("eq.true");
    expect(parsed.searchParams.get("order")).toBe("id.desc");
    expect(new Headers(init?.headers).get("Range")).toBe("0-9");
    // The original builder was not changed by the later chains.
    expect(await original).toMatchObject({ data: [{ id: 7, title: "hello" }] });
    expect(new URL(String(fetcher.mock.calls[1]![0])).searchParams.has("published")).toBe(false);
  });

  it("uses minimal mutation output unless select is chained", async () => {
    const fetcher = vi.fn<typeof fetch>()
      .mockResolvedValueOnce(new Response(null, { status: 201, statusText: "Created" }))
      .mockResolvedValueOnce(json([{ id: 1, title: "new" }], { status: 201, statusText: "Created" }));
    const client = createClient("https://kura.example", "secret", { fetch: fetcher });

    expect((await client.from("posts").insert({ id: 1, title: "new" })).data).toBeNull();
    expect(new Headers(fetcher.mock.calls[0]![1]?.headers).get("Prefer")).toBe("return=minimal");
    expect(await client.from("posts").upsert({ id: 1, title: "new" }, { onConflict: "id" }).select()).toMatchObject({
      data: [{ id: 1, title: "new" }],
    });
    const [url, init] = fetcher.mock.calls[1]!;
    expect(new URL(String(url)).searchParams.get("on_conflict")).toBe("id");
    expect(new Headers(init?.headers).get("Prefer")).toContain("resolution=merge-duplicates");
    expect(new Headers(init?.headers).get("Prefer")).toContain("return=representation");
  });

  it("enforces cardinality even when a gateway returns JSON arrays", async () => {
    const fetcher = vi.fn<typeof fetch>()
      .mockResolvedValueOnce(json([]))
      .mockResolvedValueOnce(json([{ id: 1 }, { id: 2 }]));
    const client = createClient("https://kura.example", "key", { fetch: fetcher });
    expect(await client.from("posts").select().maybeSingle()).toMatchObject({ data: null, error: null });
    expect(await client.from("posts").select().single()).toMatchObject({
      data: null,
      error: { code: "KURA_CARDINALITY" },
    });
  });

  it("normalizes server and transport errors and supports RPC/admin endpoints", async () => {
    const fetcher = vi.fn<typeof fetch>()
      .mockResolvedValueOnce(json({ message: "RLS denied", code: "KURA_RLS", hint: "sign in" }, { status: 403, statusText: "Forbidden" }))
      .mockResolvedValueOnce(json([{ total: 2 }]))
      .mockResolvedValueOnce(json({ rows: [{ id: 1 }] }));
    const client = createClient("https://kura.example", "secret", { fetch: fetcher });

    expect(await client.from("posts").delete().eq("id", 1)).toMatchObject({ status: 403, error: { code: "KURA_RLS", hint: "sign in" } });
    expect(await client.rpc("recent_posts", { limit: 2 }).gte("id", 1)).toMatchObject({ data: [{ total: 2 }] });
    expect(await client.admin.sql("select 1")).toMatchObject({ data: { rows: [{ id: 1 }] } });
    expect(new URL(String(fetcher.mock.calls[1]![0])).pathname).toBe("/rest/v1/rpc/recent_posts");
    expect(new URL(String(fetcher.mock.calls[2]![0])).pathname).toBe("/admin/v1/sql");
  });

  it("keeps one derived query reusable and isolates later filter chains", async () => {
    let id = 0;
    const fetcher = vi.fn<typeof fetch>().mockImplementation(async () => json([{ call: ++id }]));
    const client = createClient("https://kura.example", "key", { fetch: fetcher });
    const base = client.from("posts").select("id").eq("published", true);
    const older = base.lt("id", 10);
    const newer = base.gte("id", 10);

    expect(await base).toMatchObject({ data: [{ call: 1 }] });
    expect(await base).toMatchObject({ data: [{ call: 2 }] });
    await older;
    await newer;
    expect(new URL(String(fetcher.mock.calls[0]![0])).searchParams.getAll("id")).toEqual([]);
    expect(new URL(String(fetcher.mock.calls[2]![0])).searchParams.get("id")).toBe("lt.10");
    expect(new URL(String(fetcher.mock.calls[3]![0])).searchParams.get("id")).toBe("gte.10");
  });

  it("honors abort signals and returns a stable abort response", async () => {
    const controller = new AbortController();
    controller.abort();
    const fetcher = vi.fn<typeof fetch>().mockImplementation(async (_url, init) => {
      expect(init?.signal).toBe(controller.signal);
      throw new DOMException("cancelled", "AbortError");
    });
    const client = createClient("https://kura.example", "key", { fetch: fetcher });

    expect(await client.from("posts").select().abortSignal(controller.signal)).toMatchObject({
      status: 0,
      statusText: "AbortError",
      error: { code: "KURA_ABORT" },
    });
  });

  it("reports counts for HEAD reads and preserves server error details", async () => {
    const fetcher = vi.fn<typeof fetch>()
      .mockResolvedValueOnce(new Response(null, { status: 200, headers: { "content-range": "*/12" } }))
      .mockResolvedValueOnce(json({ message: "bad filter", details: "unknown column", hint: "use id", code: "KURA_FILTER" }, { status: 400, statusText: "Bad Request", headers: { "content-range": "*/12" } }));
    const client = createClient("https://kura.example", "key", { fetch: fetcher });

    expect(await client.from("posts").select("*", { head: true, count: "exact" })).toMatchObject({ data: null, count: 12, status: 200 });
    expect(new Headers(fetcher.mock.calls[0]![1]?.headers).get("Prefer")).toContain("count=exact");
    expect(await client.from("posts").select().filter("nope", "eq", "x")).toMatchObject({
      count: 12,
      error: { code: "KURA_FILTER", details: "unknown column", hint: "use id" },
    });
  });

  it("encodes bulk/upsert, RPC, complex filters, and bigint JSON explicitly", async () => {
    const fetcher = vi.fn<typeof fetch>()
      .mockResolvedValueOnce(new Response(null, { status: 201 }))
      .mockResolvedValueOnce(json([{ id: 1 }]));
    const client = createClient("https://kura.example", "key", { fetch: fetcher });

    await client.from("posts").upsert([{ id: 1n, title: "first" }, { id: 2n, title: "second" }], {
      onConflict: "id,title", ignoreDuplicates: true,
    });
    const [upsertUrl, upsertInit] = fetcher.mock.calls[0]!;
    expect(new URL(String(upsertUrl)).searchParams.get("on_conflict")).toBe("id,title");
    expect(new Headers(upsertInit?.headers).get("Prefer")).toContain("resolution=ignore-duplicates");
    expect(upsertInit?.body).toBe('[{"id":"1","title":"first"},{"id":"2","title":"second"}]');

    await client.rpc("search posts/日本語", { phrase: "a b&c", limit: 1n }, { get: true })
      .in("slug", ["a,b", "space here"])
      .contains("meta", { labels: ["red"] })
      .or("title.ilike.*a b*");
    const rpcUrl = new URL(String(fetcher.mock.calls[1]![0]));
    expect(rpcUrl.pathname).toBe("/rest/v1/rpc/search%20posts%2F%E6%97%A5%E6%9C%AC%E8%AA%9E");
    expect(rpcUrl.searchParams.get("phrase")).toBe("a b&c");
    expect(rpcUrl.searchParams.get("limit")).toBe("1");
    expect(rpcUrl.searchParams.get("slug")).toBe('in.("a,b","space here")');
    expect(rpcUrl.searchParams.get("meta")).toBe('cs.{"labels":["red"]}');
    expect(rpcUrl.searchParams.get("or")).toBe("(title.ilike.*a b*)");
  });

  it("exchanges a pre-signed wallet session and closes over its bearer token", async () => {
    const fetcher = vi.fn<typeof fetch>()
      .mockResolvedValueOnce(json({ access_token: "kura.eyJ1aWQiOiIxIn0", token_type: "bearer", user: { id: "0xuser" }, expires_at: "2026-09-10T00:00:00Z" }))
      .mockResolvedValueOnce(json([]));
    const client = createClient("https://kura.example", "publishable-key", { fetch: fetcher });
    const envelope = {
      session: { gateway: "0xgateway", user: "0xuser", uid: "0x01", claimsHash: "0x02", expiresAt: "2026-09-10T00:00:00Z", nonce: "n-1", gatewayEpoch: "7" },
      signature: "0xsigned-by-wallet",
      claims: { role: "writer" },
    };

    expect(await client.auth.exchangeSession(envelope)).toMatchObject({ data: { access_token: "kura.eyJ1aWQiOiIxIn0" }, error: null });
    const [sessionUrl, sessionInit] = fetcher.mock.calls[0]!;
    expect(new URL(String(sessionUrl)).pathname).toBe("/auth/v1/session");
    expect(new Headers(sessionInit?.headers).get("apikey")).toBe("publishable-key");
    expect(new Headers(sessionInit?.headers).has("Authorization")).toBe(false);
    expect(JSON.parse(String(sessionInit?.body))).toEqual(envelope);

    await client.from("posts").select();
    expect(new Headers(fetcher.mock.calls[1]![1]?.headers).get("Authorization")).toBe("Bearer kura.eyJ1aWQiOiIxIn0");
    client.auth.setAccessToken("manual-token");
    fetcher.mockResolvedValueOnce(json([]));
    await client.from("posts").select();
    expect(new Headers(fetcher.mock.calls[2]![1]?.headers).get("Authorization")).toBe("Bearer manual-token");
  });

  it("binds the default browser fetch to globalThis", async () => {
    const browserFetch = function (this: unknown): Promise<Response> {
      expect(this).toBe(globalThis);
      return Promise.resolve(json([]));
    } as typeof fetch;
    vi.stubGlobal("fetch", browserFetch);
    try {
      expect(await createClient("https://kura.example", "key").from("posts").select()).toMatchObject({ data: [], error: null });
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("normalizes the gateway migration envelope", async () => {
    const fetcher = vi.fn<typeof fetch>().mockResolvedValue(json({ migrations: ["202609090001", { version: "202609090002", name: "posts" }] }));
    const client = createClient("https://kura.example", "secret", { fetch: fetcher });
    expect(await client.admin.migrations.list()).toMatchObject({
      data: [{ version: "202609090001" }, { version: "202609090002", name: "posts" }],
      error: null,
    });
    expect(new URL(String(fetcher.mock.calls[0]![0])).pathname).toBe("/admin/v1/migrations");
  });
});
