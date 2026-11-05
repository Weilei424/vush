//! Helpers shared by the whole-crate tests.

const SCHEMA_DOC: &str = include_str!("../../../../docs/config-schema.md");

/// The first `lang` code block after the "Complete example" heading.
pub fn example_block(lang: &str) -> &'static str {
    let tail = &SCHEMA_DOC[SCHEMA_DOC.find("## Complete example").expect("heading")..];
    let open = format!("```{lang}\n");
    let start = tail.find(&open).expect("opening fence") + open.len();
    let end = tail[start..].find("\n```").expect("closing fence");
    &tail[start..=start + end]
}
