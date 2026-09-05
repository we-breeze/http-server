use std::borrow::Cow;
use std::ops::Deref;

use serde::de::DeserializeOwned;

/// A query object decoded using its Serde field names, defaults and collections.
#[derive(Debug)]
pub struct Query<T>(pub T);

impl<T> Deref for Query<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// Decoded query pairs retained until the business method completes.
#[doc(hidden)]
pub struct QueryParams<'a>(Vec<(Cow<'a, str>, Cow<'a, str>)>);

impl<'a> QueryParams<'a> {
    #[must_use]
    pub fn new(raw: Option<&'a str>) -> Self {
        Self(form_urlencoded::parse(raw.unwrap_or_default().as_bytes()).collect())
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_ref())
    }

    pub fn values<'b>(&'b self, key: &'b str) -> impl Iterator<Item = &'b str> {
        self.0
            .iter()
            .filter(move |(name, _)| name == key)
            .map(|(_, value)| value.as_ref())
    }
}

#[doc(hidden)]
pub fn query_object<T: DeserializeOwned>(
    raw: Option<&str>,
) -> Result<Query<T>, serde_html_form::de::Error> {
    serde_html_form::from_str(raw.unwrap_or_default()).map(Query)
}

#[doc(hidden)]
#[must_use]
pub fn decode_path(raw: &str) -> Cow<'_, str> {
    percent_encoding::percent_decode_str(raw).decode_utf8_lossy()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_decodes_reserved_characters_after_splitting_and_preserves_repetitions() {
        let params = QueryParams::new(Some(
            "q=old&q=hello%20world%2B%26x%3Dy+%E4%B8%AD&%69ds=1&ids=2",
        ));
        assert_eq!(params.get("q"), Some("hello world+&x=y 中"));
        assert_eq!(params.values("ids").collect::<Vec<_>>(), ["1", "2"]);
        assert_eq!(params.get("x"), None);
        assert_eq!(decode_path("/a+b%2Fc"), "/a+b/c");
    }
}
