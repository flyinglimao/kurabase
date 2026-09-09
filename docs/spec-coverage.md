# Hackathon specification coverage

This matrix records implemented, demonstrated behaviour as of 2026-09-09. It is intentionally narrower than the aspirational language in [spec.md](../specs/001-hackathon-core/spec.md); unsupported inputs return a stable error rather than using an approximation.

| Area | Current state | Verification |
| --- | --- | --- |
| Generic EVM state | Implemented: catalog metadata, rows/cells, append-only reconstruction events, global revision and atomic batches | 39 Foundry tests; live Anvil E2E restart/rebuild |
| Authority | Implemented: owner/developer delegation, multiple gateways, explicit privileged service scope, EIP-712 user or identity-authority sessions, expiry/revocation/epochs; authority delegation dies with its issuing administrator | Foundry adversarial and fuzz tests; Anvil forged-session test |
| Projection | Implemented: current-head event replay and immutable in-request snapshot; canonical typed cell envelope | Anvil CRUD/restart test |
| SQL migration | Implemented: timestamp identifiers, atomic DDL/DML/history, CLI discovery/sort/skip | Rust core tests; CLI tests; real Anvil CLI apply/skip and failed-migration tests |
| Core SQL | Implemented: CREATE/ALTER/DROP table, INSERT/UPDATE/DELETE/RETURNING, PK/UNIQUE/FK/CHECK/default, identity/generated columns, deferred FK/UNIQUE, cascade/set-null actions | 25 Rust core tests |
| RLS | Implemented: enable/default deny, permissive/restrictive policies, `auth.uid()`/`auth.jwt()` context in core and DataFusion reads | Core tests, DataFusion tests, Anvil denied-write test |
| Relational reads | Implemented through DataFusion: joins, aggregate/HAVING, CTE, correlated subquery, derived table, window functions, UNION/INTERSECT/EXCEPT and ordinary VIEW | 15 query tests; Anvil CTE/window/set/view test |
| Materialized view | Implemented: gateway-side, per-caller snapshot cache, atomic refresh swap, stale until refresh for row data; schema/RLS/definition changes invalidate cached caller values | Anvil stale/refresh and RLS-policy invalidation tests |
| SQL-language functions | Implemented: scalar, SETOF/table, nested calls, SQL DML, atomic rollback, INVOKER/DEFINER | Core and Anvil RPC tests |
| Supabase wire surface | Implemented subset: official supabase-js CRUD/RPC, filters `eq/neq/gt/gte/lt/lte/is/in/like/ilike`, order/limit/range/count, single/maybeSingle, bulk/upsert, FK embeds, explicit FK selection, referenced filters and JSON projection | Official SDK E2E and gateway tests |
| Independent SDK | Implemented: browser/Node client, immutable builders, session exchange, admin calls, error/count/cardinality handling | 11 SDK tests plus Anvil E2E |
| Console and reference site | Implemented: dashboard connect/tables/SQL/migrations/policy/delegation UI, docs pages and generated LLM files | TypeScript builds and headless Chrome flow |

## Deliberately unsupported in this delivery

- Full SQL:2023/PostgreSQL grammar; cross-chain/ENS and cross-contract writes; cross-contract foreign keys.
- Numeric/decimal, floating-point, temporal, enum and array execution beyond explicitly admitted paths. `numeric` query result coercions are rejected until exact Decimal256 storage/evaluation is complete.
- Full procedural SQL/PLpgSQL, arbitrary external I/O, secure randomness, storage/realtime/auth providers and production reorg/HA handling.
- Cryptographic proof that an arbitrary RLS predicate was evaluated correctly by the gateway. The contract verifies the delegated gateway and an independently issued user session; the explicitly delegated gateway is trusted to execute SQL RLS/constraints. This matches the documented hackathon trust boundary.
- Many PostgREST operators (`contains`, `containedBy`, `overlaps`, generic `or`/`filter`) and complete nested embed grammar. The API rejects these until implemented.

## How to rerun the demonstrated vertical slice

```sh
pnpm install
sh scripts/cargo.sh build -p kura-gateway
cd contracts && forge build && cd ..
pnpm test:e2e
pnpm test:browser
```

Both scripts create isolated local Anvil processes and terminate them after testing. They do not use a public network or real private keys.
