use std::convert::Infallible;
use std::ops::Deref;

use bytes::Bytes;
use serde::de::DeserializeOwned;

/// A decoded URL-encoded form body.
#[derive(Debug)]
pub struct Form<T>(pub T);

impl<T> Deref for Form<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// A complete binary request body.
#[derive(Debug)]
pub struct Body<'a>(pub &'a [u8]);

impl Deref for Body<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.0
    }
}

/// One multipart field, including files and plain text fields.
#[derive(Debug)]
pub struct Upload {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub bytes: Bytes,
}

impl Upload {
    /// # Errors
    /// Returns an error when the field is not UTF-8 text.
    pub fn text(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.bytes)
    }
}

/// Decoded fields of a bounded multipart request, in wire order.
#[derive(Debug)]
pub struct Multipart {
    pub fields: Vec<Upload>,
}

impl Multipart {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Upload> {
        self.fields.iter().rev().find(|field| field.name == name)
    }

    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Upload> {
        self.fields.iter().filter(move |field| field.name == name)
    }

    /// Decodes text fields into a business object. File contents stay in `fields`.
    ///
    /// # Errors
    /// Returns an error for invalid text or a business field that cannot be decoded.
    pub fn form<T: DeserializeOwned>(&self) -> Result<T, BodyError> {
        let mut encoded = form_urlencoded::Serializer::new(String::new());
        for field in &self.fields {
            if field.filename.is_none() {
                encoded.append_pair(
                    &field.name,
                    field
                        .text()
                        .map_err(|error| BodyError::Invalid(error.to_string()))?,
                );
            }
        }
        serde_html_form::from_str(&encoded.finish())
            .map_err(|error| BodyError::Invalid(error.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BodyError {
    #[error("unsupported request content type")]
    UnsupportedMediaType,
    #[error("invalid request body: {0}")]
    Invalid(String),
}

#[doc(hidden)]
pub fn form<T: DeserializeOwned>(
    body: &[u8],
    content_type: Option<&[u8]>,
) -> Result<Form<T>, BodyError> {
    if !is_content_type(content_type, "application/x-www-form-urlencoded") {
        return Err(BodyError::UnsupportedMediaType);
    }
    serde_html_form::from_bytes(body)
        .map(Form)
        .map_err(|error| BodyError::Invalid(error.to_string()))
}

#[doc(hidden)]
pub async fn multipart(body: &[u8], content_type: Option<&[u8]>) -> Result<Multipart, BodyError> {
    if !is_content_type(content_type, "multipart/form-data") {
        return Err(BodyError::UnsupportedMediaType);
    }
    let content_type = content_type
        .and_then(|value| std::str::from_utf8(value).ok())
        .unwrap_or_default();
    let boundary = multer::parse_boundary(content_type)
        .map_err(|error| BodyError::Invalid(error.to_string()))?;
    let bytes = Bytes::copy_from_slice(body);
    let stream = futures_util::stream::once(async move { Ok::<_, Infallible>(bytes) });
    let mut parser = multer::Multipart::new(stream, boundary);
    let mut fields = Vec::new();
    while let Some(field) = parser
        .next_field()
        .await
        .map_err(|error| BodyError::Invalid(error.to_string()))?
    {
        let name = field
            .name()
            .ok_or_else(|| BodyError::Invalid("multipart field has no name".into()))?
            .to_owned();
        let filename = field.file_name().map(str::to_owned);
        let content_type = field.content_type().map(ToString::to_string);
        let bytes = field
            .bytes()
            .await
            .map_err(|error| BodyError::Invalid(error.to_string()))?;
        fields.push(Upload {
            name,
            filename,
            content_type,
            bytes,
        });
    }
    Ok(Multipart { fields })
}

fn is_content_type(value: Option<&[u8]>, expected: &str) -> bool {
    value
        .and_then(|value| std::str::from_utf8(value).ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case(expected)
        })
}
