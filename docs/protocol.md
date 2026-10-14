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
- **Money has two spellings and they are not interchangeable.** Wallet balances, wallet
  transactions and `wallet.adjust` use the server's `model.Cents`, which serialises as a
  **decimal coin** value: `10` is ten coins (1000 cents), `12.5` is twelve and a half. Tips,
  membership prices, task rewards and make-up prices are plain integers of **whole coins**.
  A binding that passes a decimal where the server binds `int64` gets a `400`, which is why
  the Python and Node facades refuse a fractional value for those fields locally.
- **Binary results travel as base64.** `plugins.download` is the only method whose response
  is not JSON: the transport answers

  ```json
  { "size": 4096, "bytes": "UEsDBAoAAAAA…" }
  ```

  and a binding decodes `bytes` back to a buffer. The Rust API returns `Vec<u8>` directly,
  and `ofbot_call`/the sidecar both carry the envelope above.

## The wrapped endpoint groups

Every method below has a typed wrapper in the Rust API and a facade in both bindings. The
server-side contract is the one in the handlers, not in `docs/reference/`; where the two
disagree, `core/src/docs.rs` records the conflict.

### Wallet

| Method | Params | Returns |
| --- | --- | --- |
| `wallet.get` | `page?`, `limit?` | `{balance, transactions, page, limit}` |
| `wallet.adjust` | `user_id`, `amount` (signed decimal coins), `type?`, `description?` | `{balance, transaction}` |
| `wallet.recharge` | `user_id`, `amount` (positive), `description?` | as `wallet.adjust` |
| `wallet.deduct` | `user_id`, `amount` (positive), `description?` | as `wallet.adjust` |

`wallet.adjust` stores `amount` **exactly as sent** and never reconciles it with `type`, so
a positive amount labelled `deduct` is a credit. `wallet.recharge` and `wallet.deduct` take
a positive amount and set the sign themselves. A deduction that would drive the balance
below zero answers **`400 insufficient balance`** — not the `409` `wallet.md` documents.

### Tasks and sign-in

| Method | Params | Returns |
| --- | --- | --- |
| `tasks.list` | — | `{tasks, streak, makeup_cost}` |
| `tasks.claim` | `code` | `{claimed, exp, currency}` |
| `tasks.claim_daily` | `code` | `{claimed, exp, currency}` |
| `tasks.login_claim` | — | `{granted, exp, currency, streak}` |
| `tasks.login_calendar` | — | `{registered_at, today, signed, streak, makeup_cost, makeup_exp, days}` |
| `tasks.makeup` | — | `{granted, exp, cost, streak}` |
| `tasks.makeup_date` | `date` (`YYYY-MM-DD`) | `{granted, date, exp, cost, streak}` |
| `tasks.exp_history` | `limit?` | `{entries}` |

The **route** is `daily-login` (hyphen) while the task **code** is `daily_login`
(underscore). Posting the code to the route matches the generic `/tasks/:code/claim`
pattern instead, which is why `tasks.login_claim` exists as its own method.

`404` is an unknown code; `409` is `task requirements not met` or `task already claimed`.
Daily tiers are cumulative and once per local day. A make-up charges `makeup_cost` coins
and grants experience but never the daily currency.

### Membership and the payment PIN

| Method | Params | Returns |
| --- | --- | --- |
| `membership.get` | — | `{level, name, active, expires_at, exp_multiplier, tiers, member_days, member_price, auto_renew}` |
| `membership.purchase` | `level` (1..4), `pin` | `{status, paid, kind}` |
| `membership.auto_renew` | `enabled`, `pin?` | membership status |
| `membership.purchases` | `page?`, `limit?` | `{purchases, page, limit}` |
| `membership.grant` | `user_id`, `level` (0 clears), `days?` | membership status |
| `membership.exp_adjust` | `user_id`, `delta`, `description?` | `{exp, level}` |
| `account.pin_set` | `pin`, `old_pin?` | `{set, has_pin}` |
| `account.pin_change` | `old_pin`, `pin` | `{set, has_pin}` |
| `account.pin_verify` | `pin` | `{valid}` |

Every outgoing payment is authorized by the six-digit PIN, and **a freshly created bot has
none**: the payment endpoints answer `403 payment pin not set` until `account.pin_set` runs.
Five wrong entries lock payments for fifteen minutes and answer
`429 尝试次数过多，请稍后再试`; the budget is shared by verification, tips, memberships and
transfers. `membership.exp_adjust` takes `delta` — **not** `amount` — and a positive delta is
multiplied by the target's membership multiplier before it is applied.

### Moderation

| Method | Params | Returns |
| --- | --- | --- |
| `moderation.punish` | `user_id`, `punishment` = `{type, reason?, permission_key?, duration_minutes?}` | `{punishment_id, type, expires_at}` |
| `moderation.punishments` | `user_id`, `limit?` | `{punishments, permission_bans}` |

`type` is one of `warning`, `demerit`, `revoke`, `temp_ban`, `ban`, `unban`, `restore`;
`revoke`/`restore` require `permission_key` and `temp_ban` requires a positive
`duration_minutes`. `permission_bans` is a list of permission **keys** currently withheld,
not a list of records. The server refuses self-moderation and enforces rank, so a bot
punishing itself or an administrator gets `403`.

### Tips

`posts.tip` takes `post_id`, `amount` (**whole coins**, 1..100000) and `pin`. The response
is a `post_tips` row whose `amount` and `net_amount` are **cents**: a 5-coin tip records
`amount = 500` and `net_amount = 475`, the 5 % platform fee having been taken. Tipping your
own post is a `400`.

### End-to-end encryption

| Method | Params | Returns |
| --- | --- | --- |
| `e2ee.set_key` | `public_key` (empty clears it) | `{message}` |
| `e2ee.keys` | `conversation_id` | `{version, envelopes, my_envelope, conversation_id}` |
| `e2ee.put_keys` | `conversation_id`, `envelopes` | `{message, version}` |

`envelopes` is an object keyed by the **member id as a JSON string**, because the handler
binds `map[int64]string`:

```json
{ "conversation_id": 33, "envelopes": { "7": "base64…", "9": "base64…" } }
```

The server never inspects the ciphertext, refuses an empty batch with
`400 no envelopes provided`, and answers `403` to a non-member.

### Storage: chunked uploads, reuse and burn

| Method | Params | Returns |
| --- | --- | --- |
| `storage.attachment` | `attachment_id` | the attachment |
| `storage.by_hash` | `hash` (64 hex) | the attachment, or `404` |
| `storage.burn_arm` | `attachment_id`, `burn_seconds` (1..86400) | `{id, burn_at}` |
| `storage.chunk_init` | `upload` | `{upload_id, total_chunks}` |
| `storage.chunk_upload` | `part` = `{upload_id, index, bytes}` | `{upload_id, index, size}` |
| `storage.chunk_status` | `upload_id` | `{upload_id, uploaded}` |
| `storage.chunk_complete` | `upload_id`, `upload` | the attachment |
| `storage.chunked` | `upload`, `chunk_bytes?` | the attachment |

`upload` is `{filename, size, total_chunks, mime_type?, visibility?}` and the same body is
bound by `init` and `complete`. `size` may not exceed `total_chunks * 8 MiB`, the part count
is capped at 10000, and each part is at most 8 MiB — all three are checked before the
request. `chunk_upload` sends a `multipart/form-data` part under the field `chunk`, with
1-based indexes. `chunk_complete` answers `409` with the missing indexes when parts are
absent, and it verifies the **stored** byte total rather than the declaration.

`storage.chunked` runs the whole flow: it opens a session, asks `chunk_status`, sends only
the parts the server is missing, then completes — so re-running it resumes rather than
restarts. `storage.burn_arm` is idempotent and only the **first view by someone other than
the uploader** arms the countdown; for the uploader the handler answers `404`.

### Plugin store

| Method | Params | Returns |
| --- | --- | --- |
| `plugins.list` | — | `{plugins}` |
| `plugins.get` | `plugin_id` | the plugin, or `404` |
| `plugins.download` | `plugin_id` | `{size, bytes}` (base64) |
| `plugins.upload` | `upload`, `publish?` | `{message, plugin}` |
| `plugins.publish` / `plugins.unpublish` | `plugin_id` | `{message}` |
| `plugins.delete` | `plugin_id` | `{message}` |

`plugins.list` and `plugins.get` are `authPublic`, so they work on a client with no token;
the download requires one and increments the download counter. Bundles are capped at 5 MiB
and must carry a `manifest.json` at the zip root. Ids must match
`^[a-z0-9][a-z0-9._-]{2,63}$`, and `.`/`..` are refused so an id can never act as a path.

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
