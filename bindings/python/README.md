# openfield-bot (Python)

Python bindings for the OpenField community bot API. The Rust core does the work; this
package drives it through one of two transports:

| Backend | What it needs | Notes |
| --- | --- | --- |
| `sidecar` (default) | the `openfield-bot-sidecar` binary | no compiled extension, no ABI to match |
| `ffi` | `libopenfield_bot_ffi` and `ctypes` | faster, no extra process |

Both expose the identical method surface, because both dispatch through the same Rust table.

## Install

Not on PyPI; use the checkout:

```bash
git clone https://github.com/MidSanCode/openField-SDK && cd openField-SDK
cargo build --workspace
pip install -e bindings/python
```

The transports find their artifacts automatically when the package sits inside the SDK
checkout (`target/debug` is picked up). Elsewhere, set `OPENFIELD_BOT_SIDECAR` /
`OPENFIELD_BOT_FFI`, or pass `sidecar_path` / `library_path` to `Bot`.

## Use

```python
from openfield_bot import Bot

with Bot("https://openfield.example.com", "ofb_" + "1" * 64) as bot:
    check = bot.startup_check()
    print(check["user"]["nickname"], check["missing_administrative"])

    bot.publish("Hello from the SDK", visibility="public")
    bot.send_message(33, "pong", reply_to_id=5, burn_seconds=300)

    for event in bot.watch(33):
        if "ping" in event.data["content"]:
            bot.send_message(33, "pong", reply_to_id=event.data["id"])
```

Switch backends with one argument:

```python
bot = Bot(url, token, backend="ffi")
```

Anything without a wrapper is reachable through the escape hatch:

```python
bot.call("posts.create", content="hello", visibility="public")
bot.request("GET", "/api/v1/posts", query={"page": 1})
```

## Errors

Every failure is a `BotError` subclass with the same fields the Rust core reports:

```python
from openfield_bot import Bot, ForbiddenError, RateLimitedError, UnauthorizedError

try:
    bot.publish("hello")
except ForbiddenError as error:
    print("missing permission:", error.permission)          # 'posts.create'
except RateLimitedError as error:
    time.sleep(error.retry_after_secs or 1)
except UnauthorizedError:
    raise SystemExit("the token was revoked; stopping")     # never retry a 401
```

`UnauthorizedError` means the token was reset, never existed, or the bot is banned — all
three answer 401, and the documented response is to alert and stop.

## Tests

The suite drives the **same Rust mock server** the core's own tests use, so the sidecar must
carry the `testkit` feature:

```bash
cargo build -p openfield-bot-sidecar --features testkit
cargo build -p openfield-bot-ffi
cd bindings/python && python -m unittest discover -t . -s tests
```

Without the feature, the mock-backed tests skip with a message saying how to rebuild; the
protocol, error-mapping and discovery tests still run.

## Notes

- `Bot.watch` yields **only** `chat.message.created` events for the conversation, from
  anyone except the bot itself. The `connected` greeting and other event types are filtered
  out, so a `max_events` budget cannot be spent on a connection notice. Use `Bot.events` for
  the unfiltered stream.
- `chat.messages` returns messages **newest first**. The bundled `bots.md` §7.3 says
  otherwise; the server's SQL disagrees with the docs, and the SDK follows the SQL.
- Content limits are counted in **bytes**, while a bot nickname is counted in **runes**.
- **Money is spelled two ways.** `wallet()`, `recharge_wallet()` and `deduct_wallet()` speak
  **decimal coins** — `10` is ten coins, `12.5` is twelve and a half — because the server's
  `model.Cents` puts that on the wire. `tip_post()`, membership prices and task rewards are
  **whole coins**, so the facades refuse a fractional value there rather than send a `400`.
- A spending bot needs a payment PIN first: `set_payment_pin("123456")`. Until then every
  payment endpoint answers `403 payment pin not set`. Five wrong attempts lock payments for
  fifteen minutes.
- `upload_chunked()` resumes rather than restarts: pass the previous `upload_id` and only the
  parts the server is missing are re-sent.
- The event channel is best-effort and does not replay across a disconnect, so reconcile
  anything durable by polling.
