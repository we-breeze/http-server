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

## API macros

Enable the `macros` feature and define APIs in business terms. `Request` stays
inside generated transport code. `#[api]` registers in the default group;
declare the group as shown under **Composing API groups**. The standalone
examples use `register = false` for manually constructed handlers.

```toml
[dependencies]
http-server = { git = "https://github.com/we-breeze/http-server.git", tag = "v0.0.6", features = ["macros"] }
serde = { version = "1", features = ["derive"] }
```

`prefix`, `consumes`, `produces`, and `auth` belong on the API and default to
`""`, `json`, `json`, and `none`. Method options inherit API values and may
override either codec or auth mode. `protobuf` is reserved for a later codec
implementation.

```rust,no_run
use http_server::{ApiResult, api};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct UpdateUser<'a> {
    name: &'a str,
}

#[derive(Serialize)]
struct UserView<'a> {
    id: u64,
    name: &'a str,
}

struct UserApi;

#[api(prefix = "/v1/users", register = false)]
impl UserApi {
    #[http_server::get("/:id")]
    async fn get(&self, id: u64, verbose: Option<bool>) -> UserView<'static> {
        let _ = verbose;
        UserView { id, name: "read" }
    }

    #[http_server::post("/:id", headers(trace_id = "x-trace-id"))]
    async fn update<'a>(
        &self,
        id: u64,
        verbose: Option<bool>,
        input: UpdateUser<'a>,
        trace_id: Option<&'a str>,
    ) -> ApiResult<UserView<'a>> {
        let _ = (verbose, trace_id);
        Ok(UserView { id, name: input.name })
    }
}
```

The macro implements `Handler` with static route dispatch: there is no route
map, boxed handler, or exposed transport request. A route's captures bind the
first parameters in route order. Remaining scalar parameters come from query
keys with the same name. Query values are URL-decoded, and `Vec<T>` receives
repeated keys. A business struct binds the JSON body; `Form<T>`, `Multipart`,
and `&[u8]` bind URL-encoded, multipart, and raw bodies respectively.
`Authenticated<T>` is an explicit exception: it is supplied by the
authentication layer instead of body decoding.
`headers(...)` is explicit because a plain `&str` cannot otherwise be
distinguished from a query parameter. Invalid parameters return `400`; a JSON
body route rejects non-JSON `Content-Type` with `415`; an otherwise matched
path and wrong method returns `405` with `Allow`.

Business failures return `ApiResult<T>`. For example,
`Err(ApiError::forbidden("not permitted"))` produces a JSON `403 Forbidden`
response. Use `403` only after authentication identified the caller; missing
or invalid authentication belongs to `401 Unauthorized`.

## Composing API groups

Declare a group once in the application crate root, and register each API
beside its methods. `FromState<S>` constructs API instances from the group's
state type; it can retain shared state or select API-specific dependencies.

```rust,no_run
use std::sync::Arc;
use http_server::api;

struct AppState {
    service_name: String,
}

http_server::registry!(state = Arc<AppState>);

#[derive(http_server::FromState)]
struct InfoApi {
    state: Arc<AppState>,
}

#[api(prefix = "/info")]
impl InfoApi {
    #[http_server::get("/name")]
    async fn name(&self) -> String {
        self.state.service_name.clone()
    }
}

async fn bind() -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(AppState { service_name: "example".into() });
    let handler = http_server::handlers!(state)?;
    let _server = http_server::Server::bind("127.0.0.1:8080".parse()?, handler).await?;
    Ok(())
}
```

Use normal Rust `mod` declarations to include API modules. `#[api]` enrolls
the API in the default group, `crate::http_apis`; it does not search source
files. Use `register = false` to opt out. Each group uses one concrete state
and authenticator type. For an authenticated listener,
declare `registry!(state = Arc<AppState>, auth = AppAuth)` and pass the
`AppAuth` instance to `Server::bind_with_authenticator`.

`AppState` is an example name and can live in any module. When an API has
exactly one named `state` field, use `#[derive(http_server::FromState)]`. The
derive infers the state type and clones it; the field may be private. Only
the field type needs `Clone`, so `Arc<T>` works even when `T` is not `Clone`.
Generics and existing where clauses are preserved.

For APIs with additional fields or custom construction, implement `FromState<S>`
manually and omit the derive. Manual implementations can use any field layout.
The declared group state type must match `S`.

To bind a second group on another address, name the group on the API and pass
that name as the second argument to `handlers!`. Groups can use different
authenticator types. This example keeps the public default group above and
adds an authenticated admin listener:

```rust,ignore
http_server::registry!(group = admin, state = Arc<AppState>, auth = AdminAuth);

#[http_server::api(prefix = "/admin", group = admin, auth = required)]
impl AdminApi {
    // Annotated methods; AdminApi implements FromState<Arc<AppState>>.
}

let public_server = http_server::Server::bind(
    "0.0.0.0:8080".parse()?,
    http_server::handlers!(state)?,
).await?;
let admin_server = http_server::Server::bind_with_authenticator(
    "127.0.0.1:9090".parse()?,
    http_server::handlers!(state, admin)?,
    admin_auth,
).await?;
```

The short group name resolves from the crate root. For a group declared in a
nested module, use the same explicit path in both places:
`#[api(group = crate::listeners::admin)]` and
`handlers!(state, crate::listeners::admin)?`. Serve both listeners under the
application's shutdown lifecycle.

`handlers!` returns `Result<Router<A>, RegistryError>`. It constructs APIs once
at startup and rejects equal-priority routes whose paths and methods overlap;
registration order does not choose between conflicting handlers. Initialize
fallible or asynchronous dependencies before calling it. Generic implementations
need concrete specialization to register; use `register = false` when
constructing and merging generic API instances manually.

`#[api(register = false)]` types are explicitly composable without a group
declaration or `FromState` implementation. A collected group can be merged
with a manually constructed readiness API that uses `register = false`:

```rust,ignore
let handler = http_server::Router::new(readiness_api)
    .merge(http_server::handlers!(state)?);
```

`Router<A>` has a fixed type for each authenticator, regardless of how many API
instances are merged. Each API retains its own state type. Merging routers
flattens their entries. Do not manually merge an API that is also registered.

At startup, static paths are bucketed by byte length and parameter paths by
segment count; catch-all routes are handled separately. The server selects the
route once before reading the body, shares that selection with metrics, and
invokes the selected API group directly. The composition boundary boxes the
selected handler Future once per invocation. A standalone macro API retains
static dispatch without that adapter.

See [the router design](docs/router-buckets.md) for matching compatibility,
index layout, allocation tradeoffs and validation.

## Authentication

Authentication is a typed API context, not a raw `Authorization` header in a
business method. API auth defaults to `none`; use `required` at API scope and
override an individual public route with `auth = none`. `auth = optional`
injects an `Option<Authenticated<T>>`; it treats absent credentials as `None`
but rejects malformed credentials with `401`.

```rust,no_run
use std::future::Future;

use http_server::{
    AuthFailure, AuthRequest, Authenticated, Authenticator, api,
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

struct UserApi;

#[api(prefix = "/v1/users", auth = required, register = false)]
impl UserApi {
    #[http_server::get("/:id")]
    async fn get(&self, id: u64, actor: Authenticated<Actor>) -> UserView {
        let _caller = actor.principal().user_id;
        UserView { id }
    }

    #[http_server::get("/health", auth = none)]
    async fn health(&self) -> HealthView {
        HealthView { ok: true }
    }
}
```

Start it with `Server::bind_with_authenticator(addr, UserApi, InternalAuth).await?`.

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

Exported `#[api]` routes automatically register four `brz-metrics` entries when
binding the server. Profile output uses type `API` and names based on the full
route template (including its prefix):

```text
/users/:id_2xx
/users/:id_3xx
/users/:id_4xx
/users/:id_5xx
```

Classes cover 200–299, 300–399, 400–499, and 500–599 inclusively. Different IDs
and query strings share the template's slots; methods on the same template and
repeated server registrations also share them. Unused classes are registered
with zero counts. There is no per-request metric-name allocation or registration
after the route's metric handles have initialized.

Counters record the final response status once, before socket writing. They
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

Use `ServerConfig::new(arena)` when explicitly sharing an arena with other
Breeze SDKs. Each `EphemeralBytesArena`
contains two chunks and falls back to heap storage if both are live or a frame
is larger than one chunk. Use the same cloned arena in dependent Breeze SDKs
when their short-lived response/request frames should share the process-wide
budget.

## Download streams

An annotated method may return `impl Stream<Item = Result<Bytes, E>> + Send + 'static`
(`E: Display + Send + 'static`). The HTTP client's response byte stream can be
returned directly; it must own its upstream response. The runtime writes each
chunk as it becomes available and bounds read-ahead to one queued chunk.

Use `HttpResponse::new(stream)` for download metadata: `.status(status)`,
`.header(name, value)?`, and `.content_length(length)` when the exact upstream
length is known. Unknown lengths use HTTP/1.1 chunked encoding. Known lengths
are checked while writing; stream errors or mismatched lengths close the
connection. A write error, idle timeout, or cancelled connection drops the
upstream producer. HEAD and bodyless statuses do not poll the stream.

`http_server::StatusCode` re-exports `http::StatusCode`. Return `(StatusCode, T)`
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
