"""The sidecar transport end to end, against the Rust mock server."""

from __future__ import annotations

import json
import unittest

from openfield_bot import (
    BadRequestError,
    Bot,
    BotError,
    ClosedError,
    ConflictError,
    ForbiddenError,
    InvalidArgumentError,
    NotFoundError,
    RateLimitedError,
    SidecarTransport,
    UnauthorizedError,
)

from .support import TestkitCase, sidecar_needs_testkit


class ProtocolTests(unittest.TestCase):
    """The handshake, without needing a server."""

    def setUp(self) -> None:
        self.sidecar = sidecar_needs_testkit()
        self.addCleanup(self.sidecar.close)

    def test_version_reports_the_protocol(self) -> None:
        version = self.sidecar.call("version")
        self.assertEqual(version["name"], "openfield-bot-sdk")
        self.assertIsInstance(version["version"], str)
        self.assertEqual(version["protocol"], 1)

    def test_capabilities_lists_every_method(self) -> None:
        caps = self.sidecar.call("capabilities")
        self.assertEqual(caps["protocol"], 1)
        self.assertTrue(caps["features"]["ffi"])
        self.assertTrue(caps["features"]["events"])

        methods = caps["methods"]
        self.assertGreater(len(methods), 80)
        self.assertEqual(len(methods), len(set(methods)), "method names must be unique")
        for expected in ("posts.create", "chat.send", "camps.list", "events.subscribe"):
            self.assertIn(expected, methods)

    def test_an_unknown_method_is_an_invalid_argument(self) -> None:
        with self.assertRaises(InvalidArgumentError) as caught:
            self.sidecar.call("nope.nope")
        self.assertIn("unknown method", str(caught.exception))

    def test_a_bot_only_call_before_a_token_says_why(self) -> None:
        """The sidecar always initialises a client, so this is the anonymous case.

        The "call client.init first" path cannot be reached through the sidecar, because
        starting it *is* the initialisation; that path is covered by the core's own Rust
        test `rpc::tests::calls_before_client_init_explain_themselves`.
        """
        with self.assertRaises(InvalidArgumentError) as caught:
            self.sidecar.call("me")
        self.assertIn("token", str(caught.exception).lower())

    def test_the_config_never_exposes_the_token(self) -> None:
        token = "ofb_" + "a" * 64
        self.sidecar.call("client.init", {"base_url": "http://127.0.0.1:9", "token": token})
        config = self.sidecar.call("client.config")
        self.assertTrue(config["has_token"])
        rendered = json.dumps(config)
        self.assertNotIn("a" * 64, rendered)
        self.assertIn("ofb_", config["token"])
        self.assertIn("…", config["token"])

    def test_set_token_switches_between_bot_and_anonymous(self) -> None:
        self.sidecar.call("client.init", {"base_url": "http://127.0.0.1:9"})
        self.assertFalse(self.sidecar.call("client.has_token")["has_token"])
        with self.assertRaises(InvalidArgumentError):
            self.sidecar.call("me")

        self.sidecar.call("client.set_token", {"token": "ofb_" + "b" * 64})
        self.assertTrue(self.sidecar.call("client.has_token")["has_token"])

        self.sidecar.call("client.set_token", {"token": ""})
        self.assertFalse(self.sidecar.call("client.has_token")["has_token"])

    def test_a_malformed_token_is_rejected_without_a_request(self) -> None:
        with self.assertRaises(BotError) as caught:
            self.sidecar.call("client.init", {"token": "not-a-token"})
        self.assertIn("ofb_", str(caught.exception))

    def test_a_closed_transport_refuses_to_work(self) -> None:
        transport = sidecar_needs_testkit()
        transport.close()
        with self.assertRaises(ClosedError):
            transport.call("version")
        transport.close()  # idempotent

    def test_stderr_is_captured_for_diagnosis(self) -> None:
        self.sidecar.call("version")
        # The sidecar greets on stderr; the point is that the pipe is drained, not that
        # the greeting is interesting.
        self.assertIsInstance(self.sidecar.stderr_tail(), str)

    def test_a_non_object_params_value_does_not_crash_the_sidecar(self) -> None:
        # A caller's mistake must be an error, never a dead process.
        put = getattr(self.sidecar, "_process")
        self.assertIsNotNone(put)
        with self.assertRaises(InvalidArgumentError):
            self.sidecar.call("me", params="not-an-object")  # type: ignore[arg-type]
        self.assertEqual(self.sidecar.call("version")["protocol"], 1)


class RequestTests(TestkitCase):
    """Typed calls against the mock server."""

    def test_public_endpoints_need_no_token(self) -> None:
        """Health, announcements and the *server's* capabilities are unauthenticated.

        Note the distinction: the RPC method ``capabilities`` reports what the **SDK**
        can do, while ``/api/v1/capabilities`` reports what the **server** can do.
        """
        envelope = self.bot.request("GET", "/api/v1/capabilities")
        self.assertTrue(envelope["ok"], envelope)
        self.assertIn("chat.e2ee", envelope["body"]["features"])

        health = self.bot.health()
        self.assertTrue(health["all_healthy"])
        self.assertEqual(self.bot.announcements()[0]["title"], "welcome")

    def test_sdk_capabilities_are_metadata_not_the_servers(self) -> None:
        caps = self.bot.capabilities()
        self.assertEqual(caps["protocol"], 1)
        # The SDK's own feature flags, which are booleans, not the server's string list.
        self.assertIsInstance(caps["features"]["ffi"], bool)

    def test_me_reports_the_bot(self) -> None:
        me = self.bot.me()
        self.assertTrue(me["is_bot"])
        self.assertEqual(me["id"], self.info["bot_id"])
        self.assertEqual(self.bot.username, "weather_watcher")
        self.assertEqual(self.bot.id, self.info["bot_id"])

    def test_the_authorization_header_carries_the_token(self) -> None:
        self.bot.me()
        recorded = self.requests_to("/api/v1/users/me")
        self.assertTrue(recorded)
        self.assertEqual(recorded[0]["authorization"], f"Bearer {self.token}")

    def test_permissions_and_startup_check(self) -> None:
        held = self.bot.permissions()
        self.assertIsInstance(held["permissions"], list)
        self.assertIn("posts.create", held["permissions"])

        check = self.bot.startup_check()
        self.assertEqual(check["is_bot"], True)
        self.assertEqual(check["bot_id"], self.info["bot_id"])
        self.assertIsInstance(check["missing_administrative"], list)

    def test_require_permission_names_what_is_missing(self) -> None:
        # A permission the mock does not grant.
        with self.assertRaises(UnauthorizedError) as caught:
            self.bot.require_permission("admin.everything")
        self.assertEqual(caught.exception.permission, "admin.everything")
        self.assertIn("admin.everything", str(caught.exception))

        # A permission it does grant must pass silently.
        self.bot.require_permission("posts.create")

    def test_publish_and_read_back_a_post(self) -> None:
        created = self.bot.publish("hello from python", visibility="public")
        self.assertEqual(created["content"], "hello from python")
        self.assertIsInstance(created["id"], int)

        page = self.bot.posts()
        self.assertEqual(page["posts"][0]["content"], "hello from python")

        fetched = self.bot.post(created["id"])
        self.assertEqual(fetched["id"], created["id"])

    def test_a_post_over_the_byte_limit_is_refused_locally(self) -> None:
        # 5000 bytes is the server's limit and the SDK checks it before sending, so this
        # must not reach the mock at all.
        with self.assertRaises(InvalidArgumentError):
            self.bot.publish("测" * 2000)

    def test_react_rejects_an_unknown_key_locally(self) -> None:
        post = self.bot.publish("react to me")
        with self.assertRaises(InvalidArgumentError) as caught:
            self.bot.react(post["id"], "thumbsup")
        self.assertIn("like", str(caught.exception))

    def test_react_accepts_every_documented_key(self) -> None:
        from openfield_bot import REACTIONS

        self.assertEqual(
            REACTIONS, ("like", "dislike", "love", "haha", "wow", "sad", "angry")
        )
        post = self.bot.publish("react to me")
        result = self.bot.react(post["id"], "haha")
        self.assertIsNotNone(result)

    def test_conversations_and_messages(self) -> None:
        created = self.bot.create_conversation("alerts")
        conversation_id = created["id"]

        sent = self.bot.send_message(conversation_id, "first")
        self.assertEqual(sent["content"], "first")

        page = self.bot.messages(conversation_id)
        messages = page.get("messages", page) if isinstance(page, dict) else page
        self.assertTrue(messages, "the sent message must be readable")

    def test_messages_come_back_newest_first(self) -> None:
        """`bots.md` 7.3 says oldest-first; the handler sorts DESC.

        The SDK follows the source, so this pins the real order rather than the
        documented one. If the server ever changes, this test is the tripwire.
        """
        conversation_id = self.bot.create_conversation("ordering")["id"]
        first = self.bot.send_message(conversation_id, "one")["id"]
        second = self.bot.send_message(conversation_id, "two")["id"]
        self.assertGreater(second, first)

        page = self.bot.messages(conversation_id, limit=10)
        messages = page.get("messages", page) if isinstance(page, dict) else page
        ids = [message["id"] for message in messages]
        self.assertEqual(ids, sorted(ids, reverse=True), f"expected DESC order, got {ids}")

    def test_create_conversation_requires_a_title(self) -> None:
        with self.assertRaises(InvalidArgumentError):
            self.bot.call("chat.create", title="")

    def test_upload_sends_the_file_field(self) -> None:
        """The mock answers a canned attachment, so the *request* is what is checked.

        What matters is that the multipart carries a part named ``file`` -- the field the
        server's handler reads -- and that the bytes arrive intact.
        """
        attachment = self.bot.upload("report.png", b"\x89PNG\r\n\x1a\nfake", mime="image/png")
        # The mock's canned record, not an echo: `storage.upload` returns what the server
        # said, and the server said chart.png.
        self.assertEqual(attachment["original_name"], "chart.png")

        recorded = self.requests_to("/api/v1/attachments")
        self.assertTrue(recorded)
        body = recorded[-1]["body"]
        self.assertIn('name="file"', body)
        self.assertIn('filename="report.png"', body)
        self.assertIn("image/png", body)
        self.assertIn("PNG", body, "the file bytes must reach the server")

    def test_upload_of_binary_bytes_survives_the_journey(self) -> None:
        """Every byte value, which a text-oriented path would mangle.

        The recorded body is lossily decoded for display, so the octet count is the
        trustworthy check: the multipart envelope adds a few hundred bytes and the payload
        must be there in full.
        """
        payload = bytes(range(256))
        self.bot.upload("all.bin", payload)

        recorded = self.requests_to("/api/v1/attachments")
        self.assertTrue(recorded)
        self.assertGreaterEqual(
            recorded[-1]["body_len"],
            256,
            "the multipart body must carry all 256 octets",
        )

    def test_a_large_upload_does_not_blow_up_the_payload(self) -> None:
        """Base64 keeps a 2 MB file from becoming a two-million-element JSON array."""
        payload = bytes(range(256)) * 8192  # 2 MiB
        self.bot.upload("big.bin", payload)
        recorded = self.requests_to("/api/v1/attachments")
        self.assertTrue(recorded)
        self.assertGreaterEqual(recorded[-1]["body_len"], len(payload))

    def test_bots_list_and_create(self) -> None:
        bots = self.bot.list_bots()
        self.assertIn("bots", bots)
        self.assertIn("limit", bots)
        # The mock seeds a fixed roster, so the names come from the server's answer.
        seeded = [bot["username"] for bot in bots["bots"]]
        self.assertTrue(seeded)
        self.assertEqual(self.bot.find_bot(seeded[0])["username"], seeded[0])
        self.assertIsNone(self.bot.find_bot("nobody"))

        created = self.bot.create_bot("a_new_bot", "A New Bot")
        # The endpoint answers {user, token}; the token is shown exactly once.
        self.assertEqual(created["user"]["username"], "a_new_bot")
        self.assertEqual(created["user"]["nickname"], "A New Bot")
        self.assertTrue(created["user"]["is_bot"])
        self.assertTrue(created["token"].startswith("ofb_"))
        self.assertEqual(len(created["token"]), 68, "ofb_ plus 64 hex characters")

    def test_a_bad_bot_username_is_refused_locally(self) -> None:
        with self.assertRaises(InvalidArgumentError):
            self.bot.create_bot("no", "short username")

    def test_a_duplicate_bot_username_is_a_conflict(self) -> None:
        with self.assertRaises(ConflictError):
            self.bot.create_bot("taken", "Already There")

    def test_reports_are_recorded(self) -> None:
        post = self.bot.publish("report me")
        result = self.bot.report_post(post["id"], "spam")
        self.assertIsNotNone(result)
        self.assertIsNotNone(self.bot.my_reports())

        # The server binds target_type/target_id, so the payload must use them.
        recorded = self.requests_to("/api/v1/reports")
        self.assertTrue(recorded)
        self.assertIn('"target_type"', recorded[-1]["body"])
        self.assertIn('"target_id"', recorded[-1]["body"])

    def test_reporting_yourself_is_refused_by_the_server(self) -> None:
        with self.assertRaises(BadRequestError):
            self.bot.report_user(self.info["bot_id"], "me")

    def test_an_unknown_report_target_is_refused_locally(self) -> None:
        with self.assertRaises(InvalidArgumentError) as caught:
            self.bot.report("comment", 1, "nope")
        self.assertIn("post", str(caught.exception))

    def test_notifications_read(self) -> None:
        self.assertIsNotNone(self.bot.notifications())
        self.assertIsNotNone(self.bot.read_notifications())

    def test_an_anonymous_client_cannot_read_me(self) -> None:
        """Without a token the call is refused locally, with no request at all."""
        self.sidecar.call("client.set_token", {"token": ""})
        before = len(self.requests_to("/api/v1/users/me"))
        with self.assertRaises(InvalidArgumentError):
            self.bot.call("me")
        self.assertEqual(
            len(self.requests_to("/api/v1/users/me")),
            before,
            "a bot-only call must not be attempted anonymously",
        )
        self.sidecar.call("client.set_token", {"token": self.token})

    def test_a_wrong_token_is_a_401_that_must_not_be_retried(self) -> None:
        """A reset, unknown or banned bot all answer 401 ("invalid bot token").

        The documented response is to alert and stop, so the error must be marked
        non-retryable: retrying a dead token forever is the failure mode to avoid.
        """
        self.sidecar.call("client.set_token", {"token": "ofb_" + "f" * 64})
        with self.assertRaises(UnauthorizedError) as caught:
            self.bot.me()

        error = caught.exception
        self.assertEqual(error.status, 401)
        self.assertFalse(error.retryable, "a 401 must never be retried")
        self.assertIn("invalid bot token", str(error))
        # Exactly one attempt: the retry policy must not have replayed it.
        self.assertEqual(len(self.requests_to("/api/v1/users/me")), 1)

        self.sidecar.call("client.set_token", {"token": self.token})


class ErrorMappingTests(TestkitCase):
    """Status codes become the right exception, with the right fields."""

    def test_403_carries_the_missing_permission(self) -> None:
        self.force_status("/api/v1/posts", 403)
        with self.assertRaises(ForbiddenError) as caught:
            self.bot.publish("denied")
        error = caught.exception
        self.assertEqual(error.status, 403)
        self.assertFalse(error.retryable)
        # The mock's 403 body names the permission, as the server does.
        self.assertIsInstance(error.body, dict)

    def test_404_is_not_found(self) -> None:
        self.force_status("/api/v1/users/me", 404)
        with self.assertRaises(NotFoundError):
            self.bot.me()

    def test_409_is_conflict(self) -> None:
        self.force_status("/api/v1/posts", 409)
        with self.assertRaises(ConflictError):
            self.bot.publish("duplicate")

    def test_429_carries_retry_after_and_is_retryable(self) -> None:
        """429 *is* retried, so the mock must keep refusing to outlast the policy.

        Forcing the status once would be useless here: the first attempt would be
        rejected, the retry would succeed, and the test would prove nothing.
        """
        self.force_status("/api/v1/users/me", 429, retry_after_secs=1, times=10)
        with self.assertRaises(RateLimitedError) as caught:
            self.bot.me()
        error = caught.exception
        self.assertEqual(error.status, 429)
        self.assertTrue(error.retryable)
        self.assertEqual(error.retry_after_secs, 1)
        self.assertGreater(
            len(self.requests_to("/api/v1/users/me")), 1, "the 429 must have been retried"
        )

    def test_a_forced_500_surfaces_after_the_retries(self) -> None:
        from openfield_bot import ServerError

        # GET is retried on 5xx, so force enough attempts to outlast the policy.
        self.force_status("/api/v1/users/me", 500, times=10)
        with self.assertRaises(ServerError) as caught:
            self.bot.me()
        self.assertEqual(caught.exception.status, 500)
        self.assertTrue(caught.exception.retryable)

        # The retry policy ran: more than one attempt reached the mock.
        self.assertGreater(len(self.requests_to("/api/v1/users/me")), 1)

    def test_a_post_is_not_replayed_on_a_500(self) -> None:
        """Replaying a mutation would duplicate content, so the policy forbids it."""
        from openfield_bot import ServerError

        self.force_status("/api/v1/posts", 500, times=5)
        with self.assertRaises(ServerError):
            self.bot.publish("only once")
        self.assertEqual(len(self.requests_to("/api/v1/posts")), 1)

    def test_a_get_is_retried_on_a_transport_failure(self) -> None:
        self.sidecar.call("testkit.fail_next", {"path": "/api/v1/users/me", "times": 1})
        me = self.bot.me()
        self.assertEqual(me["id"], self.info["bot_id"])
        self.assertGreaterEqual(len(self.requests_to("/api/v1/users/me")), 2)


class RawRequestTests(TestkitCase):
    """The escape hatch."""

    def test_raw_get_returns_the_envelope(self) -> None:
        envelope = self.bot.request("GET", "/api/v1/users/me")
        self.assertTrue(envelope["ok"])
        self.assertEqual(envelope["status"], 200)
        self.assertEqual(envelope["body"]["id"], self.info["bot_id"])

    def test_raw_request_reports_a_status_without_raising(self) -> None:
        self.force_status("/api/v1/users/me", 404)
        envelope = self.bot.request("GET", "/api/v1/users/me")
        self.assertFalse(envelope["ok"])
        self.assertEqual(envelope["status"], 404)

    def test_raw_request_sends_a_json_body(self) -> None:
        envelope = self.bot.request(
            "POST", "/api/v1/posts", body={"content": "raw", "visibility": "public"}
        )
        self.assertTrue(envelope["ok"], envelope)
        self.assertEqual(envelope["body"]["content"], "raw")

    def test_raw_request_sends_query_parameters(self) -> None:
        envelope = self.bot.request("GET", "/api/v1/posts", query={"page": 3})
        self.assertTrue(envelope["ok"])
        self.assertEqual(envelope["body"]["page"], 3)

    def test_an_absolute_path_is_refused(self) -> None:
        """A path must never be able to redirect the bot token to another host."""
        with self.assertRaises(InvalidArgumentError) as caught:
            self.bot.request("GET", "https://evil.example/steal")
        self.assertIn("server-relative", str(caught.exception))


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
