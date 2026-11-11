//! A resolved JSON value.

use std::fmt;

use crate::plan::JsonNumber;

/// A JSON value with every template resolved.
///
/// It mirrors `plan::JsonTemplate`, with each template replaced by its
/// result. An object keeps its members in order, and keeps repeated keys.
///
/// `Debug` shows the structure and hides the data: `null`, booleans and object
/// keys are shown, while every number is `<number>` and every string is
/// `<string>`.
#[derive(Clone, PartialEq)]
pub enum JsonValue {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A number.
    Number(JsonNumber),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<JsonValue>),
    /// An object, as members in their given order.
    Object(Vec<(String, JsonValue)>),
}

impl fmt::Debug for JsonValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("null"),
            Self::Bool(value) => write!(f, "{value}"),
            Self::Number(_) => f.write_str("<number>"),
            Self::String(_) => f.write_str("<string>"),
            Self::Array(items) => f.debug_list().entries(items).finish(),
            Self::Object(members) => f
                .debug_map()
                .entries(members.iter().map(|(key, value)| (key, value)))
                .finish(),
        }
    }
}
