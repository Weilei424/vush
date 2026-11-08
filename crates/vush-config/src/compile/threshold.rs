//! The `thresholds` list.

use vush_core::plan::{
    Bound, BoundOp, PercentileStat, Rate, Ratio, Threshold, ThresholdCheck, ValueError,
    parse_duration,
};

use super::reader::{MapReader, child, item, list, one_of, parsed, string};
use crate::error::Errors;
use crate::node::Node;

#[derive(Clone, Copy)]
enum Metric {
    HttpRequestDuration,
    ErrorRate,
    RequestRate,
    IterationRate,
    DroppedWorkRatio,
}

/// Reads the list. Every threshold is read, so one bad one doesn't hide another.
pub(super) fn thresholds(node: &Node, path: &str, errors: &mut Errors) -> Option<Vec<Threshold>> {
    let items = list(node, path, errors)?;
    let mut out = Vec::with_capacity(items.len());
    let mut ok = true;
    for (index, element) in items.iter().enumerate() {
        match threshold(element, &item(path, index), errors) {
            Some(built) => out.push(built),
            None => ok = false,
        }
    }
    ok.then_some(out)
}

/// An unknown or missing `metric` stops the read, since the other keys can't
/// be judged without it.
fn threshold(node: &Node, path: &str, errors: &mut Errors) -> Option<Threshold> {
    let mut map = MapReader::new(node, path, errors)?;
    let metric_node = map.required("metric", errors)?;
    let choices = [
        ("http_request_duration", Metric::HttpRequestDuration),
        ("error_rate", Metric::ErrorRate),
        ("request_rate", Metric::RequestRate),
        ("iteration_rate", Metric::IterationRate),
        ("dropped_work_ratio", Metric::DroppedWorkRatio),
    ];
    let metric = one_of(metric_node, &child(path, "metric"), &choices, errors)?;

    let mut ok = true;
    let mut stat = None;
    if let Metric::HttpRequestDuration = metric {
        if let Some(n) = map.required("stat", errors) {
            let stats = [
                ("p50", PercentileStat::P50),
                ("p90", PercentileStat::P90),
                ("p95", PercentileStat::P95),
                ("p99", PercentileStat::P99),
            ];
            stat = one_of(n, &child(path, "stat"), &stats, errors);
        }
        ok &= stat.is_some();
    } else if map.has("stat") {
        map.reject(
            "stat",
            "`stat` only applies to http_request_duration",
            errors,
        );
        ok = false;
    }

    let scenario = match map.optional("scenario") {
        Some(n) => {
            let name = string(n, &child(path, "scenario"), errors).map(str::to_owned);
            ok &= name.is_some();
            name
        }
        None => None,
    };

    let check = match metric {
        Metric::HttpRequestDuration => bound(&mut map, path, parse_duration, errors)
            .zip(stat)
            .map(|(bound, stat)| ThresholdCheck::HttpRequestDuration { stat, bound }),
        Metric::ErrorRate => {
            bound(&mut map, path, str::parse::<Ratio>, errors).map(ThresholdCheck::ErrorRate)
        }
        Metric::RequestRate => {
            bound(&mut map, path, str::parse::<Rate>, errors).map(ThresholdCheck::RequestRate)
        }
        Metric::IterationRate => {
            bound(&mut map, path, str::parse::<Rate>, errors).map(ThresholdCheck::IterationRate)
        }
        Metric::DroppedWorkRatio => {
            bound(&mut map, path, str::parse::<Ratio>, errors).map(ThresholdCheck::DroppedWorkRatio)
        }
    };
    map.finish(errors);
    let check = check?;
    ok.then_some(Threshold { scenario, check })
}

/// Reads whichever of `lt`, `lte`, `gt` or `gte` is present.
fn bound<T>(
    map: &mut MapReader<'_>,
    path: &str,
    parse: fn(&str) -> Result<T, ValueError>,
    errors: &mut Errors,
) -> Option<Bound<T>> {
    let (key, node) = map.exactly_one(&["lt", "lte", "gt", "gte"], errors)?;
    let op = match key {
        "lt" => BoundOp::Lt,
        "lte" => BoundOp::Lte,
        "gt" => BoundOp::Gt,
        _ => BoundOp::Gte,
    };
    let value = parsed(node, &child(path, key), parse, errors)?;
    Some(Bound { op, value })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::compile::testing::{assert_fails, run};

    fn d(text: &str) -> Duration {
        parse_duration(text).unwrap()
    }
    fn rate(text: &str) -> Rate {
        text.parse().unwrap()
    }
    fn ratio(text: &str) -> Ratio {
        text.parse().unwrap()
    }
    fn bound_of<T>(op: BoundOp, value: T) -> Bound<T> {
        Bound { op, value }
    }

    #[test]
    fn thresholds_map_to_their_check() {
        use BoundOp::*;
        use PercentileStat::*;
        use ThresholdCheck::*;
        let rows: Vec<(&str, Option<&str>, ThresholdCheck)> = vec![
            (
                "{metric: http_request_duration, stat: p95, lt: 500ms}",
                None,
                HttpRequestDuration {
                    stat: P95,
                    bound: bound_of(Lt, d("500ms")),
                },
            ),
            (
                "{metric: http_request_duration, stat: p50, gte: 1s}",
                None,
                HttpRequestDuration {
                    stat: P50,
                    bound: bound_of(Gte, d("1s")),
                },
            ),
            (
                "{metric: http_request_duration, stat: p90, lte: 1s}",
                None,
                HttpRequestDuration {
                    stat: P90,
                    bound: bound_of(Lte, d("1s")),
                },
            ),
            (
                "{metric: http_request_duration, stat: p99, gt: 1s}",
                None,
                HttpRequestDuration {
                    stat: P99,
                    bound: bound_of(Gt, d("1s")),
                },
            ),
            (
                "{metric: error_rate, lte: 1%}",
                None,
                ErrorRate(bound_of(Lte, ratio("1%"))),
            ),
            (
                "{metric: request_rate, scenario: health, gte: 90/s}",
                Some("health"),
                RequestRate(bound_of(Gte, rate("90/s"))),
            ),
            (
                "{metric: iteration_rate, gt: 18/s}",
                None,
                IterationRate(bound_of(Gt, rate("18/s"))),
            ),
            (
                "{metric: dropped_work_ratio, lt: 0.5%}",
                None,
                DroppedWorkRatio(bound_of(Lt, ratio("0.5%"))),
            ),
        ];
        for (element, scenario, check) in rows {
            let yaml = format!("[{element}]");
            let (got, errors) = run(&yaml, |n, e| thresholds(n, "thresholds", e));
            assert!(errors.is_empty(), "{yaml}: {errors:?}");
            let want = Threshold {
                scenario: scenario.map(str::to_owned),
                check,
            };
            assert_eq!(got, Some(vec![want]), "{yaml}");
        }
    }

    #[test]
    fn threshold_failures_name_their_path() {
        let rows = [
            ("5", "thresholds", "expected a list"),
            ("[5]", "thresholds[0]", "expected a map"),
            (
                "[{lt: 1%}]",
                "thresholds[0]",
                "missing required key `metric`",
            ),
            (
                "[{metric: latency, lt: 1%}]",
                "thresholds[0].metric",
                "expected one of http_request_duration, error_rate, request_rate, iteration_rate or dropped_work_ratio",
            ),
            (
                "[{metric: http_request_duration, lt: 1s}]",
                "thresholds[0]",
                "missing required key `stat`",
            ),
            (
                "[{metric: http_request_duration, stat: p42, lt: 1s}]",
                "thresholds[0].stat",
                "expected one of p50, p90, p95 or p99",
            ),
            (
                "[{metric: error_rate, stat: p95, lt: 1%}]",
                "thresholds[0].stat",
                "`stat` only applies to http_request_duration",
            ),
            (
                "[{metric: error_rate}]",
                "thresholds[0]",
                "needs one of `lt`, `lte`, `gt` or `gte`",
            ),
            (
                "[{metric: error_rate, lt: 1%, gt: 2%}]",
                "thresholds[0].gt",
                "`lt` and `gt` can't be used together",
            ),
            (
                "[{metric: error_rate, lt: 101%}]",
                "thresholds[0].lt",
                "expected a percentage",
            ),
            (
                "[{metric: error_rate, lt: 1}]",
                "thresholds[0].lt",
                "expected a percentage",
            ),
            (
                "[{metric: request_rate, lt: 1%}]",
                "thresholds[0].lt",
                "expected a rate",
            ),
            (
                "[{metric: http_request_duration, stat: p95, lt: 1%}]",
                "thresholds[0].lt",
                "expected a duration",
            ),
            (
                "[{metric: error_rate, lt: 1%, scenario: 5}]",
                "thresholds[0].scenario",
                "expected a string",
            ),
            (
                "[{metric: error_rate, lt: 1%, nope: 1}]",
                "thresholds[0].nope",
                "unknown key `nope`",
            ),
        ];
        for (yaml, path, fragment) in rows {
            assert_fails(yaml, path, fragment, |n, e| thresholds(n, "thresholds", e));
        }
    }
}
