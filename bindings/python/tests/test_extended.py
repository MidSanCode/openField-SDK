"""The money, task, moderation, E2EE, tip, chunked-upload and plugin methods.

Each case runs against the Rust mock server through the sidecar, so what is being
checked is the whole path: the Python facade, the JSON method table, the Rust client
and the endpoint contract the mock reproduces from the real handlers.
"""

from __future__ import annotations

import unittest

from openfield_bot import (
    BadRequestError,
    ConflictError,
    ForbiddenError,
    InvalidArgumentError,
    NotFoundError,
    UnauthorizedError,
)

from .support import TestkitCase


class WalletTests(TestkitCase):
    """Balance, transactions and the two ways to move money."""

    def test_the_balance_is_a_decimal_coin_value(self) -> None:
        wallet = self.bot.wallet(page=1, limit=5)
        # The mock opens with 1000 coins; Cents(100000) marshals as 1000.
        self.assertEqual(wallet["balance"], 1000)
        self.assertEqual(wallet["page"], 1)
        self.assertEqual(wallet["limit"], 5)

    def test_a_recharge_reports_the_new_balance(self) -> None:
        result = self.bot.recharge_wallet(42, 12.5, description="补偿")
        self.assertEqual(result["balance"], 1012.5)
        self.assertEqual(result["transaction"]["amount"], 12.5)
        self.assertEqual(result["transaction"]["type"], "recharge")
        self.assertEqual(result["transaction"]["description"], "补偿")

    def test_deduct_sends_a_negative_amount(self) -> None:
        result = self.bot.deduct_wallet(42, 2.5)
        self.assertEqual(result["balance"], 997.5)
        self.assertEqual(result["transaction"]["amount"], -2.5)

    def test_the_raw_form_keeps_the_sign_the_caller_gave(self) -> None:
        # A positive amount with type="deduct" is a credit: the handler stores it
        # verbatim and never reconciles the two.
        result = self.bot.adjust_wallet(42, 1, type="deduct")
        self.assertEqual(result["balance"], 1001)
        self.assertEqual(result["transaction"]["type"], "deduct")

    def test_a_deduction_beyond_the_balance_is_a_bad_request(self) -> None:
        with self.assertRaises(BadRequestError) as caught:
            self.bot.deduct_wallet(42, 5000)
        self.assertEqual(caught.exception.status, 400)

    def test_an_amount_that_is_not_a_number_is_refused_locally(self) -> None:
        with self.assertRaises(InvalidArgumentError):
            self.bot.recharge_wallet(42, "10")  # type: ignore[arg-type]
        # bool is an int in Python, and never an amount.
        with self.assertRaises(InvalidArgumentError):
            self.bot.recharge_wallet(42, True)  # type: ignore[arg-type]


class TaskTests(TestkitCase):
    """The catalog, the claims and the sign-in grid."""

    def test_the_catalog_carries_progress_and_claimability(self) -> None:
        catalog = self.bot.tasks()
        self.assertEqual(catalog["streak"], 4)
        self.assertEqual(catalog["makeup_cost"], 100)
        by_code = {task["code"]: task for task in catalog["tasks"]}
        self.assertTrue(by_code["daily_posts_1"]["claimable"])
        self.assertFalse(by_code["daily_chat_100"]["claimable"])
        self.assertEqual(by_code["daily_posts_1"]["reward_currency"], 5)

    def test_claiming_twice_is_a_conflict(self) -> None:
        claimed = self.bot.claim_task("daily_posts_1")
        self.assertTrue(claimed["claimed"])
        self.assertEqual(claimed["currency"], 5)
        with self.assertRaises(ConflictError):
            self.bot.claim_task("daily_posts_1")

    def test_an_unknown_code_is_not_found(self) -> None:
        with self.assertRaises(NotFoundError):
            self.bot.claim_task("nope")

    def test_the_daily_route_and_the_task_code_are_spelled_differently(self) -> None:
        # Wrong spelling: the generic :code route answers instead.
        with self.assertRaises(ConflictError):
            self.bot.claim_task("daily_login")
        # Right spelling.
        signin = self.bot.claim_daily_login()
        self.assertTrue(signin["granted"])
        self.assertEqual(signin["currency"], 20)
        again = self.bot.claim_daily_login()
        self.assertFalse(again["granted"])

    def test_the_calendar_lists_the_missed_day(self) -> None:
        calendar = self.bot.checkin_calendar()
        self.assertEqual(calendar["today"], "2026-08-04")
        missed = [day["date"] for day in calendar["days"] if not day["signed"]]
        self.assertEqual(missed, ["2026-08-03"])

    def test_a_make_up_charges_coins(self) -> None:
        result = self.bot.makeup_checkin()
        self.assertTrue(result["granted"])
        self.assertEqual(result["cost"], 100)
        self.assertNotIn("currency", result, "a make-up never grants the daily currency")

    def test_a_make_up_date_is_shape_checked_locally(self) -> None:
        with self.assertRaises(InvalidArgumentError):
            self.bot.makeup_checkin_on("03/08/2026")
        dated = self.bot.makeup_checkin_on("2026-08-03")
        self.assertEqual(dated["date"], "2026-08-03")

    def test_experience_history(self) -> None:
        history = self.bot.exp_history(limit=5)
        self.assertEqual(len(history["entries"]), 2)
        self.assertEqual(history["entries"][0]["reason"], "daily_bonus")


class MembershipTests(TestkitCase):
    """The catalog, the payment PIN gate and the admin calls."""

    def test_a_fresh_bot_has_no_pin_so_payments_are_forbidden(self) -> None:
        membership = self.bot.membership()
        self.assertEqual(membership["level"], 0)
        self.assertEqual(len(membership["tiers"]), 4)
        with self.assertRaises(ForbiddenError):
            self.bot.purchase_membership(1, "123456")

    def test_setting_the_pin_unlocks_a_purchase(self) -> None:
        self.assertTrue(self.bot.set_payment_pin("123456")["has_pin"])
        result = self.bot.purchase_membership(1, "123456")
        self.assertEqual(result["paid"], 100)
        self.assertEqual(result["status"]["level"], 1)

    def test_the_pin_must_be_six_digits(self) -> None:
        with self.assertRaises(InvalidArgumentError):
            self.bot.set_payment_pin("12")
        with self.assertRaises(InvalidArgumentError):
            self.bot.set_payment_pin("abcdef")

    def test_a_wrong_pin_is_unauthorized(self) -> None:
        self.bot.set_payment_pin("123456")
        with self.assertRaises(UnauthorizedError):
            self.bot.purchase_membership(1, "999999")
        with self.assertRaises(UnauthorizedError):
            self.bot.verify_payment_pin("000000")
        self.assertTrue(self.bot.verify_payment_pin("123456"))

    def test_replacing_the_pin_needs_the_current_one(self) -> None:
        self.bot.set_payment_pin("123456")
        with self.assertRaises(ForbiddenError):
            self.bot.set_payment_pin("654321")
        self.bot.set_payment_pin("654321", old_pin="123456")
        self.assertTrue(self.bot.verify_payment_pin("654321"))
        self.bot.change_payment_pin("654321", "111111")
        self.assertTrue(self.bot.verify_payment_pin("111111"))

    def test_auto_renew_needs_the_pin_to_enable(self) -> None:
        self.bot.set_payment_pin("123456")
        with self.assertRaises(InvalidArgumentError):
            self.bot.set_auto_renew(True)
        self.assertTrue(self.bot.set_auto_renew(True, pin="123456")["auto_renew"])
        # Disabling needs only the token.
        self.assertFalse(self.bot.set_auto_renew(False)["auto_renew"])

    def test_purchase_history_and_the_admin_grant(self) -> None:
        self.bot.set_payment_pin("123456")
        self.bot.purchase_membership(2, "123456")
        history = self.bot.membership_purchases()
        self.assertEqual(history["purchases"][0]["price_coins"], 200)
        cleared = self.bot.grant_membership(9, 0)
        self.assertEqual(cleared["level"], 0)
        granted = self.bot.grant_membership(9, 3, days=30)
        self.assertEqual(granted["level"], 3)

    def test_bad_levels_never_leave_the_process(self) -> None:
        with self.assertRaises(InvalidArgumentError):
            self.bot.purchase_membership(5, "123456")
        with self.assertRaises(InvalidArgumentError):
            self.bot.purchase_membership(1, "")

    def test_an_exp_adjustment_uses_delta(self) -> None:
        result = self.bot.adjust_exp(9, -25, description="回滚")
        self.assertEqual(result["level"], 5)


class PunishmentTests(TestkitCase):
    """Moderation records, their per-type parameters and the rank guards."""

    def test_a_warning_is_recorded_and_listed(self) -> None:
        receipt = self.bot.punish(7, "warning", reason="刷屏")
        self.assertGreater(receipt["punishment_id"], 0)
        self.assertEqual(receipt["type"], "warning")
        history = self.bot.punishments(7, limit=10)
        self.assertEqual(len(history["punishments"]), 1)
        self.assertEqual(history["punishments"][0]["reason"], "刷屏")
        self.assertEqual(history["permission_bans"], ["posts.create"])

    def test_each_type_sends_what_it_requires(self) -> None:
        ban = self.bot.punish(7, "temp_ban", reason="冷却", duration_minutes=60)
        self.assertIsNotNone(ban["expires_at"])
        revoked = self.bot.punish(8, "revoke", reason="滥用", permission_key="posts.create")
        self.assertEqual(revoked["type"], "revoke")
        restored = self.bot.punish(8, "restore", permission_key="posts.create")
        self.assertEqual(restored["type"], "restore")

    def test_missing_parameters_and_unknown_types_are_local_errors(self) -> None:
        with self.assertRaises(InvalidArgumentError):
            self.bot.punish(7, "revoke")
        with self.assertRaises(InvalidArgumentError):
            self.bot.punish(7, "temp_ban", reason="x", duration_minutes=0)
        with self.assertRaises(InvalidArgumentError):
            self.bot.punish(7, "exile")

    def test_the_rank_and_self_guards(self) -> None:
        with self.assertRaises(ForbiddenError) as caught:
            self.bot.punish(self.bot.id or 4242, "ban", reason="nope")
        self.assertEqual(caught.exception.status, 403)
        with self.assertRaises(ForbiddenError):
            self.bot.punish(1, "warning", reason="admin")
        with self.assertRaises(ForbiddenError):
            self.bot.punish(2, "ban", reason="peer")


class TipTests(TestkitCase):
    """Tipping: whole coins in, cents out, and the PIN gate."""

    def test_a_tip_reports_cents(self) -> None:
        self.bot.set_payment_pin("123456")
        tip = self.bot.tip_post(555, 5, "123456")
        self.assertEqual(tip["amount"], 500)
        self.assertEqual(tip["net_amount"], 475)

    def test_a_tip_without_a_pin_is_forbidden(self) -> None:
        with self.assertRaises(ForbiddenError):
            self.bot.tip_post(555, 5, "123456")

    def test_tipping_yourself_is_a_bad_request(self) -> None:
        self.bot.set_payment_pin("123456")
        with self.assertRaises(BadRequestError):
            self.bot.tip_post(777, 5, "123456")

    def test_amounts_are_whole_coins(self) -> None:
        self.bot.set_payment_pin("123456")
        with self.assertRaises(InvalidArgumentError):
            self.bot.tip_post(555, 0, "123456")
        with self.assertRaises(InvalidArgumentError):
            self.bot.tip_post(555, 100001, "123456")
        # Go refuses a float for an int64, so a fractional coin count is refused here.
        with self.assertRaises(InvalidArgumentError):
            self.bot.tip_post(555, 1.5, "123456")  # type: ignore[arg-type]


class E2eeTests(TestkitCase):
    """Key distribution: the public key and the per-member envelope map."""

    def test_the_envelopes_are_keyed_by_user_id(self) -> None:
        result = self.bot.put_e2ee_keys(33, {7: "AAA", 9: "BBB"})
        self.assertEqual(result["version"], 1)
        keys = self.bot.e2ee_keys(33)
        self.assertEqual(keys["version"], 1)
        self.assertEqual(len(keys["envelopes"]), 2)
        targets = sorted(envelope["target_user_id"] for envelope in keys["envelopes"])
        self.assertEqual(targets, [7, 9])

    def test_a_rotation_bumps_the_version(self) -> None:
        self.bot.put_e2ee_keys(33, {7: "AAA"})
        self.assertEqual(self.bot.put_e2ee_keys(33, {7: "BBB", 9: "CCC"})["version"], 2)

    def test_an_empty_batch_is_refused_locally(self) -> None:
        with self.assertRaises(InvalidArgumentError):
            self.bot.put_e2ee_keys(33, {})

    def test_the_public_key_can_be_set_and_cleared(self) -> None:
        self.bot.set_e2ee_public_key("ed25519:AAAA")
        self.bot.set_e2ee_public_key("")


class ChunkedUploadTests(TestkitCase):
    """The resumable path and the parts that make it worth having."""

    def test_the_whole_flow(self) -> None:
        session = self.bot.start_chunked_upload("big.bin", 200, 2)
        self.assertEqual(session["total_chunks"], 2)
        upload_id = session["upload_id"]

        self.assertEqual(self.bot.chunk_status(upload_id)["uploaded"], [])
        self.bot.upload_chunk(upload_id, 1, b"\x07" * 100)
        self.assertEqual(self.bot.chunk_status(upload_id)["uploaded"], [1])
        self.bot.upload_chunk(upload_id, 2, b"\x08" * 100)

        attachment = self.bot.complete_chunked_upload(upload_id, "big.bin", 200, 2)
        self.assertEqual(attachment["original_name"], "big.bin")
        self.assertEqual(attachment["size_bytes"], 200)

    def test_a_gap_is_a_conflict(self) -> None:
        session = self.bot.start_chunked_upload("big.bin", 300, 3)
        self.bot.upload_chunk(session["upload_id"], 1, b"\x01" * 100)
        with self.assertRaises(ConflictError):
            self.bot.complete_chunked_upload(session["upload_id"], "big.bin", 300, 3)

    def test_the_driver_splits_the_payload(self) -> None:
        attachment = self.bot.upload_chunked("report.bin", b"\x09" * 250, chunk_bytes=100)
        self.assertEqual(attachment["original_name"], "report.bin")
        self.assertEqual(attachment["size_bytes"], 250)

    def test_the_driver_resumes_only_the_missing_parts(self) -> None:
        session = self.bot.start_chunked_upload("resume.bin", 200, 2)
        upload_id = session["upload_id"]
        self.bot.upload_chunk(upload_id, 1, b"\x01" * 100)
        self.clear_requests()
        attachment = self.bot.upload_chunked(
            "resume.bin", b"\x01" * 100 + b"\x02" * 100, chunk_bytes=100, upload_id=upload_id
        )
        self.assertEqual(attachment["original_name"], "resume.bin")
        parts = [
            request
            for request in self.requests()
            if request["path"].startswith("/api/v1/attachments/chunk/")
            and request["path"].endswith("/2")
        ]
        self.assertEqual(len(parts), 1, "only part 2 should have been re-sent")

    def test_declarations_are_checked_locally(self) -> None:
        with self.assertRaises(InvalidArgumentError):
            self.bot.start_chunked_upload("a.bin", 0, 1)
        with self.assertRaises(InvalidArgumentError):
            self.bot.start_chunked_upload("a.bin", 100, self.bot.MAX_CHUNKS + 1)
        with self.assertRaises(InvalidArgumentError):
            self.bot.start_chunked_upload("a.bin", self.bot.CHUNK_BYTES + 1, 1)
        with self.assertRaises(InvalidArgumentError):
            self.bot.upload_chunked("a.bin", b"")

    def test_chunk_indexes_and_sizes_are_checked_locally(self) -> None:
        session = self.bot.start_chunked_upload("a.bin", 10, 1)
        with self.assertRaises(InvalidArgumentError):
            self.bot.upload_chunk(session["upload_id"], 0, b"\x00")
        with self.assertRaises(InvalidArgumentError):
            self.bot.upload_chunk(session["upload_id"], 1, b"\x00" * (self.bot.CHUNK_BYTES + 1))

    def test_an_unknown_session_is_not_found(self) -> None:
        with self.assertRaises(NotFoundError):
            self.bot.chunk_status("chunk-does-not-exist")


class ReuseAndBurnTests(TestkitCase):
    """Content-hash reuse and the burn-after-view countdown."""

    def test_a_known_hash_returns_the_existing_attachment(self) -> None:
        reused = self.bot.attachment_by_hash("A" * 64)
        self.assertEqual(reused["id"], 9001)
        self.assertEqual(reused["original_name"], "chart.png")

    def test_an_unknown_hash_is_not_found(self) -> None:
        with self.assertRaises(NotFoundError):
            self.bot.attachment_by_hash("b" * 64)

    def test_a_malformed_hash_is_a_local_error(self) -> None:
        for digest in ("nope", "z" * 64, "a" * 63):
            with self.assertRaises(InvalidArgumentError):
                self.bot.attachment_by_hash(digest)

    def test_arming_a_burn(self) -> None:
        armed = self.bot.arm_attachment_burn(9001, 300)
        self.assertEqual(armed["id"], 9001)
        self.assertIsNotNone(armed["burn_at"])

    def test_the_burn_window_is_capped(self) -> None:
        for seconds in (0, -1, 86_401):
            with self.assertRaises(InvalidArgumentError):
                self.bot.arm_attachment_burn(9001, seconds)


class PluginTests(TestkitCase):
    """The store: catalog, bundle download and admin management."""

    def test_the_catalog_and_one_plugin(self) -> None:
        catalog = self.bot.plugins()
        self.assertEqual(len(catalog["plugins"]), 1)
        self.assertEqual(catalog["plugins"][0]["id"], "demo")
        plugin = self.bot.plugin("demo")
        self.assertEqual(plugin["name"], "Demo")
        self.assertTrue(plugin["verified"])

    def test_an_unknown_or_malformed_id(self) -> None:
        with self.assertRaises(NotFoundError):
            self.bot.plugin("missing")
        for plugin_id in ("..", ".", "AB", "a/b", ""):
            with self.assertRaises(InvalidArgumentError):
                self.bot.plugin(plugin_id)

    def test_a_bundle_download_returns_bytes(self) -> None:
        bundle = self.bot.download_plugin("demo")
        self.assertEqual(bundle, b"PK\x03\x04demo-bundle")

    def test_a_bundle_can_be_written_to_disk(self) -> None:
        import tempfile
        from pathlib import Path

        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "bundle.zip"
            written = self.bot.download_plugin_to("demo", target)
            self.assertEqual(written, len(target.read_bytes()))
            self.assertEqual(target.read_bytes(), b"PK\x03\x04demo-bundle")

    def test_uploading_publishing_and_deleting(self) -> None:
        uploaded = self.bot.upload_plugin("bundle.zip", b"\x01\x02\x03", publish=True)
        self.assertEqual(uploaded["message"], "uploaded")
        self.assertTrue(uploaded["plugin"]["published"])

        self.bot.unpublish_plugin("demo")
        self.assertEqual(self.bot.plugins()["plugins"], [])
        with self.assertRaises(NotFoundError):
            self.bot.plugin("demo")

        self.bot.publish_plugin("demo")
        self.assertEqual(len(self.bot.plugins()["plugins"]), 1)
        self.bot.delete_plugin("demo")
        with self.assertRaises(NotFoundError):
            self.bot.delete_plugin("demo")

    def test_an_oversize_bundle_is_refused_locally(self) -> None:
        with self.assertRaises(InvalidArgumentError):
            self.bot.upload_plugin("bundle.zip", b"\x00" * (5 * 1024 * 1024 + 1))


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
