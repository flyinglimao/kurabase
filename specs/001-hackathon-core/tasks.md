# Kurabase 黑客松 Tasks

**狀態：Draft — 待 Review**  
**依據：`spec.md`、`plan.md`**  
**原則：vertical slice first；每個 task 都必須有可驗證的 acceptance criteria。**

## T01 — 建立最小 workspace 與共用型別

### 目標

建立 Rust workspace 與最小 crate 邊界，讓後續模組可以獨立開發但共用核心型別。

### 內容

- 建立：
  - `kurasql`
  - `kura-query`
  - `kura-tx`
  - `kura-chain`
  - `kura-auth`
  - `kura-gateway`
- 加入 Axum / Tokio / sqlparser-rs / DataFusion / Arrow / Alloy / REVM / Serde。
- 建立共用：
  - schema/table/column/row stable ID
  - canonical value enum
  - block snapshot identity
  - error enum

### Acceptance Criteria

- workspace 可成功 build/test。
- 各 crate dependency direction 明確，無 circular dependency。
- 可序列化一個最小 canonical row/value。
- 尚不要求任何 database functionality。

### Depends on

無。

---

## T02 — Generic KurabaseSchema contract

### 目標

完成最小 on-chain canonical state 與 atomic generic mutation executor。

### 內容

- schema owner。
- table / column / row stable ID。
- row existence / row version。
- per-column cell storage。
- schema version。
- atomic batch apply。
- optimistic precondition check。
- mutation events。
- developer/gateway delegated executor 的最小 authority。

### Acceptance Criteria

- 可部署一個空 schema contract。
- owner 可建立最小 table metadata。
- delegated executor 可提交一個 insert WritePlan。
- row/cell state 可從 RPC 讀回。
- row version 不符合時整筆 revert。
- batch 中任一 operation 失敗時沒有 partial state。
- event 足以識別 mutation。

### Depends on

T01。

---

## T03 — KuraSQL Catalog + CREATE TABLE migration

### 目標

完成第一個 SQL → catalog → WritePlan 路徑。

### 內容

- 使用 sqlparser-rs parse migration SQL。
- 建立 KuraSQL normalized catalog。
- 支援第一條 vertical slice 所需：
  - CREATE TABLE
  - basic column types
  - PRIMARY KEY
  - NOT NULL
  - DEFAULT
- 產生 deterministic WritePlan。
- migration timestamp/version 記錄。

### Acceptance Criteria

輸入：

```sql
CREATE TABLE posts (
  id bigint PRIMARY KEY,
  title text NOT NULL
);
```

可以：

- 產生 deterministic normalized catalog mutation。
- 產生 WritePlan。
- 提交到 T02 contract。
- chain 上 schema state 可被重新讀取。
- 同 migration version 不會重複套用。
- migration 失敗時 version 不被標記 applied。

### Depends on

T01、T02。

---

## T04 — Chain projection sync

### 目標

由 chain event 重建 current relational projection。

### 內容

- Alloy provider。
- 從 deployment block replay events。
- 建立 Arrow-backed table projection。
- follow new blocks。
- `ProjectionSnapshot { block_number, block_hash, catalog, tables }`。
- block 完成後 atomic snapshot swap。

### Acceptance Criteria

- 空 gateway 可只靠 chain/event 重建 T03 建立的 schema。
- chain insert 一 row 後，projection 可重建出相同值。
- query 取得 snapshot 後，即使下一個 block 到達，舊 snapshot 內容不被 mutation。
- 可取得 projection current block。
- 不會對已知落後 chain head 的 snapshot 回 ordinary current query。

### Depends on

T02、T03。

---

## T05 — 最小 KuraSQL read engine

### 目標

讓 projection 可透過 SQL SELECT 查詢。

### 內容

- DataFusion catalog/table provider adapter。
- KuraSQL identifier/type normalization。
- SELECT / FROM / WHERE。
- projection/filter/order/limit。
- KuraSQL error normalization。

### Acceptance Criteria

對 T04 projection：

```sql
SELECT id, title
FROM posts
WHERE id = 1;
```

可得到正確 row。

另外：

- unsupported syntax 明確回 UnsupportedFeature。
- type error 不直接暴露 DataFusion raw error。
- 同一 query 全程固定在同一 snapshot。

### Depends on

T04。

---

## T06 — INSERT / UPDATE / DELETE transaction executor

### 目標

完成真正 SQL write vertical slice。

### 內容

- transaction-local staged state。
- INSERT / UPDATE / DELETE。
- WHERE predicate evaluation。
- WritePlan generation。
- row-version/read-set preconditions。
- submit + conflict handling。
- commit 後 projection sync。

### Acceptance Criteria

官方或內部 SQL 路徑可以完成：

```text
INSERT
→ chain commit
→ projection update
→ SELECT sees row

UPDATE ... WHERE ...
→ chain commit
→ SELECT sees new value

DELETE ... WHERE ...
→ chain commit
→ SELECT no longer sees row
```

且：

- multi-operation transaction all-or-nothing。
- transaction 內後續 statement 可看見前面 staged change。
- stale row version 造成 TransactionConflict。
- conflict 不造成 partial write。

### Depends on

T02、T04、T05。

---

## T07 — Supabase-compatible HTTP + supabase-js 第一條 E2E

### 目標

官方 `@supabase/supabase-js` 不修改即可操作 Kurabase。

### 內容

- `/rest/v1/:table`。
- `apikey` / Authorization header parsing。
- select query parsing。
- JSON response。
- insert / select / update / delete。
- basic filters。
- Prefer header 中 mutation representation 所需部分。
- error/status mapping。

### Acceptance Criteria

官方 supabase-js 測試可成功：

```ts
await db.from('posts').insert({ id: 1, title: 'hello' })
await db.from('posts').select('*').eq('id', 1)
await db.from('posts').update({ title: 'world' }).eq('id', 1)
await db.from('posts').delete().eq('id', 1)
```

- SDK 不 fork。
- mutation 預設 response 與 chain `.select()` behavior 符合 spec。
- response 有 `data/error/count/status/statusText` 對應資訊。

### Depends on

T05、T06。

---

## T08 — Constraints 與 schema evolution

### 目標

補齊黑客松必要 integrity 與 ALTER semantics。

### 內容

- UNIQUE。
- FOREIGN KEY。
- CHECK。
- DEFAULT。
- identity。
- generated column。
- referential actions。
- immediate / deferred validation。
- ALTER TABLE / DROP。
- schema dependency validation。
- migration atomicity。

### Acceptance Criteria

至少有 automated tests 證明：

- UNIQUE duplicate 被拒絕。
- FK invalid reference 被拒絕。
- CHECK FALSE 被拒絕、UNKNOWN 不被錯誤拒絕。
- FK action 正常。
- deferred constraint 在 transaction final state 驗證。
- ADD / RENAME / DROP column 不破壞 stable column identity。
- 破壞 VIEW/FK dependency 的 DDL 預設失敗；CASCADE 可 atomic 移除 dependency。

### Depends on

T03、T06。

---

## T09 — JWT、AuthContext 與 RLS

### 目標

完成 Supabase-style application identity 與 write authorization。

### 內容

- JWT → AuthContext。
- anon / authenticated role。
- `auth.uid()`。
- `auth.jwt()`。
- RLS enable/disable。
- SELECT / INSERT / UPDATE / DELETE / ALL policy。
- USING / WITH CHECK。
- permissive / restrictive composition。

### Acceptance Criteria

- 同一 query/write 可正確取得固定 AuthContext。
- anon 與 authenticated 得到不同 visibility。
- SELECT RLS 會過濾 API result。
- unauthorized mutation 被拒絕且 chain state 不變。
- allowed mutation 正常 commit。
- policy 可使用一般 KuraSQL expression，而不是 hardcode owner-column pattern。

### Depends on

T06、T07。

---

## T10 — Gateway delegation 與 project keys

### 目標

完成 owner/developer → gateway → publishable/secret key 的最小 trust path。

### 內容

- owner root authority。
- developer delegation。
- gateway executor delegation。
- revoke。
- gateway project record。
- publishable key。
- secret key。
- secret key effective scope 不超過 underlying delegation。

### Acceptance Criteria

- 無 delegation 的 gateway signer 無法 commit privileged write。
- developer 明確授權後 gateway 可執行 delegated scope。
- revoke 後 gateway 失去相應 chain authority。
- publishable key 可完成 anon API routing/access。
- secret key 可完成 delegated server-side operation。
- gateway 不能自行擴張成 owner authority。

### Depends on

T02、T07、T09。

---

## T11 — Relational query surface

### 目標

補齊 demo 所需 relational capability。

### 內容

- JOIN：INNER / LEFT / RIGHT / FULL / CROSS。
- subquery / correlated subquery。
- EXISTS / IN subquery。
- GROUP BY / HAVING。
- aggregates。
- UNION / UNION ALL / INTERSECT / EXCEPT。
- CTE / recursive CTE。
- window functions。
- CASE / CAST。
- JSON / array 常用操作。

### Acceptance Criteria

每類至少有一個 KuraSQL integration test。

另外：

- query semantics 經 KuraSQL normalization，不直接把 backend dialect 當 public contract。
- unsupported backend discrepancy 必須有 rewrite/custom implementation 或明確拒絕。

### Depends on

T05、T08。

---

## T12 — Supabase query compatibility surface

### 目標

把 T11 能力映射到官方 supabase-js 常用 query builder。

### 內容

- nested FK relation。
- multiple FK relation disambiguation。
- `!inner`。
- referenced-table filtering。
- JSON path。
- filters：
  - eq/neq/gt/gte/lt/lte
  - is/in
  - like/ilike
  - contains/containedBy/overlaps
  - match/not/or/filter
- order/limit/range。
- single/maybeSingle。
- exact count。
- bulk insert。
- upsert / onConflict / ignoreDuplicates。

### Acceptance Criteria

- 全部 acceptance tests 使用官方 supabase-js。
- nested FK query 正確。
- `single()` 對 0 或 >1 row 產生 cardinality behavior。
- `maybeSingle()` 對 0 row 回 null、>1 row error。
- exact count 正確。
- filters 可作用於 spec 要求的 select/update/delete/table-returning RPC。

### Depends on

T07、T08、T11。

---

## T13 — VIEW 與 Materialized View

### 目標

完成 VIEW / MV。

### 內容

- CREATE VIEW / DROP VIEW。
- normalized VIEW definition。
- VIEW dependency。
- CREATE MATERIALIZED VIEW。
- gateway-side materialized Arrow result。
- REFRESH。
- atomic result swap。
- refresh 中 query blocking。
- CREATE INDEX metadata/no-op。

### Acceptance Criteria

- VIEW 永遠使用 current snapshot。
- MV 可顯示舊 source_block。
- source table 更新後 MV 在 refresh 前仍可回舊結果。
- REFRESH 完成後一次切換到新 result。
- MV physical state 全刪後可從 chain schema + current data 重建。
- CREATE INDEX 沒有 physical index 時 query correctness 不受影響。

### Depends on

T08、T11。

---

## T14 — Function / RPC

### 目標

完成 `supabase.rpc()` read/write flow。

### 內容

- typed params。
- scalar / row / table return。
- read-only SQL function。
- mutating function。
- nested function call。
- minimal local variables / IF / ELSE / RETURN / RETURN QUERY。
- SECURITY INVOKER / DEFINER。
- `/rest/v1/rpc/:function`。

### Acceptance Criteria

官方 supabase-js 可以：

- call scalar RPC。
- call table-returning RPC 並 chain select/filter/order/range。
- call mutating RPC。
- mutating RPC 只形成一個 atomic WritePlan。
- INVOKER 使用 caller authority。
- DEFINER 使用 function defined authority，gateway 不會因此取得 root authority。

### Depends on

T09、T11、T12。

---

## T15 — Cross-contract / ENS stretch

### 目標

在單 chain 上驗證 schema = contract 的 external namespace 模型。

### 內容

- quoted external schema resolver。
- ENS → contract address。
- same-block multi-contract projection。
- cross-contract SELECT / JOIN。
- external nullable weak FK。
- cross-contract atomic write（時間允許）。

### Acceptance Criteria

至少：

```sql
SELECT ...
FROM "a.eth".posts p
JOIN "b.eth".users u ON ...
```

可以在同一 block snapshot 完成。

External FK：

- create/update 時 target 必須存在。
- metadata 綁 resolved contract identity。
- target 不存在後 logical reference 表現為 NULL。

### Depends on

T08、T11、T13。

---

## T16 — ZK execution proof stretch

### 目標

驗證 WritePlan 作為 future verifiable execution boundary 可行。

### 內容

- deterministic commitment：
  - snapshot identity
  - schema/program
  - AuthContext
  - WritePlan hash
- gateway proof generation prototype。
- write-authorizing client verification。
- client authorization 綁 exact plan hash。

不要求 production prover/performance。

### Acceptance Criteria

至少有一個 set-based mutation demo：

```text
SQL
→ gateway executes
→ WritePlan + proof
→ client verifies
→ authorize exact WritePlan
→ chain commit
```

修改 WritePlan 後舊 authorization 不得有效。

### Depends on

T06、T09。

---

# Milestones

## M1 — Chain-backed CRUD

完成：

- T01–T07

結果：

> official supabase-js 已可對 chain-backed table 做 CRUD。

這是第一個不可妥協的 vertical slice。

## M2 — Supabase-like Database

完成：

- T08–T14

結果：

> constraints、RLS、relational query、VIEW/MV、RPC 與主要 supabase-js query surface 可 demo。

這是黑客松主提交版本。

## M3 — DLT differentiation

時間允許再完成：

- T15
- T16

結果：

> ENS/cross-contract relational query 與 verifiable execution 展示 Kurabase 相較普通 hosted database 的 DLT-specific 能力。

---

# Implementation Gate

進入 coding 前只需要確認這份 task decomposition。

Implementation 必須：

1. 依 milestone / dependency 順序推進。
2. 優先讓 acceptance test 綠，而不是先完成大量 abstraction。
3. T01–T07 未形成完整 E2E 前，不提前投入 T15/T16。
4. 每個 task 完成時必須對照本文件 acceptance criteria。
