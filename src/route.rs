/// A no-allocation match result for a route template.
#[derive(Clone, Copy, Debug)]
pub struct RouteMatch<'a> {
    captures: [Option<&'a str>; 8],
    len: usize,
}

impl<'a> RouteMatch<'a> {
    #[must_use]
    pub fn capture(&self, index: usize) -> Option<&'a str> {
        (index < self.len).then(|| self.captures[index]).flatten()
    }
}

/// Matches a path against a static template containing literal and `:capture`
/// segments. Templates are validated by the API macro and have at most eight
/// captures.
#[doc(hidden)]
#[must_use]
pub fn match_route<'a>(path: &'a str, template: &str) -> Option<RouteMatch<'a>> {
    let mut captures = [None; 8];
    let mut capture_len = 0;
    let mut path_segments = path.split('/');
    let mut template_segments = template.split('/');
    let mut path_offset = 0;
    loop {
        match (template_segments.next(), path_segments.next()) {
            (None, None) => {
                return Some(RouteMatch {
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
                captures[capture_len] = Some(remainder);
                return Some(RouteMatch {
                    captures,
                    len: capture_len + 1,
                });
            }
            (Some(template), Some(path)) if template.starts_with(':') && !path.is_empty() => {
                if capture_len == captures.len() {
                    return None;
                }
                captures[capture_len] = Some(path);
                capture_len += 1;
                path_offset += path.len() + 1;
            }
            (Some(template), Some(path)) if template == path => {
                path_offset += path.len() + 1;
            }
            _ => return None,
        }
    }
}
