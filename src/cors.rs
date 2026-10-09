pub use brz_http_cors::Cors;

use crate::{Request, Response};

pub(crate) fn preflight(cors: &Cors, request: &Request<'_>) -> Option<Response> {
    let response = cors.preflight(brz_http_cors::PreflightRequest {
        method: request.method(),
        origin: request.header("origin"),
        request_method: request.header("access-control-request-method"),
        request_headers: request.header("access-control-request-headers"),
    })?;
    Some(
        Response::static_bytes(response.status, response.body)
            .with_http_headers(&response.headers, request.response_arena()),
    )
}
