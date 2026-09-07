# Kurabase 黑客松核心規格

**狀態：Draft — 待 Review**  
**範圍：黑客松版本（不包含長期產品 roadmap）**

## 1. 目標

Kurabase 是一個 **Supabase API compatible、以 DLT / blockchain 作為正式資料狀態來源的 relational database layer**。

對應用開發者而言，Kurabase 應盡量維持既有 Supabase 開發方式：

```ts
const supabase = createClient(KURABASE_URL, KURABASE_KEY)

await supabase.from('posts').select('*')
await supabase.from('posts').update({ title: 'new' }).eq('id', 1)
await supabase.rpc('publish_post', { post_id: 1 })
```

一般 application code 不應要求理解 EVM、bytecode、proof、indexer 或 transaction construction。

Kurabase **不是 PostgreSQL server replacement**，黑客松版本不要求 PostgreSQL wire protocol 或既有 PostgreSQL client 直接連線。

---

## 2. 黑客松必須證明的核心能力

黑客松版本至少要完成一條真正 end-to-end 的路徑：

```text
official supabase-js
        ↓
Supabase-compatible API
        ↓
KuraSQL / relational execution
        ↓
read: gateway relational projection
write: deterministic WritePlan
        ↓
generic on-chain executor
        ↓
canonical chain state
```

必須證明：

1. 官方 `@supabase/supabase-js` 可直接使用，不 fork SDK。
2. Supabase-style migration 可以定義 schema、constraint、RLS 與 function。
3. 常用 relational query 可正常執行。
4. RLS 對 write 是實際 authorization rule。
5. RPC/function 可以 read/write。
6. write 最後改變的是鏈上正式資料狀態，而不是 gateway local state。
7. gateway local projection 可以由鏈上狀態重建。

---

## 3. KuraSQL 定位

KuraSQL 的 relational semantics 以 **ISO SQL:2023** 為基準，但 Kurabase 不宣稱完整 SQL:2023 / Core SQL conformance。

原則：

- SQL standard 用來定義 relational semantics。
- Supabase compatibility 需要時，接受必要的 PostgreSQL / Supabase syntax extension。
- 不支援的功能必須明確報錯，不得 silent approximation。
- 普通 `CREATE INDEX` 是已明確定義的例外：它只有 optimization semantics，因此黑客松版本可以只記 metadata、甚至沒有實際效果。

### 3.1 黑客松 SQL surface

需要支援代表性的正常用法：

#### Query

- `SELECT / FROM / WHERE`
- alias / expression
- `DISTINCT`
- `ORDER BY`
- pagination
- subquery / correlated subquery
- `EXISTS`
- `IN (subquery)`
- derived table
- `INNER / LEFT / RIGHT / FULL / CROSS JOIN`
- `GROUP BY / HAVING`
- `COUNT / SUM / AVG / MIN / MAX`
- `UNION / UNION ALL / INTERSECT / EXCEPT`
- CTE
- recursive CTE
- VIEW
- window function 的常用形式

#### Mutation

- `INSERT`
- `UPDATE ... WHERE`
- `DELETE ... WHERE`
- set-based mutation
- statement atomicity
- Supabase compatibility 所需的 `RETURNING` / upsert / conflict handling

#### Schema / integrity

- `CREATE / ALTER / DROP TABLE`
- PRIMARY KEY
- FOREIGN KEY
- UNIQUE
- NOT NULL
- CHECK
- DEFAULT
- identity column
- generated column
- referential actions
- schema object dependency
- `RESTRICT / CASCADE`
- immediate / deferred constraint validation

語意要求：
- NULL 遵循 SQL three-valued logic。
- DEFAULT 在需要產生欄位值時求值，不在 migration 時固定成常數。
- identity column 提供標準自動識別值語意，底層是否使用 sequence 不構成 contract。
- generated column 的值由 expression 導出，不得被當作普通欄位任意寫入；physical storage 不構成 contract。
- CHECK 為 TRUE 時通過、FALSE 時拒絕；UNKNOWN 依 SQL CHECK 語意視為通過。
- deferred constraint 在 atomic transaction 的最終 commit 前驗證。

代表性功能需要有正確 semantics；不追求 SQL standard 每個 corner case。

---

## 4. Type system

黑客松版本的目標型別：

- smallint / integer / bigint
- numeric / decimal
- real / float / double precision
- boolean
- text / varchar / char
- date / time / timestamp
- uuid
- timestamptz
- json / jsonb
- bytea
- enum
- 一維 array

### Numeric

- `NUMERIC / DECIMAL` 是 exact decimal。
- 黑客松版本最大 precision：70。
- overflow 必須明確報錯。

### Float

- `REAL / FLOAT / DOUBLE PRECISION` 維持 approximate floating-point semantics。
- 不得 silently 轉成 decimal semantics。

### Type conversion

- 強型別。
- 只允許明顯安全的 implicit widening。
- 支援 explicit `CAST`。
- PostgreSQL-style `::type` 可作為 Supabase compatibility syntax。
- ambiguous conversion 必須報錯。

---

## 5. Migration

沿用 Supabase-style migration workflow。

```text
supabase/migrations/
  <timestamp>_<name>.sql
```

規則：

- timestamp/version 是 migration identifier。
- 依 version 順序套用尚未 applied 的 migration。
- migration history 只記 applied version。
- 不額外要求 content hash 或 immutable migration。
- migration history consistency 主要由 developer 負責。
- 一個 migration file 預設是一個 atomic unit。
- DDL + DML 一起成功或 rollback。
- 成功後才記 applied。
- 太大的 migration 超過單次 execution capacity 時可以失敗；online/large migration 不屬於黑客松範圍。
- v1 只考慮 current schema，不考慮 historical schema / time-travel。

---

## 6. Schema 與 contract

一個 Kurabase contract 對應一個 KuraSQL schema namespace。

例如：

```sql
SELECT * FROM public.posts;
```

代表目前主要 contract 的 `posts` table。

外部 Kurabase contract 可以透過 quoted schema identifier 存取，例如：

```sql
SELECT *
FROM "some.eth".users;
```

ENS 只負責名稱解析；不額外發明 `some.eth.table` 的特殊 grammar。

### Cross-contract

黑客松版本：

- 只支援同一條 chain。
- 可 cross-contract SELECT / JOIN。
- 有 authority 時可 cross-contract write。
- 單一 SQL query 的所有資料必須來自同一個 block snapshot。
- 不支援 cross-chain query / write / FK。

### External FK

External FK 是 Kurabase extension，與 local SQL FK 分開：

- 欄位必須 nullable。
- 建立或更新 reference 時 target 必須存在。
- constraint 綁定解析後的 stable contract identity，不跟著 ENS 變更。
- target 後續被刪除或無法解析時，logical value 視為 `NULL`。
- 不要求 source contract 立即被動寫入 `NULL`。
- 不建立跨 contract 的強 schema lifecycle dependency。

---

## 7. Read model

鏈上資料是正式資料來源。

Gateway 維護一份 **可重建的 local relational projection**，供 KuraSQL 執行 JOIN、CTE、VIEW、aggregate、window function 等 relational query。

Gateway local projection：

- 不是正式資料來源。
- 可以丟棄並由 chain 重建。
- 不指定必須使用 PostgreSQL 或任何特定 DBMS。
- 普通 read 必須跟隨目前 chain head，不允許因 indexing/projection 延遲而故意回舊 block。
- 單一 query 必須使用同一個 block snapshot。

如果 gateway projection 尚未追到目前 head，不能用舊 projection 假裝成 current read。

REVM / AlloyDB 類 lazy RPC-backed EVM state access可以作為 execution/simulation technique，但不是一般 relational SELECT 的替代方案；這屬 Plan 階段 HOW。

---

## 8. VIEW、Materialized View、Index

### VIEW

- v1 必做。
- VIEW definition 是 schema 的一部分。
- query result 由 current relational state 計算。

### MATERIALIZED VIEW

- v1 必做。
- materialized result 位於 gateway/query layer，屬 derived state，不上鏈。
- 使用者選擇 MV 即接受 stale snapshot。
- MV 可以由 gateway refresh / incremental maintain。
- 普通 `REFRESH MATERIALIZED VIEW` 進行中時，對該 MV 的 query 可以等待 refresh 完成。
- snapshot 切換必須 atomic。
- materialized result 丟失後可以重建。

### INDEX

普通 index：

- 只有 optimization semantics。
- 位於 gateway/query layer。
- 黑客松版本允許只解析與保存 metadata，physical index 可以是 no-op。
- index 是否存在不得影響 query correctness。

`UNIQUE` 不屬於上述規則；它是正式 constraint，必須真的被 enforce。

---

## 9. Transaction 與 write model

Kurabase 不要求在鏈上重新執行完整 SQL。

預期模型：

```text
SQL / mutating RPC
        ↓
gateway relational execution
        ↓
deterministic WritePlan
        ↓
authorization
        ↓
generic on-chain executor
        ↓
atomic state change
```

### Atomicity

- 單一 SQL statement 必須 atomic。
- atomic multi-operation transaction 必須 all-or-nothing。
- mutating RPC 走同一套 transaction model。
- 不做長時間 connection-bound `BEGIN ... COMMIT` session transaction。

### Concurrency

使用 optimistic concurrency / read-set validation：

```text
read snapshot
→ compute
→ validate assumptions at commit
→ apply writes
```

如果提交時 state assumptions 已失效：

- 整筆 transaction 失敗。
- 不允許 partial write。
- 可以重新 planning。
- 若重新 planning 產生不同 WritePlan，而使用者授權綁定舊 plan，則必須重新授權。

### Set-based write completeness

對 arbitrary set-based mutation，如何證明 gateway 沒漏掉符合條件的 row，是已知問題。

黑客松核心不要求解完；ZK execution proof 是 stretch / extension 候選。

---

## 10. RLS

RLS 是 Supabase compatibility extension。

v1：

- enable / disable RLS
- default deny
- SELECT / INSERT / UPDATE / DELETE / ALL
- `USING`
- `WITH CHECK`
- anon / authenticated
- permissive / restrictive policy composition
- `auth.uid()`
- `auth.jwt()`
- policy expression 可使用正常 KuraSQL expression / subquery / function

### SELECT RLS

Kurabase v1 的鏈上資料本來就是公開資料。

因此：

- SELECT RLS 是 Supabase-compatible **application visibility semantics**。
- 它不提供 confidentiality guarantee。
- 使用者仍可能直接從 public chain 取得 raw data。

### Write RLS

對 mutation，RLS 是實際 authorization rule，不能由 gateway 任意繞過。

---

## 11. Owner / Developer / Gateway / User

### Instance owner

Instance owner 是 root administrative authority。

預設可以：

- 修改 schema / policy
- 管理 developer/admin authority
- 直接操作資料
- 進行 migration

為符合傳統 Web2 database 的管理習慣，owner 預設可以進行 privileged data operation。

Owner 可以選擇透過 policy 限制日常操作，但只要仍保有修改 policy 的 root authority，這種限制主要是 operational safety，不是 cryptographic lockout。

### Developer

Developer authority 由 owner 明確 delegate。

### Application user

`auth.uid()` 是 application-level user identity，不等同 wallet address。

底層可以使用 wallet、JWT、session key、managed identity 等建立 session，但 SQL policy 不需要知道。

### Gateway

Gateway 是服務提供方，不是 root authority。

Gateway 可以在 developer 明確授權後：

- 代管 writes
- relay transaction
- 管理 session
- 發行 Supabase-like project keys
- 提供 query/projection service
- 收取服務費

Gateway 的 authority 必須源自 owner/developer delegation，不能自行創造或擴張 database authority。

---

## 12. Publishable key / Secret key

為維持 Supabase-like DX，publishable key 與 secret key 可以由 gateway 發行。

但其 authority model是：

```text
owner / developer
      ↓ explicit delegation
gateway
      ↓
publishable / secret keys
```

### Publishable key

- 可放 frontend。
- 用於 instance/project identification 與 anon access。
- 本身不具有 root/admin authority。

### Secret key

- server-side 使用。
- 可以代表 developer 授予 gateway 的 privileged scope。
- gateway 可以在 delegated scope 內代 developer 執行 write/admin operation。

Gateway 關閉、developer 消失時如何重建 anon/user access，屬 deferred availability/liveness 問題，不阻塞黑客松。

---

## 13. Function / RPC

官方 client interface：

```ts
supabase.rpc('function_name', args)
```

v1 function 需要能：

- typed parameters
- scalar return
- row return
- table/set return
- SELECT
- INSERT / UPDATE / DELETE
- 呼叫其他 function
- 使用 KuraSQL query capability
- mutating function 參與 atomic transaction
- SECURITY INVOKER / SECURITY DEFINER

SECURITY INVOKER 使用 caller authority；SECURITY DEFINER 使用 function 被明確授予的 definer authority。這些 authority 不得由 gateway 自行創造。

read-only function 可在 query layer執行。

mutating function：

```text
function execution
→ WritePlan
→ authorization
→ atomic chain commit
```

不要求完整 PL/pgSQL 或 SQL/PSM。

### Random

`random()` v1 可使用普通不安全 PRNG：

- 不保證公平性。
- 不保證不可預測。
- 不保證抗 gateway/user 重試操縱。
- planning 時即可決定結果。

安全 randomness / VRF 屬延伸功能，可能需要 authorization-first / deferred execution，不屬黑客松核心。

### External I/O

Arbitrary HTTP、filesystem、不可驗證外部 machine state 不屬核心 atomic SQL transaction semantics。

---

## 14. Supabase API compatibility

主要相容目標是官方 `supabase-js` observable behavior，而不是完整 PostgREST server protocol。

黑客松至少涵蓋：

### Query

- `.from().select()`
- `*`、指定欄位、alias
- nested FK relation
- 同一 relation 有多個 FK 時可明確指定 relation
- nested relation 的 inner semantics（如 `!inner`）
- referenced table filtering
- JSON path selection
- filters：`eq / neq / gt / gte / lt / lte / is / in / like / ilike / contains / containedBy / overlaps / match / not / or / filter`
- 同一套 filter 可作用於 select / update / delete / table-returning RPC
- `order`
- `limit`
- `range`（0-based、兩端 inclusive）
- `single`
- `maybeSingle`
- exact count

### Mutation

- `insert`
- bulk insert
- `update`
- `delete`
- upsert
- `onConflict`
- `ignoreDuplicates`
- mutation 預設不回傳修改後 rows；chain `.select()` 時才回傳 representation

### RPC

- `.rpc()`
- scalar / row / table-returning function
- table-returning RPC 可繼續 filter/select

### Response

維持 Supabase client 熟悉的：

```ts
{
  data,
  error,
  count,
  status,
  statusText
}
```

不要求錯誤 code 完全複製 PostgreSQL/PostgREST，但 constraint、RLS、type、cardinality、transaction conflict 等錯誤必須可穩定辨識。

`single()` 必須恰好一 row；`maybeSingle()` 允許 0 或 1 row，0 row 回 null，超過 1 row 則回 cardinality error。

---

## 15. ZK Stretch

ZK 不是黑客松核心 DB 使用流程的必要條件。

Hosted gateway 若被視為 execution-untrusted，可以產生 execution proof：

```text
canonical snapshot
      ↓
gateway executes SQL
      ↓
WritePlan + proof
      ↓
write-authorizing client verifies
      ↓
sign exact WritePlan
      ↓
chain commit
```

Proof 主要解決 arbitrary set-based write 的 completeness / correctness。

Proof 預期由真正授權 write 的 client/application 驗證，不要求鏈上一定再次驗 proof。

Developer 完全自架、信任 execution environment 時，可以不使用 proof。

---

## 16. 黑客松明確不處理

以下不屬於這份 spec：

- PostgreSQL wire protocol / psql compatibility
- 完整 PostgreSQL compatibility
- 完整 SQL:2023 conformance
- cross-chain query / write / FK
- historical schema / time-travel
- online / large migration
- 完整 procedural SQL
- secure VRF / deferred randomness execution
- gateway disappearance / admin recovery protocol
- Supabase Storage
- Supabase Realtime
- Supabase Edge Functions
- 完整 Supabase Auth provider implementation
- production-grade HA / reorg handling

---

## 17. 黑客松驗收場景

至少需要一個代表性 demo：

1. 使用 Supabase-style migration 建立兩個有 FK 的 tables。
2. 啟用 RLS，使用 `auth.uid()` 限制 write。
3. 使用官方 `supabase-js` insert/select/update/delete。
4. 進行 nested relation query。
5. 執行至少一個 CTE / VIEW query。
6. 執行 Materialized View 並展示 stale/refresh semantics。
7. 執行一個 read-only RPC。
8. 執行一個 mutating RPC。
9. mutation 經 SQL execution 產生 WritePlan，最後由 generic executor 原子改變鏈上正式 state。
10. gateway projection 從 chain change 更新，read 能看到最新 block state。
11. 至少展示一次 failed RLS / constraint write 不會產生 partial state change。

如果時間允許，再加入：

- ENS external schema query
- cross-contract JOIN
- client-side ZK proof verification
