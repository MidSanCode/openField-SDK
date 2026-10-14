//! End-to-end coverage of the money, task, membership, moderation, tip and E2EE
//! surfaces, driven against the in-process mock server.
//!
//! Run with the `testkit` feature:
//!
//! ```text
//! cargo test -p openfield-bot-sdk --features testkit --test gameplay
//! ```
#![cfg(feature = "testkit")]

use openfield_bot::testkit::MockServer;
use openfield_bot::{
    E2eeEnvelopeMap, ErrorKind, MembershipPurchaseRequest, Money, NewWalletAdjustment,
    PunishmentRequest, PunishmentType, SdkError, MONEY_SCALE,
};

/// Every test starts a server, exercises the endpoint and lets `Drop` stop it.
fn server() -> MockServer {
    MockServer::start().expect("the mock server must start")
}

/// Set the payment PIN, which a fresh bot does not have.
fn with_pin(server: &MockServer) -> openfield_bot::AuthenticatedClient {
    let bot = server.client();
    bot.set_payment_pin("123456", None)
        .expect("the pin must be settable");
    bot
}

fn status(error: &SdkError) -> Option<u16> {
    error.status()
}

// --- wallet -----------------------------------------------------------------

#[test]
fn wallet_reports_a_decimal_coin_balance() {
    let server = server();
    let wallet = server.client().wallet(None, None).unwrap();
    // The mock opens with 1000 coins, and `Cents(100000)` marshals as `1000`.
    assert_eq!(wallet.balance.cents(), 1000 * MONEY_SCALE);
    assert_eq!(wallet.balance.to_string(), "1000");
    assert_eq!(wallet.page, 1);
    assert_eq!(wallet.limit, 20);
}

#[test]
fn wallet_page_and_limit_are_echoed() {
    let server = server();
    let wallet = server.client().wallet(Some(3), Some(7)).unwrap();
    assert_eq!(wallet.page, 3);
    assert_eq!(wallet.limit, 7);
    let request = server.last_request().unwrap();
    assert_eq!(request.query_param("page").as_deref(), Some("3"));
    assert_eq!(request.query_param("limit").as_deref(), Some("7"));
}

#[test]
fn a_recharge_returns_the_new_balance_and_a_transaction() {
    let server = server();
    let bot = server.client();
    let result = bot
        .recharge_wallet(42, Money::from_coins(2.5).unwrap())
        .unwrap();
    assert_eq!(result.balance.cents(), 100_250);
    assert_eq!(result.transaction.amount.cents(), 250);
    assert_eq!(result.transaction.balance_after.cents(), 100_250);
    assert_eq!(result.transaction.kind, "recharge");

    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["user_id"], 42);
    assert_eq!(body["amount"], 2.5, "the wire amount is decimal coins");
    assert_eq!(body["type"], "recharge");
}

#[test]
fn a_deduction_larger_than_the_balance_is_refused_with_400() {
    let server = server();
    let bot = server.client();
    let error = bot
        .deduct_wallet(42, Money::from_coins(5000.0).unwrap())
        .unwrap_err();
    // `wallet.md` documents 409 here; the handler answers 400.
    assert_eq!(status(&error), Some(400));
    assert_eq!(error.api_message(), Some("insufficient balance"));
}

#[test]
fn deduct_negates_the_amount_because_the_server_trusts_the_sign() {
    let server = server();
    let bot = server.client();
    // The documented shape: `amount` is signed and `type` is only a label.
    let adjustment = NewWalletAdjustment::deduct(42, Money::from_coins(5.0).unwrap()).unwrap();
    let result = bot.adjust_wallet(&adjustment).unwrap();
    assert_eq!(result.balance.cents(), 995 * MONEY_SCALE);
    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["amount"], -5);
    assert_eq!(body["type"], "deduct");
}

#[test]
fn a_positive_amount_labelled_deduct_still_credits() {
    let server = server();
    let bot = server.client();
    // The handler stores `amount` verbatim and never reconciles it with `type`,
    // so this is a credit wearing a deduction's label. The raw form allows it;
    // `deduct()` does not.
    let adjustment = NewWalletAdjustment {
        user_id: 42,
        amount: Money::from_coins(1.0).unwrap(),
        kind: Some("deduct".to_string()),
        description: None,
    };
    let result = bot.adjust_wallet(&adjustment).unwrap();
    assert_eq!(result.balance.cents(), 1001 * MONEY_SCALE);
    assert_eq!(result.transaction.kind, "deduct");
}

#[test]
fn an_unknown_target_and_a_zero_amount_are_refused() {
    let server = server();
    let bot = server.client();
    let error = bot
        .recharge_wallet(404, Money::from_coins(1.0).unwrap())
        .unwrap_err();
    assert_eq!(status(&error), Some(404));

    let local = NewWalletAdjustment::recharge(1, Money::ZERO).unwrap_err();
    assert_eq!(local.kind(), ErrorKind::InvalidArgument);
}

// --- tasks ------------------------------------------------------------------

#[test]
fn the_task_catalog_exposes_kinds_progress_and_claimability() {
    let server = server();
    let list = server.client().tasks().unwrap();
    assert_eq!(list.streak, 4);
    assert_eq!(list.makeup_cost, 100);
    assert!(list.task("daily_posts_5").is_some());
    assert!(list.task("nope").is_none());

    let daily = list.daily();
    assert!(daily.iter().all(|task| task.is_daily()));
    assert!(daily.len() >= 5);

    let chat = list.task("daily_chat_100").unwrap();
    assert!(!chat.claimable, "60 of 100 is not claimable");
    assert!((chat.ratio() - 0.6).abs() < f64::EPSILON);

    let posts = list.task("daily_posts_1").unwrap();
    assert!(posts.claimable);
    assert_eq!(posts.reward_exp, 10);
    assert_eq!(posts.reward_currency, 5);

    assert!(list
        .claimable()
        .iter()
        .any(|task| task.code == "daily_posts_1"));
}

#[test]
fn claiming_a_task_twice_is_a_409() {
    let server = server();
    let bot = server.client();
    let first = bot.claim_task("daily_posts_1").unwrap();
    assert!(first.claimed);
    assert_eq!(first.exp, 10);

    let error = bot.claim_task("daily_posts_1").unwrap_err();
    assert_eq!(status(&error), Some(409));
    assert_eq!(error.api_message(), Some("task already claimed"));
}

#[test]
fn claiming_an_unearned_daily_tier_reports_the_shortfall() {
    let server = server();
    let error = server
        .client()
        .claim_daily_task("daily_chat_100")
        .unwrap_err();
    assert_eq!(status(&error), Some(409));
    assert_eq!(error.api_message(), Some("task requirements not met"));
}

#[test]
fn an_unknown_task_code_is_a_404() {
    let server = server();
    let error = server.client().claim_task("no_such_task").unwrap_err();
    assert_eq!(status(&error), Some(404));
}

#[test]
fn the_daily_login_route_is_a_hyphen_not_an_underscore() {
    let server = server();
    let bot = server.client();
    // Wrong spelling: `daily_login` is the task *code*, and posting to
    // `/tasks/daily_login/claim` matches the generic `:code` route instead of the
    // sign-in. The request lands, but on a route that claims an ordinary task —
    // here a streak task that is not yet earned, so it answers 409 rather than the
    // `granted`/`currency` document the sign-in returns.
    let error = bot.claim_task("daily_login").unwrap_err();
    assert_eq!(status(&error), Some(409));
    assert_eq!(
        server.last_request().unwrap().path,
        "/api/v1/tasks/daily_login/claim"
    );

    // Right spelling.
    let signin = bot.claim_daily_login().unwrap();
    assert!(signin.granted);
    assert_eq!(signin.currency, 20);
    assert_eq!(signin.streak, 5);
    assert_eq!(
        server.last_request().unwrap().path,
        "/api/v1/tasks/daily-login/claim"
    );

    // A second sign-in is not an error, it just grants nothing.
    let again = bot.claim_daily_login().unwrap();
    assert!(!again.granted);
}

#[test]
fn the_signin_calendar_reports_the_missed_days() {
    let server = server();
    let calendar = server.client().checkin_calendar().unwrap();
    assert_eq!(calendar.today, "2026-08-04");
    assert!(calendar.signed_today());
    assert_eq!(calendar.streak, 4);
    assert_eq!(calendar.makeup_cost, 100);

    let missed: Vec<&str> = calendar
        .missed()
        .iter()
        .map(|day| day.date.as_str())
        .collect();
    assert_eq!(missed, vec!["2026-08-03"]);
    assert!(calendar.days.iter().any(|day| day.is_today && day.signed));
    assert_eq!(calendar.days[0].weekday, "Sat");
}

#[test]
fn a_make_up_charges_coins_and_never_the_daily_currency() {
    let server = server();
    let bot = server.client();
    let result = bot.makeup_checkin().unwrap();
    assert!(result.granted);
    assert_eq!(result.cost, 100);
    assert_eq!(result.exp, 20);
    assert_eq!(result.date, None, "the gap form has no explicit date");

    let dated = bot.makeup_checkin_on("2026-08-03").unwrap();
    assert_eq!(dated.date.as_deref(), Some("2026-08-03"));
}

#[test]
fn a_make_up_date_is_shape_checked_locally() {
    let server = server();
    let bot = server.client();
    let error = bot.makeup_checkin_on("3/8/2026").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);

    // Today cannot be made up; that rule is the server's to enforce.
    let error = bot.makeup_checkin_on("2026-08-04").unwrap_err();
    assert_eq!(status(&error), Some(400));
}

#[test]
fn experience_history_decodes() {
    let server = server();
    let history = server.client().exp_history(Some(5)).unwrap();
    assert_eq!(history.entries.len(), 2);
    assert_eq!(history.entries[0].reason, "daily_bonus");
    assert_eq!(history.entries[0].amount, 20);
    assert_eq!(
        server
            .last_request()
            .unwrap()
            .query_param("limit")
            .as_deref(),
        Some("5")
    );
}

// --- membership and the payment PIN ----------------------------------------

#[test]
fn a_fresh_bot_has_no_payment_pin_so_payments_are_refused() {
    let server = server();
    let bot = server.client();
    let membership = bot.membership().unwrap();
    assert_eq!(membership.level, 0);
    assert!(!membership.active);
    assert_eq!(membership.tiers.len(), 4);

    let request = MembershipPurchaseRequest::new(1, "123456").unwrap();
    let error = bot.purchase_membership(&request).unwrap_err();
    // Not a 401: there is no PIN to be wrong. `checkPaymentPin` answers 403.
    assert_eq!(status(&error), Some(403));
    assert_eq!(error.api_message(), Some("payment pin not set"));
}

#[test]
fn setting_the_pin_unlocks_a_purchase() {
    let server = server();
    let bot = with_pin(&server);
    let request = MembershipPurchaseRequest::new(1, "123456").unwrap();
    let result = bot.purchase_membership(&request).unwrap();
    assert_eq!(result.paid, 100, "`paid` is in whole coins");
    assert_eq!(result.kind, "purchase");
    assert_eq!(result.status.level, 1);
    assert!(result.status.active);
    assert_eq!(result.status.exp_multiplier, 1.1);

    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["level"], 1);
    assert_eq!(body["pin"], "123456");
}

#[test]
fn a_wrong_pin_is_a_401_and_an_unknown_level_never_leaves_the_process() {
    let server = server();
    let bot = with_pin(&server);
    let request = MembershipPurchaseRequest::new(1, "999999").unwrap();
    let error = bot.purchase_membership(&request).unwrap_err();
    assert_eq!(status(&error), Some(401));

    assert!(MembershipPurchaseRequest::new(5, "123456").is_err());
    assert!(MembershipPurchaseRequest::new(0, "123456").is_err());
}

#[test]
fn auto_renew_needs_the_pin_only_to_enable() {
    let server = server();
    let bot = with_pin(&server);
    let enabled = bot.set_auto_renew(true, Some("123456")).unwrap();
    assert!(enabled.auto_renew);

    // Disabling needs the token alone.
    let disabled = bot.set_auto_renew(false, None).unwrap();
    assert!(!disabled.auto_renew);
    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["enabled"], false);
    assert!(body.get("pin").is_none());

    // Enabling without a PIN is refused before the network.
    let error = bot.set_auto_renew(true, None).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);
}

#[test]
fn purchase_history_and_the_admin_grant_decode() {
    let server = server();
    let bot = with_pin(&server);
    bot.purchase_membership(&MembershipPurchaseRequest::new(2, "123456").unwrap())
        .unwrap();

    let history = bot.membership_purchases(Some(1), Some(10)).unwrap();
    assert_eq!(history.purchases.len(), 1);
    assert_eq!(history.purchases[0].price_coins, 200);
    assert_eq!(history.purchases[0].kind, "purchase");

    // Level 0 clears the membership; the grant skips the wallet entirely.
    let cleared = bot.grant_membership(9, 0, None).unwrap();
    assert_eq!(cleared.level, 0);
    assert!(!cleared.active);

    let granted = bot.grant_membership(9, 3, Some(90)).unwrap();
    assert_eq!(granted.level, 3);
    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["level"], 3);
    assert_eq!(body["days"], 90);
}

#[test]
fn an_exp_adjustment_uses_delta_not_amount() {
    let server = server();
    let bot = server.client();
    let result = bot.adjust_exp(9, -50, Some("滥用")).unwrap();
    assert_eq!(result.level, 5);

    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["delta"], -50, "the field is `delta`");
    assert!(body.get("amount").is_none());
    assert_eq!(body["description"], "滥用");

    // A zero delta is accepted by the server and only documented here.
    bot.adjust_exp(9, 0, None).unwrap();
}

#[test]
fn the_pin_endpoints_round_trip() {
    let server = server();
    let bot = server.client();
    bot.set_payment_pin("123456", None).unwrap();
    assert!(bot.verify_payment_pin("123456").unwrap());

    let error = bot.verify_payment_pin("000000").unwrap_err();
    assert_eq!(status(&error), Some(401));

    // Replacing needs the current PIN.
    let error = bot.set_payment_pin("654321", None).unwrap_err();
    assert_eq!(status(&error), Some(403));
    bot.set_payment_pin("654321", Some("123456")).unwrap();
    assert!(bot.verify_payment_pin("654321").unwrap());

    bot.change_payment_pin("654321", "111111").unwrap();
    assert!(bot.verify_payment_pin("111111").unwrap());

    assert!(bot.set_payment_pin("12", None).is_err(), "6 digits exactly");
    assert!(bot.verify_payment_pin("  ").is_err());
}

// --- punishments ------------------------------------------------------------

#[test]
fn a_warning_is_recorded_and_listed() {
    let server = server();
    let bot = server.client();
    let receipt = bot.punish(7, &PunishmentRequest::warning("刷屏")).unwrap();
    assert!(receipt.punishment_id > 0);
    assert_eq!(receipt.kind, "warning");

    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["type"], "warning", "the wire field is `type`");
    assert_eq!(body["reason"], "刷屏");

    let list = bot.punishments(7, Some(10)).unwrap();
    assert_eq!(list.punishments.len(), 1);
    assert_eq!(list.punishments[0].kind, "warning");
    assert_eq!(list.punishments[0].reason, "刷屏");
    // The effective bans are keys, not records.
    assert_eq!(list.permission_bans, vec!["posts.create".to_string()]);
}

#[test]
fn each_punishment_type_sends_its_required_parameter() {
    let server = server();
    let bot = server.client();
    let receipt = bot
        .punish(7, &PunishmentRequest::temp_ban(60, "冷却"))
        .unwrap();
    assert_eq!(receipt.kind, "temp_ban");
    assert!(receipt.expires_at.is_some());
    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["duration_minutes"], 60);

    bot.punish(8, &PunishmentRequest::revoke("posts.create", "滥用"))
        .unwrap();
    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["permission_key"], "posts.create");

    bot.punish(8, &PunishmentRequest::restore("posts.create", "申诉成功"))
        .unwrap();
    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["type"], "restore");
}

#[test]
fn missing_per_type_parameters_are_refused_locally() {
    let server = server();
    let bot = server.client();
    let revoke = PunishmentRequest {
        kind: PunishmentType::Revoke,
        reason: None,
        permission_key: None,
        duration_minutes: None,
    };
    let error = bot.punish(7, &revoke).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);

    let temp_ban = PunishmentRequest {
        kind: PunishmentType::TempBan,
        reason: None,
        permission_key: None,
        duration_minutes: Some(0),
    };
    let error = bot.punish(7, &temp_ban).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);

    assert_eq!(
        PunishmentType::parse("exile").unwrap_err().kind(),
        ErrorKind::InvalidArgument
    );
}

#[test]
fn the_rank_and_self_guards_surface_as_403() {
    let server = server();
    let bot = server.client();
    let me = bot.me().unwrap().id;
    let error = bot.punish(me, &PunishmentRequest::ban("nope")).unwrap_err();
    assert_eq!(status(&error), Some(403));
    assert_eq!(error.api_message(), Some("cannot punish yourself"));

    let error = bot
        .punish(1, &PunishmentRequest::warning("nope"))
        .unwrap_err();
    assert_eq!(error.api_message(), Some("cannot punish an administrator"));

    let error = bot.punish(2, &PunishmentRequest::ban("nope")).unwrap_err();
    assert_eq!(
        error.api_message(),
        Some("insufficient rank to punish this user")
    );

    // A non-positive id never reaches the network.
    assert!(bot.punish(0, &PunishmentRequest::warning("x")).is_err());
}

// --- tips -------------------------------------------------------------------

#[test]
fn a_tip_reports_cents_while_the_request_carries_coins() {
    let server = server();
    let bot = with_pin(&server);
    let tip = bot.tip_post(555, 5, "123456").unwrap();
    assert_eq!(tip.post_id, 555);
    // 5 coins paid, 500 cents recorded, 95 % to the author.
    assert_eq!(tip.amount, 500);
    assert_eq!(tip.net_amount, 475);

    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["amount"], 5, "the request amount is whole coins");
    assert_eq!(body["pin"], "123456");
}

#[test]
fn a_tip_without_a_pin_is_forbidden() {
    let server = server();
    let error = server.client().tip_post(555, 5, "123456").unwrap_err();
    assert_eq!(status(&error), Some(403));
    assert_eq!(error.api_message(), Some("payment pin not set"));
}

#[test]
fn tipping_yourself_and_bad_amounts_are_refused() {
    let server = server();
    let bot = with_pin(&server);
    let error = bot.tip_post(777, 5, "123456").unwrap_err();
    assert_eq!(status(&error), Some(400));
    assert_eq!(error.api_message(), Some("cannot tip your own post"));

    let error = bot.tip_post(404, 5, "123456").unwrap_err();
    assert_eq!(status(&error), Some(404));

    // The ceiling and the floor are checked before the round trip.
    assert!(bot.tip_post(1, 0, "123456").is_err());
    assert!(bot.tip_post(1, 100_001, "123456").is_err());
    assert!(bot.tip_post(1, 5, "  ").is_err());
    assert!(bot.tip_post(0, 5, "123456").is_err());
}

#[test]
fn the_author_share_matches_the_platform_fee() {
    let server = server();
    let bot = with_pin(&server);
    // A one-coin tip is the smallest case, and 95 % of 100 cents is exact.
    let tip = bot.tip_post(556, 1, "123456").unwrap();
    assert_eq!(tip.amount, 100);
    assert_eq!(tip.net_amount, 95);
}

// --- E2EE -------------------------------------------------------------------

#[test]
fn the_public_key_round_trips_and_can_be_cleared() {
    let server = server();
    let bot = server.client();
    bot.set_e2ee_public_key("ed25519:AAAA").unwrap();
    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["public_key"], "ed25519:AAAA");

    // An empty key is how a client clears its published key.
    bot.set_e2ee_public_key("").unwrap();
    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["public_key"], "");
}

#[test]
fn group_key_envelopes_are_keyed_by_user_id_as_strings() {
    let server = server();
    let bot = server.client();
    let mut envelopes = E2eeEnvelopeMap::new();
    envelopes.insert(7, "AAA");
    envelopes.insert(9, "BBB");
    let result = bot.put_e2ee_keys(33, &envelopes).unwrap();
    assert_eq!(result.version, 1);

    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["envelopes"]["7"], "AAA");
    assert_eq!(body["envelopes"]["9"], "BBB");

    let keys = bot.e2ee_keys(33).unwrap();
    assert_eq!(keys.conversation_id, 33);
    assert_eq!(keys.version, 1);
    assert_eq!(keys.envelopes.len(), 2);
    assert!(
        keys.my_envelope.is_none(),
        "neither envelope targets this bot"
    );
}

#[test]
fn an_empty_envelope_batch_is_refused_locally() {
    let server = server();
    let bot = server.client();
    let error = bot.put_e2ee_keys(33, &E2eeEnvelopeMap::new()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);
    assert!(bot
        .put_e2ee_keys(0, &E2eeEnvelopeMap::from_pairs([(7, "A")]))
        .is_err());
}

#[test]
fn a_second_rotation_bumps_the_version() {
    let server = server();
    let bot = server.client();
    bot.put_e2ee_keys(33, &E2eeEnvelopeMap::from_pairs([(7, "A")]))
        .unwrap();
    let second = bot
        .put_e2ee_keys(33, &E2eeEnvelopeMap::from_pairs([(7, "B"), (9, "C")]))
        .unwrap();
    assert_eq!(second.version, 2);

    let keys = bot.e2ee_keys(33).unwrap();
    assert_eq!(keys.version, 2);
    assert_eq!(keys.envelopes.len(), 3);
}

#[test]
fn a_nonpositive_conversation_id_never_reaches_the_network() {
    let server = server();
    let bot = server.client();
    assert!(bot.e2ee_keys(0).is_err());
    assert!(bot
        .put_e2ee_keys(-1, &E2eeEnvelopeMap::from_pairs([(7, "A")]))
        .is_err());
}
