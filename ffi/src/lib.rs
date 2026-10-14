//! C ABI for the OpenField community bot SDK.
//!
//! This crate is the `FFI` half of the cross-language story: it exposes the Rust core
//! through a small, stable C surface so any language that can load a shared library — C,
//! C++, Python via `ctypes`, Go via `cgo`, .NET via P/Invoke, Java via JNA — can drive a
//! bot without a Rust toolchain at build time.
//!
//! The `sidecar` half lives in `openfield-bot-sidecar`, a JSON-RPC process for languages
//! where loading a native library is inconvenient. Both transports dispatch through
//! [`openfield_bot::rpc`], so they expose exactly the same method surface and neither can
//! drift from the other.
//!
//! # Contract
//!
//! * **Handles are opaque.** [`OfbotClient`] and [`OfbotEvents`] are created and freed by
//!   this library and must never be inspected field-by-field.
//! * **Strings cross as JSON.** Rather than mapping every request field to a C argument,
//!   calls take and return UTF-8 JSON. That keeps the ABI from changing when the API
//!   grows, and it means the Python binding, the sidecar protocol and the Rust API all
//!   speak the same payloads.
//! * **Ownership is explicit.** Every `char *` returned here is owned by the caller and
//!   must be released with [`ofbot_string_free`]. The two `const char *` accessors
//!   ([`ofbot_version`], [`ofbot_last_error`]) return borrowed pointers that must not be
//!   freed.
//! * **No unwinding across the boundary.** Every entry point catches panics and turns them
//!   into an error result, because unwinding into C is undefined behaviour.
//! * **No null dereference.** A null handle produces a JSON error instead of a crash.
//!
//! # Two ways to call the API
//!
//! * [`ofbot_call`] runs any named method from the shared dispatcher — `posts.create`,
//!   `chat.send`, `camps.list`, and so on. This is the one to use for normal work; the
//!   parameter shapes are the same ones the sidecar documents, and `capabilities` lists
//!   every name.
//! * [`ofbot_client_request`] performs a raw HTTP request against any `/api/v1/...` path.
//!   It is the escape hatch for an endpoint this SDK version does not wrap.
//!
//! Both answer with the same envelope, so a caller can switch between them freely.
//!
//! # Payloads
//!
//! [`ofbot_client_new`] takes an options object:
//!
//! ```json
//! {
//!   "base_url": "https://openfield.example.com",
//!   "token": "ofb_…64 hex…",
//!   "timeout_ms": 30000,
//!   "user_agent": "my-bot/1.0",
//!   "headers": { "X-Proxy": "a" },
//!   "max_response_bytes": 33554432,
//!   "retry": { "max_attempts": 3, "base_delay_ms": 500 },
//!   "reconnect": { "initial_delay_ms": 1000, "max_attempts": null }
//! }
//! ```
//!
//! `token` is optional: without it the client is anonymous and only the public endpoints
//! work.
//!
//! [`ofbot_client_request`] takes a request object:
//!
//! ```json
//! { "method": "POST", "path": "/api/v1/posts",
//!   "query": { "page": "1" }, "body": { "content": "hi" } }
//!
//! // or a multipart upload
//! { "method": "POST", "path": "/api/v1/attachments",
//!   "multipart": { "fields": { "description": "chart" },
//!                  "files": [ { "field": "file", "file_name": "chart.png",
//!                               "path": "/tmp/chart.png" } ] } }
//! ```
//!
//! [`ofbot_events_next`] always answers with a tagged object, so a caller never has to
//! guess whether `NULL` meant a timeout or a failure:
//!
//! ```json
//! { "kind": "event",   "event": { "type": "chat.message.created", "data": { } } }
//! { "kind": "timeout" }
//! { "kind": "closed" }
//! { "kind": "error",   "error": { "kind": "network" } }
//! ```

#![warn(missing_docs)]
// This crate is the one place in the workspace where `unsafe` is permitted: it is the FFI
// boundary, and every block documents the invariant it relies on.
#![allow(unsafe_code)]

use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Mutex, MutexGuard};

use serde::Deserialize;
use serde_json::Value;

use openfield_bot::{
    AnyClient, BotToken, Config, EventFilter, EventStream, ReconnectPolicy, RetryPolicy, RpcState,
    SdkError, VERSION,
};

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

thread_local! {
    /// Message behind the most recent error **on this thread**.
    ///
    /// Thread-local so two bots on two threads cannot overwrite each other's diagnosis.
    static LAST_ERROR: std::cell::RefCell<CString> = std::cell::RefCell::new(
        CString::new("").expect("an empty string is a valid CString"),
    );
}

fn set_last_error(message: impl Into<String>) {
    let text = message.into().replace('\0', " ");
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new(text).unwrap_or_default();
    });
}

const PANIC_MESSAGE: &str =
    "a panic was caught at the FFI boundary; this is a bug in openfield-bot-ffi";

/// Run `body`, turning a panic into a sentinel value instead of unwinding into C.
fn guard<T>(fallback: T, body: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(value) => value,
        Err(_) => {
            set_last_error(PANIC_MESSAGE);
            fallback
        }
    }
}

/// Convert a C string into an owned `String`, rejecting null and invalid UTF-8.
///
/// # Safety
///
/// `pointer` must be null or point to a NUL-terminated C string valid for the call.
/// Ownership is not taken.
unsafe fn read_c_string(pointer: *const c_char, field: &str) -> Result<String, String> {
    if pointer.is_null() {
        return Err(format!("`{field}` must not be null"));
    }
    // SAFETY: the caller guarantees a NUL-terminated string for the call's duration.
    match unsafe { CStr::from_ptr(pointer) }.to_str() {
        Ok(text) => Ok(text.to_string()),
        Err(err) => Err(format!("`{field}` is not valid UTF-8: {err}")),
    }
}

/// Move a JSON value into a heap C string the caller owns.
fn to_c_string(value: &Value) -> *mut c_char {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    match CString::new(text) {
        Ok(string) => string.into_raw(),
        Err(_) => CString::new("null").expect("literal is valid").into_raw(),
    }
}

/// The `{"ok":true,<key>:<value>}` envelope.
fn json_ok(key: &str, value: Value) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("ok".into(), Value::Bool(true));
    object.insert(key.into(), value);
    Value::Object(object)
}

/// The `{"ok":false,"error":{…}}` envelope.
fn json_error(error: &SdkError) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("ok".into(), Value::Bool(false));
    object.insert("error".into(), openfield_bot::error_object(error));
    Value::Object(object)
}

// ---------------------------------------------------------------------------
// Library metadata
// ---------------------------------------------------------------------------

/// The SDK version, as a borrowed NUL-terminated string. Do **not** free it.
#[no_mangle]
pub extern "C" fn ofbot_version() -> *const c_char {
    static VERSION_C: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    VERSION_C
        .get_or_init(|| CString::new(VERSION).unwrap_or_default())
        .as_ptr()
}

/// The RPC protocol revision, as a borrowed NUL-terminated string. Do **not** free it.
#[no_mangle]
pub extern "C" fn ofbot_protocol_version() -> *const c_char {
    static PROTOCOL_C: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    PROTOCOL_C
        .get_or_init(|| {
            CString::new(openfield_bot::PROTOCOL_VERSION.to_string()).unwrap_or_default()
        })
        .as_ptr()
}

/// The message behind the most recent error **on this thread**, as a borrowed
/// NUL-terminated string. Do **not** free it.
///
/// The pointer stays valid until the next FFI call on the same thread.
#[no_mangle]
pub extern "C" fn ofbot_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ptr())
}

/// Release a string returned by this library.
///
/// # Safety
///
/// `string` must be null, or a pointer previously returned by this library that has not
/// been freed. Any other pointer, or a double free, is undefined behaviour.
#[no_mangle]
pub unsafe extern "C" fn ofbot_string_free(string: *mut c_char) {
    if string.is_null() {
        return;
    }
    // SAFETY: the caller guarantees the pointer came from `CString::into_raw` here and has
    // not been freed.
    drop(unsafe { CString::from_raw(string) });
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Options accepted by [`ofbot_client_new`].
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "snake_case")]
struct ClientOptions {
    base_url: String,
    token: Option<String>,
    timeout_ms: Option<u64>,
    connect_timeout_ms: Option<u64>,
    user_agent: Option<String>,
    headers: std::collections::BTreeMap<String, String>,
    max_response_bytes: Option<u64>,
    retry: Option<RetryPolicy>,
    reconnect: Option<ReconnectPolicy>,
}

/// An opaque bot client handle.
///
/// The auth-or-anonymous decision, the token swap and the method table all live in
/// [`RpcState`], which the sidecar shares, so the two transports cannot drift.
pub struct OfbotClient {
    rpc: Mutex<RpcState>,
}

impl OfbotClient {
    fn lock(&self) -> MutexGuard<'_, RpcState> {
        // A poisoned lock means another thread panicked mid-call. The inner state holds no
        // partially-updated invariant, so recovering beats refusing to work forever.
        self.rpc
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Create a client from a JSON options object.
///
/// Returns `NULL` on failure; call [`ofbot_last_error`] for the reason. A returned handle
/// must be released with [`ofbot_client_free`].
///
/// # Safety
///
/// `options_json` must be null or a NUL-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn ofbot_client_new(options_json: *const c_char) -> *mut OfbotClient {
    guard(std::ptr::null_mut(), || {
        let raw = match unsafe { read_c_string(options_json, "options_json") } {
            Ok(raw) => raw,
            Err(message) => {
                set_last_error(message);
                return std::ptr::null_mut();
            }
        };

        let options: ClientOptions = match serde_json::from_str(&raw) {
            Ok(parsed) => parsed,
            Err(err) => {
                set_last_error(format!("options_json is not a valid options object: {err}"));
                return std::ptr::null_mut();
            }
        };

        let base_url = if options.base_url.trim().is_empty() {
            openfield_bot::DEFAULT_BASE_URL.to_string()
        } else {
            options.base_url
        };

        let token = match options.token.as_deref() {
            Some(raw) if !raw.trim().is_empty() => match BotToken::new(raw) {
                Ok(token) => Some(token),
                Err(err) => {
                    set_last_error(err.to_string());
                    return std::ptr::null_mut();
                }
            },
            _ => None,
        };

        let mut config = Config::new(base_url, token);
        if let Some(timeout) = options.timeout_ms {
            config.timeout = std::time::Duration::from_millis(timeout.max(1));
        }
        if let Some(connect) = options.connect_timeout_ms {
            config.connect_timeout = std::time::Duration::from_millis(connect.max(1));
        }
        if let Some(user_agent) = options.user_agent {
            config.user_agent = user_agent;
        }
        if let Some(limit) = options.max_response_bytes {
            config.max_response_bytes = limit.max(1024);
        }
        if let Some(retry) = options.retry {
            config.retry = retry;
        }
        if let Some(reconnect) = options.reconnect {
            config.reconnect = reconnect;
        }
        for (name, value) in options.headers {
            config.headers.push((name, value));
        }

        let client = match AnyClient::new(config) {
            Ok(client) => client,
            Err(err) => {
                set_last_error(err.to_string());
                return std::ptr::null_mut();
            }
        };

        // No event sink: this transport uses `ofbot_events_*` for events, so the
        // dispatcher's `events.*` methods report that clearly instead of silently
        // dropping frames.
        let mut rpc = RpcState::new();
        rpc.set_client(client);

        Box::into_raw(Box::new(OfbotClient {
            rpc: Mutex::new(rpc),
        }))
    })
}

/// Free a client created by [`ofbot_client_new`].
///
/// Free any outstanding event stream first; this function does not reach into it. Freeing
/// `NULL` is a no-op.
///
/// # Safety
///
/// `client` must be null or a pointer returned by [`ofbot_client_new`] that has not
/// already been freed.
#[no_mangle]
pub unsafe extern "C" fn ofbot_client_free(client: *mut OfbotClient) {
    if client.is_null() {
        return;
    }
    guard((), || {
        // SAFETY: the caller guarantees the pointer came from `Box::into_raw` above and has
        // not been freed.
        drop(unsafe { Box::from_raw(client) });
    });
}

/// Replace the client's token, or clear it with `NULL`/an empty string.
///
/// Needed because `POST /bots/:id/regenerate` invalidates the old token instantly, so a
/// long-running bot has to swap it without restarting.
///
/// Returns `1` on success, `0` on failure (see [`ofbot_last_error`]). A rejected token
/// leaves the previous one in place.
///
/// # Safety
///
/// `client` must be null or a live handle; `token` must be null or a NUL-terminated UTF-8
/// C string.
#[no_mangle]
pub unsafe extern "C" fn ofbot_client_set_token(
    client: *mut OfbotClient,
    token: *const c_char,
) -> i32 {
    guard(0, || {
        if client.is_null() {
            set_last_error("`client` must not be null");
            return 0;
        }
        let raw = match unsafe { read_c_string(token, "token") } {
            Ok(raw) => raw,
            Err(message) => {
                set_last_error(message);
                return 0;
            }
        };

        // SAFETY: the caller guarantees a live handle.
        let client = unsafe { &*client };
        let mut state = client.lock();
        match state.dispatch("client.set_token", &serde_json::json!({ "token": raw })) {
            Ok(_) => 1,
            Err(err) => {
                set_last_error(err.to_string());
                0
            }
        }
    })
}

/// The client's configuration as JSON, with the token redacted.
///
/// Answers `{"ok":true,"config":{…}}`; useful for logging and for asserting on the
/// effective retry policy from a test.
///
/// # Safety
///
/// `client` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn ofbot_client_config(client: *mut OfbotClient) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        if client.is_null() {
            set_last_error("`client` must not be null");
            return std::ptr::null_mut();
        }
        // SAFETY: the caller guarantees a live handle.
        let client = unsafe { &*client };
        let mut state = client.lock();
        match state.dispatch("client.config", &Value::Null) {
            Ok(config) => to_c_string(&json_ok("config", config)),
            Err(err) => {
                set_last_error(err.to_string());
                to_c_string(&json_error(&err))
            }
        }
    })
}

/// Whether the client currently holds a token: `1` for yes, `0` for no.
///
/// # Safety
///
/// `client` must be null or a pointer returned by [`ofbot_client_new`] that has not been
/// freed.
#[no_mangle]
pub unsafe extern "C" fn ofbot_client_has_token(client: *mut OfbotClient) -> i32 {
    guard(0, || {
        if client.is_null() {
            return 0;
        }
        // SAFETY: the caller guarantees a live handle.
        let client = unsafe { &*client };
        let mut state = client.lock();
        match state.dispatch("client.has_token", &Value::Null) {
            Ok(value) => i32::from(value["has_token"].as_bool().unwrap_or(false)),
            Err(_) => 0,
        }
    })
}

// ---------------------------------------------------------------------------
// Method calls
// ---------------------------------------------------------------------------

/// Run any method from the shared dispatcher.
///
/// `params_json` is a JSON object, or `NULL`/`"null"` for a method that takes none:
///
/// ```c
/// char *answer = ofbot_call(bot, "posts.create",
///                           "{\"content\":\"hello\"}");
/// // {"ok":true,"result":{"id":42,"content":"hello"}}
///
/// char *answer = ofbot_call(bot, "chat.send",
///                           "{\"conversation_id\":33,\"content\":\"pong\"}");
/// ```
///
/// The answer is always an owned string:
///
/// `{"ok":true,"result":…}` or `{"ok":false,"error":{"kind":…,"status":…}}`.
///
/// Ask for `capabilities` to list every method name. The event methods
/// (`events.subscribe`, …) are **not** usable here: this transport has no event sink, so
/// they report that and the caller should use [`ofbot_events_subscribe`] instead.
///
/// # Safety
///
/// `client` must be null or a live handle; `method` and `params_json` must be null or
/// NUL-terminated UTF-8 C strings.
#[no_mangle]
pub unsafe extern "C" fn ofbot_call(
    client: *mut OfbotClient,
    method: *const c_char,
    params_json: *const c_char,
) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        let invalid = |message: String| -> *mut c_char {
            set_last_error(&message);
            to_c_string(&json_error(&SdkError::InvalidArgument { message }))
        };

        if client.is_null() {
            return invalid("`client` must not be null".into());
        }
        let method = match unsafe { read_c_string(method, "method") } {
            Ok(method) => method,
            Err(message) => return invalid(message),
        };

        let params = if params_json.is_null() {
            Value::Null
        } else {
            let raw = match unsafe { read_c_string(params_json, "params_json") } {
                Ok(raw) => raw,
                Err(message) => return invalid(message),
            };
            if raw.trim().is_empty() {
                Value::Null
            } else {
                match serde_json::from_str(&raw) {
                    Ok(value) => value,
                    Err(err) => return invalid(format!("params_json is not valid JSON: {err}")),
                }
            }
        };

        // SAFETY: the caller guarantees a live handle.
        let client = unsafe { &*client };
        let mut state = client.lock();
        match state.dispatch(&method, &params) {
            Ok(result) => to_c_string(&json_ok("result", result)),
            Err(err) => {
                set_last_error(err.to_string());
                to_c_string(&json_error(&err))
            }
        }
    })
}

/// Perform a raw HTTP request against any `/api/v1/...` path.
///
/// The escape hatch for an endpoint this SDK version does not wrap. See the crate docs for
/// the request object; the answer is
/// `{"ok":true,"status":200,"body":…}` or `{"ok":false,"error":{…}}`.
///
/// Unlike the typed helpers this never raises on a non-2xx status: the status and the
/// server's error body are part of the envelope, because a bot needs to tell a `409` from a
/// `429` without string matching.
///
/// # Safety
///
/// `client` must be null or a live handle; `request_json` must be null or a
/// NUL-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn ofbot_client_request(
    client: *mut OfbotClient,
    request_json: *const c_char,
) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        let invalid = |message: String| -> *mut c_char {
            set_last_error(&message);
            to_c_string(&json_error(&SdkError::InvalidArgument { message }))
        };

        if client.is_null() {
            return invalid("`client` must not be null".into());
        }
        let raw = match unsafe { read_c_string(request_json, "request_json") } {
            Ok(raw) => raw,
            Err(message) => return invalid(message),
        };
        let params: Value = match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(err) => {
                return invalid(format!("request_json is not a valid request object: {err}"))
            }
        };

        // SAFETY: the caller guarantees a live handle.
        let client = unsafe { &*client };
        let mut state = client.lock();
        match state.dispatch("request", &params) {
            Ok(envelope) => to_c_string(&envelope),
            Err(err) => {
                set_last_error(err.to_string());
                to_c_string(&json_error(&err))
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// The filter accepted by [`ofbot_events_subscribe`].
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "snake_case")]
struct SubscribeOptions {
    /// Event types to keep; empty means every type.
    types: Vec<String>,
    /// Conversation ids to keep.
    conversation_ids: Vec<openfield_bot::Id>,
    /// Queue capacity before the oldest events are discarded.
    queue_capacity: Option<usize>,
    reconnect: Option<ReconnectPolicy>,
}

/// An opaque event-stream handle.
pub struct OfbotEvents {
    stream: EventStream,
}

/// Subscribe to the realtime channel.
///
/// Returns `NULL` on failure (see [`ofbot_last_error`]). The handle must be released with
/// [`ofbot_events_free`], which also stops the reader.
///
/// A dedicated entry point rather than `ofbot_call("events.subscribe")`, because events
/// arrive asynchronously and need their own queue. `filter_json` may be `NULL` or `{}`:
///
/// ```json
/// { "types": ["chat.message.created"], "conversation_ids": [33], "queue_capacity": 1024 }
/// ```
///
/// # Safety
///
/// `client` must be null or a live handle; `filter_json` must be null or a NUL-terminated
/// UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn ofbot_events_subscribe(
    client: *mut OfbotClient,
    filter_json: *const c_char,
) -> *mut OfbotEvents {
    guard(std::ptr::null_mut(), || {
        if client.is_null() {
            set_last_error("`client` must not be null");
            return std::ptr::null_mut();
        }

        let options = if filter_json.is_null() {
            SubscribeOptions::default()
        } else {
            let raw = match unsafe { read_c_string(filter_json, "filter_json") } {
                Ok(raw) => raw,
                Err(message) => {
                    set_last_error(message);
                    return std::ptr::null_mut();
                }
            };
            if raw.trim().is_empty() {
                SubscribeOptions::default()
            } else {
                match serde_json::from_str(&raw) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        set_last_error(format!("filter_json is not a valid filter object: {err}"));
                        return std::ptr::null_mut();
                    }
                }
            }
        };

        let mut filter = if options.types.is_empty() {
            EventFilter::all()
        } else {
            EventFilter::types(
                options
                    .types
                    .iter()
                    .map(|raw| openfield_bot::EventType::parse(raw)),
            )
        };
        for conversation_id in options.conversation_ids {
            filter = filter.conversation(conversation_id);
        }

        // SAFETY: the caller guarantees a live handle.
        let client = unsafe { &*client };
        let mut state = client.lock();

        let capacity = options.queue_capacity.unwrap_or(1024);
        let stream = match state.client_mut() {
            Ok(any) => match any.authenticated() {
                Ok(auth) => match options.reconnect {
                    Some(reconnect) => {
                        EventStream::subscribe_full(auth, filter, reconnect, capacity)
                    }
                    None => EventStream::subscribe(auth, filter),
                },
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        };

        match stream {
            Ok(stream) => Box::into_raw(Box::new(OfbotEvents { stream })),
            Err(err) => {
                set_last_error(err.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

/// Wait for the next event and return a tagged JSON object the caller owns.
///
/// Always returns a string (never `NULL`, unless allocation fails):
///
/// * `{"kind":"event","event":{…}}` — an event arrived;
/// * `{"kind":"timeout"}` — nothing arrived within `timeout_ms`;
/// * `{"kind":"closed"}` — the stream stopped;
/// * `{"kind":"error","error":{…}}` — the stream handle was null.
///
/// A negative `timeout_ms` blocks until an event arrives or the stream stops.
///
/// # Safety
///
/// `events` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn ofbot_events_next(
    events: *mut OfbotEvents,
    timeout_ms: i64,
) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        if events.is_null() {
            set_last_error("`events` must not be null");
            return to_c_string(&serde_json::json!({
                "kind": "error",
                "error": {
                    "kind": "invalid_argument",
                    "message": "`events` must not be null",
                    "retryable": false
                }
            }));
        }
        // SAFETY: the caller guarantees a live handle.
        let events = unsafe { &*events };

        let received = if timeout_ms < 0 {
            events.stream.recv()
        } else {
            events
                .stream
                .recv_timeout(std::time::Duration::from_millis(timeout_ms as u64))
        };

        let mut object = serde_json::Map::new();
        match received {
            Some(event) => {
                object.insert("kind".into(), Value::String("event".into()));
                object.insert("event".into(), openfield_bot::event_message(&event));
            }
            None if events.stream.is_closed() => {
                object.insert("kind".into(), Value::String("closed".into()));
            }
            None => {
                object.insert("kind".into(), Value::String("timeout".into()));
            }
        }
        to_c_string(&Value::Object(object))
    })
}

/// Ask the stream to stop without freeing it.
///
/// # Safety
///
/// `events` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn ofbot_events_close(events: *mut OfbotEvents) {
    guard((), || {
        if events.is_null() {
            return;
        }
        // SAFETY: the caller guarantees a live handle.
        let events = unsafe { &*events };
        events.stream.close();
    });
}

/// The stream counters as JSON: `{"ok":true,"stats":{…}}`.
///
/// # Safety
///
/// `events` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn ofbot_events_stats(events: *mut OfbotEvents) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        if events.is_null() {
            set_last_error("`events` must not be null");
            return std::ptr::null_mut();
        }
        // SAFETY: the caller guarantees a live handle.
        let events = unsafe { &*events };
        let stats = events.stream.stats();
        to_c_string(&json_ok(
            "stats",
            serde_json::to_value(stats).unwrap_or(Value::Null),
        ))
    })
}

/// Free an event stream, stopping its reader thread.
///
/// Freeing `NULL` is a no-op.
///
/// # Safety
///
/// `events` must be null or a pointer returned by [`ofbot_events_subscribe`] that has not
/// already been freed.
#[no_mangle]
pub unsafe extern "C" fn ofbot_events_free(events: *mut OfbotEvents) {
    if events.is_null() {
        return;
    }
    guard((), || {
        // SAFETY: the caller guarantees the pointer came from `Box::into_raw` above and has
        // not been freed.
        let boxed = unsafe { Box::from_raw(events) };
        boxed.stream.close();
        drop(boxed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Call a JSON→JSON entry point with a Rust string and decode the answer.
    fn json_call(input: &str, call: impl FnOnce(*const c_char) -> *mut c_char) -> Value {
        let raw = CString::new(input).unwrap();
        let out = call(raw.as_ptr());
        assert!(!out.is_null(), "the call returned NULL");
        // SAFETY: the pointer came from this library and is freed exactly once, here.
        let text = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
        unsafe { ofbot_string_free(out) };
        serde_json::from_str(&text).expect("the answer is JSON")
    }

    fn new_client(options: &str) -> *mut OfbotClient {
        let raw = CString::new(options).unwrap();
        unsafe { ofbot_client_new(raw.as_ptr()) }
    }

    /// `ofbot_client_has_token`, wrapped so the tests do not need an `unsafe` block each.
    fn has_token(client: *mut OfbotClient) -> i32 {
        // SAFETY: every caller passes a live handle from `new_client`.
        unsafe { ofbot_client_has_token(client) }
    }

    fn last_error() -> String {
        // SAFETY: the accessor returns a readable thread-local pointer.
        unsafe { CStr::from_ptr(ofbot_last_error()) }
            .to_str()
            .unwrap()
            .to_string()
    }

    /// Call `ofbot_call` with owned Rust strings.
    fn call(client: *mut OfbotClient, method: &str, params: &str) -> Value {
        let method_c = CString::new(method).unwrap();
        let params_c = CString::new(params).unwrap();
        let out = unsafe { ofbot_call(client, method_c.as_ptr(), params_c.as_ptr()) };
        assert!(!out.is_null());
        // SAFETY: as above.
        let text = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
        unsafe { ofbot_string_free(out) };
        serde_json::from_str(&text).expect("the answer is JSON")
    }

    const NO_SERVER: &str = "http://127.0.0.1:9";

    #[test]
    fn version_and_protocol_are_reported_and_cached() {
        let first = ofbot_version();
        assert_eq!(first, ofbot_version(), "the version pointer must be cached");
        // SAFETY: a `'static` literal owned by the library.
        assert_eq!(unsafe { CStr::from_ptr(first) }.to_str().unwrap(), VERSION);

        let protocol = ofbot_protocol_version();
        // SAFETY: as above.
        assert_eq!(
            unsafe { CStr::from_ptr(protocol) }.to_str().unwrap(),
            openfield_bot::PROTOCOL_VERSION.to_string()
        );
    }

    #[test]
    fn a_null_client_is_reported_not_crashed() {
        let answer = json_call(
            r#"{"method":"GET","path":"/api/v1/users/me"}"#,
            |pointer| unsafe { ofbot_client_request(std::ptr::null_mut(), pointer) },
        );
        assert_eq!(answer["ok"], false);
        assert_eq!(answer["error"]["kind"], "invalid_argument");
        assert!(last_error().contains("`client` must not be null"));
    }

    #[test]
    fn a_null_request_is_reported_not_crashed() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        let answer = json_call("", |_| unsafe {
            ofbot_client_request(client, std::ptr::null())
        });
        assert_eq!(answer["ok"], false);
        assert!(answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("`request_json` must not be null"));
        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn a_bad_options_object_is_rejected() {
        let client = new_client("{not json");
        assert!(client.is_null());
        assert!(
            last_error().contains("not a valid options object"),
            "got: {}",
            last_error()
        );
    }

    #[test]
    fn a_bad_token_is_rejected_before_network_activity() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}","token":"nope"}}"#));
        assert!(client.is_null());
        assert!(last_error().contains("ofb_"), "got: {}", last_error());
    }

    #[test]
    fn a_client_without_a_token_is_anonymous() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        assert!(!client.is_null());
        assert_eq!(has_token(client), 0);

        let config = json_call("", |_| unsafe { ofbot_client_config(client) });
        assert_eq!(config["ok"], true);
        assert_eq!(config["config"]["base_url"], NO_SERVER);
        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn the_config_redacts_the_token() {
        let token = format!("ofb_{}", "1".repeat(64));
        let client = new_client(&format!(
            r#"{{"base_url":"{NO_SERVER}","token":"{token}"}}"#
        ));
        assert_eq!(has_token(client), 1);

        let config = json_call("", |_| unsafe { ofbot_client_config(client) });
        let serialised = config.to_string();
        assert!(
            !serialised.contains(&"1".repeat(64)),
            "the token body must never be serialised: {serialised}"
        );
        assert!(serialised.contains("ofb_…"), "a redacted form is expected");
        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn set_token_swaps_and_clears() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        assert_eq!(has_token(client), 0);

        let token = CString::new(format!("ofb_{}", "2".repeat(64))).unwrap();
        assert_eq!(unsafe { ofbot_client_set_token(client, token.as_ptr()) }, 1);
        assert_eq!(has_token(client), 1);

        let bad = CString::new("bogus").unwrap();
        assert_eq!(unsafe { ofbot_client_set_token(client, bad.as_ptr()) }, 0);
        assert_eq!(
            has_token(client),
            1,
            "a rejected token must not clear the working one"
        );

        assert_eq!(
            unsafe { ofbot_client_set_token(client, std::ptr::null()) },
            0,
            "null is not the same as an empty string"
        );

        let empty = CString::new("").unwrap();
        assert_eq!(unsafe { ofbot_client_set_token(client, empty.as_ptr()) }, 1);
        assert_eq!(has_token(client), 0);

        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn ofbot_call_runs_named_methods() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        let answer = call(client, "capabilities", "null");
        assert_eq!(answer["ok"], true);
        assert_eq!(
            answer["result"]["protocol"],
            openfield_bot::PROTOCOL_VERSION
        );
        assert!(answer["result"]["methods"]
            .as_array()
            .unwrap()
            .contains(&Value::String("posts.create".into())));
        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn ofbot_call_reports_an_unknown_method() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        let answer = call(client, "nope.nope", "{}");
        assert_eq!(answer["ok"], false);
        assert!(answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown method"));
        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn ofbot_call_reports_a_missing_parameter() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        let answer = call(client, "posts.create", "{}");
        assert_eq!(answer["ok"], false);
        assert_eq!(answer["error"]["kind"], "invalid_argument");
        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn ofbot_call_refuses_events_without_a_sink() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        let answer = call(client, "events.subscribe", "{}");
        assert_eq!(answer["ok"], false);
        assert!(
            answer["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no event sink"),
            "the C ABI must point the caller at ofbot_events_*: {answer}"
        );
        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn ofbot_call_rejects_bad_json_params() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        let answer = call(client, "posts.create", "{not json");
        assert_eq!(answer["ok"], false);
        assert!(answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not valid JSON"));
        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn ofbot_call_with_a_null_params_pointer_is_allowed() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        let method = CString::new("client.has_token").unwrap();
        let out = unsafe { ofbot_call(client, method.as_ptr(), std::ptr::null()) };
        assert!(!out.is_null());
        // SAFETY: the pointer came from this library.
        let text = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
        unsafe { ofbot_string_free(out) };
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["result"]["has_token"], false);
        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn a_request_shape_error_is_reported() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        let answer = json_call(r#"{"path":"/api/v1/users/me"}"#, |pointer| unsafe {
            ofbot_client_request(client, pointer)
        });
        assert_eq!(answer["ok"], false);
        assert!(answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("`method` is required"));
        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn an_absolute_path_is_refused() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        let answer = json_call(
            r#"{"method":"GET","path":"https://evil.example/steal"}"#,
            |pointer| unsafe { ofbot_client_request(client, pointer) },
        );
        assert_eq!(answer["ok"], false);
        assert!(
            answer["error"]["message"]
                .as_str()
                .unwrap()
                .contains("server-relative"),
            "an absolute URL must never receive the bot token: {answer}"
        );
        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn a_transport_failure_is_reported_as_a_retryable_network_error() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        let answer = json_call(
            r#"{"method":"GET","path":"/api/v1/capabilities"}"#,
            |pointer| unsafe { ofbot_client_request(client, pointer) },
        );
        assert_eq!(answer["ok"], false);
        assert_eq!(answer["error"]["kind"], "network");
        assert_eq!(answer["error"]["retryable"], true);
        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn a_multipart_file_without_a_source_is_rejected() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        let answer = json_call(
            r#"{"method":"POST","path":"/api/v1/attachments",
                "multipart":{"files":[{"file_name":"a.png"}]}}"#,
            |pointer| unsafe { ofbot_client_request(client, pointer) },
        );
        assert_eq!(answer["ok"], false);
        assert!(answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("needs `path` or `bytes`"));
        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn a_missing_upload_path_is_reported() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        let answer = json_call(
            r#"{"method":"POST","path":"/api/v1/attachments",
                "multipart":{"files":[{"file_name":"a.png","path":"/no/such/file"}]}}"#,
            |pointer| unsafe { ofbot_client_request(client, pointer) },
        );
        assert_eq!(answer["ok"], false);
        assert!(answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot read upload file"));
        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn subscribed_events_need_a_token() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        let filter = CString::new("{}").unwrap();
        let events = unsafe { ofbot_events_subscribe(client, filter.as_ptr()) };
        assert!(events.is_null(), "an anonymous client cannot subscribe");
        assert!(last_error().contains("bot token"), "got: {}", last_error());
        unsafe { ofbot_client_free(client) };
    }

    #[test]
    fn a_null_stream_answers_a_tagged_error() {
        let answer = json_call("", |_| unsafe {
            ofbot_events_next(std::ptr::null_mut(), 0)
        });
        assert_eq!(answer["kind"], "error");
        assert_eq!(answer["error"]["kind"], "invalid_argument");
    }

    #[test]
    fn closing_and_freeing_null_are_no_ops() {
        unsafe {
            ofbot_events_close(std::ptr::null_mut());
            ofbot_events_free(std::ptr::null_mut());
            ofbot_client_free(std::ptr::null_mut());
            ofbot_string_free(std::ptr::null_mut());
        }
    }

    #[test]
    fn free_accepts_a_real_string_once() {
        let client = new_client(&format!(r#"{{"base_url":"{NO_SERVER}"}}"#));
        let out = unsafe { ofbot_client_config(client) };
        assert!(!out.is_null());
        unsafe { ofbot_string_free(out) };
        unsafe { ofbot_client_free(client) };
    }
}
