# Breeze HTTP Server

`http-server` is a bounded HTTP/1.1 runtime for internal Breeze services. It
is intentionally separate from the `http` client crate: the two directions
have different connection ownership, lifecycle, and observability needs.

## Data path

- Request line, headers, target, query, and a fixed-length body borrow the
  connection receive buffer. The handler finishes before that buffer is reused.
- Response bodies are written once into `EphemeralBytesArena` and held by an
  `EphemeralBytes` allocation until the socket write completes.
- The server writes the generated HTTP head and body with vectored writes; it
  never concatenates or copies the arena-backed body into another framework
  buffer.

The unavoidable kernel-to-user-space receive copy still exists. “Zero-copy”
here means no additional framework copy for parsed request fields or an
arena-backed response body.

## First-version scope

HTTP/1.1 with bounded, fixed `Content-Length` bodies and sequential request
handling per connection. Pipelined requests are supported in wire order.
Chunked request bodies, `Expect: 100-continue`, HTTP/2, streaming responses,
and WebSockets are intentionally outside the initial contract.

## API macros

Enable the `macros` feature and define APIs in business terms. `Request` stays
inside generated transport code.

```toml
[dependencies]
http-server = { version = "0.1", features = ["macros"] }
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

#[api(prefix = "/v1/users")]
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
keys with the same name; one remaining non-scalar parameter is the JSON body.
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

#[api(prefix = "/v1/users", auth = required)]
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

Start it with `Server::bind_with_authenticator(addr, UserApi, InternalAuth,
config).await?`.

`AuthRequest` exposes only method, path, headers, and peer address; the body
remains unavailable to authentication. An authenticator must return an owned
principal. A JWT implementation can use `type Principal = Jwt<User>`, yielding
the familiar `Authenticated<Jwt<User>>` business parameter without coupling the
core server to a particular JWT or crypto library.

JSON response encoding counts the exact output size first, then writes directly
into its final arena allocation. It adds no body copy, though it deliberately
uses two serialization passes. A borrowed `Deserialize<'a>` DTO can borrow
unescaped JSON strings from the receive buffer; decoding escaped strings or
using owned DTO fields may allocate by codec design.

## Usage

The application chooses arena capacity at startup. Each `EphemeralBytesArena`
contains two chunks and falls back to heap storage if both are live or a frame
is larger than one chunk. Use the same cloned arena in dependent Breeze SDKs
when their short-lived response/request frames should share the process-wide
budget.

## Verification

```bash
cargo test
cargo test --features macros
cargo clippy --all-targets --all-features -- -D warnings
```
