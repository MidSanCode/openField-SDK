# Documentation

| Document | What it is |
| --- | --- |
| [`../README.md`](../README.md) | what the SDK is, how to build it, and its shape |
| [`protocol.md`](protocol.md) | the method table and every payload, shared by both transports |
| [`ffi.md`](ffi.md) | the C ABI: linking, ownership, a threaded event loop, other languages |
| [`reference/`](reference/) | the upstream API documentation this SDK was built against, **verbatim** |

## About `reference/`

`reference/` holds the OpenField API documentation as it was supplied, unedited — `bots.md`,
`API.md`, `features.md`, `architecture.md` and the per-feature notes. It is kept because it is
what operators and bot authors read, so a disagreement between it and the SDK needs to be
checkable in one place.

It is **not** authoritative. Every model and endpoint in this SDK was verified against the
server source, and where the two disagreed the source won. The conflicts are recorded in code,
as `openfield_bot::docs::KNOWN_DIVERGENCES`, with each entry naming:

- the `topic`,
- what `bots.md` says,
- what `API.md` / `features.md` say,
- what the server source actually does,
- the `verdict` — which document the source agreed with,
- the `decision` — what this SDK does about it.

Read them from Rust:

```rust
use openfield_bot::docs::{divergence, resolved_for_neither, KNOWN_DIVERGENCES};

for conflict in KNOWN_DIVERGENCES {
    println!("{}\n  bots.md: {}\n  source:  {}\n  we do:   {}",
             conflict.topic, conflict.bots_md, conflict.source, conflict.decision);
}

// The cases where neither document was right, so nobody is misled by implication.
for conflict in resolved_for_neither() {
    println!("neither document: {}", conflict.topic);
}

if let Some(conflict) = divergence("message list ordering") {
    println!("{}", conflict.source);
}
```

The three with the widest blast radius:

1. **Message ordering.** `bots.md` §7.3 says conversation history is ascending, so the newest
   is `messages[-1]`. The handler sorts `ORDER BY m.created_at DESC, m.id DESC` — newest
   first — and the Go comment right above that SQL still says "(newest last)", so the server's
   own comment disagrees with its own query. `chat.messages` returns **newest first**.
2. **Content limits are bytes, nicknames are runes.** `len(req.Content) > 5000` counts bytes;
   `len([]rune(nickname)) > 100` counts code points. A Japanese post can be well under 5000
   characters and still be refused; the SDK mirrors both units rather than picking one.
3. **`/api/v1/health` is rate-limited.** `features.md` implies it is outside the limiter; it
   is registered after `r.Use`, so it is inside. A monitor polling it can be throttled.

`bots.md` is the primary reference for bot behaviour — permissions, quotas, the
never-reply-to-yourself rule, and the ban-avoidance guidance in its sections 15 and 16 are
all accurate and worth reading before writing a long-running bot.
