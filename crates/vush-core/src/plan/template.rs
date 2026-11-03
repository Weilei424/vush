//! Templates: text with `${...}` variable references, split into parts up front.

use std::str::FromStr;

use super::value::ValueError;

const REFERENCE_EXPECTED: &str =
    "a variable reference such as token, scenario.x, global.x, env.NAME, vu.id or vu.iteration";

const UNTERMINATED_EXPECTED: &str = "a closing } for every ${ (write $${ for a literal ${)";

/// How many characters of a reference an error echoes.
const ECHO_LIMIT: usize = 64;

/// The text a template error shows for a `${` whose reference starts at `after`.
///
/// A typo can leave anything after the `${`, such as the rest of a JSON body
/// with credentials in it, so only `${` and the leading run of reference
/// characters (`[A-Za-z0-9_.]`, at most [`ECHO_LIMIT`]) are echoed. The run is
/// followed by `}` when that is the next character, and by `…` otherwise.
fn echo_reference(after: &str) -> String {
    let len = after
        .bytes()
        .take(ECHO_LIMIT)
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.'))
        .count();
    // The run is ASCII, so `len` is a character boundary.
    let (name, next) = after.split_at(len);
    let end = if next.starts_with('}') { '}' } else { '…' };
    format!("${{{name}{end}")
}

/// Whether `text` is a variable name: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A reference to a variable or a built-in value.
///
/// This is the form used by `var:` fields (`token`, `scenario.x`), and inside
/// `${...}` in templates, without the `${}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VarRef {
    /// `token`: this user's variable.
    Vu(String),
    /// `scenario.x`: read-only, per scenario.
    Scenario(String),
    /// `global.x`: read-only, whole run.
    Global(String),
    /// `env.NAME`: an environment variable. Only the name is stored.
    Env(String),
    /// `vu.id`: the built-in user number.
    VuId,
    /// `vu.iteration`: the built-in iteration count.
    VuIteration,
}

impl FromStr for VarRef {
    type Err = ValueError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let reference = || {
            let Some((prefix, rest)) = text.split_once('.') else {
                // `env`, `global`, `scenario` and `vu` are reserved prefixes.
                let reserved = ["env", "global", "scenario", "vu"].contains(&text);
                return (is_name(text) && !reserved).then(|| VarRef::Vu(text.to_owned()));
            };
            match prefix {
                "vu" => match rest {
                    "id" => Some(VarRef::VuId),
                    "iteration" => Some(VarRef::VuIteration),
                    _ => None,
                },
                "scenario" => is_name(rest).then(|| VarRef::Scenario(rest.to_owned())),
                "global" => is_name(rest).then(|| VarRef::Global(rest.to_owned())),
                "env" => is_name(rest).then(|| VarRef::Env(rest.to_owned())),
                _ => None,
            }
        };
        reference().ok_or_else(|| ValueError {
            expected: REFERENCE_EXPECTED,
            input: text.to_owned(),
        })
    }
}

/// One piece of a [`Template`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplatePart {
    /// Literal text, with `$${` already unescaped to `${`.
    Text(String),
    /// A `${...}` reference.
    Var(VarRef),
}

/// A string split into literal text and variable references.
///
/// Adjacent text is merged, and an empty string has no parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    parts: Vec<TemplatePart>,
}

impl Template {
    /// The parts in order.
    pub fn parts(&self) -> &[TemplatePart] {
        &self.parts
    }
}

impl FromStr for Template {
    type Err = ValueError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut rest = text;
        while let Some(dollar) = rest.find('$') {
            literal.push_str(&rest[..dollar]);
            let after = &rest[dollar + 1..];
            if let Some(escaped) = after.strip_prefix("${") {
                // `$${` is a literal `${`.
                literal.push_str("${");
                rest = escaped;
            } else if let Some(reference) = after.strip_prefix('{') {
                let Some(end) = reference.find('}') else {
                    return Err(ValueError {
                        expected: UNTERMINATED_EXPECTED,
                        input: echo_reference(reference),
                    });
                };
                let var = reference[..end]
                    .parse()
                    .map_err(|error: ValueError| ValueError {
                        expected: error.expected,
                        input: echo_reference(reference),
                    })?;
                if !literal.is_empty() {
                    parts.push(TemplatePart::Text(std::mem::take(&mut literal)));
                }
                parts.push(TemplatePart::Var(var));
                rest = &reference[end + 1..];
            } else {
                // A `$` not followed by `{` is literal.
                literal.push('$');
                rest = after;
            }
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            parts.push(TemplatePart::Text(literal));
        }
        Ok(Template { parts })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(text: &str) -> TemplatePart {
        TemplatePart::Text(text.to_string())
    }
    fn v(name: &str) -> TemplatePart {
        TemplatePart::Var(VarRef::Vu(name.to_string()))
    }

    #[test]
    fn templates_split_into_parts() {
        let body = r#"{"price": "$5", "a": {"b": 1}}"#;
        let cases: Vec<(&str, Vec<TemplatePart>)> = vec![
            ("", vec![]),
            ("plain", vec![t("plain")]),
            ("${token}", vec![v("token")]),
            ("Bearer ${token}", vec![t("Bearer "), v("token")]),
            ("a${x}b", vec![t("a"), v("x"), t("b")]),
            ("${a}${b}", vec![v("a"), v("b")]),
            ("$${token}", vec![t("${token}")]),
            ("a$${b}c${d}", vec![t("a${b}c"), v("d")]),
            ("$", vec![t("$")]),
            ("$$", vec![t("$$")]),
            ("$$${x}", vec![t("$${x}")]),
            ("cost: $5", vec![t("cost: $5")]),
            (body, vec![t(body)]),
            (
                "héllo ${name} 日本語",
                vec![t("héllo "), v("name"), t(" 日本語")],
            ),
        ];
        for (text, want) in cases {
            let got: Template = text.parse().unwrap();
            assert_eq!(got.parts(), want, "{text}");
        }
    }

    #[test]
    fn each_reference_kind_parses() {
        let cases = [
            ("token", VarRef::Vu("token".to_string())),
            ("_a1", VarRef::Vu("_a1".to_string())),
            ("scenario.x", VarRef::Scenario("x".to_string())),
            ("global.x", VarRef::Global("x".to_string())),
            ("env.API_KEY", VarRef::Env("API_KEY".to_string())),
            ("vu.id", VarRef::VuId),
            ("vu.iteration", VarRef::VuIteration),
        ];
        for (text, want) in cases {
            assert_eq!(text.parse::<VarRef>(), Ok(want.clone()), "{text}");
            let template: Template = format!("${{{text}}}").parse().unwrap();
            assert_eq!(template.parts(), [TemplatePart::Var(want)], "{text}");
        }
    }

    #[test]
    fn invalid_templates_are_rejected() {
        for text in [
            "${",
            "${token",
            "a${",
            "${}",
            "${ token }",
            "${token }",
            "${foo.bar}",
            "${vu.foo}",
            "${vu}",
            "${env}",
            "${global}",
            "${scenario}",
            "${env.}",
            "${global.}",
            "${env.1A}",
            "${1abc}",
            "${a-b}",
            "${a.b.c}",
            "${global.a.b}",
            "${é}",
            "${a${b}}",
            "${aé}",
            "${scenario.}",
            "${scenario.1a}",
        ] {
            assert!(text.parse::<Template>().is_err(), "{text}");
        }
    }

    #[test]
    fn template_errors_do_not_echo_following_text() {
        let cases = [
            (r#"{"note":"${oops","api_key":"AKIA-SECRET"}"#, "${oops…"),
            ("${foo.bar}", "${foo.bar}"),
            ("${token", "${token…"),
            ("x ${a} then ${b", "${b…"),
            ("${}", "${}"),
            ("${ token }", "${…"),
        ];
        for (text, want) in cases {
            let err = text.parse::<Template>().unwrap_err();
            assert_eq!(err.input, want, "{text}");
        }

        let huge = format!("${{{}", "a".repeat(2_000_000));
        let err = huge.parse::<Template>().unwrap_err();
        assert_eq!(err.input.chars().count(), 2 + 64 + 1);
    }

    #[test]
    fn invalid_references_are_rejected() {
        for text in [
            "", "env", "vu", "global", "scenario", "${token}", "foo.bar", "vu.foo", "a b",
        ] {
            assert!(text.parse::<VarRef>().is_err(), "{text}");
        }
    }
}
