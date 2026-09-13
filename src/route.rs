use std::borrow::Cow;

use crate::params::decode_component;

/// A no-allocation match result for a route template.
#[derive(Clone, Copy, Debug)]
pub struct RouteMatch<'a> {
    captures: [Option<&'a str>; 8],
    len: usize,
}

impl<'a> RouteMatch<'a> {
    pub(crate) fn from_captures(captures: [Option<&'a str>; 8], len: usize) -> Self {
        Self { captures, len }
    }

    #[must_use]
    pub fn capture(&self, index: usize) -> Option<&'a str> {
        (index < self.len).then(|| self.captures[index]).flatten()
    }
}

/// A compatibility match result which owns decoded path captures when needed.
#[doc(hidden)]
#[derive(Debug)]
pub struct DecodedRouteMatch<'a> {
    captures: [Option<Cow<'a, str>>; 8],
    len: usize,
}

impl DecodedRouteMatch<'_> {
    #[must_use]
    pub fn capture(&self, index: usize) -> Option<&str> {
        (index < self.len)
            .then(|| self.captures[index].as_deref())
            .flatten()
    }
}

/// Matches a raw path against a template containing literal, `:capture`, and
/// terminal `*catch_all` segments. Raw `/` bytes establish segment boundaries;
/// percent decoding is applied within each segment afterwards.
#[doc(hidden)]
#[must_use]
pub fn match_route<'a>(path: &'a str, template: &str) -> Option<DecodedRouteMatch<'a>> {
    let mut captures = std::array::from_fn(|_| None);
    let mut capture_len = 0;
    let encoded = path.as_bytes().contains(&b'%');
    let mut path_segments = path.split('/');
    let mut template_segments = template.split('/');
    let mut path_offset = 0;
    loop {
        match (template_segments.next(), path_segments.next()) {
            (None, None) => {
                return Some(DecodedRouteMatch {
                    captures,
                    len: capture_len,
                });
            }
            (Some(template), segment) if template.starts_with('*') => {
                if capture_len == captures.len() || template_segments.next().is_some() {
                    return None;
                }
                let remainder = if segment.is_some() {
                    &path[path_offset..]
                } else {
                    ""
                };
                captures[capture_len] = Some(if encoded {
                    decode_component(remainder)
                } else {
                    Cow::Borrowed(remainder)
                });
                return Some(DecodedRouteMatch {
                    captures,
                    len: capture_len + 1,
                });
            }
            (Some(template), Some(path)) if template.starts_with(':') && !path.is_empty() => {
                if capture_len == captures.len() {
                    return None;
                }
                captures[capture_len] = Some(if encoded {
                    decode_component(path)
                } else {
                    Cow::Borrowed(path)
                });
                capture_len += 1;
                path_offset += path.len() + 1;
            }
            (Some(template), Some(path))
                if if encoded {
                    decode_component(path) == template
                } else {
                    path == template
                } =>
            {
                path_offset += path.len() + 1;
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::match_route;

    #[test]
    fn raw_slashes_define_segments_before_component_decoding() {
        let matched = match_route("/files/a%2Fb", "/files/:key").unwrap();
        assert_eq!(matched.capture(0), Some("a/b"));
        assert!(match_route("/files/a%2Fb", "/files/:parent/:name").is_none());
        assert!(match_route("/files/a/b", "/files/:key").is_none());
    }

    #[test]
    fn literals_and_captures_decode_once() {
        assert!(match_route("/users/%E4%B8%AD", "/users/中").is_some());
        let matched = match_route("/files/a%252Fb", "/files/:key").unwrap();
        assert_eq!(matched.capture(0), Some("a%2Fb"));
    }
}
