# Kurabase 黑客松實作計畫

**狀態：Draft — 待 Review**  
**依據：`spec.md`**  
**本文件只定義 HOW，不新增產品需求。**

## 1. 整體架構

黑客松版本採最短 end-to-end 路徑：

```text
supabase-js
   ↓
Gateway / Supabase-compatible HTTP API
   ↓
KuraSQL semantic layer
   ├─ read  → DataFusion → immutable projection snapshot
   └─ write → transaction executor → WritePlan
                                  ↓
                         KurabaseSchema contract
                                  ↓
                          EVM canonical state
```

核心取捨：

1. 不從零實作 relational query executor。
2. KuraSQL 自己掌握 parser normalization、type semantics、catalog、RLS、constraint、transaction 與 WritePlan。
3. DataFusion 只作 query execution backend，不把 DataFusion dialect 當成 KuraSQL contract。
4. Gateway projection 是 derived state；鏈上 contract state 才是正式持久狀態。
5. 黑客松 core 採 developer operationally trusts delegated gateway；ZK / client verification 為 stretch。
6. 先完成單 chain、單 gateway、單 schema contract 的完整 vertical slice，再擴充 cross-contract。

---

## 2. Implementation stack

### Gateway / KuraSQL

使用 Rust：

- HTTP：Axum
- async runtime：Tokio
- SQL parser：`sqlparser-rs`
- relational query execution：Apache DataFusion
- relational representation：Apache Arrow
- Ethereum RPC / transaction：Alloy
- EVM simulation：REVM + AlloyDB
- serialization：Serde

DataFusion 只負責 relational execution；KuraSQL semantic layer 先做 identifier/type/function/RLS/VIEW/unsupported-feature 處理，再交給 backend。

### Chain contract

黑客松先用 Solidity + Foundry 實作 generic schema/executor contract。

這只是 Kurabase 內部 implementation；application developer 不需要 Solidity。未來若改成直接產生 EVM bytecode，不應影響 KuraSQL / API contract。

### Projection persistence

黑客松第一版不要求 projection 自己持久化：

```text
deployment block
→ replay Kurabase events
→ rebuild Arrow projection
→ follow new blocks
```

資料量以 hackathon demo 為準。若 rebuild 成本成為問題，再加 local checkpoint；checkpoint 仍不是正式資料來源。

---

## 3. 預期 repository architecture

```text
kurabase/
├─ specs/
│  └─ 001-hackathon-core/
│     ├─ spec.md
│     └─ plan.md
├─ crates/
│  ├─ kurasql/          # AST normalization, catalog, types, semantic analysis
│  ├─ kura-query/       # DataFusion adapter / projection / VIEW / MV
│  ├─ kura-tx/          # transaction context + WritePlan
│  ├─ kura-chain/       # Alloy, contract bindings, block sync
│  ├─ kura-auth/        # JWT/auth context/delegation
│  └─ kura-gateway/     # Supabase-compatible HTTP API
├─ contracts/
│  └─ KurabaseSchema.sol
└─ tests/
   └─ supabase-js/
```

實際建立 workspace 留到 Tasks / Implementation 階段。

---

## 4. Canonical on-chain data model

### 4.1 一個 contract = 一個 schema

每個 `KurabaseSchema` contract 表示一個 KuraSQL schema namespace。

Contract 保存：

- schema/catalog metadata
- table / column stable IDs
- row existence
- row/cell canonical values
- row versions
- migration versions
- owner / delegated execution authority

### 4.2 Stable identity

Table、column、row 使用 stable internal identity：

```text
table_id
column_id
row_id
```

名稱只是 catalog metadata。Rename 不改 internal ID。

`row_id` 是 Kurabase hidden identity，不等同 SQL PRIMARY KEY；application PK 即使更新，row identity 仍保持。

### 4.3 Row storage

黑客松優先用 schema-evolution-friendly 的 cell storage：

```text
rowExists[table_id][row_id]
rowVersion[table_id][row_id]

cell[
  keccak256(table_id, row_id, column_id)
] = canonical encoded value
```

每個 table 維護 internal row allocator。

這樣 ADD / DROP / RENAME COLUMN 不必重寫所有 row，generic executor 也不需要為 application schema 產生 Solidity struct。

### 4.4 Canonical value encoding

KuraSQL 定義 versioned canonical value codec，至少區分：

- NULL
- signed integer
- exact decimal
- approximate float
- boolean
- UTF-8 text
- temporal value
- UUID
- JSON
- bytes
- enum
- array

Gateway / contract / future proof system 都以同一 encoding 計算 hash。

具體 binary layout 到 implementation task 才固定，但不得依賴 Rust memory layout 或 DataFusion internal serialization。

---

## 5. Catalog 與 migration

Migration flow：

```text
SQL
→ sqlparser AST
→ KuraSQL normalized AST
→ semantic analysis against current Catalog
→ catalog/data mutation
→ WritePlan
```

Catalog 包含：

- tables
- columns / types / default / generated
- PK / FK / UNIQUE / CHECK
- VIEW / MV definitions
- function definitions
- RLS policies
- dependencies
- applied migration versions

一個 migration file 建立一個 transaction context。

成功時：

```text
all schema/data changes
+ applied migration version
→ one atomic chain commit
```

任一步失敗則不記 applied version。

---

## 6. Projection model

### 6.1 Event-driven rebuild

Contract 每次正式 mutation emit 足以重建 projection 的 event，例如：

```text
CatalogChanged
RowInserted
CellChanged
RowDeleted
MigrationApplied
```

Gateway 從 deployment block replay events。

Projection snapshot：

```text
ProjectionSnapshot {
  block_number,
  block_hash,
  catalog,
  tables: Arrow-backed relations
}
```

### 6.2 Immutable block snapshot

每處理完 block H：

```text
snapshot H-1
+ block H events
→ snapshot H
→ atomic swap Arc<ProjectionSnapshot>
```

Query 開始時取得 immutable snapshot，因此同一 SQL 的所有 read 都固定在同一 block。

### 6.3 Current-head requirement

普通 query：

1. 取得 chain head H。
2. projection 未到 H 時等待 sync。
3. 到 H 後取得 snapshot。
4. 整個 query 固定使用該 snapshot。

不回傳已知落後 current head 的普通 table / VIEW 結果。

黑客松不處理 reorg，但 snapshot 保留 `block_hash` 方便後續加入。

---

## 7. Query path

```text
HTTP request
→ resolve API key / JWT
→ AuthContext
→ PostgREST-style request parser
→ normalized relational query
→ RLS rewrite
→ KuraSQL type/semantic check
→ DataFusion logical plan
→ execute against ProjectionSnapshot
→ Supabase-compatible response
```

KuraSQL 負責：

- identifier resolution
- Supabase/PostgreSQL compatibility syntax normalization
- type compatibility
- function resolution
- RLS injection
- VIEW/catalog resolution
- unsupported feature rejection

DataFusion 負責：

- scan / filter / projection
- JOIN
- aggregate
- sort
- window
- set operations
- CTE / recursive CTE
- 可安全委派的 expression execution

若 backend semantics 與 KuraSQL contract 不一致，KuraSQL 必須 rewrite、custom expression 或自行處理；不能 silent inherit backend semantics。

### VIEW

VIEW 保存 normalized definition，query 時在目前 snapshot 執行。

### MATERIALIZED VIEW

MV physical result只存在 gateway：

```text
MaterializedView {
  definition,
  source_block,
  Arrow result
}
```

Refresh 在 immutable snapshot 計算新結果，完成後 atomic swap。普通 refresh 期間可阻塞該 MV query。

### INDEX

`CREATE INDEX` 只進 catalog metadata。黑客松 physical index 可以完全 no-op。

---

## 8. Write path

### 8.1 Transaction execution

```text
SQL / mutating RPC
→ pin snapshot H
→ transaction-local relation state
→ execute statements in order
→ evaluate RLS / constraints
→ compute final changed rows
→ build WritePlan
→ submit to chain
```

同一 transaction 後面的 statement 必須看得到前面 staged changes。

### 8.2 WritePlan

至少包含：

```text
WritePlan {
  schema_contract,
  base_block,
  expected_schema_version,
  actor_context_hash,
  preconditions[],
  operations[],
  migration_versions[],
}
```

Precondition 主要包括：

- expected row version
- expected row existence
- expected allocator/schema version

Operation 主要包括：

- catalog change
- insert row
- set/unset cell
- delete row
- migration marker

整個 plan 產生 deterministic hash。

### 8.3 Optimistic concurrency

Contract commit 時驗證 preconditions；任一失效：

```text
revert TransactionConflict
```

整筆 transaction revert，不允許 partial write。

Gateway 可重新在最新 snapshot planning。

### 8.4 Set-based completeness

Row-version/read-set validation 只能證明已讀 row 沒改變，不能證明 arbitrary predicate 沒漏 row。

黑客松 core 接受 developer 對 delegated gateway 的 operational trust；ZK stretch 在 WritePlan boundary 處理解決 completeness / correctness。

---

## 9. Generic chain executor

`KurabaseSchema` 不執行 SQL parser / relational planner。

它只負責：

1. 驗證 caller authority / delegation。
2. 驗證 WritePlan preconditions。
3. atomic apply catalog / row operations。
4. 更新 row/schema versions。
5. emit projection events。
6. 任一失敗即整筆 revert。

也就是：

```text
KuraSQL semantics → Gateway
state-transition enforcement → Contract
```

Application schema 改變不需要重新編譯新的 table-specific contract code。

---

## 10. RLS / constraints trust model

### Core hackathon

- developer 明確 delegate gateway execution authority；
- gateway 的 KuraSQL semantic layer 執行 RLS / constraints；
- gateway 只能使用 delegation 授予的 chain authority；
- root/admin operation 仍需要相應 owner/developer scope。

這個 core mode 明確承認 developer 對 hosted gateway 有 operational trust。

User write：

```text
publishable key
+ JWT / user session
→ AuthContext
→ auth.uid() / auth.jwt()
→ RLS
→ WritePlan
→ delegated gateway signer
→ chain
```

### Verifiable stretch

```text
snapshot + SQL + AuthContext
→ gateway execution
→ WritePlan + proof
→ client verifies
→ authorize exact plan_hash
→ commit
```

因此 core code 必須讓 WritePlan / AuthContext 可 deterministic commitment，但不要求先完成 proof system。

---

## 11. Auth / credential path

### Owner / developer

Schema contract 有 root owner。

Owner 可授權 developer；developer 再明確授權 gateway execution identity：

```text
gateway signer
+ scope
+ expiry/revocation metadata
```

Delegation 的具體形式可用 on-chain registration 或 signed capability；黑客松選最簡單可 revoke 的版本。

### Gateway-issued keys

Gateway 取得 delegation 後建立 project：

```text
developer delegation
→ gateway project
   ├─ publishable key
   └─ secret key
```

Publishable key：

- project/schema routing
- anon API access
- frontend safe

Secret key：

- server-side only
- gateway-side credential
- effective authority 不得超過 underlying developer delegation

API key 本身不需要上鏈。

### JWT / AuthContext

Gateway 將 JWT normalize 成：

```text
AuthContext {
  role,
  uid,
  jwt_claims,
  credential_class
}
```

`auth.uid()` / `auth.jwt()` 從這個 query/transaction context 取得值。

---

## 12. Function / RPC

### Read-only

```text
rpc()
→ function resolver
→ function body
→ current ProjectionSnapshot
→ result
```

### Mutating

```text
rpc()
→ function execution context
→ staged mutations
→ RLS / constraints
→ WritePlan
→ chain commit
```

不實作完整 PL/pgSQL；建立最小 interpreter 支援：

- parameters
- local variables
- expression assignment
- IF / ELSE
- RETURN / RETURN QUERY
- SQL statement execution
- nested function call

`SECURITY INVOKER / DEFINER` 在 function execution context 切換 effective authority；gateway 本身不因此得到額外 root authority。

---

## 13. Supabase-compatible HTTP layer

目標不是重做完整 PostgREST，而是實作 `supabase-js` 需要的 wire behavior。

主要 routes：

```text
/rest/v1/:table
/rest/v1/rpc/:function
```

Gateway 處理：

- `apikey`
- `Authorization: Bearer ...`
- select query parameter
- PostgREST-style filters
- order / limit / range
- hackathon 需要的 Prefer headers
- JSON request / response
- status / error mapping

Acceptance tests 直接使用官方 `@supabase/supabase-js`，不以內部 API 測試取代。

---

## 14. Cross-contract

不是第一條 vertical slice，但 architecture 不封死。

```text
schema name
├─ local schema → current contract
└─ quoted ENS name → resolve contract
```

Query 時，同一 block H 的多 contract projection 註冊到同一 relational catalog。

External FK 保存 resolved stable contract identity。

Cross-contract write 最後需形成同一 EVM transaction；若時間不足，留在 demo stretch，不阻塞單 schema core。

---

## 15. REVM / AlloyDB

不作 relational query engine。

用途：

- generic executor transaction simulation
- gas/error 預檢
- 指定 block 的 EVM state access
- future proof/execution integration

普通 SQL SELECT 仍由 gateway relational projection 執行。

---

## 16. Error boundary

內部 error normalize 成：

```text
SyntaxError
UnsupportedFeature
TypeError
ConstraintViolation
RlsViolation
PrivilegeError
CardinalityError
TransactionConflict
ExecutionFailure
ResourceLimit
```

HTTP layer 再映射成 supabase-js 可正常取得的 `error / status / statusText`。

不直接暴露 DataFusion / Alloy / EVM 原始錯誤作為 public contract。

---

## 17. 實作順序原則

Tasks 階段採 vertical slice first。

第一條 slice：

```text
CREATE TABLE migration
→ chain schema state
→ rebuild projection
→ supabase-js INSERT
→ WritePlan
→ chain commit
→ projection update
→ supabase-js SELECT
```

之後依序補：

```text
constraints / UPDATE / DELETE
→ JWT + RLS
→ JOIN + nested relation
→ RPC
→ CTE / VIEW / aggregate / window
→ MV
→ migration ALTER
→ cross-contract
→ ZK stretch
```

具體 task dependency 與 acceptance criteria 下一階段再寫。

---

## 18. Plan review gate

進 Tasks / Implementation 前需確認：

1. Rust + sqlparser-rs + DataFusion。
2. projection 黑客松先採 Arrow in-memory + event replay。
3. on-chain row 採 stable `row_id` + per-column cell storage。
4. generic `KurabaseSchema` 不執行完整 SQL，只 apply WritePlan。
5. 黑客松 core 採 developer-trusted delegated gateway，ZK 為 stretch。
6. Solidity/Foundry 只作 generic executor 的內部 hackathon implementation。

以上確認後才進 Tasks。
