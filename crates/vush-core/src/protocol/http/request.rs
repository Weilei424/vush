//! The HTTP request that core resolves and hands to an executor.
//!
//! Everything in it is resolved: no template is left. Query pairs, form pairs
//! and a JSON body are not encoded yet, and the executor encodes them. A URL,
//! whether `Absolute` or `Joined`'s base and path, is used as given. Phase 3
//! decides how substituted values are encoded inside a `path` or `url`. The
//! executor checks the URL, the scheme and the headers before anything is
//! sent.
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
    /// Query parameters. Resolved, not encoded. Appended in this order.
    pub query: Vec<(String, String)>,
    /// Headers. Resolved. Order and repeats kept. Not checked by core.
    pub headers: Vec<(String, String)>,
    /// The body, if any.
    pub body: Option<HttpBody>,
}

/// The resolved target of a request.
#[derive(Clone, PartialEq, Eq)]
pub enum RequestUrl {
    /// A full URL, used as written: a request's `url`, or the next URL
    /// `follow` resolves from a redirect's `Location`.
    Absolute(String),
    /// A request's `path`, joined to the resolved `base_url`. P2.2 defines the
    /// join.
    Joined {
        /// The resolved `base_url`.
        base: String,
        /// The request's resolved `path`.
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
    /// Form fields. Resolved, not encoded. The executor encodes them.
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
