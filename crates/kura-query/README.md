# kura-query

An immutable, invoker-RLS query adapter for DataFusion 54.1 and Arrow 58.

```rust,ignore
let rows = kura_query::execute(&snapshot.database, sql, &auth_context).await?;
let table = kura_query::materialize(&snapshot.database, sql, &auth_context, "summary").await?;
```

`execute` accepts one SQL query and returns JSON row objects. It supports joins,
aggregates, CTEs, subqueries, windows and set operations over a fixed snapshot.
Every referenced base table enters the backend through `Database::read_table`,
including reads inside views and subqueries. Ordinary views execute with invoker
RLS. No global relation cache can accidentally retain privileged rows.

The caller authenticates requests and pins the current chain block before entry.
The adapter supplies query semantics; it does not independently authenticate a
session or prove that the supplied database snapshot came from the chain.

The SQL boundary rejects non-query statements, mutating CTEs, SELECT INTO,
locking, table functions, file/URL sources, unknown relations and unsupported
functions/types. DataFusion DDL/DML/statement capabilities are disabled as a
second check. SQL has a 256 KiB limit and 64-level query/view nesting limit;
execution has a 128 MiB managed-memory limit, disk spilling disabled, a 15-second
timeout and a 100000-row output limit. These are per-query limits, not a global
process quota; the hosting gateway still needs admission control.

Stored integer widths currently use Int64 Arrow arrays after canonical core
validation; projected explicit casts retain their backend integer result widths.
Boolean, text/varchar, UUID and JSON columns are accepted. JSON is internally
encoded as UTF-8 and direct/aliased field projections carry logical-type metadata
so the adapter reconstructs JSON values in its response. Unsupported JSON
operators fail explicitly. `auth.uid()` is safely replaced with a literal from
the immutable context; direct `auth.jwt()` query output is rejected until the
native JSON expression adapter is available. RLS policies continue to obtain
claims from `Database::read_table` and its native evaluator.

Finite floating-point query outputs, such as AVG on integer inputs, are returned
as JSON numbers. This does not implement the specification's exact DECIMAL(70)
surface. Stored unsupported types and unsupported casts fail explicitly.
`materialize` derives column types even when the result is empty, and currently
rejects result types without a supported canonical table type, including floats
and decimals. The gateway must keep its resulting table/cache scoped to the
authenticated caller and install it only in that caller's temporary query
database. The returned derived table has no additional RLS policies because its
rows have already passed the caller's original policies.

Uncached materialized views fail explicitly. The gateway owns refresh, source
block metadata and atomic cache replacement. This crate never silently refreshes
an MV or substitutes fresh rows for its intended stale snapshot.

Run the tests from the repository root:

```sh
rtk proxy sh scripts/cargo.sh test -p kura-query
```

Dependency features and session security controls were checked against
[DataFusion's official SessionContext documentation](https://docs.rs/datafusion/54.1.0/datafusion/execution/context/struct.SessionContext.html)
and [its versioned feature definitions](https://github.com/apache/datafusion/blob/54.1.0/datafusion/core/Cargo.toml).
