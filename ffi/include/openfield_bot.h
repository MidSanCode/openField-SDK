/*
 * openfield_bot.h — C ABI for the OpenField community bot SDK.
 *
 * This header is hand-maintained next to `ffi/src/lib.rs`; keep the two in step.
 *
 * Link against either the shared library (`libopenfield_bot_ffi.so` / `.dylib` /
 * `openfield_bot_ffi.dll`) or the static archive (`libopenfield_bot_ffi.a`).
 *
 * Contract
 * --------
 *  - Handles (`ofbot_client *`, `ofbot_events *`) are opaque. Create them with the
 *    `_new`/`_subscribe` functions and release them with the matching `_free`.
 *  - Strings cross as UTF-8 JSON, so the ABI does not change when the API grows.
 *  - Every `char *` returned by this library is owned by the caller and must be
 *    released with `ofbot_string_free`. The `const char *` returned by
 *    `ofbot_version` and `ofbot_last_error` is borrowed: do **not** free it.
 *  - A null handle never crashes: the call answers with a JSON error and sets the
 *    thread-local message from `ofbot_last_error`.
 *  - Nothing unwinds across the boundary; an internal panic becomes an error result.
 *
 * Threading
 * ---------
 * A client is safe to share between threads; calls serialise on an internal lock.
 * `ofbot_last_error` is thread-local, so read it on the thread that saw the failure.
 *
 * Minimal example
 * ---------------
 *     ofbot_client *bot = ofbot_client_new(
 *         "{\"base_url\":\"http://localhost:8080\",\"token\":\"ofb_…\"}");
 *     if (!bot) { fprintf(stderr, "%s\n", ofbot_last_error()); return 1; }
 *
 *     char *answer = ofbot_client_request(bot,
 *         "{\"method\":\"GET\",\"path\":\"/api/v1/users/me\"}");
 *     puts(answer);                    // {"ok":true,"status":200,"body":{…}}
 *     ofbot_string_free(answer);
 *
 *     ofbot_client_free(bot);
 */

#ifndef OPENFIELD_BOT_H
#define OPENFIELD_BOT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32)
#  define OFBOT_API __declspec(dllimport)
#else
#  define OFBOT_API
#endif

/* Opaque handles. */
typedef struct OfbotClient OfbotClient;
typedef struct OfbotEvents OfbotEvents;

/* ------------------------------------------------------------------ */
/* Library metadata                                                    */
/* ------------------------------------------------------------------ */

/* The SDK version. Borrowed pointer; do not free. */
OFBOT_API const char *ofbot_version(void);

/* The RPC protocol revision, as a decimal string. Borrowed pointer; do not free. */
OFBOT_API const char *ofbot_protocol_version(void);

/* The message behind the most recent error on this thread. Borrowed pointer; do not
 * free. Valid until the next FFI call on the same thread. */
OFBOT_API const char *ofbot_last_error(void);

/* Release a string returned by this library. Passing NULL is a no-op; freeing any
 * other pointer, or freeing twice, is undefined behaviour. */
OFBOT_API void ofbot_string_free(char *string);

/* ------------------------------------------------------------------ */
/* Client                                                             */
/* ------------------------------------------------------------------ */

/*
 * Create a client from a JSON options object:
 *
 *   {
 *     "base_url":          "https://openfield.example.com",
 *     "token":             "ofb_…64 hex…",        optional; omit for anonymous calls
 *     "timeout_ms":        30000,                 optional; default 30000
 *     "connect_timeout_ms": 10000,                optional
 *     "user_agent":        "my-bot/1.0",          optional
 *     "headers":           { "X-Proxy": "a" },    optional, sent on every request
 *     "max_response_bytes": 33554432,             optional
 *     "retry":     { "max_attempts": 3, "base_delay_ms": 500 },        optional
 *     "reconnect": { "initial_delay_ms": 1000, "max_attempts": null }  optional
 *   }
 *
 * Returns NULL on failure; call ofbot_last_error for the reason. A malformed token is
 * rejected here, before any request is made.
 */
OFBOT_API OfbotClient *ofbot_client_new(const char *options_json);

/* Free a client. Free any outstanding event stream first. NULL is a no-op. */
OFBOT_API void ofbot_client_free(OfbotClient *client);

/*
 * Replace the token, or clear it with NULL/"" to go anonymous.
 *
 * Needed because POST /bots/:id/regenerate invalidates the old token immediately.
 * Returns 1 on success, 0 on failure; a rejected token leaves the previous one in
 * place.
 */
OFBOT_API int ofbot_client_set_token(OfbotClient *client, const char *token);

/* The client's configuration as {"ok":true,"config":{…}}, with the token redacted.
 * Returns an owned string, or NULL on failure. */
OFBOT_API char *ofbot_client_config(OfbotClient *client);

/* 1 when the client holds a token, 0 otherwise (including a null handle). */
OFBOT_API int ofbot_client_has_token(OfbotClient *client);

/* ------------------------------------------------------------------ */
/* Method calls                                                       */
/* ------------------------------------------------------------------ */

/*
 * Run any method from the shared dispatcher. This is the one to use for normal work;
 * ofbot_client_request below is the raw escape hatch.
 *
 * `params_json` is a JSON object, or NULL / "null" for a method that takes none:
 *
 *   ofbot_call(bot, "posts.create", "{\"content\":\"hello\"}");
 *     -> {"ok":true,"result":{"id":42,"content":"hello"}}
 *
 *   ofbot_call(bot, "chat.send",
 *              "{\"conversation_id\":33,\"content\":\"pong\"}");
 *
 *   ofbot_call(bot, "posts.list", "{\"page\":1,\"limit\":20}");
 *
 *   // A greeting with a mention and a burn-after-reading timer:
 *   ofbot_call(bot, "chat.send",
 *              "{\"conversation_id\":33,\"content\":\"@alice hi\","
 *              "\"mentions\":[7],\"burn_seconds\":300}");
 *
 * The answer is always an owned string:
 *
 *   { "ok": true,  "result": … }
 *   { "ok": false, "error": { "kind": "forbidden", "status": 403,
 *                             "permission": "posts.create", "retryable": false } }
 *
 * Ask for `capabilities` to list every method name: `methods` in that answer is the
 * complete index, and it is the same list the sidecar serves, so a binding written once
 * runs against either transport.
 *
 * The event methods (events.subscribe, …) are NOT usable here: this transport has no
 * event sink, so they report that. Use ofbot_events_subscribe instead.
 */
OFBOT_API char *ofbot_call(OfbotClient *client, const char *method, const char *params_json);

/* ------------------------------------------------------------------ */
/* Raw requests                                                       */
/* ------------------------------------------------------------------ */

/*
 * Perform a request. `request_json` is an object:
 *
 *   {
 *     "method":     "POST",                          required
 *     "path":       "/api/v1/posts",                 required, server-relative
 *     "query":      { "page": "1" },                 optional
 *     "body":       { "content": "hi" },             optional JSON body
 *     "body_json":  "{\"content\":\"hi\"}",          optional pre-serialised body
 *     "bytes":      [1, 2, 3],                       optional raw body
 *     "empty_body": true,                            optional: send {}
 *     "multipart":  {                                optional file upload
 *       "fields": { "description": "chart" },
 *       "files":  [ { "field": "file", "file_name": "chart.png",
 *                     "path": "/tmp/chart.png" } ]   // or "bytes": [1,2,3]
 *     }
 *   }
 *
 * Answers with an envelope, always owned by the caller:
 *
 *   { "ok": true,  "status": 201, "body": { "id": 1 } }
 *   { "ok": false, "error": { "kind": "http", "status": 403,
 *                             "api_error": "permission denied",
 *                             "permission": "posts.create",
 *                             "retryable": false,
 *                             "body": { … } } }
 *
 * `error.kind` is one of: invalid_argument, decode, transport, network, bad_request,
 * unauthorized, forbidden, not_found, conflict, rate_limited, server_error, other.
 *
 * Non-2xx statuses are reported in the envelope rather than raised, because a bot has
 * to distinguish a 409 from a 429 without string matching.
 *
 * The client's own retry policy already applied: GET/HEAD are retried on transport
 * failures and 5xx, and every verb is retried on 429 honouring Retry-After. Mutations
 * are deliberately not retried on 5xx, so a POST cannot be duplicated.
 */
OFBOT_API char *ofbot_client_request(OfbotClient *client, const char *request_json);

/* ------------------------------------------------------------------ */
/* Realtime events                                                    */
/* ------------------------------------------------------------------ */

/*
 * Subscribe to the WebSocket channel. `filter_json` may be NULL or "" for everything:
 *
 *   {
 *     "types":            ["chat.message.created"],  optional; empty means all
 *     "conversation_ids": [33],                      optional
 *     "queue_capacity":   1024,                      optional; oldest dropped when full
 *     "reconnect":        { "initial_delay_ms": 1000, "max_attempts": null }
 *   }
 *
 * Returns NULL on failure. Requires a token: the channel is per-account, and the
 * gateway only accepts a single-use ticket that this library mints for you.
 */
OFBOT_API OfbotEvents *ofbot_events_subscribe(OfbotClient *client, const char *filter_json);

/*
 * Wait for the next event. `timeout_ms` < 0 blocks until an event or the stream ends.
 *
 * Always returns an owned string (only allocation failure yields NULL):
 *
 *   { "kind": "event",   "event": { "type": "chat.message.created", "data": { … } } }
 *   { "kind": "timeout" }
 *   { "kind": "closed" }
 *   { "kind": "error",   "error": { … } }
 *
 * Events are best effort: the server relays them through Postgres NOTIFY and does not
 * replay them across a disconnect. Poll the REST API periodically to reconcile.
 */
OFBOT_API char *ofbot_events_next(OfbotEvents *events, int64_t timeout_ms);

/* The stream counters as {"ok":true,"stats":{"events_delivered":…,"events_dropped":…}}.
 * Returns an owned string, or NULL on failure. */
OFBOT_API char *ofbot_events_stats(OfbotEvents *events);

/* Ask the stream to stop without freeing it. NULL is a no-op. */
OFBOT_API void ofbot_events_close(OfbotEvents *events);

/* Free a stream, stopping its reader thread. NULL is a no-op. */
OFBOT_API void ofbot_events_free(OfbotEvents *events);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* OPENFIELD_BOT_H */
