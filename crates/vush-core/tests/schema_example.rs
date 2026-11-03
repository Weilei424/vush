//! The complete example in `docs/config-schema.md`, built by hand through the
//! public plan API.
//!
//! This proves the plan types can hold everything the example uses. If the
//! example and the types drift apart, this test stops compiling or stops
//! describing a valid plan.

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::time::Duration;

use vush_core::plan::{
    Body, Bound, BoundOp, ByteSize, ClosedVusLoad, Comparison, Condition, Expect, ExtractSource,
    Extraction, HttpSettings, HttpVersion, IfStep, JsonAssertion, JsonNumber, JsonPath,
    JsonTemplate, LoadModel, PercentileStat, PlanDraft, PoolScope, QueuePolicy, Rate, RateSchedule,
    Ratio, RepeatStep, RequestStep, RequestTarget, ResourceLimits, RetryPolicy, Scalar, Scenario,
    Stage, StatePolicy, StatusMatch, Step, Template, Threshold, ThresholdCheck, TlsSettings,
    VarRef, WorkloadPlan, parse_duration,
};

fn dur(text: &str) -> Duration {
    parse_duration(text).unwrap()
}

fn rate(text: &str) -> Rate {
    text.parse().unwrap()
}

fn size(text: &str) -> ByteSize {
    text.parse().unwrap()
}

fn percent(text: &str) -> Ratio {
    text.parse().unwrap()
}

fn tpl(text: &str) -> Template {
    text.parse().unwrap()
}

fn json_path(text: &str) -> JsonPath {
    text.parse().unwrap()
}

fn var(text: &str) -> VarRef {
    text.parse().unwrap()
}

fn nonzero(count: u32) -> NonZeroU32 {
    NonZeroU32::new(count).unwrap()
}

/// A header, query or form entry.
fn pair(name: &str, value: &str) -> (String, Template) {
    (name.to_owned(), tpl(value))
}

fn target_path(text: &str) -> RequestTarget {
    RequestTarget::Path(tpl(text))
}

/// A request with every optional key at its default. `timeout` is the
/// `http.timeout` of the settings the step runs under.
fn request(name: &str, method: &str, target: RequestTarget, timeout: Duration) -> RequestStep {
    RequestStep {
        name: Some(name.to_owned()),
        method: method.parse().unwrap(),
        target,
        query: vec![],
        headers: vec![],
        body: None,
        timeout,
        required: true,
        retry: None,
        expect: Expect::default(),
        extract: vec![],
    }
}

fn step(request: RequestStep) -> Step {
    Step::Request(Box::new(request))
}

fn extract_json(var: &str, path: &str) -> Extraction {
    Extraction {
        var: var.to_owned(),
        source: ExtractSource::JsonPath(json_path(path)),
    }
}

fn extract_header(var: &str, header: &str) -> Extraction {
    Extraction {
        var: var.to_owned(),
        source: ExtractSource::Header(header.to_owned()),
    }
}

fn status(codes: &[u16]) -> StatusMatch {
    StatusMatch::OneOf(codes.to_vec())
}

fn assertion(path: &str, op: Comparison) -> JsonAssertion {
    JsonAssertion {
        path: json_path(path),
        op,
    }
}

fn compare(reference: &str, op: Comparison) -> Condition {
    Condition::Compare {
        var: var(reference),
        op,
    }
}

fn string(text: &str) -> Scalar {
    Scalar::String(tpl(text))
}

/// The file-level `http` block, merged over the defaults.
fn file_http() -> HttpSettings {
    HttpSettings {
        version: HttpVersion::Http2,
        timeout: dur("10s"),
        max_response_size: size("2MiB"),
        tls: TlsSettings {
            ca_file: Some("certs/ca.pem".to_owned()),
            insecure: false,
        },
        ..HttpSettings::default()
    }
}

/// The file-level `limits` block, merged over the defaults.
fn file_limits() -> ResourceLimits {
    ResourceLimits {
        max_variables: 128,
        hook_timeout: dur("30s"),
        ..ResourceLimits::default()
    }
}

fn file_setup(timeout: Duration) -> Vec<Step> {
    vec![
        step(RequestStep {
            body: Some(Body::Json(JsonTemplate::Object(vec![
                ("user".to_owned(), JsonTemplate::String(tpl("admin"))),
                (
                    "password".to_owned(),
                    JsonTemplate::String(tpl("${env.ADMIN_PASSWORD}")),
                ),
            ]))),
            extract: vec![extract_json("admin_token", "$.token")],
            ..request("admin_login", "POST", target_path("/admin/login"), timeout)
        }),
        Step::Wait(dur("1s")),
    ]
}

fn file_teardown(timeout: Duration) -> Vec<Step> {
    vec![step(RequestStep {
        headers: vec![pair("Authorization", "Bearer ${global.admin_token}")],
        ..request(
            "admin_logout",
            "POST",
            target_path("/admin/logout"),
            timeout,
        )
    })]
}

fn thresholds() -> Vec<Threshold> {
    vec![
        Threshold {
            scenario: None,
            check: ThresholdCheck::HttpRequestDuration {
                stat: PercentileStat::P95,
                bound: Bound {
                    op: BoundOp::Lt,
                    value: dur("500ms"),
                },
            },
        },
        Threshold {
            scenario: None,
            check: ThresholdCheck::ErrorRate(Bound {
                op: BoundOp::Lt,
                value: percent("1%"),
            }),
        },
        Threshold {
            scenario: Some("health".to_owned()),
            check: ThresholdCheck::RequestRate(Bound {
                op: BoundOp::Gte,
                value: rate("90/s"),
            }),
        },
        Threshold {
            scenario: Some("checkout".to_owned()),
            check: ThresholdCheck::IterationRate(Bound {
                op: BoundOp::Gte,
                value: rate("18/s"),
            }),
        },
        Threshold {
            scenario: Some("checkout".to_owned()),
            check: ThresholdCheck::DroppedWorkRatio(Bound {
                op: BoundOp::Lte,
                value: percent("0.5%"),
            }),
        },
    ]
}

/// Closed model, ramping. Overrides `http.pool` and `http.redirects` only, and
/// sets no `limits` and no `max_request_rate` of its own.
fn browse() -> Scenario {
    let http = HttpSettings {
        pool: PoolScope::Scenario,
        redirects: 0,
        ..file_http()
    };
    let timeout = http.timeout;
    Scenario {
        name: "browse".to_owned(),
        load: LoadModel::ClosedVus(ClosedVusLoad::Ramping {
            start: 0,
            stages: vec![
                Stage {
                    to: 50,
                    over: dur("2m"),
                },
                Stage {
                    to: 50,
                    over: dur("10m"),
                },
                Stage {
                    to: 0,
                    over: dur("1m"),
                },
            ],
        }),
        http,
        limits: file_limits(),
        max_request_rate: None,
        state: StatePolicy::ResetEachIteration,
        setup: vec![],
        teardown: vec![],
        vu_setup: vec![step(RequestStep {
            body: Some(Body::Json(JsonTemplate::Object(vec![
                (
                    "username".to_owned(),
                    JsonTemplate::String(tpl("shopper-${vu.id}")),
                ),
                (
                    "password".to_owned(),
                    JsonTemplate::String(tpl("${env.SHOPPER_PASSWORD}")),
                ),
            ]))),
            expect: Expect {
                status: status(&[200]),
                body_contains: None,
                json: vec![assertion("$.token", Comparison::Exists(true))],
            },
            extract: vec![extract_json("token", "$.token")],
            ..request("sign_in", "POST", target_path("/login"), timeout)
        })],
        vu_teardown: vec![step(RequestStep {
            headers: vec![pair("Authorization", "Bearer ${token}")],
            required: false,
            ..request("sign_out", "POST", target_path("/logout"), timeout)
        })],
        steps: vec![
            step(RequestStep {
                query: vec![pair("page", "${vu.iteration}"), pair("sort", "price")],
                headers: vec![
                    pair("Authorization", "Bearer ${token}"),
                    pair("Accept", "application/json"),
                ],
                timeout: dur("5s"),
                expect: Expect {
                    status: status(&[200, 206]),
                    body_contains: Some(tpl("products")),
                    json: vec![],
                },
                extract: vec![extract_header("etag", "ETag")],
                ..request("list_products", "GET", target_path("/products"), timeout)
            }),
            Step::Wait(dur("500ms")),
            Step::Repeat(RepeatStep {
                count: nonzero(3),
                deadline: dur("30s"),
                steps: vec![
                    step(RequestStep {
                        headers: vec![pair("If-None-Match", "${etag}")],
                        expect: Expect {
                            status: status(&[200, 304]),
                            body_contains: None,
                            json: vec![],
                        },
                        ..request(
                            "view_featured",
                            "GET",
                            target_path("/products/featured"),
                            timeout,
                        )
                    }),
                    Step::Wait(dur("250ms")),
                ],
            }),
        ],
    }
}

/// Open iteration-arrival model, fixed, with its own queue and its own
/// `limits` (including a request-rate budget). It inherits the file-level
/// `http` unchanged.
fn checkout() -> Scenario {
    let http = file_http();
    let timeout = http.timeout;
    Scenario {
        name: "checkout".to_owned(),
        load: LoadModel::IterationRate {
            schedule: RateSchedule::Fixed {
                rate: rate("20/s"),
                duration: dur("10m"),
            },
            max_vus: nonzero(200),
            queue: QueuePolicy {
                capacity: nonzero(500),
                max_wait: dur("2s"),
            },
        },
        http,
        limits: ResourceLimits {
            max_variables: 64,
            max_variable_size: size("16KiB"),
            max_cookies: 32,
            ..file_limits()
        },
        max_request_rate: Some(rate("100/s")),
        state: StatePolicy::Persist,
        setup: vec![step(RequestStep {
            headers: vec![pair("Authorization", "Bearer ${global.admin_token}")],
            extract: vec![extract_json("currency", "$.currency")],
            ..request("load_config", "GET", target_path("/config"), timeout)
        })],
        teardown: vec![step(RequestStep {
            headers: vec![pair("Authorization", "Bearer ${global.admin_token}")],
            ..request("flush_carts", "POST", target_path("/admin/flush"), timeout)
        })],
        vu_setup: vec![],
        vu_teardown: vec![],
        steps: vec![
            step(RequestStep {
                body: Some(Body::Json(JsonTemplate::Object(vec![
                    ("sku".to_owned(), JsonTemplate::String(tpl("ABC-1"))),
                    (
                        "quantity".to_owned(),
                        JsonTemplate::Number(JsonNumber::Int(2)),
                    ),
                    ("gift".to_owned(), JsonTemplate::Bool(false)),
                    ("note".to_owned(), JsonTemplate::Null),
                    (
                        "tags".to_owned(),
                        JsonTemplate::Array(vec![
                            JsonTemplate::String(tpl("sale")),
                            JsonTemplate::String(tpl("${scenario.currency}")),
                        ]),
                    ),
                    (
                        "options".to_owned(),
                        JsonTemplate::Object(vec![
                            ("express".to_owned(), JsonTemplate::Bool(true)),
                            (
                                "discount".to_owned(),
                                JsonTemplate::Number(JsonNumber::Float(0.15)),
                            ),
                        ]),
                    ),
                ]))),
                retry: Some(RetryPolicy {
                    attempts: 3,
                    backoff: dur("250ms"),
                    allow_non_idempotent: true,
                }),
                expect: Expect {
                    status: status(&[201]),
                    body_contains: None,
                    json: vec![
                        assertion("$.items[0].sku", Comparison::Equals(string("ABC-1"))),
                        assertion(
                            "$.items[0].quantity",
                            Comparison::Equals(Scalar::Number(JsonNumber::Int(2))),
                        ),
                        assertion("$.total", Comparison::Gt(JsonNumber::Int(0))),
                        assertion(
                            "$.currency",
                            Comparison::Equals(string("${scenario.currency}")),
                        ),
                        assertion("$.message", Comparison::Contains(tpl("added"))),
                        assertion("$.error", Comparison::Exists(false)),
                    ],
                },
                extract: vec![extract_json("cart_id", "$.id")],
                ..request("add_to_cart", "POST", target_path("/cart"), timeout)
            }),
            step(RequestStep {
                body: Some(Body::Form(vec![
                    pair("code", "SAVE10"),
                    pair("currency", "${scenario.currency}"),
                ])),
                required: false,
                extract: vec![extract_json("discount", "$.discount")],
                ..request(
                    "apply_coupon",
                    "POST",
                    target_path("/cart/${cart_id}/coupon"),
                    timeout,
                )
            }),
            Step::If(IfStep {
                when: Condition::All(vec![
                    compare("discount", Comparison::Exists(true)),
                    Condition::Any(vec![
                        compare("discount", Comparison::Gte(JsonNumber::Int(10))),
                        compare("scenario.currency", Comparison::Equals(string("USD"))),
                    ]),
                    Condition::Not(Box::new(compare(
                        "vu.iteration",
                        Comparison::Equals(Scalar::Number(JsonNumber::Int(0))),
                    ))),
                ]),
                then: vec![step(RequestStep {
                    body: Some(Body::Text(tpl("Discount applied: ${discount}"))),
                    ..request(
                        "add_note",
                        "PUT",
                        target_path("/cart/${cart_id}/note"),
                        timeout,
                    )
                })],
                otherwise: vec![Step::Wait(dur("200ms"))],
            }),
        ],
    }
}

/// Request-start rate model, ramping. Overrides `http.timeout` and
/// `http.tls.insecure`, and keeps the file-level `tls.ca_file`.
fn health() -> Scenario {
    let file = file_http();
    let http = HttpSettings {
        timeout: dur("2s"),
        tls: TlsSettings {
            insecure: true,
            ..file.tls.clone()
        },
        ..file
    };
    let timeout = http.timeout;
    Scenario {
        name: "health".to_owned(),
        load: LoadModel::RequestRate {
            schedule: RateSchedule::Ramping {
                start: rate("0/s"),
                stages: vec![
                    Stage {
                        to: rate("100/s"),
                        over: dur("1m"),
                    },
                    Stage {
                        to: rate("100/s"),
                        over: dur("5m"),
                    },
                ],
            },
            vus: nonzero(10),
        },
        http,
        limits: file_limits(),
        max_request_rate: None,
        state: StatePolicy::Persist,
        setup: vec![],
        teardown: vec![],
        vu_setup: vec![],
        vu_teardown: vec![],
        steps: vec![step(RequestStep {
            expect: Expect {
                status: status(&[200]),
                body_contains: None,
                json: vec![],
            },
            ..request(
                "ping",
                "HEAD",
                RequestTarget::Url(tpl("${env.STATUS_URL}/ping")),
                timeout,
            )
        })],
    }
}

/// Adds the kind of every step in `steps`, and of the steps nested inside
/// them, to `kinds`.
fn collect_kinds(steps: &[Step], kinds: &mut BTreeSet<&'static str>) {
    for step in steps {
        match step {
            Step::Request(_) => {
                kinds.insert("request");
            }
            Step::Wait(_) => {
                kinds.insert("wait");
            }
            Step::If(branch) => {
                kinds.insert("if");
                collect_kinds(&branch.then, kinds);
                collect_kinds(&branch.otherwise, kinds);
            }
            Step::Repeat(repeat) => {
                kinds.insert("repeat");
                collect_kinds(&repeat.steps, kinds);
            }
        }
    }
}

#[test]
fn schema_example_builds_through_public_api() {
    let draft = PlanDraft {
        name: "storefront-load".to_owned(),
        base_url: Some(tpl("${env.BASE_URL}")),
        http: file_http(),
        limits: file_limits(),
        max_request_rate: Some(rate("500/s")),
        setup: file_setup(file_http().timeout),
        teardown: file_teardown(file_http().timeout),
        thresholds: thresholds(),
        scenarios: vec![browse(), checkout(), health()],
    };

    let plan = WorkloadPlan::new(draft).expect("the documented example must be a valid plan");

    // Every accessor returns what the example sets.
    assert_eq!(plan.name(), "storefront-load");
    assert_eq!(plan.base_url(), Some(&tpl("${env.BASE_URL}")));
    assert_eq!(plan.http(), &file_http());
    assert_ne!(plan.http(), &HttpSettings::default());
    assert_eq!(plan.http().version, HttpVersion::Http2);
    assert_eq!(plan.http().timeout, Duration::from_secs(10));
    assert_eq!(plan.limits(), &file_limits());
    assert_ne!(plan.limits(), &ResourceLimits::default());
    assert_eq!(plan.limits().max_variables, 128);
    assert_eq!(plan.max_request_rate(), Some(rate("500/s")));
    assert_eq!(plan.setup(), file_setup(Duration::from_secs(10)).as_slice());
    assert_eq!(
        plan.teardown(),
        file_teardown(Duration::from_secs(10)).as_slice()
    );
    assert_eq!((plan.setup().len(), plan.teardown().len()), (2, 1));
    assert_eq!(plan.thresholds(), thresholds().as_slice());
    assert_eq!(plan.thresholds().len(), 5);
    assert_eq!(
        plan.scenarios(),
        [browse(), checkout(), health()].as_slice()
    );

    // One scenario per load model, in the order the example lists them.
    let scenarios = plan.scenarios();
    let names: Vec<&str> = scenarios.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["browse", "checkout", "health"]);
    assert!(matches!(
        scenarios[0].load,
        LoadModel::ClosedVus(ClosedVusLoad::Ramping { .. })
    ));
    assert!(matches!(
        scenarios[1].load,
        LoadModel::IterationRate {
            schedule: RateSchedule::Fixed { .. },
            ..
        }
    ));
    assert!(matches!(
        scenarios[2].load,
        LoadModel::RequestRate {
            schedule: RateSchedule::Ramping { .. },
            ..
        }
    ));

    // Overrides are merged key by key, and a scenario's request-rate budget is
    // its own: it does not inherit the file-level one.
    assert_eq!(scenarios[0].http.pool, PoolScope::Scenario);
    assert_eq!(scenarios[0].http.timeout, plan.http().timeout);
    assert_eq!(scenarios[1].http, *plan.http());
    assert!(scenarios[2].http.tls.insecure);
    assert_eq!(
        scenarios[2].http.tls.ca_file.as_deref(),
        Some("certs/ca.pem")
    );
    assert_eq!(scenarios[1].limits.max_variables, 64);
    assert_eq!(scenarios[1].limits.hook_timeout, plan.limits().hook_timeout);
    assert_eq!(
        scenarios
            .iter()
            .map(|s| s.max_request_rate)
            .collect::<Vec<_>>(),
        [None, Some(rate("100/s")), None]
    );

    // The plan holds a step of every kind, counting steps nested in `if`,
    // `repeat` and every kind of hook.
    let mut kinds = BTreeSet::new();
    collect_kinds(plan.setup(), &mut kinds);
    collect_kinds(plan.teardown(), &mut kinds);
    for scenario in scenarios {
        for hook in [
            &scenario.setup,
            &scenario.teardown,
            &scenario.vu_setup,
            &scenario.vu_teardown,
            &scenario.steps,
        ] {
            collect_kinds(hook, &mut kinds);
        }
    }
    assert_eq!(
        kinds.into_iter().collect::<Vec<_>>(),
        ["if", "repeat", "request", "wait"]
    );
}
