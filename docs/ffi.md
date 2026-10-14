# The C ABI

`openfield-bot-ffi` exposes the Rust core through a small, stable C surface, so any language
that can load a shared library can drive a bot without a Rust toolchain at build time. The
header is `ffi/include/openfield_bot.h`; keep it and `ffi/src/lib.rs` in step.

The sidecar exposes the same method table over a subprocess; see
[`protocol.md`](protocol.md). Choose the C ABI when you want no extra process, and the
sidecar when loading a native library is awkward.

## Building and linking

```bash
cargo build -p openfield-bot-ffi            # debug
cargo build -p openfield-bot-ffi --release  # optimised
```

```text
target/debug/libopenfield_bot_ffi.so        # Linux
target/debug/libopenfield_bot_ffi.dylib     # macOS
target/debug/openfield_bot_ffi.dll          # Windows
target/debug/libopenfield_bot_ffi.a         # static, for a C program
```

```bash
cc bot.c -Iffi/include -Ltarget/debug -lopenfield_bot_ffi -o bot
```

Because only `char *` and integers cross the boundary, the C ABI has no header-version
coupling beyond the function signatures: rebuilding the library does not require rebuilding
the caller, as long as a signature has not changed.

## The contract

- **Handles are opaque.** `OfbotClient *` and `OfbotEvents *` are created and freed by the
  library. Never inspect them field by field.
- **Strings cross as JSON.** Calls take and return UTF-8 JSON, so the ABI does not change
  when the API grows. The payloads are exactly the ones in [`protocol.md`](protocol.md).
- **Ownership is explicit.** Every `char *` you receive is yours and must be released with
  `ofbot_string_free`. The two `const char *` accessors return borrowed pointers that must
  **not** be freed.
- **Nothing unwinds across the boundary.** Every entry point catches a panic and turns it
  into an error result, because unwinding into C is undefined behaviour.
- **A null handle never crashes.** It produces a JSON error, and `ofbot_last_error` explains
  it.
- **Thread safety.** A client may be shared between threads; calls serialise on an internal
  lock. `ofbot_last_error` is thread-local, so read it on the thread that saw the failure.

## Minimal program

```c
#include <stdio.h>
#include "openfield_bot.h"

int main(void) {
    OfbotClient *bot = ofbot_client_new(
        "{\"base_url\":\"http://localhost:8080\",\"token\":\"ofb_…\","
        "\"user_agent\":\"my-bot/1.0\"}");
    if (!bot) {
        fprintf(stderr, "could not create the client: %s\n", ofbot_last_error());
        return 1;
    }

    char *me = ofbot_call(bot, "me", NULL);
    puts(me);
    ofbot_string_free(me);

    char *post = ofbot_call(bot, "posts.create",
                            "{\"content\":\"hello from C\",\"visibility\":\"public\"}");
    puts(post);
    ofbot_string_free(post);

    ofbot_client_free(bot);
    return 0;
}
```

## Entry points

### Library metadata

| Function | Notes |
| --- | --- |
| `const char *ofbot_version(void)` | the SDK version; **borrowed** |
| `const char *ofbot_protocol_version(void)` | the RPC protocol revision; **borrowed** |
| `const char *ofbot_last_error(void)` | this thread's last error message; **borrowed** |
| `void ofbot_string_free(char *)` | release an owned string; `NULL` is a no-op |

### Client

| Function | Notes |
| --- | --- |
| `OfbotClient *ofbot_client_new(const char *options_json)` | `NULL` on failure |
| `void ofbot_client_free(OfbotClient *)` | `NULL` is a no-op |
| `int ofbot_client_set_token(OfbotClient *, const char *)` | `1`/`0`; `NULL` clears |
| `char *ofbot_client_config(OfbotClient *)` | `{"ok":true,"config":{…}}`, token redacted |
| `int ofbot_client_has_token(OfbotClient *)` | `1`/`0` |

`ofbot_client_set_token` with a **malformed** token answers `0` and leaves the previous token
working, which matters because a regenerate invalidates the old token immediately and a bot
may need to keep serving until the swap completes.

### Method calls

| Function | Notes |
| --- | --- |
| `char *ofbot_call(OfbotClient *, const char *method, const char *params_json)` | `{"ok":true,"result":…}` or `{"ok":false,"error":{…}}` |
| `char *ofbot_client_request(OfbotClient *, const char *request_json)` | the raw `request` method; returns `{ok, status, body}` |

`params_json` may be `NULL` or `"null"` for a method that takes none. Ask for
`capabilities` to list every method.

A method whose result is **binary** still answers JSON: `plugins.download` returns
`{"size":…,"bytes":"<base64>"}`, because this transport carries strings and not buffers.
Decode `bytes` yourself; the sidecar behaves the same way.

The event methods are **not** usable through `ofbot_call`: this transport has no event sink,
so `events.subscribe` answers an error saying so rather than starting a subscription nobody
can read. Use `ofbot_events_*`.

### Events

| Function | Notes |
| --- | --- |
| `OfbotEvents *ofbot_events_subscribe(OfbotClient *, const char *filter_json)` | `NULL` on failure |
| `char *ofbot_events_next(OfbotEvents *, int64_t timeout_ms)` | always an owned string |
| `char *ofbot_events_stats(OfbotEvents *)` | `{"ok":true,"stats":{…}}` |
| `void ofbot_events_close(OfbotEvents *)` | ask the stream to stop |
| `void ofbot_events_free(OfbotEvents *)` | free it, stopping the reader |

`ofbot_events_next` always answers a tagged object, so a caller never has to guess whether
`NULL` meant a timeout or a failure:

```json
{ "kind": "event",   "event": { "event": "chat.message.created", "data": {…} } }
{ "kind": "timeout" }
{ "kind": "closed" }
{ "kind": "error",   "error": { "kind": "network" } }
```

A negative `timeout_ms` blocks until an event arrives or the stream stops.

> **Free the stream before the client.** `ofbot_client_free` does not reach into an
> outstanding `OfbotEvents`. Freeing the client first leaves the stream holding state whose
> owner is gone.

## A threaded event loop

```c
static void *listen(void *arg) {
    OfbotEvents *events = arg;
    for (;;) {
        char *frame = ofbot_events_next(events, 1000);   /* 1 s poll */
        if (!frame) break;                               /* allocation failure */

        /* The tagged object says what happened, so this needs no string matching. */
        if (strstr(frame, "\"kind\":\"timeout\"")) { ofbot_string_free(frame); continue; }
        if (strstr(frame, "\"kind\":\"closed\""))  { ofbot_string_free(frame); break; }

        handle_frame(frame);
        ofbot_string_free(frame);
    }
    return NULL;
}

OfbotEvents *events = ofbot_events_subscribe(bot,
    "{\"types\":[\"chat.message.created\"],\"conversation_ids\":[33]}");
if (!events) { fprintf(stderr, "%s\n", ofbot_last_error()); return 1; }

pthread_t thread;
pthread_create(&thread, NULL, listen, events);

/* … the rest of the bot … */

ofbot_events_close(events);
pthread_join(thread, NULL);
ofbot_events_free(events);
ofbot_client_free(bot);
```

> **Do not free the stream while a thread is inside `ofbot_events_next`.** `close` is safe to
> call concurrently — it takes a shared reference to the same stream, whose internals are
> atomics and a mutex — but `free` drops the memory the thread is reading. Close, join, then
> free.

## From Python, without the package

The bundled `openfield_bot.ffi` backend does this; it is reproduced here so the ABI is
usable on its own.

```python
import ctypes, json
from ctypes import c_char_p, c_int, c_int64, c_void_p

lib = ctypes.CDLL("target/debug/libopenfield_bot_ffi.so")

# Declare every signature. Without this, ctypes assumes `int`, which truncates pointers on
# a 64-bit host -- the classic way a binding corrupts memory instead of failing.
lib.ofbot_version.restype = c_char_p
lib.ofbot_version.argtypes = []
lib.ofbot_string_free.restype = None
lib.ofbot_string_free.argtypes = [c_void_p]
lib.ofbot_client_new.restype = c_void_p
lib.ofbot_client_new.argtypes = [c_char_p]
lib.ofbot_client_free.restype = None
lib.ofbot_client_free.argtypes = [c_void_p]
lib.ofbot_call.restype = c_void_p
lib.ofbot_call.argtypes = [c_void_p, c_char_p, c_char_p]

def take(pointer):
    """Copy an owned C string out and free it."""
    if not pointer:
        raise RuntimeError(lib.ofbot_last_error().decode())
    try:
        return ctypes.string_at(pointer).decode()
    finally:
        lib.ofbot_string_free(pointer)

handle = lib.ofbot_client_new(json.dumps({
    "base_url": "http://localhost:8080",
    "token": "ofb_…",
}).encode())

answer = json.loads(take(lib.ofbot_call(handle, b"me", None)))
print(answer["result"]["nickname"])

lib.ofbot_client_free(handle)
```

Two rules that matter for correctness rather than style:

1. **Declare `argtypes`/`restype`.** A truncated pointer is a crash, not a Python exception.
2. **Free every string.** Each call allocates; leaking one per request grows the process.
   The package's own test suite asserts that 2000 calls do not move `ru_maxrss`.

## Other languages

- **Go** — `cgo` with `//export`-free `import "C"`, or `purego` to avoid cgo entirely. The
  `char *` ownership rules above apply unchanged.
- **.NET** — `[DllImport("openfield_bot_ffi", CallingConvention = CallingConvention.Cdecl)]`
  with `IntPtr` for handles and `[MarshalAs(UnmanagedType.LPStr)] string` for the JSON, plus
  a manual `ofbot_string_free` on every return.
- **Java** — JNA with `Pointer` and `String`, mapping `ofbot_events_next`'s `int64_t` to
  `long`.

In all three the shape is the same: build the options JSON, call `ofbot_call`, free the
result.

## When something goes wrong

| Symptom | Likely cause |
| --- | --- |
| `ofbot_client_new` returns `NULL` | the options are not valid JSON, or the token is malformed; `ofbot_last_error` says which |
| every call answers `{"ok":false,"error":{"kind":"network"}}` | the `base_url` is wrong, or nothing is listening |
| `{"ok":false,"error":{"kind":"unauthorized","status":401}}` | the token was reset, is unknown, or the bot is banned — do not retry |
| `{"ok":false,"error":{"kind":"forbidden","status":403,"permission":"…"}}` | the bot's role lacks that permission |
| `ofbot_events_subscribe` returns `NULL` | an anonymous client cannot subscribe; the channel is per-account |
| a crash after `ofbot_events_free` | a thread was still inside `ofbot_events_next` — close, join, then free |
