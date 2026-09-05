use crate::{EphemeralBytesArena, Response, StatusCode};

/// The binding that failed before a business method could run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectionKind {
    Path,
    Query,
    Header,
    Body,
    ContentType,
}

/// Binding failure metadata for an application's HTTP error mapping.
#[derive(Debug)]
pub struct Rejection {
    pub kind: RejectionKind,
    pub parameter: &'static str,
    pub input: Option<String>,
    pub message: String,
}

impl Rejection {
    #[must_use]
    pub fn new(
        kind: RejectionKind,
        parameter: &'static str,
        input: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            parameter,
            input: input.map(str::to_owned),
            message: message.into(),
        }
    }
}

pub type RejectionHandler = fn(Rejection, &EphemeralBytesArena) -> Response;

#[allow(clippy::needless_pass_by_value)] // Matches the application callback signature.
pub(crate) fn default_rejection(error: Rejection, _arena: &EphemeralBytesArena) -> Response {
    Response::empty(if error.kind == RejectionKind::ContentType {
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    } else {
        StatusCode::BAD_REQUEST
    })
}
