//! The typed workload plan and the value types it is built from.
//!
//! Every text form the test-file schema accepts is parsed here, with no
//! dependencies, into a typed value, so the engine never interprets text at
//! request time.

mod json_path;
mod load;
mod settings;
mod step;
mod template;
mod threshold;
mod value;

pub use json_path::{JsonPath, JsonPathSegment};
pub use load::{ClosedStop, ClosedVusLoad, LoadModel, QueuePolicy, RateSchedule, Stage};
pub use settings::{HttpSettings, HttpVersion, PoolScope, ResourceLimits, TlsSettings};
pub use step::{
    Body, Comparison, Condition, DEFAULT_REPEAT_DEADLINE, DEFAULT_RETRY_BACKOFF, Expect,
    ExtractSource, Extraction, HttpMethod, IfStep, JsonAssertion, JsonTemplate, RepeatStep,
    RequestStep, RequestTarget, RetryPolicy, Scalar, StatusMatch, Step,
};
pub use template::{Template, TemplatePart, VarRef};
pub use threshold::{Bound, BoundOp, PercentileStat, Threshold, ThresholdCheck};
pub use value::{ByteSize, JsonNumber, Rate, RateUnit, Ratio, ValueError, parse_duration};
