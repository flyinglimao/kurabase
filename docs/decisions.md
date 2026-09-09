# Implementation decisions

## 2026-09-09 — User clarifications

- Delivery deadline: Saturday 2026-09-12 23:59 Asia/Tokyo (system timezone).
- Begin development immediately; resume at 2026-09-09 03:23, then every 301 minutes until the deadline or completed delivery.
- Use Anvil for reproducible development and acceptance. Keep EVM chain configuration external and allow ENS-capable networks later.
- Build an independent JavaScript SDK with a familiar Supabase-style interface. Do not wrap or fork supabase-js. Keep official supabase-js as an independent compatibility test client.
- Deliver a working gateway/executor, SDK, instance dashboard, reference site and LLM-readable reference, plus automated tests and an explicit limitations report.
- Multiple gateways can be explicitly delegated by the database developer. Undelegated gateways cannot write. User sessions must originate in real user/identity authority authorization and cannot be fabricated by gateways.

## Authorization implementation

Use revocable on-chain gateway delegation and independently signed, expiring user sessions bound to the gateway, chain and instance. An external identity authority can attest application IDs; wallet sessions are a local development option. Never treat a gateway-created JWT or actor hash as proof of user authorization. Owner/developer privileged operations are distinct from ordinary user writes.

RLS SQL evaluation runs inside the explicitly delegated executor. A user session proves authority to act as a user; it does not cryptographically prove arbitrary SQL policy evaluation. This trust boundary must be stated in reference documentation, not represented as a general on-chain SQL proof. Gateway authority is limited by developer delegation and session scope.

Identity-authority grants inherit the active administrator and that administrator's epoch, just like gateway grants. Revoking a developer immediately revokes the identity authority the developer delegated; re-adding the developer does not revive it. This prevents a removed developer from retaining an indirect session-minting or user-session-revocation capability.

## Concurrency

Use one monotonically increasing schema revision, validated for every atomic batch. This conservatively catches predicate/negative-read races (UNIQUE and foreign-key conflicts) that individual row versions miss. Coarser conflict granularity is acceptable for the initial implementation.

Materialized query values deliberately remain stale across data writes until an administrator refreshes them. Their cache identity includes the canonical schema version, so RLS, policy, view-definition and other schema changes cannot reuse a previously authorized caller-specific value.

## Work coordination

The main agent owns integration, gateway, workspace and end-to-end acceptance. Astra owns the generic executor/security tests and Rust SQL semantics; Terra owns the independent SDK. Further bounded UI/docs work follows once a slot is free. Do not claim implementation complete from mock tests alone.
