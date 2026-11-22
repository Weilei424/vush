# Vush test file reference (schema v1)

> **Status:** this page defines the test file format. The `vush-config` library reads YAML and JSON test files into a plan and checks every rule on this page, and `vush validate` checks a file against every rule on this page without sending any traffic. It accepts every key described here, including keys that take no effect until a later phase. The "Runs" columns show which phase makes each area take effect. Phase numbers refer to the roadmap in [`planning/IMPLEMENTATION_PLAN.md`](planning/IMPLEMENTATION_PLAN.md). Until 1.0 the schema may change without notice (see [Versioning](#versioning)).

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
| Duration | an integer plus `ms`, `s`, `m` or `h`; parts can be combined in descending order | `500ms`, `5m`, `1h30m` | `5`, `1.5s`, `5 m`, `30m1h`, a value too large to represent, longer than `720h` (30 days) |
| Rate | an integer, `/`, then `s`, `m` or `h` | `20/s`, `300/m`, `0/s` | `20`, `0.5/s`, `20/d` |
| Size | an integer plus `B`, `KB`, `MB`, `GB`, `KiB`, `MiB` or `GiB` | `1MiB`, `500KB` | `1024`, `1.5MB`, a value too large to represent |
| Percentage | a number from 0 to 100 with up to 4 decimals, then `%` | `1%`, `0.5%`, `0%` | `0.01`, `101%`, `0.00001%` |

- `KB`, `MB` and `GB` are powers of 1000. `KiB`, `MiB` and `GiB` are powers of 1024.
- Rates are whole numbers. Write `30/m` instead of `0.5/s`.
- No duration can be longer than `720h` (30 days). Each key says whether it can be zero.
- Whether a zero rate is acceptable depends on where it is used. `0/s` is valid as a ramp `start` or a stage target, but a fixed `rate` and `max_request_rate` must be above zero.
- Numbers, booleans and `null` in `json` bodies and in comparisons (`equals`, `gt` and so on) are ordinary JSON values. Integers keep their exact value. An integer too large for a 64-bit unsigned value is an error. `NaN` and infinity are not allowed.

## Variables

A string can contain `${...}` references to variables. Vush splits each string into text and references when it reads the file, so nothing is interpreted while the test runs.

| Reference | Meaning | Value comes from |
|---|---|---|
| `${token}` | this user's variable | `extract` in `vu_setup`, `steps` or `vu_teardown` |
| `${scenario.x}` | read-only, one per scenario | `extract` in the scenario's `setup` or `teardown` |
| `${global.x}` | read-only, shared by the whole run | `extract` in the file-level `setup` or `teardown` |
| `${env.NAME}` | an environment variable, read once when the run starts | the environment |
| `${vu.id}`, `${vu.iteration}` | the user's number and the user's iteration count | Vush |

Rules:

- `$${` is a literal `${`. A `$` that is not followed by `{` is a literal `$`.
- `$${` is always the escape, so there is no way to write a literal `$` immediately before a reference. `Total: $${amount}` produces the literal text `Total: ${amount}`, not `Total: $` followed by the value of `amount`.
- Variable names match `[A-Za-z_][A-Za-z0-9_]*`.
- `env`, `global`, `scenario` and `vu` are reserved. They can't be variable names.
- These are errors: an unterminated `${`, an empty `${}`, whitespace inside the braces (`${ token }`), an unknown prefix (`${foo.bar}`) and an unknown built-in (`${vu.foo}`).
- Reading a variable that is missing or unset fails that step when the test runs. It is never replaced with an empty string.
- `extract` writes to the scope of the place it runs. An `extract` in the file-level `setup` or `teardown` writes `global.x`, one in a scenario's `setup` or `teardown` writes `scenario.x`, and one anywhere else writes the user's own variable. In every case the `extract` key is the bare name, for example `x`.
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

Each reference must be readable where it is used, and the table shows which can be read where. `${env.NAME}` can be read anywhere. In the table, "set in" means some `extract` in that place must set the name.

| Reading place | Own `x` | `${vu.id}`, `${vu.iteration}` | `${scenario.x}` | `${global.x}` |
|---|---|---|---|---|
| `base_url` | no | no | no | no |
| file-level `setup` | no | no | no | set in the file-level `setup` |
| file-level `teardown` | no | no | no | set in the file-level `setup` or `teardown` |
| a scenario's `setup` | no | no | set in its `setup` | set in the file-level `setup` |
| a scenario's `teardown` | no | no | set in its `setup` or `teardown` | set in the file-level `setup` |
| `vu_setup`, `steps`, `vu_teardown` | set in that scenario's `vu_setup`, `steps` or `vu_teardown` | yes | set in its `setup` | set in the file-level `setup` |

`base_url` can only read `${env.…}`, because it's used before any setup runs.

Validation checks every reference against this table, including the `var` field of a `when` condition. A name is also checked by where it is set: some `extract` that the table above makes visible here must set it. Where in a list the `extract` is, and whether its step is optional, are checked when the test runs. How a value is encoded when it is substituted into a URL or a JSON body is not specified yet, and this page will state it once it is.

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
| `base_url` | template. Must start with `http://` or `https://`, and can't hold a user name or password, a query or a fragment | no | none | 2 |
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
| `pool` | `shared`, `scenario` or `vu` | `shared` | Where connections are kept. `shared`: one pool for every part of the file with the same `version` and TLS trust (the system's roots, the same `tls.ca_file`, or `tls.insecure`); parts that differ never share a connection. `scenario`: one pool per scenario, used by its steps and hooks, and one for the file-level hooks. `vu`: one pool per user and per hook run, each with its own TLS session cache. Idle connections close after 90 s. Sharing a pool never shares cookies or credentials |
| `timeout` | duration | `30s` | For one physical attempt, including every redirect it follows and the time spent preparing each request. Longer than zero |
| `redirects` | integer, 0 to 20 | `5` | The most redirects one attempt follows. Only `301`, `302`, `303`, `307` and `308` with a `Location` are followed. When a chain needs more, the attempt fails with `too_many_redirects`. `0` means don't follow, and then the `3xx` is the response. A redirect to another origin drops `Authorization`, `Proxy-Authorization`, `Cookie` and `Host` |
| `cookies` | boolean | `true` | Whether each user keeps cookies. Cookies are always per user and per hook run. See [Cookies](#cookies) |
| `max_response_size` | size, at most 64MiB | `1MiB` | Upper bound on the response body kept. A longer body is still read to the end, so its connection is reused, but only its first `max_response_size` bytes are kept. The response is marked `truncated` and still counts as a response. If the hop's `timeout` runs out while the rest is read, the hop fails with `timeout` |
| `decompress` | boolean | `false` | Ask for and decode compressed bodies. See [Compression](#compression) |
| `tls.ca_file` | path | none | PEM certificates to trust as well as the system's roots. Relative to the test file's directory. Read once when the run starts. At most 1 MiB, with at least one certificate, and every certificate in it must be usable. Not read when `tls.insecure` is `true` |
| `tls.insecure` | boolean | `false` | Accepts any certificate for any name. The connection is still encrypted. Connections made this way are never shared with settings that verify |

The system's roots are the platform's trust store. When `SSL_CERT_FILE` or `SSL_CERT_DIR` is set, they replace it.

```yaml
http:
  version: http2
  timeout: 10s
  tls:
    ca_file: certs/ca.pem
```

### Cookies

Each user, and each hook run, has its own cookie jar. A jar is never shared, and a copy of a user's state gets its own copy of the jar. Pools and connections hold no cookies, so two users on one connection never see each other's. `cookies: false`, or `max_cookies: 0`, means no jar: `Set-Cookie` is ignored and nothing is added to requests.

What is stored:

- A `Set-Cookie` header is read when the response head arrives. That includes a `3xx` the attempt follows, and a response whose body then fails. The response keeps its `Set-Cookie` headers.
- A cookie is stored for the host in the request's URL. Without `Domain`, only that host gets it back. With `Domain`, the domain and every host under it do, so `Domain=vush.test` from `a.vush.test` also reaches `vush.test` and `b.vush.test`. `Domain` must match the host, a `Domain` with a trailing `.` or one that isn't ASCII makes the cookie ignored, and a one-label domain such as `com` is refused unless it is the host itself. There is no public-suffix list.
- For an IP host, `Domain` must equal the host, and the cookie is for that host only. For an IPv6 host, `Domain` must be written with brackets, as in `Domain=[::1]`. A domain cookie is never sent to an IP host.
- A cookie without `Path` gets the default path: the request path up to its last `/`, without that `/`, and `/` when the request path has only one `/`.
- `Secure` cookies are stored and sent only over `https`. A `Secure` cookie set over `http` is ignored. A cookie without `Secure` set over `http` can replace or delete a `Secure` one with the same name, domain and path.
- `Max-Age` beats `Expires`. A server deletes a cookie by sending the same name, `Domain` and `Path` with `Max-Age=0`, a negative `Max-Age`, or a past `Expires`.
- A cookie is ignored when its name is empty, its name or value holds a control character, or its name and value together are over 4096 bytes. A `Domain` or `Path` over 1024 bytes ignores the cookie too, and so does a stored path or domain over 1024 bytes, whether it comes from `Path` or `Domain` or from the request.
- Cookies don't separate by port, as in browsers. `HttpOnly`, `SameSite` and `Partitioned`, and the `__Secure-` and `__Host-` prefixes, are ignored.

What is sent:

- Before each hop, expired cookies are removed. The cookies that match the host, the path and the scheme are sent in one `Cookie` header: longest path first, then oldest first.
- A request's own `Cookie` header names win. A jar cookie with the same name as one the request sets is left out. Names are compared exactly, letter case included. The request's own values stay as written, first, and the jar's follow.
- Cookie values are bytes. A value that isn't UTF-8 is sent back as it came.

A redirect to another origin drops the request's own `Cookie` and `Authorization` headers. The jar then adds only what it holds for the new host. `Authorization` is never kept in the jar.

### Compression

With `decompress: true`, Vush asks for compressed bodies and decodes them.

- Each request gets `Accept-Encoding: gzip, br`, unless it sets its own `Accept-Encoding`. A request's own value is sent as written, and a `gzip` or `br` body sent back is still decoded.
- A body is decoded only when the response has exactly one `Content-Encoding` header, and its value is `gzip`, `x-gzip` or `br`, in any letter case. Anything else is kept raw, and isn't a failure: no header, `identity`, `deflate`, `zstd`, a list such as `gzip, br`, or several headers.
- The response's headers are left as received, `Content-Encoding` and `Content-Length` included.
- `max_response_size` counts decoded bytes. Decoding stops at the limit, and the rest of the body is read to the end but not decoded. The response is marked `truncated`. Vush keeps at most `max_response_size`, but the decoder can run past the limit before it stops: by up to about 64 KiB for `gzip`, and by up to the stream's window for `br`, which is at most 16 MiB because large-window brotli is refused. A `br` hop can also hold up to that window in decoder memory.
- Decoding runs in steps, and Vush's other work runs between them. So the hop's `timeout` covers decoding too: a hop whose time runs out while its body decodes fails as `timeout`. A step hands back at most 64 KiB of decoded output. A `br` step can decode up to the stream's window, at most 16 MiB, before it hands any back, because brotli hands out decoded output only when its window buffer fills.
- A decoded body exactly at the limit isn't `truncated`, and its gzip checksum or brotli end marker is still checked.
- A corrupt compressed body fails as `protocol`, and so does one that ends too early. A `br` body that uses brotli's large-window extension counts as corrupt. So do bytes after the end of the compressed stream.
- A gzip body of up to 1024 members decodes as one body. A body with more members is corrupt.
- These checks apply to what is decoded before the limit: the gzip checksum and length, the brotli end marker, a corrupt stream, bytes after the end and the member cap. Once the limit cuts a body, the rest is read to the end but neither decoded nor checked, so nothing after the cut can make the hop fail as corrupt.
- A response with no body, such as the answer to a `HEAD`, has nothing to decode.

With `decompress: false`, nothing is asked for and nothing is decoded. A request's own `Accept-Encoding` is still sent, and its body comes back as received.

## `limits`

`limits` is allowed at file level and on a scenario.

| Key | Type | Default | Notes | Runs |
|---|---|---|---|---|
| `max_request_rate` | rate | none | At file level, a budget across all scenarios. On a scenario, that scenario's own budget. Both can apply at once. A scenario does not inherit the file-level value as its own. Above zero | 5 |
| `max_variables` | integer | `256` | Per user. A scenario's value overrides the file's | 3 |
| `max_variable_size` | size | `64KiB` | Per variable value. A scenario's value overrides the file's | 3 |
| `max_cookies` | integer, 0 to 3000 | `64` | Per user and per hook run. A scenario's value overrides the file's. `0` keeps none. A full jar drops expired cookies first, then the one least recently sent or set | 2 |
| `hook_timeout` | duration | `60s` | For one whole hook run. Longer than zero. A scenario's value overrides the file's | 3 |

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

Each model is either fixed or ramping. A fixed load uses the fixed keys below. A ramping load uses `stages`, and can't be combined with the fixed keys. A stage moves from where the previous stage ended (or from `start`, for the first stage) to `to` over the time `over`. Two stages with the same `to` hold a level. A ramp must be above zero at `start` or at some stage, and its stages must last longer than zero in total. A single stage's `over` can be `0s`.

### `closed_vus`

- Fixed: `vus` (1 or more) and exactly one of `duration` (longer than zero) or `iterations` (1 or more, counted per user).
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
- `queue` is optional, with `capacity` (1 or more, default `1000`) and `max_wait` (a duration, default `1s`, can be `0s`). An iteration waits in the queue until a user is free. When the queue is full, the newest arrival is dropped. Anything that waits longer than `max_wait` expires and never runs late. With `max_wait: 0s`, an arrival is dropped when no user is free.
- Fixed: `rate` (above zero) and `duration` (longer than zero).
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
- Fixed: `rate` (above zero) and `duration` (longer than zero).
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
| `method` | an HTTP method: uppercase letters, digits and `-`, starting with a letter. Any method except `CONNECT` is allowed | yes | |
| `path` or `url` | template; exactly one. `path` is joined to `base_url` with one `/` between them, and needs a `base_url`. `url` is used as written, must start with `http://` or `https://`, and can't hold a user name or password | yes | |
| `query`, `headers` | map of string to template. `query` order is kept. For `headers`, see the rules below | no | empty |
| `json`, `body` or `form` | a JSON value, a template, or a map of string to template; at most one | no | none |
| `timeout` | duration, longer than zero | no | the step's `http.timeout` |
| `required` | boolean | no | `true` |
| `retry` | [block](#retry) | no | no retries |
| `expect` | [block](#expect) | no | see below |
| `extract` | map of variable name to [source](#extract) | no | empty |

Header names are HTTP tokens: letters, digits and any of `! # $ % & ' * + - . ^ _` the backtick, `|` and `~`. The client sets `content-length`, `transfer-encoding`, `connection`, `keep-alive`, `proxy-connection`, `upgrade` and `te` itself, so a request can't set them, in any letter case. `Host`, `Authorization` and `Cookie` can be set. A header value can't hold control characters except tab.

A failed required step ends the current iteration. A failed optional step (`required: false`) lets the iteration continue, and leaves its extractions unset.

#### `retry`

| Key | Type | Required | Default |
|---|---|---|---|
| `attempts` | integer, 2 to 10. The total number of attempts, including the first | yes | |
| `backoff` | duration, can be `0s`. A fixed delay between attempts | no | `100ms` |
| `allow_non_idempotent` | boolean. Must be `true` to retry any method other than `GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS` and `TRACE` | no | `false` |

Retries are off unless you add a `retry` block. An attempt is retried when it ends with a `5xx` response, or fails for any reason except `invalid_request`, `invalid_redirect` and `too_many_redirects`. Each attempt gets the full `timeout`, starts again from the original request, and follows its redirects again. `backoff` is waited between attempts, never after the last one. Every attempt counts as a physical request attempt. Any method other than `CONNECT` is allowed, so a custom method is not retried unless `allow_non_idempotent` is `true`. The request's own method decides, even when a redirect changed it.

#### `expect`

| Key | Type | Notes |
|---|---|---|
| `status` | an integer, or a list of integers | Each code is from 100 to 599, and a list can't be empty. If absent, any 2xx or 3xx status passes, so a 4xx or 5xx response fails the step |
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

A duration to pause for. It can be `0s`.

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
- `{ all: [<condition>, ...] }`: every condition holds. The list holds at least one condition
- `{ any: [<condition>, ...] }`: at least one condition holds. The list holds at least one condition
- `{ not: <condition> }`: the condition does not hold

`when` is not an expression language. Conditions are built only from these four forms.

### `repeat`

| Key | Type | Required | Default |
|---|---|---|---|
| `count` | integer, 1 or more | yes | |
| `deadline` | duration, longer than zero. A time limit for the whole loop | no | `60s` |
| `steps` | non-empty step list | yes | |

## How requests are sent

This section says what a request becomes on the wire. It holds for every step.

### URLs

- A URL is parsed the way browsers parse it. The scheme and host are lowercased, a space in the path becomes `%20`, and a non-ASCII host becomes punycode.
- The scheme must be `http` or `https`, the host can't be empty, and the URL can't hold a user name or password.
- A fragment (`#...`) is removed, because it is never sent.
- A `base_url` can't hold a query or a fragment.
- A `path` is joined to `base_url` by removing any trailing `/` from the base, any leading `/` from the path, and putting one `/` between them. An empty `path` gives the base itself, and a `path` can carry its own `?query`.

| `base_url` | `path` | URL sent |
|---|---|---|
| `https://api.test/v1` | `/users` | `https://api.test/v1/users` |
| `https://api.test/v1/` | `users` | `https://api.test/v1/users` |
| `https://api.test` | `/users?active=1` | `https://api.test/users?active=1` |
| `https://api.test/v1` | empty | `https://api.test/v1` |

### Query, body and headers

- `query` pairs are added in order as `name=value` joined with `&`, after a `?`, or after a `&` when the URL already has a query. Names and values are percent-encoded, so a space becomes `%20`.
- `json` is sent compact, with members in their given order. Integers are exact, and non-ASCII text is sent as UTF-8.
- `form` is sent as `application/x-www-form-urlencoded`, and a space becomes `+`.
- `body` is sent as its UTF-8 bytes.
- `Content-Type` is added when there is a body and the request has none: `application/json` for `json`, `application/x-www-form-urlencoded` for `form` and `text/plain; charset=utf-8` for `body`.
- `User-Agent: vush/<version>` is added when the request has none.
- No `Accept-Encoding` is sent, and no body is decoded, unless `http.decompress` is `true`. A request's own `Accept-Encoding` is always sent as written.
- Repeats of one header name are sent in their given order. The order across different names isn't kept.
- A request sets at most 256 header names of its own. Repeats of one name, in any letter case, count once. Headers Vush adds (`Content-Type`, `User-Agent`, `Accept-Encoding` and the cookie jar's `Cookie`) never count. With more, it fails as `invalid_request`, and nothing is sent. `vush validate` checks this too.
- A response fails as `protocol` when an HTTP/1.1 head has more than 100 headers or doesn't fit the HTTP library's read buffer, or when an HTTP/2 head is over 16 KiB. An HTTP/1.1 head of up to 408 KiB (417,792 bytes) always fits. A longer one can still pass when one read brings it in, and memory stays bounded by the read buffer either way.

### Redirects

On a followed redirect, the next request's method and body depend on the status:

| Status | Method | Body |
|---|---|---|
| `303` | `GET`, except that `HEAD` stays `HEAD` | dropped |
| `301`, `302` | `POST` becomes `GET`, any other method is kept | dropped for `POST`, kept otherwise |
| `307`, `308` | kept | kept |

When the body is dropped, the `Content-Type` header is dropped too. The `Location` is resolved against the URL that was sent, and it must follow the URL rules above. Two `Location` headers, one that can't be parsed, or one that fails those rules fails the attempt with `invalid_redirect`. The next request keeps the original headers, except those dropped for another origin (see `redirects` in [`http`](#http)).

### Values from variables

What a variable supplies is checked when the request is sent, with the same rules as a literal value. A request that breaks one fails as `invalid_request`, and nothing is sent.

### HTTP versions

`version: http1` always uses HTTP/1.1. `version: http2` needs the server's agreement over TLS: a server that refuses `h2`, or that picks no protocol, fails the request as `tls`. Over plaintext, `http2` speaks HTTP/2 without negotiation. `version: auto` uses HTTP/2 over TLS when the server picks it, and HTTP/1.1 otherwise.

## `thresholds`

A list of pass/fail checks on the results of a run.

| Key | Type | Notes |
|---|---|---|
| `metric` | one of the metrics below | Required |
| `stat` | `p50`, `p90`, `p95` or `p99` | Required for `http_request_duration`, not allowed for any other metric |
| `lt`, `lte`, `gt`, `gte` | duration, percentage or rate, by metric | Exactly one is required. A duration bound can be `0s` |
| `scenario` | a scenario name | Optional. Limits the check to that scenario. It must name a scenario in the file, matched exactly |

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

The request rules above (header names and values, the URL scheme, credentials in a URL, a query in `base_url`, and `CONNECT`) and the number of header names a request sets (at most 256, counted ignoring case; every name in a file is literal, so this check is complete) are checked here only where the text is literal. They are checked again when the request is sent, for values that come from variables.

Most errors give a key path, a line and a column. Syntax errors give a line and a column, and errors about the whole file, such as its size, give neither. Some rules are checked only after the file has no other errors, so fixing one round of errors can reveal more. An error never quotes a value from the file, so a token in a test file can't reach a log through an error message. Key names can appear, escaped and cut to 64 characters, so don't put secrets in key names.

`vush validate <FILE>...` checks one or more files. It exits `0` if every file is valid and `2` otherwise.

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

An error inside aliased or merged content points to the line where it's defined. A `<<` whose value isn't a map or a list of maps is reported at the `<<`, even when the value came through an alias. Errors found after reading, such as a wrong type, also give the path where it's used.

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
