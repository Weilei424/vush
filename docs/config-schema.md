# Vush test file reference (schema v1)

> **Status:** this page defines the test file format, but nothing can read a test file yet. `vush validate` arrives in P1.4, the fourth step of Phase 1, and it will accept every key described here, including keys that take no effect until a later phase. The "Runs" columns show which phase makes each area take effect. Phase numbers refer to the roadmap in [`planning/IMPLEMENTATION_PLAN.md`](planning/IMPLEMENTATION_PLAN.md). Until 1.0 the schema may change without notice (see [Versioning](#versioning)).

A Vush test is declared in a YAML or JSON file. The two formats use the same keys and the same structure, and a given test means the same thing in either. This page shows tables and YAML snippets, and ends with a [complete example](#complete-example) in both formats.

Conventions used throughout:

- All keys are `snake_case`.
- Unknown keys are errors. They are never ignored. Duplicate keys in a map are errors too, in both YAML and JSON.
- Durations, rates, sizes and percentages are strings in both formats (`"5s"` in JSON). Counts such as `vus`, `count` and `max_vus` are plain integers.
- In YAML, put quotation marks around any value that contains `${`.

## Value formats

Units are always required.

| Kind | Form | Accepted | Rejected |
|---|---|---|---|
| Duration | an integer plus `ms`, `s`, `m` or `h`; parts can be combined in descending order | `500ms`, `5m`, `1h30m` | `5`, `1.5s`, `5 m`, `30m1h`, a value too large to represent |
| Rate | an integer, `/`, then `s`, `m` or `h` | `20/s`, `300/m`, `0/s` | `20`, `0.5/s`, `20/d` |
| Size | an integer plus `B`, `KB`, `MB`, `GB`, `KiB`, `MiB` or `GiB` | `1MiB`, `500KB` | `1024`, `1.5MB`, a value too large to represent |
| Percentage | a number from 0 to 100 with up to 4 decimals, then `%` | `1%`, `0.5%`, `0%` | `0.01`, `101%`, `0.00001%` |

- `KB`, `MB` and `GB` are powers of 1000. `KiB`, `MiB` and `GiB` are powers of 1024.
- Rates are whole numbers. Write `30/m` instead of `0.5/s`.
- Whether a zero rate is acceptable depends on where it is used. `0/s` is valid as a ramp `start` or a stage target, but a fixed `rate` must be above zero.
- Numbers, booleans and `null` in `json` bodies and in comparisons (`equals`, `gt` and so on) are ordinary JSON values. Integers keep their exact value. An integer too large for a 64-bit unsigned value is an error. `NaN` and infinity are not allowed.

## Variables

A string can contain `${...}` references to variables. Vush splits each string into text and references when it reads the file, so nothing is interpreted while the test runs.

| Reference | Meaning | Value comes from |
|---|---|---|
| `${token}` | this user's variable | `extract` in `vu_setup`, `steps` or `vu_teardown` |
| `${scenario.x}` | read-only, one per scenario | `extract` in the scenario's `setup` |
| `${global.x}` | read-only, shared by the whole run | `extract` in the file-level `setup` |
| `${env.NAME}` | an environment variable, read once when the run starts | the environment |
| `${vu.id}`, `${vu.iteration}` | the user's number and the user's iteration count | Vush |

Rules:

- `$${` is a literal `${`. A `$` that is not followed by `{` is a literal `$`.
- `$${` is always the escape, so there is no way to write a literal `$` immediately before a reference. `Total: $${amount}` produces the literal text `Total: ${amount}`, not `Total: $` followed by the value of `amount`.
- Variable names match `[A-Za-z_][A-Za-z0-9_]*`.
- `env`, `global`, `scenario` and `vu` are reserved. They can't be variable names.
- These are errors: an unterminated `${`, an empty `${}`, whitespace inside the braces (`${ token }`), an unknown prefix (`${foo.bar}`) and an unknown built-in (`${vu.foo}`).
- Reading a variable that is missing or unset fails that step when the test runs. It is never replaced with an empty string.
- `extract` writes to the scope of the place it runs. An `extract` in the file-level `setup` writes `global.x`, one in a scenario's `setup` writes `scenario.x`, and one anywhere else writes the user's own variable. In every case the `extract` key is the bare name, for example `x`.
- `${env.NAME}` stores only the name in the test plan. The value is read when the run starts, so secrets from the environment are never part of the plan.

Templates are accepted in these places:

- `base_url`, so `"${env.BASE_URL}"` works
- a request's `path` and `url`
- values in `query`, `headers` and `form` (names are always plain text)
- a request's `body`
- string values inside a `json` body (keys are always plain text)
- string values in `expect` and `when` comparisons: `equals`, `not_equals` and `contains` in both, plus `body_contains` in `expect`

Everywhere else a string is plain text, including `method` and `tls.ca_file`. A request `name` is a fixed metrics label, so `${` in it is an error.

The `var` field in a `when` condition names a variable with the same reference form, without the `${}`: `var: token`, `var: scenario.x`. JSON assertions use `path` instead (see [JSON paths](#json-paths)).

Validation checks that each reference can be resolved where it is used. How a value is encoded when it is substituted into a URL or a JSON body is not specified yet, and this page will state it once it is.

## JSON paths

A JSON path points at one value in a JSON response body. It is `$` followed by any number of `.name` or `[N]` segments:

- `.name` selects an object field. The name can use `A-Z`, `a-z`, `0-9`, `_` and `-`.
- `[N]` selects an array element. `N` is a non-negative integer.
- `$` alone is the whole document.

| Accepted | Rejected |
|---|---|
| `$`, `$.token`, `$.items[0].id`, `$.data[2][1]`, `$.order-id` | `token`, `$.items[*]`, `$..id`, `$.items[-1]`, `$["id"]`, `$.items[?(@.ok)]` |

There are no wildcards, filters, recursive descent, negative indexes or quoted keys. The subset can grow in later versions.

JSON paths are used in `extract` (`json_path`) and in `expect.json` (`path`).

## Top level

| Key | Type | Required | Default | Runs |
|---|---|---|---|---|
| `schema_version` | integer, must be `1` | yes | | 1 |
| `name` | string | yes | | 6 |
| `base_url` | template | no | none | 2 |
| `http` | [block](#http) | no | the defaults in that table | 2 |
| `limits` | [block](#limits) | no | the defaults in that table | 3–5 |
| `setup`, `teardown` | [step list](#steps) | no | empty | 3 |
| `thresholds` | [list](#thresholds) | no | empty | 6 |
| `scenarios` | non-empty list of [scenarios](#scenarios) | yes | | 3–5 |

File-level `setup` runs once, before any scenario. File-level `teardown` runs once, after every scenario.

## `http`

`http` is allowed at file level and on a scenario. A scenario overrides the file-level settings key by key, including the keys inside `tls`. File-level `http` applies to file-level `setup` and `teardown`. A scenario's merged settings apply to its hooks and its steps.

| Key | Type | Default | Notes |
|---|---|---|---|
| `version` | `auto`, `http1` or `http2` | `auto` | `auto` negotiates the version with the server |
| `pool` | `shared`, `scenario` or `vu` | `shared` | One connection pool for the whole run, one per scenario, or one per user. Sharing a pool never shares cookies or credentials |
| `timeout` | duration | `30s` | For one physical attempt |
| `redirects` | integer, 0 or more | `5` | The most redirects to follow. `0` means don't follow |
| `cookies` | boolean | `true` | Whether each user keeps cookies. Cookies are always per user |
| `max_response_size` | size | `1MiB` | Upper bound on the size of a response body |
| `tls.ca_file` | path | none | Relative to the test file's directory. Read when the run starts |
| `tls.insecure` | boolean | `false` | Skips certificate verification |

```yaml
http:
  version: http2
  timeout: 10s
  tls:
    ca_file: certs/ca.pem
```

## `limits`

`limits` is allowed at file level and on a scenario.

| Key | Type | Default | Notes | Runs |
|---|---|---|---|---|
| `max_request_rate` | rate | none | At file level, a budget across all scenarios. On a scenario, that scenario's own budget. Both can apply at once. A scenario does not inherit the file-level value as its own | 5 |
| `max_variables` | integer | `256` | Per user. A scenario's value overrides the file's | 3 |
| `max_variable_size` | size | `64KiB` | Per variable value. A scenario's value overrides the file's | 3 |
| `max_cookies` | integer | `64` | Per user. A scenario's value overrides the file's | 3 |
| `hook_timeout` | duration | `60s` | For one whole hook run. A scenario's value overrides the file's | 3 |

These defaults are provisional. They can change as measurements come in.

Whether `max_request_rate` is a strict cap or a target, and how bursts are handled, is not specified yet.

```yaml
limits:
  max_request_rate: 500/s
  max_variables: 128
```

## `scenarios`

Each scenario has its own load and its own steps. Scenarios run side by side.

| Key | Type | Required | Default | Runs |
|---|---|---|---|---|
| `name` | string, unique in the file | yes | | 3 |
| `load` | [block](#load) | yes | | 4–5 |
| `http` | overrides, see [`http`](#http) | no | inherits the file-level settings | 2 |
| `limits` | overrides, see [`limits`](#limits) | no | inherits the file-level limits | 3–5 |
| `state` | `persist` or `reset_each_iteration` | no | `persist` | 4 |
| `setup`, `teardown` | [step list](#steps); once per scenario per run | no | empty | 3 |
| `vu_setup`, `vu_teardown` | [step list](#steps); once per user | no | empty | 4 |
| `steps` | non-empty [step list](#steps) | yes | | 3 |

`state` decides what a user keeps between iterations. `persist` keeps its variables and cookies. `reset_each_iteration` returns them to how they were right after `vu_setup`.

Hooks are step lists that use the same step kinds as `steps`.

## `load`

`model` is required. It is one of three models, and they are not interchangeable:

| Model | What it controls | Runs |
|---|---|---|
| `closed_vus` | a number of users, each starting its next iteration when its last one ends | 4 |
| `iteration_rate` | new iterations start at a rate, whatever the users are doing | 5 |
| `request_rate` | physical HTTP request starts are paced at a rate | 5 |

An iteration is one pass through a scenario's `steps`. One iteration can make many requests, so an iteration rate is not a request rate.

Each model is either fixed or ramping. A fixed load uses the fixed keys below. A ramping load uses `stages`, and can't be combined with the fixed keys. A stage moves from where the previous stage ended (or from `start`, for the first stage) to `to` over the time `over`. Two stages with the same `to` hold a level.

### `closed_vus`

- Fixed: `vus` (1 or more) and exactly one of `duration` or `iterations` (1 or more, counted per user).
- Ramping: `stages` (a non-empty list of `{ to: <integer, 0 or more>, over: <duration> }`) and an optional `start` (an integer, default `0`). It can't be combined with `vus`, `duration` or `iterations`.

```yaml
# 20 users for 5 minutes
load: { model: closed_vus, vus: 20, duration: 5m }
```

```yaml
# 5 users, each running 10 iterations and then stopping
load: { model: closed_vus, vus: 5, iterations: 10 }
```

```yaml
# ramping: up to 100 users, hold, then down
load:
  model: closed_vus
  start: 0
  stages:
    - { to: 100, over: 2m }
    - { to: 100, over: 10m }
    - { to: 0, over: 1m }
```

### `iteration_rate`

- `max_vus` (1 or more) is required. It is the most users that can run iterations at once.
- `queue` is optional, with `capacity` (1 or more, default `1000`) and `max_wait` (a duration, default `1s`). An iteration waits in the queue until a user is free. When the queue is full, the newest arrival is dropped. Anything that waits longer than `max_wait` expires and never runs late.
- Fixed: `rate` and `duration`.
- Ramping: `stages` (a non-empty list of `{ to: <rate>, over: <duration> }`) and an optional `start` (a rate, default `0/s`).

```yaml
# 20 iterations per second for 10 minutes
load: { model: iteration_rate, rate: 20/s, duration: 10m, max_vus: 200 }
```

```yaml
# ramping: up to 50 iterations per second, hold, then down
load:
  model: iteration_rate
  max_vus: 300
  start: 0/s
  stages:
    - { to: 50/s, over: 2m }
    - { to: 50/s, over: 10m }
    - { to: 0/s, over: 1m }
```

### `request_rate`

- `vus` (1 or more) is required. These are the users that generate the requests.
- `rate` counts physical HTTP request starts. How retries count against it is not specified yet. It is a target, and the run reports it against what was achieved. A flow whose steps depend on each other may not be able to reach the target.
- Fixed: `rate` and `duration`.
- Ramping: `stages` and an optional `start`, the same as for `iteration_rate`.

```yaml
# 200 requests per second for 5 minutes
load: { model: request_rate, vus: 50, rate: 200/s, duration: 5m }
```

```yaml
# ramping: up to 500 requests per second over 5 minutes
load:
  model: request_rate
  vus: 100
  stages:
    - { to: 500/s, over: 5m }
```

## Steps

A step list holds steps in order. A step is a map with exactly one key, which names its kind: `request`, `wait`, `if` or `repeat`.

`if` and `repeat` can nest, within the [nesting limit](#file-rules) of 128 levels of maps and lists. Each `if` or `repeat` adds three levels.

### `request`

| Key | Type | Required | Default |
|---|---|---|---|
| `name` | string, a fixed label for metrics. `${` in it is an error | no | none |
| `method` | an HTTP method: uppercase letters, digits and `-`, starting with a letter. Any method is allowed | yes | |
| `path` or `url` | template; exactly one. `path` is joined to `base_url`. `url` is used as written | yes | |
| `query`, `headers` | map of string to template. Order is kept | no | empty |
| `json`, `body` or `form` | a JSON value, a template, or a map of string to template; at most one | no | none |
| `timeout` | duration | no | the step's `http.timeout` |
| `required` | boolean | no | `true` |
| `retry` | [block](#retry) | no | no retries |
| `expect` | [block](#expect) | no | see below |
| `extract` | map of variable name to [source](#extract) | no | empty |

A failed required step ends the current iteration. A failed optional step (`required: false`) lets the iteration continue, and leaves its extractions unset.

#### `retry`

| Key | Type | Required | Default |
|---|---|---|---|
| `attempts` | integer, 2 or more. The total number of attempts, including the first | yes | |
| `backoff` | duration. A fixed delay between attempts | no | `100ms` |
| `allow_non_idempotent` | boolean. Must be `true` to retry any method other than `GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS` and `TRACE` | no | `false` |

Retries are off unless you add a `retry` block. They happen on transport errors and `5xx` responses. Every attempt counts as a physical request attempt. Any method is allowed, so a custom method is not retried unless `allow_non_idempotent` is `true`.

#### `expect`

| Key | Type | Notes |
|---|---|---|
| `status` | an integer, or a list of integers | If absent, any 2xx or 3xx status passes, so a 4xx or 5xx response fails the step |
| `body_contains` | template | The response body must contain this text |
| `json` | list of `{ path: <JSON path>, <comparison> }` | Each entry checks one value in a JSON response body |

#### `extract`

`extract` maps a variable name to where its value comes from. A source has exactly one of:

- `json_path: <JSON path>`: a value from a JSON response body
- `header: <name>`: a response header

If the value is missing, the step fails. Where the value is written depends on where the step runs (see [Variables](#variables)).

```yaml
extract:
  token:
    json_path: "$.access_token"
  etag:
    header: ETag
```

#### Comparisons

A comparison is exactly one of these keys. It is used in `expect.json` and in `when` conditions.

| Key | Value | Passes when |
|---|---|---|
| `exists` | boolean | the value is present (`true`) or absent (`false`) |
| `equals`, `not_equals` | a string, number, boolean or `null` | the value is equal, or not equal, to this |
| `gt`, `gte`, `lt`, `lte` | a number | the value is greater than, greater than or equal to, less than, or less than or equal to this |
| `contains` | a string | the value contains this text |

### `wait`

A duration to pause for.

```yaml
- wait: 500ms
```

### `if`

| Key | Type | Required |
|---|---|---|
| `when` | a condition | yes |
| `then` | non-empty step list | yes |
| `else` | step list | no |

A condition is exactly one of:

- `{ var: <reference>, <comparison> }`: compares a variable, for example `{ var: token, exists: true }`
- `{ all: [<condition>, ...] }`: every condition holds
- `{ any: [<condition>, ...] }`: at least one condition holds
- `{ not: <condition> }`: the condition does not hold

`when` is not an expression language. Conditions are built only from these four forms.

### `repeat`

| Key | Type | Required | Default |
|---|---|---|---|
| `count` | integer, 1 or more | yes | |
| `deadline` | duration. A time limit for the whole loop | no | `60s` |
| `steps` | non-empty step list | yes | |

## `thresholds`

A list of pass/fail checks on the results of a run.

| Key | Type | Notes |
|---|---|---|
| `metric` | one of the metrics below | Required |
| `stat` | `p50`, `p90`, `p95` or `p99` | Required for `http_request_duration`, not allowed for any other metric |
| `lt`, `lte`, `gt`, `gte` | duration, percentage or rate, by metric | Exactly one is required |
| `scenario` | a scenario name | Optional. Limits the check to that scenario |

| Metric | Bound is |
|---|---|
| `http_request_duration` | a duration |
| `error_rate` | a percentage |
| `request_rate` | a rate |
| `iteration_rate` | a rate |
| `dropped_work_ratio` | a percentage |

Metric names are provisional and may change before 1.0.

```yaml
thresholds:
  - metric: http_request_duration
    stat: p95
    lt: 500ms
  - metric: error_rate
    scenario: checkout
    lt: 1%
```

## File rules

A test file holds one YAML or JSON document. These rules apply however the file is read.

| Rule | Detail |
|---|---|
| Format | Chosen by the extension: `.yaml`, `.yml` or `.json`, in any letter case |
| Encoding | UTF-8. A leading byte-order mark is skipped |
| File size | At most 10 MiB |
| Nesting | At most 128 levels of maps and lists |
| Values | At most 250,000, counting copies made by YAML anchors and aliases |
| Text | At most 64 MiB of keys and strings, counting copies made by YAML anchors and aliases |

Most errors give a key path, a line and a column. Syntax errors give a line and a column, and errors about the whole file, such as its size, give neither. An error never quotes a value from the file, so a token in a test file can't reach a log through an error message. Key names can appear, escaped and cut to 64 characters, so don't put secrets in key names.

JSON files follow strict JSON: no comments and no trailing commas.

### YAML

An unquoted value is read like this. Quoted values, `|` blocks and `>` blocks are always strings.

| Unquoted value | Reads as |
|---|---|
| Nothing, `~`, `null`, `Null`, `NULL` | null |
| `true`, `True`, `TRUE`, `false`, `False`, `FALSE` | boolean |
| A JSON number: `2`, `-3`, `2.5`, `1e3` | number |
| `010`, `+5`, `.5`, `5.`, `1_000`, `0x10`, `0o7`, `0b1`, `.inf`, `.nan` | an error, because YAML versions read these differently. Quote it to make it a string, or write it as a plain decimal |
| Anything else, including `yes`, `no`, `on` and `off` | string |

- Keys are always text, so `1: x` has the key `"1"`, as it would in JSON.
- An unquoted key can't contain `:`. Quote it, or put a space after the colon if a value follows. This catches `{Authorization:Bearer …}`, which would otherwise make the value part of the key.
- An empty value is null, which isn't the same as leaving the key out. `http:` with nothing after it is an error.
- Tags such as `!!str`, more than one document in a file, and keys that are maps, lists or aliases aren't supported.

Anchors and aliases copy a whole value. Merge keys copy a map's entries into another map, where `<<` is written. A key written in the map itself always wins, and in a list of maps an earlier map beats a later one. A quoted `"<<"` is an ordinary key.

```yaml
steps:
  - request:
      method: GET
      path: /a
      headers: &auth
        Authorization: "Bearer ${token}"
        Accept: application/json
  - request:
      method: GET
      path: /b
      headers: *auth             # the same two headers
  - request:
      method: GET
      path: /c
      headers:
        <<: *auth                # Authorization from above
        Accept: text/plain       # replaces the merged Accept
```

An error inside aliased or merged content points to the line where it's defined. Errors found after reading, such as a wrong type, also give the path where it's used.

## Versioning

`schema_version` must be `1`, and it stays `1` through the MVP. Before 1.0 the schema may change without notice, as the [compatibility policy](../README.md#compatibility-policy) says. After 1.0, a breaking change bumps the version. A file with an unsupported version is rejected.

## Complete example

This test has one scenario per load model: `browse` ramps a closed set of users, `checkout` starts iterations at a fixed rate, and `health` ramps a request-start rate. Between them it uses every step kind, file-level and scenario hooks, `vu_setup` and `vu_teardown`, `http` and `limits` overrides, `retry`, `expect`, `extract` and thresholds. The YAML and the JSON describe exactly the same test.

```yaml
schema_version: 1
name: storefront-load
base_url: "${env.BASE_URL}"

http:
  version: http2
  timeout: 10s
  max_response_size: 2MiB
  tls:
    ca_file: certs/ca.pem

limits:
  max_request_rate: 500/s
  max_variables: 128
  hook_timeout: 30s

# File-level hooks run once per run. What they extract is available as global.NAME.
setup:
  - request:
      name: admin_login
      method: POST
      path: /admin/login
      json:
        user: admin
        password: "${env.ADMIN_PASSWORD}"
      extract:
        admin_token:
          json_path: "$.token"
  - wait: 1s

teardown:
  - request:
      name: admin_logout
      method: POST
      path: /admin/logout
      headers:
        Authorization: "Bearer ${global.admin_token}"

thresholds:
  - metric: http_request_duration
    stat: p95
    lt: 500ms
  - metric: error_rate
    lt: 1%
  - metric: request_rate
    scenario: health
    gte: 90/s
  - metric: iteration_rate
    scenario: checkout
    gte: 18/s
  - metric: dropped_work_ratio
    scenario: checkout
    lte: 0.5%

scenarios:
  # Closed model: a ramping number of users, each looping over the steps.
  - name: browse
    load:
      model: closed_vus
      start: 0
      stages:
        - to: 50
          over: 2m
        - to: 50
          over: 10m
        - to: 0
          over: 1m
    http:
      pool: scenario
      redirects: 0
    state: reset_each_iteration
    # Per-user hooks run once for each user.
    vu_setup:
      - request:
          name: sign_in
          method: POST
          path: /login
          json:
            username: "shopper-${vu.id}"
            password: "${env.SHOPPER_PASSWORD}"
          expect:
            status: 200
            json:
              - path: "$.token"
                exists: true
          extract:
            token:
              json_path: "$.token"
    vu_teardown:
      - request:
          name: sign_out
          method: POST
          path: /logout
          headers:
            Authorization: "Bearer ${token}"
          required: false
    steps:
      - request:
          name: list_products
          method: GET
          path: /products
          query:
            page: "${vu.iteration}"
            sort: price
          headers:
            Authorization: "Bearer ${token}"
            Accept: application/json
          timeout: 5s
          expect:
            status: [200, 206]
            body_contains: products
          extract:
            etag:
              header: ETag
      - wait: 500ms
      - repeat:
          count: 3
          deadline: 30s
          steps:
            - request:
                name: view_featured
                method: GET
                path: /products/featured
                headers:
                  If-None-Match: "${etag}"
                expect:
                  status: [200, 304]
            - wait: 250ms

  # Open model: iterations start at a fixed rate, whatever the users are doing.
  - name: checkout
    load:
      model: iteration_rate
      rate: 20/s
      duration: 10m
      max_vus: 200
      queue:
        capacity: 500
        max_wait: 2s
    limits:
      max_request_rate: 100/s
      max_variables: 64
      max_variable_size: 16KiB
      max_cookies: 32
    # Scenario hooks run once for this scenario. What setup extracts is available as scenario.NAME.
    setup:
      - request:
          name: load_config
          method: GET
          path: /config
          headers:
            Authorization: "Bearer ${global.admin_token}"
          extract:
            currency:
              json_path: "$.currency"
    teardown:
      - request:
          name: flush_carts
          method: POST
          path: /admin/flush
          headers:
            Authorization: "Bearer ${global.admin_token}"
    steps:
      - request:
          name: add_to_cart
          method: POST
          path: /cart
          json:
            sku: ABC-1
            quantity: 2
            gift: false
            note: null
            tags:
              - sale
              - "${scenario.currency}"
            options:
              express: true
              discount: 0.15
          retry:
            attempts: 3
            backoff: 250ms
            allow_non_idempotent: true
          expect:
            status: 201
            json:
              - path: "$.items[0].sku"
                equals: ABC-1
              - path: "$.items[0].quantity"
                equals: 2
              - path: "$.total"
                gt: 0
              - path: "$.currency"
                equals: "${scenario.currency}"
              - path: "$.message"
                contains: added
              - path: "$.error"
                exists: false
          extract:
            cart_id:
              json_path: "$.id"
      - request:
          name: apply_coupon
          method: POST
          path: "/cart/${cart_id}/coupon"
          form:
            code: SAVE10
            currency: "${scenario.currency}"
          required: false
          extract:
            discount:
              json_path: "$.discount"
      - if:
          when:
            all:
              - var: discount
                exists: true
              - any:
                  - var: discount
                    gte: 10
                  - var: scenario.currency
                    equals: USD
              - not:
                  var: vu.iteration
                  equals: 0
          then:
            - request:
                name: add_note
                method: PUT
                path: "/cart/${cart_id}/note"
                body: "Discount applied: ${discount}"
          else:
            - wait: 200ms

  # Request-start rate model: physical HTTP request starts are paced.
  - name: health
    load:
      model: request_rate
      vus: 10
      start: 0/s
      stages:
        - to: 100/s
          over: 1m
        - to: 100/s
          over: 5m
    http:
      timeout: 2s
      tls:
        insecure: true
    steps:
      - request:
          name: ping
          method: HEAD
          url: "${env.STATUS_URL}/ping"
          expect:
            status: 200
```

```json
{
  "schema_version": 1,
  "name": "storefront-load",
  "base_url": "${env.BASE_URL}",
  "http": {
    "version": "http2",
    "timeout": "10s",
    "max_response_size": "2MiB",
    "tls": { "ca_file": "certs/ca.pem" }
  },
  "limits": {
    "max_request_rate": "500/s",
    "max_variables": 128,
    "hook_timeout": "30s"
  },
  "setup": [
    {
      "request": {
        "name": "admin_login",
        "method": "POST",
        "path": "/admin/login",
        "json": { "user": "admin", "password": "${env.ADMIN_PASSWORD}" },
        "extract": { "admin_token": { "json_path": "$.token" } }
      }
    },
    { "wait": "1s" }
  ],
  "teardown": [
    {
      "request": {
        "name": "admin_logout",
        "method": "POST",
        "path": "/admin/logout",
        "headers": { "Authorization": "Bearer ${global.admin_token}" }
      }
    }
  ],
  "thresholds": [
    { "metric": "http_request_duration", "stat": "p95", "lt": "500ms" },
    { "metric": "error_rate", "lt": "1%" },
    { "metric": "request_rate", "scenario": "health", "gte": "90/s" },
    { "metric": "iteration_rate", "scenario": "checkout", "gte": "18/s" },
    { "metric": "dropped_work_ratio", "scenario": "checkout", "lte": "0.5%" }
  ],
  "scenarios": [
    {
      "name": "browse",
      "load": {
        "model": "closed_vus",
        "start": 0,
        "stages": [
          { "to": 50, "over": "2m" },
          { "to": 50, "over": "10m" },
          { "to": 0, "over": "1m" }
        ]
      },
      "http": { "pool": "scenario", "redirects": 0 },
      "state": "reset_each_iteration",
      "vu_setup": [
        {
          "request": {
            "name": "sign_in",
            "method": "POST",
            "path": "/login",
            "json": {
              "username": "shopper-${vu.id}",
              "password": "${env.SHOPPER_PASSWORD}"
            },
            "expect": {
              "status": 200,
              "json": [{ "path": "$.token", "exists": true }]
            },
            "extract": { "token": { "json_path": "$.token" } }
          }
        }
      ],
      "vu_teardown": [
        {
          "request": {
            "name": "sign_out",
            "method": "POST",
            "path": "/logout",
            "headers": { "Authorization": "Bearer ${token}" },
            "required": false
          }
        }
      ],
      "steps": [
        {
          "request": {
            "name": "list_products",
            "method": "GET",
            "path": "/products",
            "query": { "page": "${vu.iteration}", "sort": "price" },
            "headers": {
              "Authorization": "Bearer ${token}",
              "Accept": "application/json"
            },
            "timeout": "5s",
            "expect": { "status": [200, 206], "body_contains": "products" },
            "extract": { "etag": { "header": "ETag" } }
          }
        },
        { "wait": "500ms" },
        {
          "repeat": {
            "count": 3,
            "deadline": "30s",
            "steps": [
              {
                "request": {
                  "name": "view_featured",
                  "method": "GET",
                  "path": "/products/featured",
                  "headers": { "If-None-Match": "${etag}" },
                  "expect": { "status": [200, 304] }
                }
              },
              { "wait": "250ms" }
            ]
          }
        }
      ]
    },
    {
      "name": "checkout",
      "load": {
        "model": "iteration_rate",
        "rate": "20/s",
        "duration": "10m",
        "max_vus": 200,
        "queue": { "capacity": 500, "max_wait": "2s" }
      },
      "limits": {
        "max_request_rate": "100/s",
        "max_variables": 64,
        "max_variable_size": "16KiB",
        "max_cookies": 32
      },
      "setup": [
        {
          "request": {
            "name": "load_config",
            "method": "GET",
            "path": "/config",
            "headers": { "Authorization": "Bearer ${global.admin_token}" },
            "extract": { "currency": { "json_path": "$.currency" } }
          }
        }
      ],
      "teardown": [
        {
          "request": {
            "name": "flush_carts",
            "method": "POST",
            "path": "/admin/flush",
            "headers": { "Authorization": "Bearer ${global.admin_token}" }
          }
        }
      ],
      "steps": [
        {
          "request": {
            "name": "add_to_cart",
            "method": "POST",
            "path": "/cart",
            "json": {
              "sku": "ABC-1",
              "quantity": 2,
              "gift": false,
              "note": null,
              "tags": ["sale", "${scenario.currency}"],
              "options": { "express": true, "discount": 0.15 }
            },
            "retry": {
              "attempts": 3,
              "backoff": "250ms",
              "allow_non_idempotent": true
            },
            "expect": {
              "status": 201,
              "json": [
                { "path": "$.items[0].sku", "equals": "ABC-1" },
                { "path": "$.items[0].quantity", "equals": 2 },
                { "path": "$.total", "gt": 0 },
                { "path": "$.currency", "equals": "${scenario.currency}" },
                { "path": "$.message", "contains": "added" },
                { "path": "$.error", "exists": false }
              ]
            },
            "extract": { "cart_id": { "json_path": "$.id" } }
          }
        },
        {
          "request": {
            "name": "apply_coupon",
            "method": "POST",
            "path": "/cart/${cart_id}/coupon",
            "form": { "code": "SAVE10", "currency": "${scenario.currency}" },
            "required": false,
            "extract": { "discount": { "json_path": "$.discount" } }
          }
        },
        {
          "if": {
            "when": {
              "all": [
                { "var": "discount", "exists": true },
                {
                  "any": [
                    { "var": "discount", "gte": 10 },
                    { "var": "scenario.currency", "equals": "USD" }
                  ]
                },
                { "not": { "var": "vu.iteration", "equals": 0 } }
              ]
            },
            "then": [
              {
                "request": {
                  "name": "add_note",
                  "method": "PUT",
                  "path": "/cart/${cart_id}/note",
                  "body": "Discount applied: ${discount}"
                }
              }
            ],
            "else": [{ "wait": "200ms" }]
          }
        }
      ]
    },
    {
      "name": "health",
      "load": {
        "model": "request_rate",
        "vus": 10,
        "start": "0/s",
        "stages": [
          { "to": "100/s", "over": "1m" },
          { "to": "100/s", "over": "5m" }
        ]
      },
      "http": { "timeout": "2s", "tls": { "insecure": true } },
      "steps": [
        {
          "request": {
            "name": "ping",
            "method": "HEAD",
            "url": "${env.STATUS_URL}/ping",
            "expect": { "status": 200 }
          }
        }
      ]
    }
  ]
}
```
