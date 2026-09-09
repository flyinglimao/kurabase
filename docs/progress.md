# Kurabase development checkpoint

Updated: 2026-09-09 18:46 Asia/Tokyo.

## Current work

- Contract complete for current protocol: 39 Foundry tests including two 256-run fuzz cases. Identity-authority grants now inherit their delegating administrator's epoch, so developer revocation also removes that authority's signing and revocation powers. ABI/security protocol in docs/contract-protocol.md.
- Rust core: 25 passing tests for CRUD/constraints/RLS/upsert/migrations/functions/views, ALTER/dependency lifecycle, referential actions, generated/identity columns and deferred constraints.
- Independent SDK: 11 passing tests, session exchange helper and browser-safe fetch binding. CLI has 3 unit tests plus a live Anvil migration apply/skip test. Dashboard/docs production builds pass.
- Gateway: Axum + Alloy connects real Anvil, signed session exchange, current-block event replay, stable cell writes, admin SQL/migrations and official SDK CRUD.
- DataFusion query adapter: 15 passing relational/RLS tests. It admits CTE, joins, subqueries, aggregates, windows, set operations and views through immutable RLS-filtered Arrow relations; unsafe/external SQL is rejected. Materialized cache keys include canonical schema version so policy/definition changes cannot reuse a caller's prior materialization.
- Real E2E: `pnpm test:e2e` passed 11 tests on 2026-09-09 18:43. It covers both SDKs, real CLI migrations, CRUD, nested/FK-filtered relations, RLS/constraint atomic failures, auth forgery, upsert/count/cardinality, gateway restart rebuild, failed migrations, RPC, CTE/window/set/VIEW/MV stale-refresh and RLS-cache invalidation.
- Browser E2E passed after verifying dashboard connection/table read/SQL change, bearer-session and secret non-persistence, mobile layout, docs and LLM reference.
- Scheduler PID originally 11088, exact thread queue confirmed at 03:23 and 08:24, next 13:25. Do not start a duplicate. State/logs in ignored .runtime/.

## Reproduction

- Rust toolchain installed project-local in ignored .tools; run `rtk proxy sh scripts/cargo.sh <command>`.
- `rtk proxy pnpm install`, then `rtk proxy pnpm -r build` and `rtk proxy pnpm -r test`.
- `rtk proxy forge test --offline` from contracts may need network/sandbox escalation on macOS proxy lookup.
- `rtk proxy sh scripts/cargo.sh build -p kura-gateway`, then local E2E above (requires permission for localhost child processes).
- `node scripts/dev.mjs` starts local Anvil + separate ordinary and administrative gateways. Public test-only Anvil credentials; no real funds/accounts.

## Acceptance order

1. Compile and test contract, SQL core and SDK.
2. Deploy to Anvil; migration and CRUD commit to chain; rebuild after gateway restart.
3. Official supabase-js black-box test, own SDK parity, atomic failure and stale-revision tests.
4. RLS/session/delegation integration, relational queries, functions and materialized views.
5. Instance dashboard, reference website and LLM reference; browser tests.
6. Final spec coverage matrix, reproducible demo and decisions/limitations report. Current matrix: `docs/spec-coverage.md`.

This is an in-progress checkpoint, not a completion claim. Resume by inspecting current changes and test results; preserve other agents' work.

## Remaining gaps before delivery

- The intentionally explicit gaps remain full PostgreSQL/SQL:2023 coverage, exact decimal/float/temporal execution, procedural SQL, realtime/storage/auth providers, ENS/cross-chain operation, and production reorg/finality/HA work. See `docs/spec-coverage.md` for the bounded acceptance surface.
- RLS and constraint evaluation remains a delegated-gateway trust boundary. The contract proves gateway delegation and independently signed session authority, not arbitrary SQL execution.
- Materialized views intentionally retain prior row data until `REFRESH MATERIALIZED VIEW`; their caller-specific values are now invalidated on schema/RLS/definition change.
- The local scheduler is still active (one lock only) and will enqueue the next continuation at 2026-09-09 23:27 JST. Do not create `docs/delivery-complete.json` until the accepted final delivery is verified.
