//! Turns the located value tree into a plan draft.

mod reader;
mod settings;

#[cfg(test)]
pub(crate) mod testing {
    use crate::error::{ConfigError, Errors};
    use crate::node::Node;

    /// Reads YAML into a tree, panicking if the reader reports errors.
    pub(crate) fn node(yaml: &str) -> Node {
        let mut errors = Errors::new();
        let node = crate::yaml::read(yaml, &mut errors);
        assert!(
            errors.is_empty(),
            "reader errors: {:?}",
            errors.into_sorted()
        );
        node.expect("the reader returned no tree")
    }

    /// Runs `f` on the tree and returns its result with the errors it recorded.
    pub(crate) fn run<T>(
        yaml: &str,
        f: impl FnOnce(&Node, &mut Errors) -> T,
    ) -> (T, Vec<ConfigError>) {
        let tree = node(yaml);
        let mut errors = Errors::new();
        let value = f(&tree, &mut errors);
        (value, errors.into_sorted())
    }

    /// Asserts exactly one error, with this path and message fragment.
    pub(crate) fn assert_fails<T>(
        yaml: &str,
        path: &str,
        fragment: &str,
        f: impl FnOnce(&Node, &mut Errors) -> T,
    ) {
        let (_, errors) = run(yaml, f);
        assert_eq!(errors.len(), 1, "{yaml}: {errors:?}");
        assert_eq!(errors[0].path, path, "{yaml}");
        assert!(
            errors[0].message.contains(fragment),
            "{yaml}: {:?} lacks {fragment:?}",
            errors[0].message
        );
    }
}
