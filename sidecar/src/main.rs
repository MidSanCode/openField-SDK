//! JSON-RPC sidecar for the OpenField bot SDK.
//!
//! This is the second half of the cross-language story. Where `openfield-bot-ffi`
//! exposes a C ABI for languages that can load a shared library, the sidecar is a plain
//! child process speaking newline-delimited JSON over stdin/stdout, so any language with
//! a subprocess API — Python, Node, Ruby, PHP, Go, shell — can drive a bot without a Rust
//! toolchain and without matching ABI versions.
//!
//! The method table itself lives in [`openfield_bot::rpc`], shared with the C ABI, so the
//! two transports expose exactly the same surface. This binary only owns stdio: read a
//! line, dispatch, write the answer, and forward events.
//!
//! # Protocol
//!
//! One JSON object per line, in both directions. Everything on stdout is protocol;
//! diagnostics go to stderr so a caller can pipe stdout straight into a parser.
//!
//! ## Requests
//!
//! ```json
//! {"id": 1, "method": "posts.create", "params": {"content": "hello"}}
//! ```
//!
//! `id` is echoed back verbatim and may be any JSON value. A request without an `id` is a
//! notification and is answered with nothing.
//!
//! ## Responses
//!
//! ```json
//! {"id": 1, "result": {"id": 42, "content": "hello"}}
//! {"id": 2, "error": {"kind": "forbidden", "status": 403, "permission": "posts.create",
//!                     "message": "http 403: permission denied", "retryable": false}}
//! ```
//!
//! ## Events
//!
//! Pushed unsolicited while a subscription is active, in arrival order:
//!
//! ```json
//! {"event": "chat.message.created", "data": {…}, "recipients": [4242]}
//! ```
//!
//! # Lifecycle
//!
//! Send `client.init` first; every other method answers an error until then. Closing
//! stdin, sending `shutdown`, or terminating the process all stop it cleanly.
//!
//! ```text
//! $ echo '{"id":1,"method":"client.init","params":{"base_url":"http://localhost:8080","token":"ofb_…"}}
//! {"id":1,"method":"me"}' | openfield-bot-sidecar
//! {"id":1,"result":{"ok":true,"has_token":true}}
//! {"id":2,"result":{"id":4242,"username":"weather_watcher","is_bot":true,…}}
//! ```

use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use openfield_bot::{error_object, event_message, EventSink, RpcState, PROTOCOL_VERSION, VERSION};

/// A writer shared by the main thread and the event forwarder.
type Out = Arc<Mutex<BufWriter<io::Stdout>>>;

fn emit(out: &Out, message: &Value) {
    let mut guard = match out.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    // A write failure means the parent went away; there is nowhere to report it, so the
    // line is dropped and the process will exit on the next stdin read.
    if writeln!(guard, "{message}").is_err() {
        return;
    }
    let _ = guard.flush();
}

/// Answer a request, unless it was a notification.
fn respond(out: &Out, id: &Value, result: Result<Value, openfield_bot::SdkError>) {
    if id.is_null() {
        return;
    }
    match result {
        Ok(value) => emit(out, &json!({ "id": id, "result": value })),
        Err(err) => emit(out, &json!({ "id": id, "error": error_object(&err) })),
    }
}

/// Read, dispatch and answer until stdin closes or `shutdown` arrives.
fn serve(state: &mut RpcState, out: &Out) -> io::Result<()> {
    let stdin = io::stdin();
    let reader = BufReader::new(stdin.lock());

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let parsed: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(err) => {
                // A malformed line cannot be answered by id, because the id is exactly
                // what failed to parse.
                emit(
                    out,
                    &json!({
                        "id": Value::Null,
                        "error": {
                            "kind": "invalid_argument",
                            "message": format!("each line must be a JSON object: {err}"),
                            "retryable": false,
                        }
                    }),
                );
                continue;
            }
        };

        let id = parsed.get("id").cloned().unwrap_or(Value::Null);
        let method = match parsed.get("method").and_then(Value::as_str) {
            Some(method) => method.to_string(),
            None => {
                respond(
                    out,
                    &id,
                    Err(openfield_bot::SdkError::InvalidArgument {
                        message: "`method` is required".into(),
                    }),
                );
                continue;
            }
        };

        let params = parsed.get("params").cloned().unwrap_or(Value::Null);

        if method == "shutdown" {
            respond(out, &id, Ok(json!({ "ok": true })));
            return Ok(());
        }

        let result = state.dispatch(&method, &params);
        respond(out, &id, result);
    }

    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("openfield-bot-sidecar {VERSION} (protocol {PROTOCOL_VERSION})");
        return;
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "openfield-bot-sidecar {VERSION} (protocol {PROTOCOL_VERSION})\n\
             \n\
             Speaks newline-delimited JSON on stdin/stdout; diagnostics go to stderr.\n\
             Start with: {{\"id\":1,\"method\":\"client.init\",\"params\":{{\"base_url\":\"…\",\"token\":\"ofb_…\"}}}}\n\
             \n\
             Methods: version, ping, capabilities, client.init, client.set_token,\n\
             client.config, client.has_token, request, me, permissions, startup_check,\n\
             posts.create, chat.send, camps.list, storage.upload, events.subscribe, …\n\
             Ask for `capabilities` for the full list.\n\
             \n\
             Options:\n\
             \x20 --version   print the version and exit\n\
             \x20 --help      print this help and exit"
        );
        return;
    }

    let out: Out = Arc::new(Mutex::new(BufWriter::new(io::stdout())));

    // Events arrive on a reader thread; the sink writes them to the same stdout as the
    // responses, which is why the writer is behind a mutex.
    let sink_out = Arc::clone(&out);
    let sink: EventSink = Arc::new(move |event| {
        emit(&sink_out, &event_message(event));
    });

    let mut state = RpcState::with_event_sink(sink);

    // A greeting on stderr, never stdout: stdout carries protocol only.
    eprintln!("openfield-bot-sidecar {VERSION} protocol {PROTOCOL_VERSION} ready");

    if let Err(err) = serve(&mut state, &out) {
        eprintln!("sidecar read loop failed: {err}");
    }
}
