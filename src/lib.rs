//! A bounded HTTP/1.1 server for short-lived Breeze request/response frames.
//!
//! The server receives each request into one connection buffer, parses its
//! request line, headers, and fixed-length body as borrowed slices, and calls
//! the handler before that buffer is reused. A handler writes an owned response
//! body into [`EphemeralBytesArena`]; the server writes the HTTP head and that
//! body as separate socket buffers without concatenating or copying the body.
//!
//! This is deliberately a small HTTP/1.1 runtime, rather than a general web
//! framework. Chunked request bodies, HTTP/2, streaming response bodies, and
//! routing are outside the initial contract.

mod auth;
mod error;
mod extract;
mod json;
mod request;
mod response;
mod route;
mod server;

pub use auth::{AuthFailure, AuthRequest, Authenticated, Authenticator, NoAuthenticator};
pub use brz_ds::{EphemeralBytes, EphemeralBytesArena, EphemeralBytesMut};
pub use error::{Error, Result};
pub use extract::{ExtractError, FromHeader, FromPath, FromQuery};
pub use json::{ApiError, ApiResult, IntoHttpError};
pub use request::{Header, Request};
pub use response::{HeaderBlock, HeaderBlockError, Response, ResponseBody, StatusCode};
pub use server::{Handler, Server, ServerConfig};

#[cfg(feature = "macros")]
pub use http_server_macros::{api, delete, get, patch, post, put};

/// Implementation details used by `#[http_server::api]` generated code.
#[doc(hidden)]
pub mod __private {
    pub use crate::auth::{authenticate_optional, authenticate_required};
    pub use crate::extract::{
        header_optional, header_required, path, query_optional, query_required,
    };
    pub use crate::json::is_json_content_type;
    pub use crate::json::{json_body, json_response, json_result_response};
    pub use crate::route::match_route;
}
