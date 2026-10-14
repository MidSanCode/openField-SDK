# OpenField Bot SDK

A cross-language SDK for the **OpenField** community bot API (`/api/v1/…`).

One Rust core does all the work. Two transports expose it to everything else:

| Transport | What it is | Who uses it |
| --- | --- | --- |
| `openfield-bot-sidecar` | a child process speaking newline-delimited JSON | any language with subprocesses: Python, Node, Ruby, PHP, Go, shell |
| `openfield-bot-ffi` | a C ABI (`cdylib` + `staticlib`) | C, C++, Python via `ctypes`, Go via `cgo`, .NET via P/Invoke, Java via JNA |

Both dispatch through the same method table (`core/src/rpc.rs`), so **the two transports
expose exactly the same surface** and neither can drift from the other. A binding written
once runs on either.

```
                         ┌──────────────────────────────┐
   Rust  ───────────────▶ │                              │
   Python (sidecar) ────▶ │   openfield-bot-sdk (core)   │
   Python (ctypes) ─────▶ │   blocking · rustls · no     │
   Node (sidecar) ──────▶ │   unsafe · forward-compat    │
   C / Go / .NET / JNA ─▶ │                              │
                         └───────────────┬──────────────┘
                                         │
                          ┌──────────────┴──────────────┐
                     sidecar (stdio)              ffi (C ABI)
                          │                             │
                     core/src/rpc.rs  ◀── one method table ──▶
```

## Layout

| Path | What it is |
| --- | --- |
| `core/` | the SDK: client, models, retry, events, and the shared method dispatcher |
| `ffi/` | the C ABI (`ffi/include/openfield_bot.h`) |
| `sidecar/` | the JSON-RPC process |
| `bindings/python/` | the Python package and its tests |
| `bindings/node/` | the Node package and its tests |
| `docs/` | protocol reference, FFI guide, and the API documentation this SDK was built against (`docs/reference/`) |

## Build

```bash
cargo build --workspace                     # core, ffi, sidecar
cargo build -p openfield-bot-sidecar --features testkit   # + the in-process mock server
```

The artifacts land in `target/debug/`:

```
target/debug/libopenfield_bot_ffi.so      # or .dylib / openfield_bot_ffi.dll
target/debug/openfield-bot-sidecar
```

`CARGO_TARGET_DIR` and `CARGO_HOME` are respected as usual. If the real `~/.cargo` is
read-only (a sandboxed or Nix-managed home), point `CARGO_HOME` at a writable directory.

## Quick start

### Rust

```rust
use openfield_bot::{AuthenticatedClient, Config, NewPost};

let client = AuthenticatedClient::new("ofb_…")?;      // or with_config for a custom base URL
let me = client.me()?;
println!("{}", me.nickname);

client.create_post(&NewPost::text("hello from the SDK"))?;

for event in client.subscribe(EventFilter::all())? {
    println!("{:?}", event);
}
```

`core/src/lib.rs` lists every re-export; each client module documents the endpoints it
covers, with the handler it was verified against.

### Python

```python
from openfield_bot import Bot

with Bot("https://openfield.example.com", "ofb_" + "1" * 64) as bot:
    check = bot.startup_check()
    print(check["user"]["nickname"], check["missing_administrative"])

    bot.publish("Hello from the SDK", visibility="public")

    for event in bot.watch(33):
        if "ping" in event.data["content"]:
            bot.send_message(33, "pong", reply_to_id=event.data["id"])
```

### Node

```js
import { Bot } from 'openfield-bot';

const bot = await new Bot('https://openfield.example.com', 'ofb_' + '1'.repeat(64)).ready();
console.log((await bot.startupCheck()).user.nickname);

for await (const event of bot.watch(33)) {
  if (event.data.content?.includes('ping')) {
    await bot.sendMessage(33, 'pong', { replyToId: event.data.id });
  }
}
```

The two bindings are deliberately the same shape: the same method names, the same error
categories, the same event objects. Only the spelling changes.

## The method surface

87 methods, all listed by `capabilities`:

```bash
echo '{"id":1,"method":"capabilities"}' | ./target/debug/openfield-bot-sidecar
```

| Group | Examples |
| --- | --- |
| Meta | `version`, `ping`, `capabilities` |
| Client | `client.init`, `client.set_token`, `client.config`, `client.has_token` |
| Escape hatch | `request` — any `/api/v1/…` path |
| Account | `me`, `permissions`, `startup_check`, `update_profile`, `user`, `follow`, `storage_usage`, `upload_avatar`, `health`, `announcements` |
| Bots | `bots.create`, `bots.list`, `bots.regenerate`, `bots.delete` |
| Posts | `posts.create`, `posts.list`, `posts.search`, `posts.get`, `posts.update`, `posts.delete`, `posts.pin`, `posts.react`, `posts.unreact`, `posts.favorite`, `posts.reply`, `posts.replies` |
| Chat | `chat.conversations`, `chat.create`, `chat.get`, `chat.messages`, `chat.latest`, `chat.search`, `chat.send`, `chat.edit`, `chat.delete`, `chat.read`, `chat.typing`, `chat.join`, `chat.invite`, `chat.leave`, `chat.start`, `chat.consent_requests`, `chat.files` |
| Camps | `camps.list`, `camps.get`, `camps.create`, `camps.update`, `camps.delete`, `camps.posts`, `camps.join`, `camps.leave`, `camps.members`, `camps.subgroups` |
| Storage | `storage.upload`, `storage.upload_many`, `storage.list`, `storage.delete` |
| Moderation | `reports.create`, `reports.mine` |
| Notifications | `notifications.list`, `notifications.read` |
| Events | `events.subscribe`, `events.unsubscribe`, `events.stats` |

`docs/protocol.md` documents every payload. Anything the table does not name is still
reachable through `request`, because the escape hatch takes any server-relative path.

## What the SDK handles for you

Four things that are easy to get wrong and expensive to debug:

- **Retries with the right shape.** `GET` and `HEAD` are retried on transport failures and
  `5xx`; `429` is retried on every verb honouring `Retry-After`; `401`/`403` are never
  retried. A mutating verb is **never** replayed on `5xx`, because replaying
  `POST /posts` would duplicate content.
- **Token redaction.** `BotToken` prints as `ofb_…`, including through `Debug`, `Display`
  and serde, so a token cannot leak into a log line or an error report.
- **A bounded event queue.** The server disconnects a slow consumer, so the SDK buffers
  with a drop-oldest cap (`1024`) instead of growing without bound, and reports the drops
  in `events.stats`.
- **Disconnect-safe chat.** The event channel is best-effort and does not replay across a
  reconnect, so `watch` pairs the subscription with periodic polling, de-duplicates ids,
  and skips the bot's own messages to stop self-reply loops.

## Where the documentation and the server disagree

The bundled API docs (`docs/reference/`) are useful but not authoritative. Every model and
endpoint here was verified against the server source, and where the two disagree **the
source won**; the conflicts are recorded in `core/src/docs.rs` as `KNOWN_DIVERGENCES` and
can be queried at runtime:

```rust
use openfield_bot::docs::{divergence, KNOWN_DIVERGENCES};

for conflict in KNOWN_DIVERGENCES {
    println!("{}: {}", conflict.topic, conflict.decision);
}
println!("{}", divergence("message list ordering").expect("recorded").source);
```

The one that bites hardest:

> **`bots.md` §7.3 says conversation history is oldest-first. It is not.** The handler sorts
> `ORDER BY m.created_at DESC, m.id DESC`, so `chat.messages` returns **newest first**. Its
> own Go comment says "(newest last)", contradicting the SQL right below it. The SDK follows
> the SQL.

Two more worth knowing:

- Content limits are counted in **bytes** (`len(req.Content) > 5000`), while a bot nickname
  is counted in **runes** (`len([]rune(nickname))`). The SDK mirrors both units, so a CJK
  post can be far fewer than 5000 characters and still be rejected.
- `/api/v1/health` **is** inside the rate limiter (it is registered after `r.Use`), contrary
  to `features.md`.

## Testing

```bash
# Rust: the core, the C ABI, and the mock server's own tests
cargo test --workspace --features openfield-bot-sidecar/testkit

# Python and Node suites (they drive the same Rust mock server)
cargo build -p openfield-bot-sidecar --features testkit
cd bindings/python && python -m unittest discover -t . -s tests
cd bindings/node   && node --test test/
```

The bindings are tested against the **same mock server** the Rust tests use — a
hand-written HTTP/1.1 + WebSocket implementation in `core/src/testkit/`. A second mock
written in Python or JavaScript would drift from the handler it imitates, and would then be
proving the wrong thing.

The mock's `testkit.*` methods are compiled out unless the `testkit` feature is on, so a
production sidecar contains no mock server at all.

## Status

Rust core, FFI, sidecar, Python and Node bindings are complete and green:

| Suite | Result |
| --- | --- |
| `cargo test --workspace` | 209 tests + 1 doc-test |
| Python (`unittest`) | 119 tests |
| Node (`node --test`) | 61 tests |

## Licence

MIT OR Apache-2.0.
