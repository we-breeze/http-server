# Flat router with bucket indexes

`Router<A>` stores endpoint adapters in one vector. Its type depends only on the
listener authenticator, so adding routes does not deepen the handler/future type.
`handlers!` collects function APIs; `Router::new(handler).merge(handler)` also
supports handwritten handlers and merging collected routers. Function adapters
within a group share one container of named dependencies.

## Registration and indexing

Each function route macro emits a path/method descriptor and an
endpoint dispatcher. A descriptor contains the template, methods,
priority and fixed metric factory. `merge` appends descriptors and one boxed API
instance. Merging another Router moves its entries into the destination and
rebases handler identifiers; it never retains a nested Router as a handler.

A `OnceLock` builds the immutable index during metric registration at server
startup (or the first explicit route query). Further merges invalidate the index.
Entries are stored contiguously within buckets:

- Static paths of byte length 0 through 127 use a 128-entry directory of ranges.
  Only the corresponding range is searched with exact string comparisons.
- Longer static paths use a sorted directory keyed by byte length. There is no
  new URL length limit.
- Parameter templates use a sorted directory keyed by segment count. Templates
  are parsed once into literal checks and capture positions. Literal checks are
  ordered by their frequency within that segment-count bucket, checking rarer
  literals first and preferring suffixes on ties.
- Terminal catch-all templates are stored separately in priority order; they
  can also match an empty remainder.

The request path is decoded once using the existing lossy UTF-8 percent-decoding
rules, after separating the query. Static hits require no path segmentation.
Parameter/catch-all lookup records segment offsets once on a 32-slot stack fast
path, with a vector fallback for deeper URLs. Leading, trailing and interior
empty segments are retained. Up to eight captured values use inline offsets;
values continue to borrow the decoded path. No candidate handler Future is
created during lookup.

## Selection and invocation

The server prepares the route immediately after parsing the request head,
before reading the body. The decision owns or borrows the decoded path and keeps
handler/group identifiers, capture offsets, metrics and allowed methods. This
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
| Static, first API | 72,072 | 82 |
| Static, last API | 5,884 | 148 |
| Parameter, first API | 68,442 | 167 |
| Parameter, last API | 5,697 | 364 |
| Unmatched path | 73,023 | 293 |

Parameter hits in the last API measured 1,205 ns at 128 APIs and 4,596 ns at
512 APIs. These fixtures deliberately put all parameter routes in the same
segment-count bucket, illustrating the remaining linear candidate scan. Large
buckets can receive a secondary literal-position index in a future change;
that optimization is not required to eliminate merge depth or repeated lookup.
