# Shared route index

`Router<A>` stores endpoint adapters in one vector. Its type depends only on the
listener authenticator, so adding routes does not deepen the handler or future
type. `handlers!` collects function APIs; `Router::new(handler).merge(handler)`
also supports handwritten handlers and merging collected routers.

## Registration and indexing

Each generated route descriptor contains its static path, method bits,
priority, API-log policy, and metric factory. `merge` appends the descriptor and
one boxed API instance. Merging another `Router` moves its entries into the
destination and rebases handler identifiers; it does not retain a nested router.

A `OnceLock` builds an immutable `brz-http-router::RouteIndex` on the first route
query. The shared index owns path parsing and validation, static lookup,
segment-count buckets, the most selective literal selector, priority ordering,
method selection, and raw capture ranges. Further merges invalidate the index.
The server keeps only handler, endpoint, metrics, and API-log metadata beside it.

Exact paths use a byte-length direct index. A length containing one path avoids
hashing; same-length collisions use a map. Dynamic paths are grouped by segment
count. At compile time each group selects the literal position which minimizes
the largest candidate set; routes with a parameter at that position remain a
fallback candidate list. This makes lookup stable when hundreds of routes end
in parameters.

The request path stores eight segment ranges inline using 32-bit offsets.
Ordinary matching therefore allocates nothing and keeps the temporary segment
view compact. Paths deeper than eight segments use a vector overflow and retain
correct matching without a fixed depth limit. HTTP request sizes are already
bounded far below the 32-bit offset limit.

## Server path semantics

The raw request path is separated from the query before routing. Raw `/` bytes
establish segment boundaries; literal comparisons then percent-decode within
each segment. Thus `%2F` remains one route segment while the handler receives
`/`. Only captures of the selected route are materialized as `Cow<str>` values,
and unescaped captures continue to borrow the request head.

The gateway and server intentionally choose different terminal catch-all
semantics through the shared index. Gateway `*rest` requires a remaining segment.
The server preserves its existing behavior in which `/*rest` may capture an
empty remainder. Leading, trailing, and interior empty segments are retained.

The server prepares a route immediately after parsing the request head, before
reading the body. The decision supplies timeout/body-read metrics, the selected
handler and endpoint, decoded captures, API-log policy, and the standard methods
needed for a 405 response. Priority and registration-order tie breaking remain
unchanged. Handwritten handlers without descriptors continue through the legacy
compatibility hooks after indexed resolution.

Only the selected API future is boxed. That request-level allocation belongs to
the dynamic handler adapter; route selection itself does not allocate for normal
paths.

## Validation

- Differential tests compare selection, captures, priority, and allowed methods
  with the original matcher across overlapping and encoded paths.
- Tests cover empty segments, empty server catch-alls, deep paths, eight captures,
  same-length exact paths, 404/405 behavior, and both registration orders.
- A counting allocator verifies that ordinary exact and dynamic lookup has zero
  request-time allocations.
- HTTP integration tests cover authentication, body handling, timeouts, metrics,
  API/slow logging, streaming, and pipelining.

Run `cargo test --all-features`, `cargo test --no-default-features`, and
`cargo clippy --all-targets --all-features -- -D warnings` in this repository.
Run the shared router's tests and clippy checks as well.

## Lookup benchmark

Run `cargo bench --bench router --features macros`. It measures warmed route and
metric selection only, excluding body parsing, handler invocation, network I/O,
and the selected future allocation. Timings vary by machine and load.

One local optimized run after the shared-index migration (ns/op):

| Case | Shared index |
| --- | ---: |
| Static, first of 24 APIs | 103 |
| Static, last of 24 APIs | 104 |
| Parameter, first of 24 APIs | 183 |
| Parameter, last of 24 APIs | 182 |
| Unmatched path | 92 |
| Parameter, last of 128 APIs | 180 |
| Parameter, last of 512 APIs | 181 |
| Distinct literal suffix, 512 APIs | 182 |

The previous server index was faster for the first static entry (about 60 ns)
but depended on registration position: the last dynamic entry measured about
376 ns with 24 APIs and about 5 microseconds with 512 APIs. The shared selector
keeps the common cases close together and removes that route-count degradation.
