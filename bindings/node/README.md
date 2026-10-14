# openfield-bot (Node)

Node bindings for the OpenField community bot API. The Rust core does the work; this package
drives it through the **sidecar**, a child process speaking newline-delimited JSON.

Node is the one language of the three where the C ABI would mean a native addon
(`ffi-napi` and friends) — a build step and a Node-version coupling — so the subprocess
transport is the supported one. The method surface is identical to the Python bindings' and
to `libopenfield_bot_ffi`, because all three dispatch through the same Rust table.

Requires Node 18 or newer (`node:test` and ESM).

## Install

Not on npm; use the checkout:

```bash
git clone https://github.com/MidSanCode/openField-SDK && cd openField-SDK
cargo build --workspace
cd bindings/node && npm install   # no dependencies; sets up the package
```

The transport finds the binary automatically when this package sits inside the SDK checkout
(`target/debug` is picked up). Elsewhere, set `OPENFIELD_BOT_SIDECAR` or pass `sidecarPath`.

## Use

```js
import { Bot } from 'openfield-bot';

const bot = await new Bot(
  'https://openfield.example.com',
  'ofb_' + '1'.repeat(64),
).ready();

const check = await bot.startupCheck();
console.log(check.user.nickname, check.missing_administrative);

await bot.publish('Hello from the SDK', { visibility: 'public' });
await bot.sendMessage(33, 'pong', { replyToId: 5, burnSeconds: 300 });

for await (const event of bot.watch(33)) {
  if (event.data.content?.includes('ping')) {
    await bot.sendMessage(33, 'pong', { replyToId: event.data.id });
  }
}

bot.close();
```

Anything without a wrapper is reachable through the escape hatch:

```js
await bot.call('posts.create', { content: 'hello', visibility: 'public' });
await bot.request('GET', '/api/v1/posts', { query: { page: 1 } });
```

## Errors

Every failure is a `BotError` subclass with the same fields the Rust core reports:

```js
import { ForbiddenError, RateLimitedError, UnauthorizedError } from 'openfield-bot';

try {
  await bot.publish('hello');
} catch (error) {
  if (error instanceof ForbiddenError) console.error('missing:', error.permission);
  else if (error instanceof RateLimitedError) await sleep(error.retryAfterSecs ?? 1);
  else if (error instanceof UnauthorizedError) throw new Error('token revoked; stopping');
  else throw error;
}
```

`UnauthorizedError` means the token was reset, never existed, or the bot is banned — all three
answer 401, and the documented response is to alert and stop.

## Tests

The suite drives the **same Rust mock server** the core's own tests use, so the sidecar must
carry the `testkit` feature:

```bash
cargo build -p openfield-bot-sidecar --features testkit
cd bindings/node && node --test test/
```

## Notes

- `watch` yields **only** `chat.message.created` events for the conversation, from anyone
  except the bot itself. The `connected` greeting and other event types are filtered out, so
  a `maxEvents` budget cannot be spent on a connection notice. Use `events()` for the
  unfiltered stream.
- `messages()` returns messages **newest first**. The bundled `bots.md` §7.3 says otherwise;
  the server's SQL disagrees with the docs, and the SDK follows the SQL.
- Content limits are counted in **bytes**, while a bot nickname is counted in **runes**.
- **Money is spelled two ways.** `wallet()`, `rechargeWallet()` and `deductWallet()` speak
  **decimal coins** — `10` is ten coins, `12.5` is twelve and a half — because the server's
  `model.Cents` puts that on the wire. `tipPost()`, membership prices and task rewards are
  **whole coins**, so the facades refuse a fractional value there rather than send a `400`.
- A spending bot needs a payment PIN first: `setPaymentPin('123456')`. Until then every
  payment endpoint answers `403 payment pin not set`. Five wrong attempts lock payments for
  fifteen minutes.
- `uploadChunked()` resumes rather than restarts: pass the previous `uploadId` and only the
  parts the server is missing are re-sent.
- `downloadPlugin()` resolves to a `Buffer`; the transport carries it as base64 because the
  JSON envelope has no bytes.
- The event channel is best-effort and does not replay across a disconnect, so reconcile
  anything durable by polling.
