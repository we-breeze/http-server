//! A bounded HTTP/1.1 server for short-lived Breeze request/response frames.
//!
//! The server receives each request into one connection buffer, parses its
//! request line, headers, and fixed-length body as borrowed slices, and calls
//! the handler before that buffer is reused. A handler writes an owned response
//! body into [`EphemeralBytesArena`]; the server writes the HTTP head and that
//! body as separate socket buffers without concatenating or copying the body.
//!
//! This is deliberately a small HTTP/1.1 runtime, rather than a general web
//! framework. Chunked request bodies and HTTP/2 are outside the current contract.
//! API methods may return an owned byte stream for finite downloads.

mod api_metrics;
mod auth;
mod body;
mod cors;
mod error;
mod extract;
mod json;
mod params;
mod rejection;
mod reply;
mod request;
mod response;
mod route;
mod router;
mod server;
mod stream;

#[doc(hidden)]
pub use api_metrics::ApiMetrics;

pub use auth::{AuthFailure, AuthRequest, Authenticated, Authenticator, NoAuthenticator};
pub use body::{Body, BodyError, Form, Multipart, Upload};
pub use brz_ds::{EphemeralBytes, EphemeralBytesArena, EphemeralBytesMut};
pub use bytes::Bytes;
pub use cors::Cors;
pub use error::{Error, Result};
pub use extract::{ExtractError, FromHeader, FromPath, FromQuery};
pub use futures_util::Stream;
pub use json::{ApiError, ApiResult, IntoHttpError};
pub use params::Query;
pub use rejection::{Rejection, RejectionHandler, RejectionKind};
pub use reply::{Binary, Html, HttpResponse, IntoHttpResponse, Redirect, Text};
pub use request::{Header, Request};
pub use response::{HeaderBlock, HeaderBlockError, Response, ResponseBody, StatusCode};
pub use router::Router;
pub use server::{Handler, Server, ServerConfig};
pub use stream::ResponseStream;

#[cfg(feature = "macros")]
pub use http_server_macros::{api, delete, get, head, options, patch, post, put};

/// Implementation details used by `#[http_server::api]` generated code.
#[doc(hidden)]
pub mod __private {
    pub use crate::auth::{authenticate_optional, authenticate_required};
    pub use crate::body::{form, multipart};
    pub use crate::extract::{
        header_optional, header_required, path, query_many, query_optional, query_required,
    };
    pub use crate::json::is_json_content_type;
    pub use crate::json::{json_body, json_response, json_result_response};
    pub use crate::params::{QueryParams, decode_path, query_object};
    pub use crate::reply::kind;
    pub use crate::reply::{response, result_response};
    pub use crate::route::match_route;
    pub use crate::router::unmatched;
}
