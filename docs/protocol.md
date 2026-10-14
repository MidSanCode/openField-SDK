# The SDK protocol

Both transports speak the same method table, so this document describes one protocol with
two framings:

- **the sidecar** carries it as newline-delimited JSON over a child process's
  stdin/stdout;
- **the C ABI** carries the same JSON in `char *` arguments (`ofbot_call`).

A binding written against this document works on either. In the Python and Node packages,
`Bot.call(method, params)` is the sidecar framing and `Bot.request(...)` the escape hatch.

## Framing

### Sidecar (stdio)

One JSON object per line, in both directions. Everything on stdout is protocol;
diagnostics go to stderr, so a caller can pipe stdout straight into a parser.

```text
-> {"id": 1, "method": "posts.create", "params": {"content": "hello"}}
<- {"id": 1, "result": {"id": 42, "content": "hello"}}
<- {"id": 2, "error": {"kind": "forbidden", "status": 403, "retryable": false}}
```

`id` is echoed back verbatim and may be any JSON value. A request without an `id` is a
notification and is answered with nothing.

The first request should be `client.init`; every other method answers an error until then.
Closing stdin, sending `shutdown`, or terminating the process all stop it cleanly.

### C ABI

The same objects, as strings:

```c
char *answer = ofbot_call(bot, "posts.create", "{\"content\":\"hello\"}");
// {"ok":true,"result":{"id":42,"content":"hello"}}
ofbot_string_free(answer);
```

The difference is the envelope: the C ABI wraps every answer in `{"ok":…}` because a
`char *` cannot signal failure any other way. `{"ok":true,"result":…}` is the success form;
`{"ok":false,"error":{…}}` the failure form. The error object is byte-identical to the
sidecar's.

Two calls do not go through the dispatcher:

- `ofbot_client_request` runs the `request` method and returns its envelope directly
  (`{ok, status, body}`, no wrapping), so a caller can read a non-2xx status deliberately;
- `ofbot_events_subscribe` / `ofbot_events_next` / `ofbot_events_stats` /
  `ofbot_events_close` / `ofbot_events_free` are the event path, because events arrive
  asynchronously and need their own queue. Reaching `events.subscribe` through `ofbot_call`
  reports that the transport has no event sink — a deliberate error rather than a silently
  dropped subscription.

## The error object

Every failure is reported with the same fields:

| Field | Meaning |
| --- | --- |
| `kind` | the category, see below |
| `message` | the server's own error text, wrapped when there is none |
| `status` | the HTTP status, when the failure came from a response |
| `permission` | the missing permission key, on a `403` permission failure |
| `retryable` | whether repeating the call could plausibly succeed |
| `retry_after_secs` | the server's `Retry-After` hint, when it sent one |
| `api_error` | the raw `error` string from the server's envelope |
| `body` | the server's parsed error envelope |

`kind` is one of:

`transport` · `invalid_argument` · `network` · `bad_request` · `unauthorized` ·
`forbidden` · `not_found` · `conflict` · `rate_limited` · `server_error` · `decode` ·
`timeout` · `closed` · `other`

> The Rust error enum tags an HTTP failure as `http`. Both bindings normalise that to the
> status-derived category, which is what callers actually branch on, and keep the raw form
> working too.

## Client lifecycle

### `client.init`

Configure the client. Every other method needs it first.

```json
{
  "base_url": "https://openfield.example.com",
  "token": "ofb_…64 lowercase hex…",
  "timeout_ms": 30000,
  "connect_timeout_ms": 10000,
  "user_agent": "my-bot/1.0",
  "headers": { "X-Proxy": "a" },
  "max_response_bytes": 33554432,
  "retry": { "max_attempts": 3, "base_delay_ms": 500 },
  "reconnect": { "initial_delay_ms": 1000, "max_attempts": null }
}
```

| Field | Default | Notes |
| --- | --- | --- |
| `base_url` | `http://localhost:8080` | the instance root, without `/api/v1` |
| `token` | none | `ofb_` plus 64 lowercase hex; omit for the public endpoints |
| `timeout_ms` | 30000 | per-request timeout |
| `connect_timeout_ms` | 10000 | connection establishment |
| `user_agent` | `openfield-bot-sdk/0.1.0` | identify your bot (`bots.md` 15.2) |
| `headers` | none | extra headers on every request |
| `max_response_bytes` | 33554432 | refuse a larger response |
| `retry.max_attempts` | 3 | total attempts, not retries |
| `retry.base_delay_ms` | 500 | first backoff |
| `retry.max_delay_ms` | 30000 | backoff ceiling |
| `retry.multiplier` | 2.0 | growth per attempt |
| `retry.jitter` | 0.25 | ±25% so a fleet does not retry in lockstep |
| `reconnect.initial_delay_ms` | 1000 | event-stream reconnect |
| `reconnect.max_delay_ms` | 60000 | |
| `reconnect.max_attempts` | `null` | unlimited |

A malformed token is rejected here, before any request is made.

Answers `{"ok":true,"has_token":true}`.

### `client.set_token`

```json
{ "token": "ofb_…" }
```

Replaces the token; an empty string or `null` clears it and the client becomes anonymous. A
**rejected token leaves the previous one in place**, which matters because a bot may be
part-way through a `regenerate`.

### `client.config`

Answers `{"base_url":…,"token":"ofb_…","has_token":true,…}`. The token is always redacted;
the raw value never leaves the process.

### `client.has_token`

Answers `{"has_token":true}`.

## `request` — the escape hatch

Any server-relative path, so nothing in the API is unreachable.

```json
{
  "method": "POST",
  "path": "/api/v1/posts",
  "query": { "page": "1" },
  "body": { "content": "hi" }
}
```

| Field | Notes |
| --- | --- |
| `method` | required: `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS` |
| `path` | required, and must be **server-relative** |
| `query` | appended as a query string; values are stringified |
| `body` | a JSON body |
| `body_json` | the same, pre-serialised |
| `bytes` | a raw body: an array of octets or a base64 string |
| `empty_body` | `true` sends `{}` explicitly |
| `multipart` | a file upload, see below |

> `path` may not be an absolute URL. That is not a nicety: an absolute URL would receive the
> bot's `Authorization` header, so the SDK refuses one rather than let a caller leak a token
> to another host.

A multipart upload:

```json
{
  "method": "POST",
  "path": "/api/v1/attachments",
  "multipart": {
    "fields": { "description": "chart" },
    "files": [
      { "field": "file", "file_name": "chart.png", "path": "/tmp/chart.png" }
    ]
  }
}
```

Each file needs `path` (read from disk) or `bytes` (an octet array or base64). `field`
defaults to `file`, which is the field the server's handler reads — for **both** the
attachment and the avatar/banner endpoints, and for a group avatar.

Answers without raising on a non-2xx status:

```json
{ "ok": true,  "status": 201, "body": { "id": 1 } }
{ "ok": false, "status": 403, "body": { "error": "permission denied" },
  "retry_after_secs": null }
```

A failure *before* the request (a bad path, an unreadable upload file) answers the `error`
envelope instead, because no status exists.

## Names and ids

- Ids are integers. Where a field is named `user_id`, `post_id`, `conversation_id`,
  `camp_id`, `bot_id`, `message_id`, `reply_id` or `attachment_id`, it takes an integer;
  most also tolerate a numeric string, because the server sometimes marshals one.
- `page` and `limit` are integers, and only the endpoints documented as page-numbered use
  them.
- Message paging uses a **cursor**: `limit` and `before` (an exclusive message id).
- A single `query` object carries the free-form filters (`q`, `tag`, `sender_id`, …); see
  `posts.search`, `chat.search` and `camps.list`.

## Events

`events.subscribe` takes the filter the WebSocket endpoint supports:

```json
{
  "types": ["chat.message.created", "post.created"],
  "conversation_ids": [33],
  "queue_capacity": 1024,
  "reconnect": { "initial_delay_ms": 1000, "max_attempts": null }
}
```

Omitted `types` subscribes to everything. `queue_capacity` bounds the buffer: when a
consumer falls behind, the **oldest** events are dropped and the count appears in
`events.stats`, mirroring the server's own "disconnect a slow consumer" policy.

Answers `{"ok":true,"subscribed":true}`. The sidecar then pushes frames as they arrive:

```json
{ "event": "chat.message.created", "data": { … }, "recipients": [4242] }
```

`recipients` is omitted for a broadcast, so its absence — not an empty array — means
"everyone". Every subscriber receives a greeting first, with no payload:

```json
{ "event": "connected", "data": null }
```

(On the wire, from the server, that frame is `{"type":"connected"}`; the SDK normalises the
`type` key to `event` so a caller reads one shape for every frame.)

The 13 event types:

`chat.message.created` · `chat.message.updated` · `chat.message.deleted` · `chat.typing` ·
`chat.conversation.updated` · `chat.consent.requested` · `chat.e2ee.keys.updated` ·
`post.created` · `post.deleted` · `post.reply.created` · `post.reply.updated` ·
`post.reply.deleted`

`events.unsubscribe` answers `{"ok":true,"subscribed":false}`.

`events.stats` answers the counters:

```json
{
  "connection_attempts": 1, "connections_opened": 1, "disconnects": 0,
  "events_delivered": 12, "events_dropped": 0, "frames_invalid": 0,
  "events_queued": 0
}
```

> **Delivery is best-effort.** The server relays through Postgres `NOTIFY` and does not
> replay events across a disconnect, so anything with a durable effect — votes, tips, wallet
> changes — must be reconciled periodically rather than assumed from the stream
> (`bots.md` 9.3). `Bot.watch` implements that reconciliation for chat.

## Event transport internals

Worth knowing when debugging a connection:

- `POST /api/v1/ws` mints a **single-use ticket** with a 30-second TTL; a second use is
  rejected.
- `GET /api/v1/ws?ticket=…` upgrades to WebSocket and immediately sends `connected`.
- The server pings every 54 s and drops the connection at 60 s of silence.
- The send buffer is 256 messages; a consumer that cannot keep up is disconnected rather
  than buffered indefinitely — hence the SDK's own bounded queue.

## `capabilities`

Answers the SDK's own metadata, **not** the server's:

```json
{
  "protocol": 1,
  "features": { "ffi": true, "sidecar": true, "events": true, "multipart": true,
                "retry": true, "reconnect": true, "testkit": false },
  "methods": ["version", "ping", …]
}
```

The server's feature list is a different call: `GET /api/v1/capabilities`.

## `testkit.*` — test support

Compiled in only with the `testkit` cargo feature, so a production sidecar contains no mock
server at all. These exist so the Python and Node suites can drive the same Rust mock server
the core's own tests use, instead of a second mock that would drift.

| Method | Purpose |
| --- | --- |
| `testkit.start` | start the in-process mock; answers `{base_url, token, bot_id}` |
| `testkit.stop` | stop it |
| `testkit.force_status` | force `{path, status, retry_after_secs, times}` |
| `testkit.fail_next` | drop the next `{path, times}` requests |
| `testkit.broadcast` | push `{event}` to every connected WebSocket client |
| `testkit.requests` | every recorded request, with `body_len` |
| `testkit.clear_requests` | forget them |
| `testkit.ws_client_count` | attached WebSocket clients |
| `testkit.seed_message` | insert a message from another user, optionally broadcasting it |

## Protocol revision

`PROTOCOL_VERSION` is `1`. Ask for `version` and compare:

```json
{ "name": "openfield-bot-sdk", "version": "0.1.0", "protocol": 1 }
```

A change that would break an existing client bumps it.
