//! The JSON method table is the contract both cross-language transports share, so
//! it gets its own end-to-end pass against the mock server.
//!
//! Every method added here is reachable from the sidecar *and* the C ABI by
//! construction, because both forward to [`openfield_bot::RpcState::dispatch`].
//!
//! ```text
//! cargo test -p openfield-bot-sdk --features testkit --test rpc_surface
//! ```
#![cfg(feature = "testkit")]

use serde_json::{json, Value};

use openfield_bot::testkit::MockServer;
use openfield_bot::{error_object, RpcState, METHODS};

/// A state wired to `server`, with the token supplied only when `token` is true.
fn state_for(server: &MockServer, token: bool) -> RpcState {
    let mut state = RpcState::new();
    let params = if token {
        json!({"base_url": server.base_url(), "token": server.token()})
    } else {
        json!({"base_url": server.base_url()})
    };
    state.dispatch("client.init", &params).unwrap();
    state
}

fn call(state: &mut RpcState, method: &str, params: Value) -> Value {
    state
        .dispatch(method, &params)
        .unwrap_or_else(|err| panic!("{method} failed: {err}"))
}

// --- the table --------------------------------------------------------------

#[test]
fn the_table_carries_every_new_group() {
    for method in [
        "wallet.get",
        "wallet.adjust",
        "wallet.recharge",
        "wallet.deduct",
        "tasks.list",
        "tasks.claim",
        "tasks.claim_daily",
        "tasks.login_claim",
        "tasks.login_calendar",
        "tasks.makeup",
        "tasks.makeup_date",
        "tasks.exp_history",
        "membership.get",
        "membership.purchase",
        "membership.auto_renew",
        "membership.purchases",
        "membership.grant",
        "membership.exp_adjust",
        "account.pin_set",
        "account.pin_change",
        "account.pin_verify",
        "moderation.punish",
        "moderation.punishments",
        "e2ee.set_key",
        "e2ee.keys",
        "e2ee.put_keys",
        "plugins.list",
        "plugins.get",
        "plugins.download",
        "plugins.upload",
        "plugins.publish",
        "plugins.unpublish",
        "plugins.delete",
        "posts.tip",
        "storage.attachment",
        "storage.by_hash",
        "storage.burn_arm",
        "storage.chunk_init",
        "storage.chunk_upload",
        "storage.chunk_status",
        "storage.chunk_complete",
        "storage.chunked",
    ] {
        assert!(METHODS.contains(&method), "{method} must be listed");
    }
    // Every listed method still has to be dispatched somewhere: a listed name with
    // no arm is worse than a missing one, because the transports advertise it.
    let mut state = RpcState::new();
    for method in METHODS {
        if let Err(err) = state.dispatch(method, &Value::Null) {
            let text = err.to_string();
            // Whatever the reason, it must be a real refusal from a real arm —
            // "unknown method" is the one answer that means no arm exists.
            assert!(
                !text.contains("unknown method"),
                "{method} is listed but not dispatched"
            );
        }
    }
}

#[test]
fn capabilities_advertises_the_full_table() {
    let mut state = RpcState::new();
    let caps = state.dispatch("capabilities", &Value::Null).unwrap();
    let listed = caps["methods"].as_array().unwrap();
    assert_eq!(listed.len(), METHODS.len());
    assert!(listed.len() > 120, "the surface grew as expected");
}

// --- wallet through the dispatcher -----------------------------------------

#[test]
fn wallet_get_decodes_a_decimal_balance() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);
    let wallet = call(&mut state, "wallet.get", json!({"page": 2, "limit": 5}));
    assert_eq!(wallet["balance"], 1000);
    assert_eq!(wallet["page"], 2);
    assert_eq!(wallet["limit"], 5);
}

#[test]
fn wallet_recharge_and_deduct_carry_the_sign() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);

    let credited = call(
        &mut state,
        "wallet.recharge",
        json!({"user_id": 42, "amount": 12.5}),
    );
    assert_eq!(credited["balance"], 1012.5);
    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["amount"], 12.5);
    assert_eq!(body["type"], "recharge");

    let debited = call(
        &mut state,
        "wallet.deduct",
        json!({"user_id": 42, "amount": 2.5, "description": "罚款"}),
    );
    assert_eq!(debited["balance"], 1010);
    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["amount"], -2.5);
    assert_eq!(body["description"], "罚款");
}

#[test]
fn wallet_adjust_takes_a_signed_amount_verbatim() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);
    // The raw form is the wire shape: a negative amount with `type` left alone.
    let adjusted = call(
        &mut state,
        "wallet.adjust",
        json!({"user_id": 42, "amount": -10, "type": "deduct"}),
    );
    assert_eq!(adjusted["balance"], 990);
}

#[test]
fn wallet_adjust_rejects_a_zero_amount_and_a_bad_parameter_type() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);
    let error = state
        .dispatch("wallet.adjust", &json!({"user_id": 42, "amount": 0}))
        .unwrap_err();
    assert!(
        error.to_string().contains("must not be zero"),
        "got: {error}"
    );

    let error = state
        .dispatch("wallet.recharge", &json!({"user_id": 42, "amount": "abc"}))
        .unwrap_err();
    assert!(error.to_string().contains("amount"), "got: {error}");

    let error = state
        .dispatch("wallet.recharge", &json!({"amount": 1}))
        .unwrap_err();
    assert!(error.to_string().contains("user_id"), "got: {error}");
}

// --- tasks ------------------------------------------------------------------

#[test]
fn tasks_list_and_claim_round_trip() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);
    let list = call(&mut state, "tasks.list", Value::Null);
    assert_eq!(list["streak"], 4);
    assert_eq!(list["makeup_cost"], 100);
    assert!(list["tasks"].as_array().unwrap().len() >= 5);

    let claimed = call(&mut state, "tasks.claim", json!({"code": "daily_posts_1"}));
    assert_eq!(claimed["claimed"], true);
    assert_eq!(claimed["currency"], 5);

    let error = state
        .dispatch("tasks.claim", &json!({"code": "daily_posts_1"}))
        .unwrap_err();
    assert_eq!(error.status(), Some(409));

    let error = state
        .dispatch("tasks.claim_daily", &json!({"code": "daily_chat_100"}))
        .unwrap_err();
    assert_eq!(error.api_message(), Some("task requirements not met"));

    let error = state.dispatch("tasks.claim", &Value::Null).unwrap_err();
    assert!(error.to_string().contains("code"), "got: {error}");
}

#[test]
fn the_signin_methods_round_trip() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);

    let calendar = call(&mut state, "tasks.login_calendar", Value::Null);
    assert_eq!(calendar["today"], "2026-08-04");
    assert_eq!(calendar["days"].as_array().unwrap().len(), 4);

    let signin = call(&mut state, "tasks.login_claim", Value::Null);
    assert_eq!(signin["granted"], true);
    assert_eq!(signin["currency"], 20);

    let makeup = call(&mut state, "tasks.makeup", Value::Null);
    assert_eq!(makeup["cost"], 100);

    let dated = call(
        &mut state,
        "tasks.makeup_date",
        json!({"date": "2026-08-03"}),
    );
    assert_eq!(dated["date"], "2026-08-03");

    let history = call(&mut state, "tasks.exp_history", json!({"limit": 5}));
    assert_eq!(history["entries"].as_array().unwrap().len(), 2);
}

// --- membership and the payment PIN ----------------------------------------

#[test]
fn the_payment_pin_gates_membership_and_tips() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);

    let membership = call(&mut state, "membership.get", Value::Null);
    assert_eq!(membership["level"], 0);
    assert_eq!(membership["tiers"].as_array().unwrap().len(), 4);

    // No PIN yet.
    let error = state
        .dispatch("membership.purchase", &json!({"level": 1, "pin": "123456"}))
        .unwrap_err();
    assert_eq!(error.status(), Some(403));
    assert_eq!(error.api_message(), Some("payment pin not set"));

    let error = state
        .dispatch(
            "posts.tip",
            &json!({"post_id": 555, "amount": 5, "pin": "123456"}),
        )
        .unwrap_err();
    assert_eq!(error.status(), Some(403));

    // Set it, then both work.
    assert_eq!(
        call(&mut state, "account.pin_set", json!({"pin": "123456"}))["has_pin"],
        true
    );
    let purchase = call(
        &mut state,
        "membership.purchase",
        json!({"level": 2, "pin": "123456"}),
    );
    assert_eq!(purchase["paid"], 200);
    assert_eq!(purchase["status"]["level"], 2);

    let tip = call(
        &mut state,
        "posts.tip",
        json!({"post_id": 555, "amount": 5, "pin": "123456"}),
    );
    assert_eq!(tip["amount"], 500, "the response is in cents");
    assert_eq!(tip["net_amount"], 475);

    assert_eq!(
        call(&mut state, "account.pin_verify", json!({"pin": "123456"}))["valid"],
        true
    );
    let wrong = state
        .dispatch("account.pin_verify", &json!({"pin": "000000"}))
        .unwrap_err();
    assert_eq!(wrong.status(), Some(401));

    call(
        &mut state,
        "account.pin_change",
        json!({"old_pin": "123456", "pin": "654321"}),
    );
    assert_eq!(
        call(&mut state, "account.pin_verify", json!({"pin": "654321"}))["valid"],
        true
    );

    // A malformed PIN is caught before the request.
    let error = state
        .dispatch("account.pin_set", &json!({"pin": "12"}))
        .unwrap_err();
    assert!(error.to_string().contains("6 digits"), "got: {error}");
}

#[test]
fn auto_renew_passes_the_pin_only_when_asked() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);
    call(&mut state, "account.pin_set", json!({"pin": "123456"}));

    let enabled = call(
        &mut state,
        "membership.auto_renew",
        json!({"enabled": true, "pin": "123456"}),
    );
    assert_eq!(enabled["auto_renew"], true);

    let disabled = call(
        &mut state,
        "membership.auto_renew",
        json!({"enabled": false}),
    );
    assert_eq!(disabled["auto_renew"], false);
    let body = server.last_request().unwrap().json().unwrap();
    assert!(body.get("pin").is_none());

    let error = state
        .dispatch("membership.auto_renew", &json!({"enabled": true}))
        .unwrap_err();
    assert!(error.to_string().contains("payment pin"), "got: {error}");
}

#[test]
fn membership_admin_calls_and_exp_decode() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);

    let purchases = call(&mut state, "membership.purchases", json!({"limit": 3}));
    assert_eq!(purchases["purchases"].as_array().unwrap().len(), 0);

    let granted = call(
        &mut state,
        "membership.grant",
        json!({"user_id": 9, "level": 3, "days": 30}),
    );
    assert_eq!(granted["level"], 3);

    let adjusted = call(
        &mut state,
        "membership.exp_adjust",
        json!({"user_id": 9, "delta": -25, "description": "回滚"}),
    );
    assert_eq!(adjusted["level"], 5);
    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["delta"], -25);

    let error = state
        .dispatch("membership.exp_adjust", &json!({"user_id": 9}))
        .unwrap_err();
    assert!(error.to_string().contains("delta"), "got: {error}");
}

// --- punishments and E2EE ---------------------------------------------------

#[test]
fn punishments_dispatch_through_the_table() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);

    let receipt = call(
        &mut state,
        "moderation.punish",
        json!({"user_id": 7, "punishment": {"type": "temp_ban", "reason": "刷屏",
                                            "duration_minutes": 60}}),
    );
    assert_eq!(receipt["type"], "temp_ban");
    assert!(receipt["punishment_id"].as_i64().unwrap() > 0);

    let list = call(
        &mut state,
        "moderation.punishments",
        json!({"user_id": 7, "limit": 10}),
    );
    assert_eq!(list["punishments"].as_array().unwrap().len(), 1);
    assert_eq!(list["permission_bans"][0], "posts.create");

    // The per-type requirement is enforced before the request.
    let error = state
        .dispatch(
            "moderation.punish",
            &json!({"user_id": 7, "punishment": {"type": "revoke", "reason": "x"}}),
        )
        .unwrap_err();
    assert!(error.to_string().contains("permission_key"), "got: {error}");

    // An unknown type is rejected while decoding the request, before it is sent.
    let error = state
        .dispatch(
            "moderation.punish",
            &json!({"user_id": 7, "punishment": {"type": "exile"}}),
        )
        .unwrap_err();
    assert!(error.to_string().contains("exile"), "got: {error}");
}

#[test]
fn e2ee_envelopes_travel_with_string_keys() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);

    let put = call(
        &mut state,
        "e2ee.put_keys",
        json!({"conversation_id": 33, "envelopes": {"7": "AAA", "9": "BBB"}}),
    );
    assert_eq!(put["version"], 1);
    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["envelopes"]["7"], "AAA");

    let keys = call(&mut state, "e2ee.keys", json!({"conversation_id": 33}));
    assert_eq!(keys["version"], 1);
    assert_eq!(keys["envelopes"].as_array().unwrap().len(), 2);
    assert!(keys["my_envelope"].is_null());

    call(
        &mut state,
        "e2ee.set_key",
        json!({"public_key": "ed25519:ZZZ"}),
    );

    let error = state
        .dispatch(
            "e2ee.put_keys",
            &json!({"conversation_id": 33, "envelopes": {}}),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("at least one envelope"),
        "got: {error}"
    );
}

// --- storage ----------------------------------------------------------------

#[test]
fn the_chunked_protocol_dispatches_end_to_end() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);
    let request = json!({"filename": "big.bin", "size": 200, "total_chunks": 2});

    let session = call(&mut state, "storage.chunk_init", json!({"upload": request}));
    let upload_id = session["upload_id"].as_str().unwrap().to_string();
    assert_eq!(session["total_chunks"], 2);

    let empty = call(
        &mut state,
        "storage.chunk_status",
        json!({"upload_id": upload_id}),
    );
    assert_eq!(empty["uploaded"].as_array().unwrap().len(), 0);

    // Base64 is the documented way for a binding to send bytes.
    call(
        &mut state,
        "storage.chunk_upload",
        json!({"part": {"upload_id": upload_id, "index": 1, "bytes": "BwcHBwc="}}),
    );
    let half = call(
        &mut state,
        "storage.chunk_status",
        json!({"upload_id": upload_id}),
    );
    assert_eq!(half["uploaded"], json!([1]));

    let error = state
        .dispatch(
            "storage.chunk_complete",
            &json!({"upload_id": upload_id, "upload": request}),
        )
        .unwrap_err();
    assert_eq!(error.status(), Some(409));
    assert_eq!(error.api_message(), Some("missing chunks"));

    call(
        &mut state,
        "storage.chunk_upload",
        json!({"part": {"upload_id": upload_id, "index": 2, "bytes": [7, 8, 9]}}),
    );
    let attachment = call(
        &mut state,
        "storage.chunk_complete",
        json!({"upload_id": upload_id, "upload": request}),
    );
    assert_eq!(attachment["original_name"], "big.bin");
}

#[test]
fn the_whole_file_driver_dispatches() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);
    let attachment = call(
        &mut state,
        "storage.chunked",
        json!({
            "upload": {"file_name": "report.bin", "bytes": "BwcHBwcHBwc="},
            "chunk_bytes": 4
        }),
    );
    assert_eq!(attachment["original_name"], "report.bin");
    assert_eq!(attachment["size_bytes"], 8);
}

#[test]
fn reuse_and_burn_dispatch() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);

    let reused = call(
        &mut state,
        "storage.by_hash",
        json!({"hash": "A".repeat(64)}),
    );
    assert_eq!(reused["id"], 9001);

    let armed = call(
        &mut state,
        "storage.burn_arm",
        json!({"attachment_id": 9001, "burn_seconds": 60}),
    );
    assert_eq!(armed["id"], 9001);
    assert!(armed["burn_at"].is_string());

    let error = state
        .dispatch(
            "storage.burn_arm",
            &json!({"attachment_id": 9001, "burn_seconds": 0}),
        )
        .unwrap_err();
    assert!(error.to_string().contains("burn_seconds"), "got: {error}");

    let error = state
        .dispatch("storage.by_hash", &json!({"hash": "nope"}))
        .unwrap_err();
    assert!(error.to_string().contains("SHA-256"), "got: {error}");
}

// --- plugins ----------------------------------------------------------------

#[test]
fn the_plugin_catalog_works_without_a_token() {
    let server = MockServer::start().unwrap();
    // No token at all: the catalog is authPublic.
    let mut state = state_for(&server, false);

    let catalog = call(&mut state, "plugins.list", Value::Null);
    assert_eq!(catalog["plugins"].as_array().unwrap().len(), 1);
    assert_eq!(catalog["plugins"][0]["id"], "demo");

    let plugin = call(&mut state, "plugins.get", json!({"plugin_id": "demo"}));
    assert_eq!(plugin["verified"], true);

    // The download needs a token, and this client has none.
    let error = state
        .dispatch("plugins.download", &json!({"plugin_id": "demo"}))
        .unwrap_err();
    assert!(error.to_string().contains("bot token"), "got: {error}");

    let error = state
        .dispatch("plugins.get", &json!({"plugin_id": ".."}))
        .unwrap_err();
    assert!(error.to_string().contains("plugin id"), "got: {error}");
}

#[test]
fn a_bundle_download_arrives_as_base64() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);
    let downloaded = call(&mut state, "plugins.download", json!({"plugin_id": "demo"}));
    assert_eq!(downloaded["size"], 15);

    let encoded = downloaded["bytes"].as_str().unwrap();
    let decoded = openfield_bot::models::base64_decode(encoded).unwrap();
    assert_eq!(decoded, b"PK\x03\x04demo-bundle".to_vec());
    // Round-trips through the shared encoder.
    assert_eq!(openfield_bot::models::base64_encode(&decoded), encoded);
}

#[test]
fn uploading_publishing_and_deleting_through_the_table() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);

    let uploaded = call(
        &mut state,
        "plugins.upload",
        json!({
            "upload": {"file_name": "bundle.zip", "bytes": "AQID"},
            "publish": true
        }),
    );
    assert_eq!(uploaded["message"], "uploaded");
    assert_eq!(uploaded["plugin"]["id"], "demo");
    assert_eq!(uploaded["plugin"]["published"], true);

    assert_eq!(
        call(
            &mut state,
            "plugins.unpublish",
            json!({"plugin_id": "demo"})
        )["message"],
        "updated"
    );
    assert_eq!(
        call(&mut state, "plugins.publish", json!({"plugin_id": "demo"}))["message"],
        "updated"
    );
    assert_eq!(
        call(&mut state, "plugins.delete", json!({"plugin_id": "demo"}))["message"],
        "deleted"
    );

    let error = state
        .dispatch("plugins.delete", &json!({"plugin_id": "demo"}))
        .unwrap_err();
    assert_eq!(error.status(), Some(404));
}

// --- error rendering --------------------------------------------------------

#[test]
fn an_endpoint_failure_renders_the_transport_error_object() {
    let server = MockServer::start().unwrap();
    let mut state = state_for(&server, true);
    // A deduction it cannot afford, forced through the mock's own wallet.
    let error = state
        .dispatch("wallet.deduct", &json!({"user_id": 42, "amount": 10_000}))
        .unwrap_err();
    let object = error_object(&error);
    // Error kinds are status-specific, so a 400 is `bad_request` rather than `http`.
    assert_eq!(object["kind"], "bad_request");
    assert_eq!(object["status"], 400);
    assert_eq!(object["api_error"], "insufficient balance");
    assert!(object["retryable"].is_boolean());
    assert!(object["message"].is_string());
    // The server's own body survives for inspection.
    assert_eq!(object["body"]["error"], "insufficient balance");
}
