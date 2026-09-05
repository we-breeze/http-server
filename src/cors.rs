use http::header::{HeaderMap, HeaderName, HeaderValue};

use crate::{EphemeralBytesArena, Request, Response, StatusCode};

/// Application origin policy, applied by the server before authentication.
#[derive(Clone, Debug)]
pub struct Cors {
    pub allow_origins: Vec<String>,
    pub allow_methods: Vec<String>,
    pub allow_headers: Vec<String>,
    pub expose_headers: Vec<String>,
    pub allow_credentials: bool,
    pub max_age: u64,
}

impl Default for Cors {
    fn default() -> Self {
        Self {
            allow_origins: Vec::new(),
            allow_methods: ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]
                .map(str::to_owned)
                .to_vec(),
            allow_headers: Vec::new(),
            expose_headers: Vec::new(),
            allow_credentials: false,
            max_age: 600,
        }
    }
}

impl Cors {
    #[must_use]
    pub fn permissive() -> Self {
        Self {
            allow_origins: vec!["*".into()],
            allow_headers: vec!["*".into()],
            ..Self::default()
        }
    }

    pub(crate) fn validate(&self) -> bool {
        self.allow_origins
            .iter()
            .all(|value| HeaderValue::from_str(value).is_ok())
            && self
                .allow_methods
                .iter()
                .all(|value| http::Method::from_bytes(value.as_bytes()).is_ok())
            && self
                .allow_headers
                .iter()
                .chain(&self.expose_headers)
                .all(|value| value == "*" || HeaderName::from_bytes(value.as_bytes()).is_ok())
    }

    fn permits_origin(&self, origin: &str) -> bool {
        self.allow_origins
            .iter()
            .any(|allowed| allowed == "*" || allowed == origin)
    }

    fn response_headers(&self, origin: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if !self.permits_origin(origin) {
            return headers;
        }
        let reflected =
            self.allow_credentials || !self.allow_origins.iter().any(|allowed| allowed == "*");
        if let Ok(value) = HeaderValue::from_str(if reflected { origin } else { "*" }) {
            headers.insert("access-control-allow-origin", value);
        }
        if reflected {
            headers.insert("vary", HeaderValue::from_static("Origin"));
        }
        if self.allow_credentials {
            headers.insert(
                "access-control-allow-credentials",
                HeaderValue::from_static("true"),
            );
        }
        if !self.expose_headers.is_empty() {
            insert(
                &mut headers,
                "access-control-expose-headers",
                &self.expose_headers.join(", "),
            );
        }
        headers
    }

    pub(crate) fn preflight(&self, request: &Request<'_>) -> Option<Response> {
        if request.method() != "OPTIONS" {
            return None;
        }
        let origin = std::str::from_utf8(request.header("origin")?).ok()?;
        let method = std::str::from_utf8(request.header("access-control-request-method")?).ok()?;
        let requested_headers = request
            .header("access-control-request-headers")
            .and_then(|value| std::str::from_utf8(value).ok())
            .unwrap_or_default();
        let headers_allowed = self.allow_headers.iter().any(|value| value == "*")
            || requested_headers
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .all(|name| {
                    [
                        "accept",
                        "accept-language",
                        "content-language",
                        "content-type",
                    ]
                    .iter()
                    .any(|allowed| name.eq_ignore_ascii_case(allowed))
                        || self
                            .allow_headers
                            .iter()
                            .any(|allowed| name.eq_ignore_ascii_case(allowed))
                });
        let allowed = self.permits_origin(origin)
            && self.allow_methods.iter().any(|allowed| allowed == method)
            && headers_allowed;
        let mut headers = self.response_headers(origin);
        insert(
            &mut headers,
            "access-control-allow-methods",
            &self.allow_methods.join(", "),
        );
        insert(
            &mut headers,
            "access-control-max-age",
            &self.max_age.to_string(),
        );
        if !requested_headers.is_empty() && headers_allowed {
            insert(
                &mut headers,
                "access-control-allow-headers",
                requested_headers,
            );
        }
        let response = if allowed {
            Response::static_bytes(StatusCode::OK, b"OK")
        } else {
            Response::static_bytes(StatusCode::BAD_REQUEST, b"Disallowed CORS request")
        };
        Some(
            response
                .content_type("text/plain; charset=utf-8")
                .with_http_headers(&headers, request.response_arena()),
        )
    }

    pub(crate) fn apply(
        &self,
        origin: Option<&[u8]>,
        response: Response,
        arena: &EphemeralBytesArena,
    ) -> Response {
        let Some(origin) = origin.and_then(|value| std::str::from_utf8(value).ok()) else {
            return response;
        };
        response.with_http_headers(&self.response_headers(origin), arena)
    }
}

fn insert(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}
