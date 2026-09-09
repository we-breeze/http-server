## Function APIs and named dependencies

Enable the `macros` and `metrics` features. Define one async free function per route and use
ordinary Rust modules to organize related endpoints. Each function declares its
full path, authentication policy, HTTP inputs, injected dependencies, and output.
`#[api] impl` and `FromState` are no longer supported.

The `metrics` flag is reserved for future conditional collection. Existing route
metrics currently remain enabled even when this flag is omitted.

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

### Compile-time diagnostics

An unknown injection name is a compile-time error:

```compile_fail,E0609
use brz_http_server::{get, registry};
registry!(dependencies(primary: String));
#[get("/")]
async fn read(#[inject(replica)] db: &str) -> String { db.to_owned() }
# fn main() {}
```

Injection types must match the named dependency:

```compile_fail,E0308
use brz_http_server::{get, registry};
registry!(dependencies(state: String));
#[get("/")]
async fn read(#[inject(state)] state: &u64) -> u64 { *state }
# fn main() {}
```

All declared dependencies must be supplied:

```compile_fail,E0063
use brz_http_server::{handlers, registry};
registry!(dependencies(state: String, users: String));
fn main() { let _ = handlers!(state = String::new()); }
```

Unknown names and incorrect value types are rejected at construction:

```compile_fail,E0560
use brz_http_server::{handlers, registry};
registry!(dependencies(state: String));
fn main() { let _ = handlers!(state = String::new(), other = 42); }
```

```compile_fail,E0308
use brz_http_server::{handlers, registry};
registry!(dependencies(state: String));
fn main() { let _ = handlers!(state = 42); }
```

The injection source must be explicit:

```compile_fail
use brz_http_server::{get, registry};
registry!(dependencies(state: String));
#[get("/")]
async fn read(#[inject] state: &str) -> String { state.to_owned() }
# fn main() {}
```

Shared dependencies cannot be injected as exclusive mutable references:

```compile_fail
use brz_http_server::{get, registry};
registry!(dependencies(state: String));
#[get("/")]
async fn read(#[inject(state)] state: &mut String) -> String { state.clone() }
# fn main() {}
```

Owned injection requires `Clone`; borrowed injection does not:

```compile_fail,E0277
use brz_http_server::{get, registry};
struct State;
registry!(dependencies(state: State));
#[get("/")]
async fn read(#[inject(state)] _state: State) -> bool { true }
# fn main() {}
```
