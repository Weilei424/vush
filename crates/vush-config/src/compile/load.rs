//! The scenario `load` block.

use std::num::{NonZeroU32, NonZeroU64};

use vush_core::plan::{
    ClosedStop, ClosedVusLoad, LoadModel, QueuePolicy, Rate, RateSchedule, RateUnit, Stage,
    parse_duration,
};

use super::reader::{MapReader, child, integer, item, list, one_of, parsed};
use crate::error::Errors;
use crate::node::Node;

const U32_MAX: u64 = u32::MAX as u64;

#[derive(Clone, Copy)]
enum Model {
    ClosedVus,
    IterationRate,
    RequestRate,
}

/// Reads a `load` block. An unknown or missing `model` stops the read, since
/// the other keys can't be judged without it.
pub(super) fn load(node: &Node, path: &str, errors: &mut Errors) -> Option<LoadModel> {
    let mut map = MapReader::new(node, path, errors)?;
    let model_node = map.required("model", errors)?;
    let choices = [
        ("closed_vus", Model::ClosedVus),
        ("iteration_rate", Model::IterationRate),
        ("request_rate", Model::RequestRate),
    ];
    let model = one_of(model_node, &child(path, "model"), &choices, errors)?;

    let built = match model {
        Model::ClosedVus => closed_vus(&mut map, path, errors).map(LoadModel::ClosedVus),
        Model::IterationRate => {
            let max_vus = non_zero_u32(&mut map, "max_vus", path, errors);
            let schedule = rate_schedule(&mut map, path, errors);
            let queue = queue(&mut map, path, errors);
            match (schedule, max_vus, queue) {
                (Some(schedule), Some(max_vus), Some(queue)) => Some(LoadModel::IterationRate {
                    schedule,
                    max_vus,
                    queue,
                }),
                _ => None,
            }
        }
        Model::RequestRate => {
            let vus = non_zero_u32(&mut map, "vus", path, errors);
            let schedule = rate_schedule(&mut map, path, errors);
            match (schedule, vus) {
                (Some(schedule), Some(vus)) => Some(LoadModel::RequestRate { schedule, vus }),
                _ => None,
            }
        }
    };
    map.finish(errors);
    built
}

/// A required key holding an integer of at least 1 that fits `u32`.
fn non_zero_u32(
    map: &mut MapReader<'_>,
    key: &'static str,
    path: &str,
    errors: &mut Errors,
) -> Option<NonZeroU32> {
    let node = map.required(key, errors)?;
    integer(node, &child(path, key), 1, U32_MAX, errors)
        .and_then(|v| NonZeroU32::new(u32::try_from(v).ok()?))
}

fn closed_vus(map: &mut MapReader<'_>, path: &str, errors: &mut Errors) -> Option<ClosedVusLoad> {
    if let Some(stages_node) = map.optional("stages") {
        for key in ["vus", "duration", "iterations"] {
            map.conflict(key, "stages", errors);
        }
        let start = match map.optional("start") {
            Some(n) => integer(n, &child(path, "start"), 0, U32_MAX, errors)
                .and_then(|v| u32::try_from(v).ok()),
            None => Some(0),
        };
        let stages = stages(stages_node, &child(path, "stages"), vus_target, errors);
        return Some(ClosedVusLoad::Ramping {
            start: start?,
            stages: stages?,
        });
    }

    map.reject("start", "`start` needs `stages`", errors);
    let vus = non_zero_u32(map, "vus", path, errors);
    let stop = match map.exactly_one(&["duration", "iterations"], errors) {
        Some(("duration", n)) => {
            parsed(n, &child(path, "duration"), parse_duration, errors).map(ClosedStop::Duration)
        }
        Some((_, n)) => integer(n, &child(path, "iterations"), 1, u64::MAX, errors)
            .and_then(NonZeroU64::new)
            .map(ClosedStop::IterationsPerVu),
        None => None,
    };
    Some(ClosedVusLoad::Fixed {
        vus: vus?,
        stop: stop?,
    })
}

fn vus_target(node: &Node, path: &str, errors: &mut Errors) -> Option<u32> {
    integer(node, path, 0, U32_MAX, errors).and_then(|v| u32::try_from(v).ok())
}

fn rate_target(node: &Node, path: &str, errors: &mut Errors) -> Option<Rate> {
    parsed(node, path, str::parse::<Rate>, errors)
}

/// The `rate`/`duration` pair, or `start` and `stages`, of a rate model.
fn rate_schedule(map: &mut MapReader<'_>, path: &str, errors: &mut Errors) -> Option<RateSchedule> {
    if let Some(stages_node) = map.optional("stages") {
        for key in ["rate", "duration"] {
            map.conflict(key, "stages", errors);
        }
        let start = match map.optional("start") {
            Some(n) => rate_target(n, &child(path, "start"), errors),
            None => Some(Rate {
                count: 0,
                per: RateUnit::Second,
            }),
        };
        let stages = stages(stages_node, &child(path, "stages"), rate_target, errors);
        return Some(RateSchedule::Ramping {
            start: start?,
            stages: stages?,
        });
    }

    map.reject("start", "`start` needs `stages`", errors);
    let rate = map
        .required("rate", errors)
        .and_then(|n| rate_target(n, &child(path, "rate"), errors));
    let duration = map
        .required("duration", errors)
        .and_then(|n| parsed(n, &child(path, "duration"), parse_duration, errors));
    Some(RateSchedule::Fixed {
        rate: rate?,
        duration: duration?,
    })
}

/// Reads every stage so one bad stage doesn't hide another.
fn stages<T>(
    node: &Node,
    path: &str,
    target: fn(&Node, &str, &mut Errors) -> Option<T>,
    errors: &mut Errors,
) -> Option<Vec<Stage<T>>> {
    let items = list(node, path, errors)?;
    let mut out = Vec::with_capacity(items.len());
    let mut ok = true;
    for (index, stage_node) in items.iter().enumerate() {
        let stage_path = item(path, index);
        let Some(mut map) = MapReader::new(stage_node, &stage_path, errors) else {
            ok = false;
            continue;
        };
        let to = map
            .required("to", errors)
            .and_then(|n| target(n, &child(&stage_path, "to"), errors));
        let over = map
            .required("over", errors)
            .and_then(|n| parsed(n, &child(&stage_path, "over"), parse_duration, errors));
        map.finish(errors);
        match (to, over) {
            (Some(to), Some(over)) => out.push(Stage { to, over }),
            _ => ok = false,
        }
    }
    ok.then_some(out)
}

/// An absent `queue` is the default. Each key present replaces one value.
fn queue(map: &mut MapReader<'_>, path: &str, errors: &mut Errors) -> Option<QueuePolicy> {
    let Some(node) = map.optional("queue") else {
        return Some(QueuePolicy::default());
    };
    let queue_path = child(path, "queue");
    let mut queue_map = MapReader::new(node, &queue_path, errors)?;
    let mut policy = QueuePolicy::default();
    let mut ok = true;
    if let Some(n) = queue_map.optional("capacity") {
        match integer(n, &child(&queue_path, "capacity"), 1, U32_MAX, errors)
            .and_then(|v| NonZeroU32::new(u32::try_from(v).ok()?))
        {
            Some(v) => policy.capacity = v,
            None => ok = false,
        }
    }
    if let Some(n) = queue_map.optional("max_wait") {
        match parsed(n, &child(&queue_path, "max_wait"), parse_duration, errors) {
            Some(v) => policy.max_wait = v,
            None => ok = false,
        }
    }
    queue_map.finish(errors);
    ok.then_some(policy)
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};
    use std::time::Duration;

    use vush_core::plan::{
        ClosedStop, ClosedVusLoad, QueuePolicy, Rate, RateSchedule, RateUnit, Stage, parse_duration,
    };

    use super::*;
    use crate::compile::testing::{assert_fails, run};

    fn d(text: &str) -> Duration {
        parse_duration(text).unwrap()
    }

    fn rate(text: &str) -> Rate {
        text.parse::<Rate>().unwrap()
    }

    fn nz(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    fn queue(capacity: u32, max_wait: &str) -> QueuePolicy {
        QueuePolicy {
            capacity: nz(capacity),
            max_wait: d(max_wait),
        }
    }

    fn zero_rate() -> Rate {
        Rate {
            count: 0,
            per: RateUnit::Second,
        }
    }

    #[test]
    fn loads_map_to_their_model() {
        let rows: Vec<(&str, LoadModel)> = vec![
            (
                "{model: closed_vus, vus: 20, duration: 5m}",
                LoadModel::ClosedVus(ClosedVusLoad::Fixed {
                    vus: nz(20),
                    stop: ClosedStop::Duration(d("5m")),
                }),
            ),
            (
                "{model: closed_vus, vus: 5, iterations: 10}",
                LoadModel::ClosedVus(ClosedVusLoad::Fixed {
                    vus: nz(5),
                    stop: ClosedStop::IterationsPerVu(NonZeroU64::new(10).unwrap()),
                }),
            ),
            (
                "{model: closed_vus, stages: [{to: 100, over: 2m}]}",
                LoadModel::ClosedVus(ClosedVusLoad::Ramping {
                    start: 0,
                    stages: vec![Stage {
                        to: 100,
                        over: d("2m"),
                    }],
                }),
            ),
            (
                "{model: closed_vus, start: 5, stages: [{to: 0, over: 1s}]}",
                LoadModel::ClosedVus(ClosedVusLoad::Ramping {
                    start: 5,
                    stages: vec![Stage {
                        to: 0,
                        over: d("1s"),
                    }],
                }),
            ),
            (
                "{model: closed_vus, stages: []}",
                LoadModel::ClosedVus(ClosedVusLoad::Ramping {
                    start: 0,
                    stages: vec![],
                }),
            ),
            (
                "{model: iteration_rate, max_vus: 200, rate: 20/s, duration: 10m}",
                LoadModel::IterationRate {
                    schedule: RateSchedule::Fixed {
                        rate: rate("20/s"),
                        duration: d("10m"),
                    },
                    max_vus: nz(200),
                    queue: QueuePolicy::default(),
                },
            ),
            (
                "{model: iteration_rate, max_vus: 1, rate: 0/s, duration: 1s}",
                LoadModel::IterationRate {
                    schedule: RateSchedule::Fixed {
                        rate: rate("0/s"),
                        duration: d("1s"),
                    },
                    max_vus: nz(1),
                    queue: QueuePolicy::default(),
                },
            ),
            (
                "{model: iteration_rate, max_vus: 1, rate: 1/s, duration: 1s, queue: {capacity: 500}}",
                LoadModel::IterationRate {
                    schedule: RateSchedule::Fixed {
                        rate: rate("1/s"),
                        duration: d("1s"),
                    },
                    max_vus: nz(1),
                    queue: queue(500, "1s"),
                },
            ),
            (
                "{model: iteration_rate, max_vus: 1, rate: 1/s, duration: 1s, queue: {max_wait: 2s}}",
                LoadModel::IterationRate {
                    schedule: RateSchedule::Fixed {
                        rate: rate("1/s"),
                        duration: d("1s"),
                    },
                    max_vus: nz(1),
                    queue: queue(1000, "2s"),
                },
            ),
            (
                "{model: iteration_rate, max_vus: 300, stages: [{to: 50/s, over: 2m}]}",
                LoadModel::IterationRate {
                    schedule: RateSchedule::Ramping {
                        start: zero_rate(),
                        stages: vec![Stage {
                            to: rate("50/s"),
                            over: d("2m"),
                        }],
                    },
                    max_vus: nz(300),
                    queue: QueuePolicy::default(),
                },
            ),
            (
                "{model: request_rate, vus: 50, rate: 200/s, duration: 5m}",
                LoadModel::RequestRate {
                    schedule: RateSchedule::Fixed {
                        rate: rate("200/s"),
                        duration: d("5m"),
                    },
                    vus: nz(50),
                },
            ),
            (
                "{model: request_rate, vus: 10, start: 5/s, stages: [{to: 100/s, over: 1m}]}",
                LoadModel::RequestRate {
                    schedule: RateSchedule::Ramping {
                        start: rate("5/s"),
                        stages: vec![Stage {
                            to: rate("100/s"),
                            over: d("1m"),
                        }],
                    },
                    vus: nz(10),
                },
            ),
        ];
        for (yaml, expected) in rows {
            let (got, errors) = run(yaml, |n, e| load(n, "load", e));
            assert!(errors.is_empty(), "{yaml}: {errors:?}");
            assert_eq!(got, Some(expected), "{yaml}");
        }
    }

    #[test]
    fn load_failures_name_their_path() {
        const MODELS: &str = "expected one of closed_vus, iteration_rate or request_rate";
        const U32: &str = "expected an integer from 1 to 4294967295";
        const U32_0: &str = "expected an integer from 0 to 4294967295";
        let rows = [
            ("5", "load", "expected a map"),
            (
                "{vus: 1, duration: 1s}",
                "load",
                "missing required key `model`",
            ),
            ("{model: open}", "load.model", MODELS),
            ("{model: open, vus: 1}", "load.model", MODELS),
            (
                "{model: closed_vus, duration: 1s}",
                "load",
                "missing required key `vus`",
            ),
            (
                "{model: closed_vus, vus: 1}",
                "load",
                "needs one of `duration` or `iterations`",
            ),
            (
                "{model: closed_vus, vus: 1, duration: 1s, iterations: 2}",
                "load.iterations",
                "`duration` and `iterations` can't be used together",
            ),
            ("{model: closed_vus, vus: 0, duration: 1s}", "load.vus", U32),
            (
                "{model: closed_vus, vus: 1, iterations: 0}",
                "load.iterations",
                "expected an integer from 1 to 18446744073709551615",
            ),
            (
                "{model: closed_vus, vus: 1, duration: 5}",
                "load.duration",
                "expected a duration",
            ),
            (
                "{model: closed_vus, vus: 1, duration: 1s, start: 0}",
                "load.start",
                "`start` needs `stages`",
            ),
            (
                "{model: closed_vus, stages: [], vus: 1}",
                "load.vus",
                "`stages` and `vus` can't be used together",
            ),
            (
                "{model: closed_vus, stages: [], duration: 1s}",
                "load.duration",
                "`stages` and `duration` can't be used together",
            ),
            (
                "{model: closed_vus, stages: [], iterations: 1}",
                "load.iterations",
                "`stages` and `iterations` can't be used together",
            ),
            (
                "{model: closed_vus, vus: 1, stages: []}",
                "load.stages",
                "`vus` and `stages` can't be used together",
            ),
            (
                "{model: closed_vus, duration: 1s, stages: []}",
                "load.stages",
                "`duration` and `stages` can't be used together",
            ),
            (
                "{model: closed_vus, iterations: 1, stages: []}",
                "load.stages",
                "`iterations` and `stages` can't be used together",
            ),
            (
                "{model: iteration_rate, max_vus: 1, rate: 1/s, stages: []}",
                "load.stages",
                "`rate` and `stages` can't be used together",
            ),
            (
                "{model: iteration_rate, max_vus: 1, duration: 1s, stages: []}",
                "load.stages",
                "`duration` and `stages` can't be used together",
            ),
            (
                "{model: request_rate, vus: 1, rate: 1/s, stages: []}",
                "load.stages",
                "`rate` and `stages` can't be used together",
            ),
            (
                "{model: request_rate, vus: 1, duration: 1s, stages: []}",
                "load.stages",
                "`duration` and `stages` can't be used together",
            ),
            (
                "{model: closed_vus, start: x, stages: []}",
                "load.start",
                U32_0,
            ),
            (
                "{model: closed_vus, stages: 5}",
                "load.stages",
                "expected a list",
            ),
            (
                "{model: closed_vus, stages: [5]}",
                "load.stages[0]",
                "expected a map",
            ),
            (
                "{model: closed_vus, stages: [{to: 1}]}",
                "load.stages[0]",
                "missing required key `over`",
            ),
            (
                "{model: closed_vus, stages: [{to: -1, over: 1s}]}",
                "load.stages[0].to",
                U32_0,
            ),
            (
                "{model: closed_vus, stages: [{to: 1, over: 1s, x: 1}]}",
                "load.stages[0].x",
                "unknown key `x`",
            ),
            (
                "{model: closed_vus, vus: 1, duration: 1s, queue: {}}",
                "load.queue",
                "unknown key `queue`",
            ),
            (
                "{model: iteration_rate, rate: 1/s, duration: 1s}",
                "load",
                "missing required key `max_vus`",
            ),
            (
                "{model: iteration_rate, max_vus: 0, rate: 1/s, duration: 1s}",
                "load.max_vus",
                U32,
            ),
            (
                "{model: iteration_rate, max_vus: 1, rate: 1/s}",
                "load",
                "missing required key `duration`",
            ),
            (
                "{model: iteration_rate, max_vus: 1, duration: 1s}",
                "load",
                "missing required key `rate`",
            ),
            (
                "{model: iteration_rate, max_vus: 1, rate: 20, duration: 1s}",
                "load.rate",
                "expected a rate",
            ),
            (
                "{model: iteration_rate, max_vus: 1, rate: 0.5/s, duration: 1s}",
                "load.rate",
                "expected a rate",
            ),
            (
                "{model: iteration_rate, max_vus: 1, stages: [], rate: 1/s}",
                "load.rate",
                "`stages` and `rate` can't be used together",
            ),
            (
                "{model: iteration_rate, max_vus: 1, stages: [], duration: 1s}",
                "load.duration",
                "`stages` and `duration` can't be used together",
            ),
            (
                "{model: iteration_rate, max_vus: 1, rate: 1/s, duration: 1s, start: 0/s}",
                "load.start",
                "`start` needs `stages`",
            ),
            (
                "{model: iteration_rate, max_vus: 1, rate: 1/s, duration: 1s, queue: 5}",
                "load.queue",
                "expected a map",
            ),
            (
                "{model: iteration_rate, max_vus: 1, rate: 1/s, duration: 1s, queue: {capacity: 0}}",
                "load.queue.capacity",
                U32,
            ),
            (
                "{model: iteration_rate, max_vus: 1, rate: 1/s, duration: 1s, queue: {max_wait: x}}",
                "load.queue.max_wait",
                "expected a duration",
            ),
            (
                "{model: iteration_rate, max_vus: 1, rate: 1/s, duration: 1s, queue: {nope: 1}}",
                "load.queue.nope",
                "unknown key `nope`",
            ),
            (
                "{model: request_rate, rate: 1/s, duration: 1s}",
                "load",
                "missing required key `vus`",
            ),
            (
                "{model: request_rate, vus: 1, stages: [{to: 20, over: 1s}]}",
                "load.stages[0].to",
                "expected a rate",
            ),
            (
                "{model: request_rate, vus: 1, rate: 1/s, duration: 1s, max_vus: 1}",
                "load.max_vus",
                "unknown key `max_vus`",
            ),
            (
                "{model: request_rate, vus: 1, rate: 1/s, duration: 1s, queue: {}}",
                "load.queue",
                "unknown key `queue`",
            ),
        ];
        for (yaml, path, fragment) in rows {
            assert_fails(yaml, path, fragment, |n, e| load(n, "load", e));
        }
    }

    #[test]
    fn several_fixed_keys_before_stages_each_conflict() {
        let (_, errors) = run(
            "{model: closed_vus, vus: 1, duration: 1s, stages: []}",
            |n, e| load(n, "load", e),
        );
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors.iter().all(|e| e.path == "load.stages"));
        for key in ["vus", "duration"] {
            let want = format!("`{key}` and `stages` can't be used together");
            assert!(errors.iter().any(|e| e.message.contains(&want)), "{want}");
        }
    }
}
