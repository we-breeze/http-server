# Flat router with bucket indexes

`Router<A>` stores endpoint adapters in one vector. Its type depends only on the
listener authenticator, so adding routes does not deepen the handler/future type.
`handlers!` collects function APIs; `Router::new(handler).merge(handler)` also
supports handwritten handlers and merging collected routers. Function adapters
within a group share one container of named dependencies.

## Registration and indexing

Each function route macro emits a path/method descriptor, a constant segment
program, and an endpoint dispatcher. The segment program contains literal and
capture positions, segment count, and an optional terminal catch-all position;
the router does not parse macro-generated templates at startup. A descriptor
also contains the methods, priority and fixed metric factory. `merge` appends
descriptors and one boxed API instance. Merging another Router moves its entries
into the destination and rebases handler identifiers; it never retains a nested
Router as a handler. Handwritten descriptors retain a startup parsing fallback.

A `OnceLock` builds the immutable index during metric registration at server
startup (or the first explicit route query). Further merges invalidate the index.
Entries are stored contiguously within buckets:

- Static paths of byte length 0 through 127 use a 128-entry directory of ranges.
  Only the corresponding range is searched with exact string comparisons.
- Longer static paths use a sorted directory keyed by byte length. There is no
  new URL length limit.
- Parameter templates use a sorted directory keyed by segment count. Templates
  ending in a literal use that final segment as a secondary hash index; routes
  ending in a parameter form a fallback list. The two candidate lists are merged
  in priority order without a request allocation. Remaining literal checks are
  ordered by their frequency within that segment-count bucket, checking rarer
  literals first and preferring suffixes on ties.
- Terminal catch-all templates are stored separately in priority order; they
  can also match an empty remainder.

The raw request path is separated from the query before routing. Raw `/` bytes
establish segment boundaries, then individual segments are percent-decoded for
literal comparison and selected captures. Thus `%2F` remains inside one route
segment while the handler receives `/`; selected captures are decoded once. Plain
static hits require neither segmentation nor decoding. An encoded static path
which misses that fast path uses a segment-count fallback so, for example,
`/users/%E4%B8%AD` matches `/users/中`. Parameter/catch-all lookup records raw
segment offsets once on a 32-slot stack fast path, with a vector fallback for
deeper URLs. Leading, trailing and interior empty segments are retained. Up to
eight selected captures borrow raw segments when possible and own only decoded
values that require it. No candidate handler Future is created during lookup.

## Selection and invocation

The server prepares the route immediately after parsing the request head,
before reading the body. The decision borrows the raw path and keeps decoded
selected captures, handler/group identifiers, metrics and allowed methods. This
same decision supplies timeout/body-read metrics and subsequent dispatch.

Static path hits return immediately only when the method also matches. Otherwise
lookup continues through parameters and catch-alls. Among eligible templates,
existing route priority and registration order are preserved. If no method
matches, all matching templates contribute to Allow, and the most specific
matching template supplies 405 metrics. Unmatched paths remain 404. Unknown
methods do not cause path-only static hits to bypass 405 handling.

The selected API's generated dispatcher uses its group identifier and method;
it does not repeat template matching. The transport request and extracted body
remain borrowed. The internal dynamic adapter boxes only the selected Future,
adding a request-level allocation and dynamic polling. This is an explicit
tradeoff against recursive static composition, not a claim of allocation-free
routing or a guaranteed end-to-end throughput increase.

Handwritten Handler implementations continue to work directly with Server.
When merged, implementations without descriptors use their existing priority,
metrics and method hooks as a compatibility path. They are queried separately;
the no-repeat guarantee applies to indexed function APIs. Generated adapters
call their business functions directly after extraction.

## Validation

- Differential index tests compare route selection, captures and allowed methods
  with the original matcher, including overlapping templates and encoded paths.
- Boundaries cover lengths 127/128/129, Unicode byte lengths, same-length
  collisions, more than 32 segments, eight captures and empty catch-all values.
- HTTP tests cover typed authentication, distinct API states, nested merges,
  merging after index initialization, registration-order ties, 404/405 and body
  timeouts. Instrumented handlers fail if indexed invocation repeats matching;
  the server is checked to prepare each request exactly once.
- A 512-addition test compiles under the default recursion limit and verifies
  that Router can be reassigned after every merge.
- Existing JSON borrowing, response, streaming and metrics tests remain required.

Run `cargo test --workspace --features macros`, `cargo test --workspace
--no-default-features`, and `cargo clippy --workspace --all-targets --features
macros -- -D warnings`.

## Lookup benchmark

Run `cargo bench --bench router --features macros`. The benchmark retains the
previous generic Router as a baseline and uses identical API descriptors and
cached metrics. It measures route/metric selection only, with two routes per
API, warmed indexes, 20,000 iterations per sample and the median of three
samples. It does not measure body parsing, business logic, network I/O or the
new adapter's Future allocation/polling. It is not an end-to-end QPS benchmark.

One local optimized run (ns/op; machine/load dependent):

| Case, 24 APIs / 48 routes | Previous Router | Bucket index |
| --- | ---: | ---: |
| Static, first API | 63,439 | 67 |
| Static, last API | 5,203 | 111 |
| Parameter, first API | 65,282 | 165 |
| Parameter, last API | 5,602 | 385 |
| Unmatched path | 68,730 | 264 |

Parameter hits in the last API measured 1,332 ns at 128 APIs and 5,070 ns at
512 APIs. These fixtures deliberately end every parameter route in a capture,
so they share the fallback list and illustrate its remaining linear scan. A
separate 512-route fixture with distinct literal suffixes measured 340 ns using
the secondary index. A future index may select a discriminating literal at any
position for large terminal-capture buckets.
