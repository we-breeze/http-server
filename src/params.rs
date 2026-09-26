use std::borrow::Cow;
use std::ops::Deref;
use std::sync::OnceLock;

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

const INLINE_PAIRS: usize = 4;
type QueryPair<'a> = (Cow<'a, str>, Cow<'a, str>);

struct QueryPairs<'a> {
    inline: [Option<QueryPair<'a>>; INLINE_PAIRS],
    overflow: Vec<QueryPair<'a>>,
}

impl<'a> QueryPairs<'a> {
    fn parse(raw: &'a str) -> Self {
        let mut pairs = Self {
            inline: std::array::from_fn(|_| None),
            overflow: Vec::new(),
        };
        for (index, pair) in form_urlencoded::parse(raw.as_bytes()).enumerate() {
            if index < INLINE_PAIRS {
                pairs.inline[index] = Some(pair);
            } else {
                pairs.overflow.push(pair);
            }
        }
        pairs
    }

    fn iter(&self) -> impl DoubleEndedIterator<Item = &QueryPair<'a>> {
        self.inline.iter().flatten().chain(self.overflow.iter())
    }
}

/// Decode on first scalar/multi-value lookup, not on entry to the handler.
/// Four pairs stay inline; escaped fields and larger collections retain their
/// existing owned fallback. A Query<T>-only handler never builds this cache.
#[doc(hidden)]
pub struct QueryParams<'a> {
    raw: &'a str,
    pairs: OnceLock<QueryPairs<'a>>,
}

impl<'a> QueryParams<'a> {
    #[must_use]
    pub fn new(raw: Option<&'a str>) -> Self {
        Self {
            raw: raw.unwrap_or_default(),
            pairs: OnceLock::new(),
        }
    }

    fn pairs(&self) -> &QueryPairs<'a> {
        self.pairs.get_or_init(|| QueryPairs::parse(self.raw))
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs()
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_ref())
    }

    pub fn values<'b>(&'b self, key: &'b str) -> impl Iterator<Item = &'b str> {
        self.pairs()
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

pub(crate) fn decode_component(raw: &str) -> Cow<'_, str> {
    if raw.as_bytes().contains(&b'%') {
        percent_encoding::percent_decode_str(raw).decode_utf8_lossy()
    } else {
        Cow::Borrowed(raw)
    }
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
    }

    #[test]
    fn unused_query_is_not_parsed_or_materialized() {
        let params = QueryParams::new(Some("unused=%E4%B8%AD+hello&x=%26"));
        assert!(params.pairs.get().is_none());
    }

    #[test]
    fn ordinary_pairs_borrow_input_and_stay_inline() {
        let raw = "a=one&b=two&c=three&d=four";
        let params = QueryParams::new(Some(raw));
        assert_eq!(params.get("a").unwrap().as_ptr(), raw[2..].as_ptr());
        assert_eq!(params.get("d"), Some("four"));
        assert!(params.pairs().overflow.is_empty());
        for (name, value) in params.pairs().iter() {
            assert!(matches!(name, Cow::Borrowed(_)));
            assert!(matches!(value, Cow::Borrowed(_)));
        }
    }

    #[test]
    fn repetitions_keep_order_across_inline_overflow_boundary() {
        let params = QueryParams::new(Some("a=0&a=1&a=2&a=3&a=4&a=5"));
        assert_eq!(params.get("a"), Some("5"));
        assert_eq!(
            params.values("a").collect::<Vec<_>>(),
            ["0", "1", "2", "3", "4", "5"]
        );
        assert_eq!(params.pairs().overflow.len(), 2);
    }
}
