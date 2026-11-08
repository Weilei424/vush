//! Finds the known key closest to a misspelled one.

/// Keys longer than this get no hint, which bounds the work per key.
const MAX_KEY_LEN: usize = 64;

/// The choice nearest to `key` by optimal string alignment distance, if it is
/// within a third of the choice's length (at least one edit). Case is ignored.
/// The first of equally near choices wins.
pub(super) fn closest<'c>(
    key: &str,
    choices: impl IntoIterator<Item = &'c str>,
) -> Option<&'c str> {
    if key.len() > MAX_KEY_LEN {
        return None;
    }
    let key: Vec<char> = key.to_lowercase().chars().collect();
    choices
        .into_iter()
        .filter_map(|choice| {
            let distance = distance(&key, &choice.chars().collect::<Vec<_>>());
            (distance <= (choice.len() / 3).max(1)).then_some((distance, choice))
        })
        .min_by_key(|&(distance, _)| distance)
        .map(|(_, choice)| choice)
}

/// Optimal string alignment distance: insertions, deletions, replacements and
/// swaps of neighbors.
fn distance(a: &[char], b: &[char]) -> usize {
    let width = b.len() + 1;
    let mut table = vec![0usize; (a.len() + 1) * width];
    for (j, cell) in table.iter_mut().take(width).enumerate() {
        *cell = j;
    }
    for i in 0..=a.len() {
        table[i * width] = i;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (table[(i - 1) * width + j] + 1)
                .min(table[i * width + j - 1] + 1)
                .min(table[(i - 1) * width + j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(table[(i - 2) * width + j - 2] + 1);
            }
            table[i * width + j] = best;
        }
    }
    table[a.len() * width + b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: [&str; 6] = [
        "name",
        "timeout",
        "base_url",
        "hook_timeout",
        "if",
        "max_response_size",
    ];

    #[test]
    fn close_keys_are_found() {
        for (key, want) in [
            ("nmae", "name"),
            ("nam", "name"),
            ("namee", "name"),
            ("nane", "name"),
            ("NAME", "name"),
            ("TimeOut", "timeout"),
            ("timout", "timeout"),
            ("baseUrl", "base_url"),
            ("base-url", "base_url"),
            ("hookTimeout", "hook_timeout"),
            ("nàme", "name"),
            ("of", "if"),
        ] {
            assert_eq!(closest(key, KEYS), Some(want), "{key}");
        }
    }

    #[test]
    fn keys_that_are_not_close_find_nothing() {
        for key in ["banana", "x", "xy", "", "1", "日本語"] {
            assert_eq!(closest(key, KEYS), None, "{key}");
        }
    }

    #[test]
    fn the_limit_is_a_third_of_the_choice() {
        assert_eq!(closest("timexxt", ["timeout"]), Some("timeout"));
        assert_eq!(closest("timxxxt", ["timeout"]), None);
    }

    #[test]
    fn ties_go_to_the_first_choice() {
        assert_eq!(closest("at", ["lt", "gt"]), Some("lt"));
        assert_eq!(closest("at", ["gt", "lt"]), Some("gt"));
    }

    #[test]
    fn long_keys_get_no_hint() {
        let choice = "a".repeat(64);
        let at_limit = format!("{}b", "a".repeat(63));
        assert_eq!(closest(&at_limit, [choice.as_str()]), Some(choice.as_str()));
        assert_eq!(closest(&"a".repeat(65), [choice.as_str()]), None);
    }
}
