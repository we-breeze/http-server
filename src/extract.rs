use std::fmt;
use std::str::FromStr;

/// A failure while decoding a path, query, or header parameter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtractError;

impl fmt::Display for ExtractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid HTTP API parameter")
    }
}

impl std::error::Error for ExtractError {}

/// Decodes one `:capture` from a route path.
pub trait FromPath<'a>: Sized {
    ///
    /// # Errors
    ///
    /// Returns [`crate::ExtractError`] when the captured path segment cannot become
    /// this business type.
    fn from_path(value: &'a str) -> Result<Self, ExtractError>;
}

/// Decodes one query-string value.
///
/// The input has been percent-decoded, including `+` as a query space.
/// Borrowed values remain valid until the API method finishes.
pub trait FromQuery<'a>: Sized {
    ///
    /// # Errors
    ///
    /// Returns [`crate::ExtractError`] when the raw query value cannot become this
    /// business type.
    fn from_query(value: &'a str) -> Result<Self, ExtractError>;
}

/// Decodes one header value.
pub trait FromHeader<'a>: Sized {
    ///
    /// # Errors
    ///
    /// Returns [`crate::ExtractError`] when the header value cannot become this
    /// business type.
    fn from_header(value: &'a [u8]) -> Result<Self, ExtractError>;
}

impl<'a> FromPath<'a> for &'a str {
    fn from_path(value: &'a str) -> Result<Self, ExtractError> {
        Ok(value)
    }
}

impl<'a> FromQuery<'a> for &'a str {
    fn from_query(value: &'a str) -> Result<Self, ExtractError> {
        Ok(value)
    }
}

impl<'a> FromHeader<'a> for &'a str {
    fn from_header(value: &'a [u8]) -> Result<Self, ExtractError> {
        std::str::from_utf8(value).map_err(|_| ExtractError)
    }
}

impl FromPath<'_> for String {
    fn from_path(value: &str) -> Result<Self, ExtractError> {
        Ok(value.to_owned())
    }
}

impl FromQuery<'_> for String {
    fn from_query(value: &str) -> Result<Self, ExtractError> {
        Ok(value.to_owned())
    }
}

impl FromHeader<'_> for String {
    fn from_header(value: &[u8]) -> Result<Self, ExtractError> {
        String::from_utf8(value.to_vec()).map_err(|_| ExtractError)
    }
}

macro_rules! impl_from_str {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl FromPath<'_> for $ty {
                fn from_path(value: &str) -> Result<Self, ExtractError> {
                    <$ty>::from_str(value).map_err(|_| ExtractError)
                }
            }

            impl FromQuery<'_> for $ty {
                fn from_query(value: &str) -> Result<Self, ExtractError> {
                    <$ty>::from_str(value).map_err(|_| ExtractError)
                }
            }

            impl FromHeader<'_> for $ty {
                fn from_header(value: &[u8]) -> Result<Self, ExtractError> {
                    let value = std::str::from_utf8(value).map_err(|_| ExtractError)?;
                    <$ty>::from_str(value).map_err(|_| ExtractError)
                }
            }
        )+
    };
}

impl_from_str!(
    i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize, f32, f64
);

impl FromQuery<'_> for bool {
    fn from_query(value: &str) -> Result<Self, ExtractError> {
        match value.to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => Ok(true),
            "false" | "0" | "off" | "no" => Ok(false),
            _ => Err(ExtractError),
        }
    }
}

impl FromPath<'_> for bool {
    fn from_path(value: &str) -> Result<Self, ExtractError> {
        Self::from_query(value)
    }
}

impl FromHeader<'_> for bool {
    fn from_header(value: &[u8]) -> Result<Self, ExtractError> {
        Self::from_query(std::str::from_utf8(value).map_err(|_| ExtractError)?)
    }
}

#[doc(hidden)]
pub fn path<'a, T>(value: &'a str) -> Result<T, ExtractError>
where
    T: FromPath<'a>,
{
    T::from_path(value)
}

#[doc(hidden)]
pub fn query_required<'a, T>(
    query: &'a crate::params::QueryParams<'_>,
    key: &str,
) -> Result<T, ExtractError>
where
    T: FromQuery<'a>,
{
    query.get(key).ok_or(ExtractError).and_then(T::from_query)
}

#[doc(hidden)]
pub fn query_optional<'a, T>(
    query: &'a crate::params::QueryParams<'_>,
    key: &str,
) -> Result<Option<T>, ExtractError>
where
    T: FromQuery<'a>,
{
    query.get(key).map(T::from_query).transpose()
}

#[doc(hidden)]
pub fn query_many<'a, T>(
    query: &'a crate::params::QueryParams<'_>,
    key: &'a str,
) -> Result<Vec<T>, ExtractError>
where
    T: FromQuery<'a>,
{
    query.values(key).map(T::from_query).collect()
}

#[doc(hidden)]
pub fn header_required<'a, T>(value: Option<&'a [u8]>) -> Result<T, ExtractError>
where
    T: FromHeader<'a>,
{
    value.ok_or(ExtractError).and_then(T::from_header)
}

#[doc(hidden)]
pub fn header_optional<'a, T>(value: Option<&'a [u8]>) -> Result<Option<T>, ExtractError>
where
    T: FromHeader<'a>,
{
    value.map(T::from_header).transpose()
}
