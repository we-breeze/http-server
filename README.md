# Breeze HTTP Server

`http-server` is a bounded HTTP/1.1 runtime for internal Breeze services. It
is intentionally separate from the `http` client crate: the two directions
have different connection ownership, lifecycle, and observability needs.

## Data path

- Request line, headers, target, and query borrow the connection's header
  buffer. Fixed-length bodies are collected into an arena-backed `brz_io::Writer`
  under `max_request_body_bytes`, then frozen into a segmented `Reader`.
- JSON parameters use `brz_json::JsonReader` directly over the segments. Strings
  within one segment borrow it; cross-segment strings and decoded escapes use
  additional arena storage for that field. No intermediate `serde_json::Value`
  tree or whole-body merge is needed.
- `Request::body()` preserves contiguous byte access for raw bodies, forms,
  and multipart. A cross-segment body is merged once on demand and cached.
- JSON responses serialize once into a `Writer`, then send its segments with a
  known `Content-Length`. `Response::segmented` also accepts an existing reader
  and sends its unread portion. Owned bytes and download streams keep their
  existing write paths.

The socket receive path includes copying into arena segments. Parsed headers
and single-segment JSON strings need no extra payload copy; response segments
are sent without concatenating the complete body.

Dependencies are pinned to `io v0.0.1`, `json v0.0.1`, and `metrics v0.0.2`
through Git tags.

## Transport scope

HTTP/1.1 with bounded, fixed `Content-Length` bodies and sequential request
handling per connection. Pipelined requests are supported in wire order.
Finite download streams support automatic chunked response framing or a known
`Content-Length`. Chunked request bodies, `Expect: 100-continue`, HTTP/2,
SSE-specific behavior, and WebSockets remain outside this release.

Protocol gateways that need transparent chunked uploads or HTTP upgrades should
own that transport boundary in their integration crate and dispatch ordinary
application requests into this server. Those proxy policies are intentionally
not coupled to the Breeze runtime.

## HTTP filters

Use `HandlerExt::with_filter` to compose transport-independent request and
response policy around any handler. A filter can short-circuit before parameter
extraction and can transform both successful and rejected responses. Nesting
filters preserves middleware ordering: request hooks run outside-in and response
hooks run inside-out.

```rust,no_run
use brz_http_server::{FilterDecision, HandlerExt, HttpFilter, Request, Response};

struct Maintenance;

impl HttpFilter for Maintenance {
    fn before(&self, request: &Request<'_>) -> FilterDecision {
        if request.path().starts_with("/internal/") {
            FilterDecision::respond(Response::empty(
                brz_http_server::StatusCode::SERVICE_UNAVAILABLE,
            ))
        } else {
            FilterDecision::Continue
        }
    }
}

# fn decorate<H: brz_http_server::Handler>(handler: H) -> impl brz_http_server::Handler {
let handler = handler.with_filter(Maintenance);
# handler
# }
```

## API macros

Enable the `macros` and `metrics` features. Define one async free function per route and use
ordinary Rust modules to organize related endpoints. Each function declares its
full path, authentication policy, HTTP inputs, injected dependencies, and output.
`#[api] impl` and `FromState` are no longer supported.

Declare dependency names and types once for each listener group:

```rust
use brz_http_server::{get, handlers, registry};
use std::sync::Arc;

struct AppState { name: String }
struct UserService;

registry!(dependencies(state: Arc<AppState>, users: Arc<UserService>));

#[get("/info/name")]
async fn name<'a>(#[inject(state)] application: &'a AppState) -> &'a str {
    &application.name
}

#[get("/health")]
async fn health() -> bool { true }

fn main() {
    let app_state = Arc::new(AppState { name: "example".into() });
    let user_service = Arc::new(UserService);
    let handler = handlers!(state = app_state, users = user_service).unwrap();
    // Pass handler to Server::bind(address, handler).await.
    drop(handler);
}
```

`handlers!(state, users)` is shorthand for
`handlers!(state = state, users = users)`. Argument order does not affect matching.
Each value expression is evaluated once and moved into a shared container; its
fields do not need to implement `Clone`. Use `Arc<T>` when the caller also needs
shared ownership. Dependencies must be `Send + Sync + 'static` when serving routes.
Initialize fallible or async dependencies before calling `handlers!`.

Always mark injected parameters with `#[inject(dependency_name)]`. The parameter's
local name is independent of the dependency name. `&T` borrows the stored value
with normal Rust deref coercion (for example, `Arc<T>` to `&T`); an owned `T`
clones that dependency using the `Clone` trait on each invocation. `&mut T` is not
supported: shared mutable services should expose their own synchronization.
There is no automatic matching by parameter name or type and no runtime lookup.

Injected and header parameters can appear anywhere without occupying a path-capture position.
Among the remaining parameters, path captures bind first in route order and must
have the capture names. Scalars then bind query keys; one business struct binds
the body. `#[header]` reads a header with the parameter's name (without a raw
identifier's `r#` prefix); `#[header("x-api-key")]` specifies its name explicitly.
Underscores remain underscores. `Option<T>` permits a missing header; `T` requires
one. Repeated annotations and conflicting parameter sources are compile errors.
The route-level `headers(...)` syntax is no longer supported.
`Authenticated<T>` is supplied by the authentication layer. A parameter cannot
bind both a dependency and a path capture or header. Functions remain directly
callable with ordinary Rust arguments, including borrowed inputs and outputs.

For a dependency-free group, use `registry!()` and `handlers!()`.
Modules enroll their functions through normal `mod` inclusion, with no filesystem
scan or module annotation. Separate listeners use explicit groups, not module
names inferred by the macros:

```rust
use brz_http_server::{get, handlers, registry};
registry!(group = admin, dependencies(label: String));

mod endpoints {
    #[brz_http_server::get("/admin/name", group = admin)]
    async fn name(#[inject(label)] label: &str) -> String { label.to_owned() }
}

fn main() {
    let handler = handlers!(label = "admin".into(); group = admin).unwrap();
    drop(handler);
}
```

The default group is `crate::http_apis`. A bare group name resolves from the crate
root; a nested registry uses its full path in both the function attribute and
`handlers!`, for example `group = crate::listeners::admin`.
Group names are logical identifiers. The macros use an internal module prefix,
so a business module can have the same name as a group, including `http_apis`.
Keep using the logical name in route attributes and `handlers!`; for a qualified
path, only the final group segment is mapped to the generated module.
`registry!(group = admin, auth = AdminAuth, dependencies(...))` fixes that group's
authenticator type; use `Server::bind_with_authenticator` to supply its instance.
Set `auth = required` or `auth = optional` on each protected function. The default
is `auth = none`; modules do not implicitly change authentication or route paths.
`consumes` and `produces` default to `json`; `protobuf` is reserved.

`handlers!` returns `Result<Router<A>, RegistryError>` and rejects conflicting
routes before serving. Each endpoint's adapter shares the group's container.
The existing indexed dispatch, 404/405 responses, authentication, borrowed JSON,
streaming responses, and metrics remain available.


A function can combine injected state with path, query, body, and header inputs:

```rust,no_run
use brz_http_server::{ApiResult, post, registry};
use serde::{Deserialize, Serialize};

struct AppState;
registry!(dependencies(state: AppState));

#[derive(Deserialize)]
struct UpdateUser<'a> { name: &'a str }
#[derive(Serialize)]
struct UserView<'a> { id: u64, name: &'a str }

#[post("/v1/users/:id")]
async fn update<'a>(
    #[inject(state)] _state: &AppState,
    id: u64,
    verbose: Option<bool>,
    input: UpdateUser<'a>,
    #[header("x-trace-id")] trace_id: Option<&'a str>,
) -> ApiResult<UserView<'a>> {
    let _ = (verbose, trace_id);
    Ok(UserView { id, name: input.name })
}
```

Invalid parameters return `400`; JSON body routes reject non-JSON content types
with `415`. Query strings are URL-decoded and `Vec<T>` receives repeated keys.
Path routing establishes segment boundaries from raw `/` bytes before decoding
each segment, so `%2F` stays within one capture and is passed to the function as
`/`. Path captures and query values are each percent-decoded exactly once.
`Query<T>`, `Form<T>`, `Multipart`, `Body`, and `&[u8]` retain their existing
query/body extraction behavior. Business failures use `ApiResult<T>` and
`ApiError`, while custom statuses, redirects, and streams remain supported.

See [the function API guide](docs/function-api.md) for compile-time diagnostics
and [the router design](docs/router-buckets.md) for matching and allocation details.

## Authentication

Authentication is a typed API context, not a raw `Authorization` header in a
business method. Function auth defaults to `none`; set `auth = required` on each protected route. `auth = optional`
injects an `Option<Authenticated<T>>`; it treats absent credentials as `None`
but rejects malformed credentials with `401`.

```rust,no_run
use std::future::Future;

use brz_http_server::{
    AuthFailure, AuthRequest, Authenticated, Authenticator,
};
use serde::Serialize;

struct Actor {
    user_id: u64,
}

struct InternalAuth;

impl Authenticator for InternalAuth {
    type Principal = Actor;

    fn authenticate<'a>(
        &'a self,
        request: AuthRequest<'a>,
    ) -> impl Future<Output = Result<Actor, AuthFailure>> + Send + 'a {
        async move {
            let Some(value) = request.header("x-internal-token") else {
                return Err(AuthFailure::missing_credentials("Internal"));
            };
            if value != b"trusted" {
                return Err(AuthFailure::invalid_credentials("Internal"));
            }
            Ok(Actor { user_id: 42 })
        }
    }
}

#[derive(Serialize)]
struct UserView {
    id: u64,
}

#[derive(Serialize)]
struct HealthView {
    ok: bool,
}

brz_http_server::registry!(auth = InternalAuth);

#[brz_http_server::get("/v1/users/:id", auth = required)]
async fn get(id: u64, actor: Authenticated<Actor>) -> UserView {
    let _caller = actor.principal().user_id;
    UserView { id }
}

#[brz_http_server::get("/v1/users/health")]
async fn health() -> HealthView { HealthView { ok: true } }
```

Start it with `Server::bind_with_authenticator(addr, handlers!()?, InternalAuth).await?`.

`AuthRequest` exposes only method, path, headers, and peer address; the body
remains unavailable to authentication. An authenticator must return an owned
principal. A JWT implementation can use `type Principal = Jwt<User>`, yielding
the familiar `Authenticated<Jwt<User>>` business parameter without coupling the
core server to a particular JWT or crypto library.

JSON response encoding uses one serialization pass into arena segments. The
API macro keeps the JSON reader alive across the business handler's awaits and
until borrowed response fields have been serialized. Manual handlers can use
`let json = request.json_body(); let input: Params<'_> = json.decode()?;` with
`Params` deriving `Deserialize`. Each JSON reader has an independent cursor;
raw body access remains available after parsing, including on rejection paths.

## API metrics

The `metrics` feature enables route metrics and the `brz-metrics` dependency.
Without it, metric recording compiles out.

Exported function routes lazily register five `brz-metrics` entries when the
route first matches. The route template remains the metric name while the
status class is represented by the metric type:

```text
API    /users/:id
API3XX /users/:id
API4XX /users/:id
API5XX /users/:id
APITO  /users/:id
```

Classes cover 200–299, 300–399, 400–499, and 500–599 inclusively. Different IDs
and query strings share the template's slots; methods on the same template and
repeated server registrations also share them. Unused classes are registered
with zero counts. There is no per-request metric-name allocation or registration
after the route's metric handles have initialized.

The `APITO` row records the configured request timeout and is not also counted
as `API4XX`. Other counters record the final response status once, before socket writing. They
include authentication/extraction rejections, matched-path 405 responses,
serialization failures, and body/handler timeouts after route identification.
Latency spans route identification through response construction, with the
existing service policy (200 ms slow threshold); 4xx and 5xx also increment
`error_count`. Download completion and socket-write errors are not a second API
observation. Unknown routes, failures before route identification, and closed
connections producing no response do not create API entries.

The metrics dependency is pinned to the Git tag `v0.0.2`.

## Usage

Call `Server::bind(address, handler)` or
`Server::bind_with_authenticator(address, handler, authenticator)`. Both use
default limits and a server-owned arena with two 16 MiB chunks (32 MiB total).
Use the corresponding `_with_config` / `_and_config` entrypoint when setting
application policies such as CORS or validation error mapping.

Defaults are a 15 second request timeout and an 8 MiB fixed-length request-body
limit. At most 64 MiB of request bodies may be retained across concurrent
handlers; set `ServerConfig::max_in_flight_request_body_bytes` when an upload
workload needs a different aggregate budget. Enable `api-log` to emit body-free positional access lines containing
method, raw target, status, millisecond latency, request length, and response
length to `breeze.api`, and
`slow-log` to emit requests taking at least 3 seconds, including a body excerpt
capped at 2 KiB, to `breeze.slow`.

An exported route may opt out of `api.log` while retaining metrics and slow
request logging with `api_log = false`, for example
`#[get("/health", api_log = false)]`.

The API line is positional and contains no key/value fields or body:

```text
2026-09-19 14:03:21 [API] GET /api/items?q=a 200 156ms 128 512
```

An unknown response length is written as `-`.

Slow server lines use the positional form below. The request-body detail is
always the final field:

```text
2026-09-19 14:03:24 [SLOW] http-server POST /api/items 200 3102ms 128 512 127.0.0.1:50000 false {"name":"demo"}
```

Use `ServerConfig::new(arena)` when explicitly sharing an arena with other
Breeze SDKs. Each `EphemeralBytesArena`
contains two chunks and falls back to heap storage if both are live or a frame
is larger than one chunk. Use the same cloned arena in dependent Breeze SDKs
when their short-lived response/request frames should share the process-wide
budget.

## Download streams

An annotated function may return `impl Stream<Item = Result<Bytes, E>> + Send + 'static`
(`E: Display + Send + 'static`). The HTTP client's response byte stream can be
returned directly; it must own its upstream response. The runtime writes each
chunk as it becomes available and bounds read-ahead to one queued chunk.

Use `HttpResponse::new(stream)` for download metadata: `.status(status)`,
`.header(name, value)?`, and `.content_length(length)` when the exact upstream
length is known. Unknown lengths use HTTP/1.1 chunked encoding. Known lengths
are checked while writing; stream errors or mismatched lengths close the
connection. A write error, idle timeout, or cancelled connection drops the
upstream producer. HEAD and bodyless statuses do not poll the stream.

`brz_http_server::StatusCode` re-exports `http::StatusCode`. Return `(StatusCode, T)`
for an explicit status with a JSON value or stream. A plain business value
still receives status 200; redirects use `Redirect::found` (302) or
`Redirect::temporary` (307).

## Verification

```bash
cargo test
cargo test --features macros
cargo clippy --all-targets --all-features -- -D warnings
```

API 指标的完整名称由宏生成的 `concat!` 在编译时确定；注册时缓存指标句柄，请求处理时不拼接指标名称。

## Releases

CI runs formatting, Clippy, and tests. To publish, open **Actions → Publish → Run workflow** on `main`. Leave `retry_tag` empty to allocate the next `v0.0.x` tag. The workflow validates the code, commits the version, pushes the commit and tag atomically, and publishes to crates.io using the organization secret `CARGO_REGISTRY_TOKEN`.

If publication fails after the tag was pushed, rerun with that existing tag in `retry_tag`. A normal push or pull request does not publish. Historical tags retain their original version numbers; use new release tags for registry packages.

## License

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

The workflow releases `brz-http-server-macros` before `brz-http-server` at the same version. It verifies the macro package before tagging, then verifies the main package after the macro becomes available in the registry. Retrying skips an uploaded package only when its checksum matches the locally packaged artifact.
