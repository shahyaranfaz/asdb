# asdb

A dependency-free, from-scratch document database written in Rust.

asdb implements its own page storage, buffer pool, slotted heap files, persistent catalog, B-tree indexes, document codec, query planner, iterator-based execution, and 2 network interfaces: HTTP/1.1 and the binary ABP/1 protocol. Queries are written in **ASL (Ave-Shay Language)**, a compact pipeline language whose reading order matches its execution order.

> **Status: unreleased and under active development.** APIs, protocols, and on-disk formats may change without compatibility guarantees. The implemented storage, indexing, document, query, HTTP, ABP/1, and TTL paths pass 239 tests; 3 additional stress tests are ignored by default. See [Current limitations](#current-limitations) before experimenting with asdb outside a local development environment.

## Highlights

- 4 KiB page storage with allocation, free-list reuse, and malformed-file checks
- Fixed-size buffer pool with pinning, dirty tracking, and LRU-style eviction
- Variable-length records in slotted heap pages
- BSON-inspired documents with nested arrays and documents
- Persistent collection and index catalog, spanning as many pages as it needs
- B-tree point lookups, range scans, duplicate keys, splits, redistribution,
  merges, and deletion
- Sort-preserving scalar index keys, with unique and composite-unique constraints
- ASL lexer, parser, binder, planner, and lazy iterator-based executor
- Filters, projection, ordering, pagination, aggregation, joins, and mutations
- Minimal HTTP/1.1 interface returning JSON
- Persistent ABP/1 connections with text-query, binary-document and keyed-upsert operations
- Optional TTL policies with a background expiry sweeper
- No runtime crate dependencies

## Quick start

Requires a stable Rust toolchain.

```bash
cargo build --release
cargo test
cargo run --release -- data.db
```

The server binds to `127.0.0.1:7070` by default:

```text
GET  /health
POST /query
```

The ABP/1 binary interface listens on `127.0.0.1:7071` by default. Disable it with `--abp-port 0` or select another port with `--abp-port N`. Its framing, opcodes, value encoding, limits, and benchmark methodology are documented in [PROTOCOL.txt](PROTOCOL.txt).

Run a query by sending an ASL statement as the request body:

```bash
curl -X POST http://127.0.0.1:7070/query \
  --data 'create users { id: int, name: string, age: int }'

curl -X POST http://127.0.0.1:7070/query \
  --data 'from users | insert { id: 1, name: "alice", age: 25 }'

curl -X POST http://127.0.0.1:7070/query \
  --data 'from users | where age >= 18 | select name, age'
```

Custom port, bind address, and repeatable TTL policies are supported:

```bash
cargo run --release -- telemetry.db \
  --port 7070 \
  --abp-port 7071 \
  --ttl telemetry.received_at=7d
```

Use `--bind` only on a trusted private network. The current server does not
implement authentication or TLS.

## ASL

ASL is a left-to-right pipeline language. Pipes and commas between stages are
optional; stage keywords delimit the pipeline.

These statements are equivalent:

```text
from users | where age >= 18 | select name, email
from users, where age >= 18, select name, email
from users where age >= 18 select name, email
```

A larger example:

```text
from orders
where status == "shipped" and total > 100
join users on user_id == id
select name, total
order total desc
limit 25
```

Implemented operations include:

- **Sources:** collection scans and index hints
- **Filtering:** comparisons, Boolean operators, membership, field existence,
  and string matching
- **Projection:** select, drop, aliases, arithmetic, and conditional expressions
- **Ordering and pagination:** multi-key order, offset, and limit
- **Aggregation:** group, count, sum, average, min, max, and collect
- **Joins:** inner hash joins
- **Mutations:** insert, update, upsert, and delete
- **DDL:** collections and indexes, with existence guards, unique constraints, and
  composite unique indexes

The complete language reference is in [asl.txt](asl.txt).

## Architecture

```text
HTTP / JSON -----+
                 +-> ASL lexer -> parser -> binder -> planner
ABP/1 EXEC -------+                                  |
                                                     v
                                           lazy physical operators
                                                     |
ABP/1 INSERT ----------------------------------------+
    |
Database / catalog / index manager
    |
B-tree indexes + slotted heap files
    |
buffer pool
    |
4 KiB pages on disk
```

### Storage

The disk manager owns page allocation and free-list reuse. The buffer pool
tracks pinned and dirty frames and flushes pages on eviction. Heap files store
variable-length records in slotted pages linked across the database file.

### Documents and collections

Documents support integers, floats, strings, booleans, nulls, arrays, and nested
documents. A persistent catalog on page 0 tracks collections and index roots so
the database can reopen them after restart.

### Indexing

B-tree nodes fit in one 4 KiB page. The implementation supports root and leaf
splits, duplicate keys ordered by document ID, range scans, deletion,
redistribution, merging, and persisted index metadata. Indexes are backfilled
when created and maintained on insert and delete.

### Query execution

ASL is converted into a bound logical pipeline, planned into physical
operators, and evaluated through lazy iterators. The planner can select indexed
access for supported predicates; execution supports scans, filters, projection,
ordering, grouping, joins, pagination, and mutations.

### Network interfaces and TTL

The binary exposes a small HTTP/1.1 surface for JSON responses and an ABP/1 interface for persistent, length-prefixed binary connections. Both listeners share the same database and query engine. Each connection runs on its own thread while database statements are serialized through one mutex. TTL policies periodically remove documents whose integer timestamp field is older than the configured duration.

## Benchmarks

The checked-in Phase 3 benchmark was run in release mode on an AMD EPYC 74F3
system. Results are machine-specific and are not presented as cross-database
comparisons.

| Workload | Result |
| --- | ---: |
| 10M sequential B-tree inserts | 1.77M ops/s |
| 10M random B-tree inserts | 169.8K ops/s |
| 1M random point lookups | p50 5.01 µs, p95 5.44 µs, p99 6.31 µs |
| 0.1% range scan (1,000 documents) | 0.09 ms |
| 1% range scan (10,000 documents) | 0.72 ms |
| 10% range scan (100,000 documents) | 6.71 ms |

The full command, results, and machine information are preserved in
[phase3_bench.txt](phase3_bench.txt).

## Testing

```bash
cargo test
```

The suite covers persistence, page allocation, buffer-pool eviction, heap-file round trips, document encoding, catalog recovery, B-tree operations, index maintenance, parsing and binding, physical planning, query execution, HTTP and ABP/1 lifecycle behavior, wire encoding, concurrent request serialization, and TTL expiry. The last intentional run completed 239 tests successfully with 3 ignored stress tests.

Larger storage stress tests are marked ignored:

```bash
cargo test -- --ignored
```

## Current limitations

- No stable release, compatibility policy, or migration tooling
- APIs, ABP/1 framing, ASL syntax, and the on-disk format may change
- No transactions or multi-statement atomicity
- No authentication, TLS, or user permissions
- One database statement executes at a time
- One HTTP request per connection; no keep-alive or chunked request bodies
- Single-field indexes only
- Parsed schema declarations are not yet persisted or enforced
- No outer joins, subqueries, window functions, full-text search, or replication

asdb should currently be bound to localhost or kept behind a trusted private
service boundary.

## Project documents

- [asl.txt](asl.txt) — language specification and examples
- [plans.txt](plans.txt) — implementation phases, completed work, and roadmap
- [phase3_bench.txt](phase3_bench.txt) — raw B-tree benchmark output
- [PROTOCOL.txt](PROTOCOL.txt) — ABP/1 framing, decisions, and measurements

## License

MIT. See [LICENSE](LICENSE).

## Authors

Developed by [Shahyar Anfaz](https://github.com/shahyaranfaz) and
[Averi Wylie](https://github.com/AveriWylie).
