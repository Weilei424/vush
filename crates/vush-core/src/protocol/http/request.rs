//! The HTTP request that core resolves and hands to an executor.
//!
//! Everything in it is resolved and nothing is encoded: no template is left,
//! and no value is percent-encoded or serialized. The executor checks the URL,
//! the scheme and the headers before anything is sent.
//!
//! `Debug` never shows a URL or any part of one, a header, query or form
//! value, a JSON string or number, or body bytes. It shows the method, names
//! (through `{:?}`, so control characters are escaped), and byte sizes.

use std::fmt;

use crate::plan::HttpMethod;
use crate::protocol::JsonValue;

/// One resolved HTTP request.
#[derive(Clone, PartialEq)]
pub struct HttpRequest {
    /// The request method.
    pub method: HttpMethod,
    /// Where the request goes.
    pub url: RequestUrl,
    /// Query parameters in order, as resolved text. Not encoded.
    pub query: Vec<(String, String)>,
    /// Headers in order, as resolved text. Not checked.
    pub headers: Vec<(String, String)>,
    /// The body, if any.
    pub body: Option<HttpBody>,
}

/// The resolved target of a request.
#[derive(Clone, PartialEq, Eq)]
pub enum RequestUrl {
    /// A full URL, from a step's `url`.
    Absolute(String),
    /// A base URL and a path, from a step's `path`. P2.2 defines the join.
    Joined {
        /// The base URL.
        base: String,
        /// The path.
        path: String,
    },
}

/// A resolved request body.
#[derive(Clone, PartialEq)]
pub enum HttpBody {
    /// A JSON value, which the executor serializes.
    Json(JsonValue),
    /// Text, sent as it is.
    Text(String),
    /// Form fields in order, which the executor encodes.
    Form(Vec<(String, String)>),
}

/// A list of the names in `pairs`, each printed through `{:?}`.
struct Names<'a>(&'a [(String, String)]);

impl fmt::Debug for Names<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(name, _)| name))
            .finish()
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method.as_str())
            .field("url", &self.url)
            .field("query", &Names(&self.query))
            .field("headers", &Names(&self.headers))
            .field("body", &self.body)
            .finish()
    }
}

impl fmt::Debug for RequestUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absolute(url) => write!(f, "Absolute(<{} bytes>)", url.len()),
            Self::Joined { base, path } => write!(
                f,
                "Joined {{ base: <{} bytes>, path: <{} bytes> }}",
                base.len(),
                path.len()
            ),
        }
    }
}

impl fmt::Debug for HttpBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(value) => f.debug_tuple("Json").field(value).finish(),
            Self::Text(text) => write!(f, "Text(<{} bytes>)", text.len()),
            Self::Form(fields) => f.debug_tuple("Form").field(&Names(fields)).finish(),
        }
    }
}
