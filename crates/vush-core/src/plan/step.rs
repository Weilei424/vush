//! Steps: what one iteration, hook or `repeat` body does.

use std::num::NonZeroU32;
use std::str::FromStr;
use std::time::Duration;

use super::json_path::JsonPath;
use super::template::{Template, VarRef};
use super::value::{JsonNumber, ValueError};

/// The deadline of a `repeat` that does not set one.
pub const DEFAULT_REPEAT_DEADLINE: Duration = Duration::from_secs(60);

/// The fixed delay between retry attempts when `backoff` is not set.
pub const DEFAULT_RETRY_BACKOFF: Duration = Duration::from_millis(100);

const METHOD_EXPECTED: &str = "an HTTP method such as GET, POST or M-SEARCH (uppercase A-Z, 0-9 and -, starting with a letter)";

/// One step of a step list.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Boxed because a request is much larger than the other variants.
    Request(Box<RequestStep>),
    Wait(Duration),
    If(IfStep),
    Repeat(RepeatStep),
}

/// An HTTP method: `[A-Z][A-Z0-9-]*`. Any method is allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpMethod(String);

impl HttpMethod {
    /// The method as written, e.g. `GET`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for HttpMethod {
    type Err = ValueError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut bytes = text.bytes();
        let valid = bytes.next().is_some_and(|b| b.is_ascii_uppercase())
            && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-');
        if valid {
            Ok(HttpMethod(text.to_owned()))
        } else {
            Err(ValueError {
                expected: METHOD_EXPECTED,
                input: text.to_owned(),
            })
        }
    }
}

/// Where a request goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestTarget {
    /// Joined to `base_url`.
    Path(Template),
    /// Used as written.
    Url(Template),
}

/// A request body.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Json(JsonTemplate),
    Text(Template),
    Form(Vec<(String, Template)>),
}

/// A JSON value whose string values (not keys) may contain variable references.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonTemplate {
    Null,
    Bool(bool),
    Number(JsonNumber),
    String(Template),
    Array(Vec<JsonTemplate>),
    /// Member order is kept.
    Object(Vec<(String, JsonTemplate)>),
}

/// An HTTP request step. Every default is already applied.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestStep {
    /// A static metrics label.
    pub name: Option<String>,
    pub method: HttpMethod,
    pub target: RequestTarget,
    pub query: Vec<(String, Template)>,
    pub headers: Vec<(String, Template)>,
    pub body: Option<Body>,
    /// The timeout of one physical attempt.
    pub timeout: Duration,
    /// Whether a failure of this step ends the iteration.
    pub required: bool,
    pub retry: Option<RetryPolicy>,
    pub expect: Expect,
    pub extract: Vec<Extraction>,
}

/// Retries of one request step. Every attempt counts as a physical attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts, including the first.
    pub attempts: u32,
    /// A fixed delay between attempts.
    pub backoff: Duration,
    /// Allow retrying any method other than GET, HEAD, PUT, DELETE, OPTIONS and TRACE.
    pub allow_non_idempotent: bool,
}

/// What a response must satisfy for the step to pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Expect {
    pub status: StatusMatch,
    pub body_contains: Option<Template>,
    pub json: Vec<JsonAssertion>,
}

impl Default for Expect {
    fn default() -> Self {
        Self {
            status: StatusMatch::AnySuccess,
            body_contains: None,
            json: Vec::new(),
        }
    }
}

/// Which status codes pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusMatch {
    /// Any 2xx or 3xx.
    AnySuccess,
    OneOf(Vec<u16>),
}

/// A check on one value in a JSON response body.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonAssertion {
    pub path: JsonPath,
    pub op: Comparison,
}

/// A value taken from a response and stored in a variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extraction {
    /// A bare variable name. The scope is decided by where the step runs.
    pub var: String,
    pub source: ExtractSource,
}

/// Where an extracted value comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractSource {
    JsonPath(JsonPath),
    Header(String),
}

/// A conditional step.
#[derive(Debug, Clone, PartialEq)]
pub struct IfStep {
    pub when: Condition,
    pub then: Vec<Step>,
    /// The `else` steps.
    pub otherwise: Vec<Step>,
}

/// A bounded loop over steps.
#[derive(Debug, Clone, PartialEq)]
pub struct RepeatStep {
    /// How many times the steps run, 1 or more.
    pub count: NonZeroU32,
    /// A time limit for the whole loop.
    pub deadline: Duration,
    pub steps: Vec<Step>,
}

/// A condition on variables, for [`IfStep`].
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    Compare { var: VarRef, op: Comparison },
    All(Vec<Condition>),
    Any(Vec<Condition>),
    Not(Box<Condition>),
}

/// A comparison of one value.
#[derive(Debug, Clone, PartialEq)]
pub enum Comparison {
    Exists(bool),
    Equals(Scalar),
    NotEquals(Scalar),
    Gt(JsonNumber),
    Gte(JsonNumber),
    Lt(JsonNumber),
    Lte(JsonNumber),
    Contains(Template),
}

/// A JSON scalar, with variable references allowed in strings.
#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Null,
    Bool(bool),
    Number(JsonNumber),
    String(Template),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_defaults_match_schema() {
        assert_eq!(
            Expect::default(),
            Expect {
                status: StatusMatch::AnySuccess,
                body_contains: None,
                json: vec![]
            }
        );
        assert_eq!(DEFAULT_REPEAT_DEADLINE, Duration::from_secs(60));
        assert_eq!(DEFAULT_RETRY_BACKOFF, Duration::from_millis(100));
    }

    #[test]
    fn http_methods_parse() {
        for text in ["GET", "POST", "PURGE", "M-SEARCH", "A1"] {
            assert_eq!(text.parse::<HttpMethod>().unwrap().as_str(), text);
        }
    }

    #[test]
    fn invalid_http_methods_are_rejected() {
        for text in ["", "get", "Get", "G T", "1GET", "-GET", "GÉT"] {
            assert!(text.parse::<HttpMethod>().is_err(), "{text}");
        }
    }
}
