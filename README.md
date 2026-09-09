# Kurabase

An EVM-backed relational database layer with a familiar developer interface. Applications use tables, SQL migrations, policies and RPC; Solidity is an internal implementation detail.

**Status: active implementation.** Real chain-backed CRUD, session authorization, independent JS SDK and official supabase-js compatibility tests are working. The full specification is not yet implemented. See [progress](docs/progress.md), [decisions](docs/decisions.md) and [core specification](specs/001-hackathon-core/spec.md).

## Packages

| Component | Location | Purpose |
| --- | --- | --- |
| JavaScript SDK | `packages/kurabase-js` | Independent `@kurabase/js` client for browsers and Node |
| Dashboard | `apps/dashboard` | Instance connection, tables, SQL, migrations and policies |
| Reference site | `apps/docs` | Human documentation plus `llms.txt` and `llms-full.txt` |
| SQL core | `crates/kurasql` | Catalog, atomic mutations, RLS, constraints and functions |
| Query engine | `crates/kura-query` | Immutable Arrow/DataFusion relational reads |
| Chain adapter | `crates/kura-chain` | Alloy signing, commits and pinned event replay |
| HTTP gateway | `crates/kura-gateway` | Supabase-compatible API and admin endpoints |
| Generic executor | `contracts` | Delegation, signed sessions, canonical cells and atomic revisions |

## Local development

Requires Node 20+, pnpm 10, a current Rust toolchain, and Foundry (`forge`, `anvil`). The development stack uses public, disposable Anvil test accounts; do not transfer real assets to these addresses.

```sh
pnpm install
pnpm -r build
cd contracts
forge build
cd ..
cargo build -p kura-gateway
node scripts/dev.mjs
```

On the current development machine, Rust was installed within the ignored `.tools/` directory. Use `sh scripts/cargo.sh build -p kura-gateway` instead of `cargo` there.

The script prints the ordinary gateway (54321), administrative gateway (54322), contract address, local project keys and a signed test session. A separate admin signer owns the schema; the ordinary gateway has an explicit, non-administrative delegation.

Start the dashboard using the `dev` script in `apps/dashboard` and enter the **administrative** gateway URL for SQL/catalog management. The secret key and any bearer session remain in browser memory; the URL and publishable key may be remembered locally. Use the ordinary gateway and a user session for application queries.

```ts
import { createClient } from '@kurabase/js'

const db = createClient('http://127.0.0.1:54321', 'kura_pub_local_anvil')
db.auth.setAccessToken(signedSessionToken)
const { data, error } = await db.from('posts').select('*').eq('id', 1)
```

## Verification

```sh
pnpm -r test
cargo test --workspace
cd contracts && forge test && cd ..
pnpm test:e2e
```

The E2E suite starts its own Anvil and two gateways on isolated ports, deploys a real contract, applies SQL migrations, exercises both SDKs, checks failures leave chain state unchanged, and restarts the gateway to prove event-only reconstruction. Build the gateway and contract first.

## Authority and data model

The database owner explicitly delegates one or more gateways. User sessions are independently signed and bound to chain, contract, gateway, identity, claims, expiration and revocation epoch. A gateway cannot fabricate another user's valid session. See the [contract protocol](docs/contract-protocol.md).

SQL RLS and constraints are evaluated by the explicitly delegated gateway within its granted authority. This is a trusted executor model, not a proof that arbitrary SQL was evaluated on-chain. The contract enforces delegation, valid session authority, revision checks and atomic state updates. SELECT RLS controls application visibility; public-chain data is not confidential.

Canonical catalog and cell values live on-chain. Gateway projections are disposable and reconstructed from events at one pinned head block. Anvil is the current acceptance environment; production availability/reorg handling is outside this version's scope.
