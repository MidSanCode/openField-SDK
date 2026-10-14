"""The FFI transport, against the Rust mock server.

This suite exists to prove the ``ctypes`` boundary is correct -- that pointers are not
truncated, that owned strings are freed, and that the FFI answers exactly what the
sidecar answers for the same call. It is skipped when the shared library has not been
built, since a missing ``cdylib`` is a build step, not a bug.
"""

from __future__ import annotations

import unittest

from openfield_bot import (
    Bot,
    BotError,
    ForbiddenError,
    InvalidArgumentError,
    RateLimitedError,
    SidecarTransport,
    UnauthorizedError,
    find_library,
)
from openfield_bot import FfiTransport

from .support import SIDECAR_FEATURE_HINT


def library_available() -> bool:
    """Whether the shared library can be found."""
    try:
        find_library()
    except BotError:
        return False
    return True


@unittest.skipUnless(library_available(), "build the shared library: cargo build -p openfield-bot-ffi")
class FfiTransportTests(unittest.TestCase):
    """Protocol-level behaviour of the C ABI."""

    def setUp(self) -> None:
        self.transport = FfiTransport(options={"base_url": "http://127.0.0.1:9"})
        self.addCleanup(self.transport.close)

    def test_the_library_reports_its_version(self) -> None:
        self.assertRegex(self.transport.version, r"^\d+\.\d+\.\d+")
        self.assertEqual(self.transport.protocol_version, "1")

    def test_capabilities_match_the_sidecar_index(self) -> None:
        caps = self.transport.call("capabilities")
        self.assertEqual(caps["protocol"], 1)
        self.assertGreater(len(caps["methods"]), 80)
        self.assertIn("posts.create", caps["methods"])

    def test_the_config_redacts_the_token(self) -> None:
        self.transport.set_token("ofb_" + "c" * 64)
        self.assertTrue(self.transport.has_token())
        config = self.transport.config()
        self.assertEqual(config["has_token"], True)
        self.assertNotIn("c" * 64, str(config))

    def test_an_unknown_method_is_an_invalid_argument(self) -> None:
        with self.assertRaises(InvalidArgumentError):
            self.transport.call("nope.nope")

    def test_a_malformed_token_is_rejected(self) -> None:
        with self.assertRaises(BotError) as caught:
            self.transport.set_token("nope")
        self.assertIn("ofb_", str(caught.exception))

    def test_a_network_failure_is_reported(self) -> None:
        from openfield_bot import NetworkError

        with self.assertRaises(NetworkError) as caught:
            self.transport.call("health")
        self.assertTrue(caught.exception.retryable)

    def test_a_closed_transport_refuses_to_work(self) -> None:
        from openfield_bot import ClosedError

        self.transport.close()
        with self.assertRaises(ClosedError):
            self.transport.call("version")
        self.transport.close()  # idempotent

    def test_events_need_a_token(self) -> None:
        with self.assertRaises(BotError):
            next(self.transport.events())

    def test_raw_request_reports_a_failure_without_raising(self) -> None:
        """The escape hatch never raises: a failure comes back in the envelope.

        This transport points at a closed port, so the failure is a network one and the
        envelope carries an error object rather than a status. The mock-backed case
        below covers a real status code.
        """
        envelope = self.transport.raw_request(
            {"method": "GET", "path": "/api/v1/capabilities"}
        )
        self.assertFalse(envelope["ok"])
        self.assertEqual(envelope["error"]["kind"], "network")
        self.assertTrue(envelope["error"]["retryable"])

    def test_many_calls_do_not_leak_handles(self) -> None:
        """Every call returns an owned string; leaking one would grow the process.

        The assertion is deliberately loose -- it catches a gross leak (a per-call
        allocation that is never freed) without depending on allocator behaviour.
        """
        import resource

        self.transport.call("version")
        before = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        for _ in range(2000):
            self.transport.call("client.has_token")
        after = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        # 2000 leaked JSON envelopes would be far more than 4 MB.
        self.assertLess(after - before, 4096, "the FFI appears to leak returned strings")


@unittest.skipUnless(library_available(), "build the shared library: cargo build -p openfield-bot-ffi")
class FfiAgainstMockTests(unittest.TestCase):
    """The FFI transport driving the real mock server.

    The mock is started through a sidecar built with the `testkit` feature, which keeps
    one implementation of the server for both transports.
    """

    def setUp(self) -> None:
        try:
            self.sidecar = SidecarTransport(options={})
        except BotError as exc:  # pragma: no cover
            raise unittest.SkipTest(f"no usable sidecar: {exc}") from exc
        try:
            self.info = self.sidecar.call("testkit.start")
        except BotError as exc:
            self.sidecar.close()
            raise unittest.SkipTest(f"{SIDECAR_FEATURE_HINT} ({exc})") from exc

        self.addCleanup(self.sidecar.close)
        self.transport = FfiTransport(
            options={"base_url": self.info["base_url"], "token": self.info["token"]}
        )
        self.addCleanup(self.transport.close)
        self.bot = Bot(transport=self.transport)

    def test_me_and_permissions(self) -> None:
        me = self.bot.me()
        self.assertEqual(me["id"], self.info["bot_id"])
        self.assertTrue(me["is_bot"])
        self.assertIn("posts.create", self.bot.permissions()["permissions"])

    def test_publish_and_read_back(self) -> None:
        created = self.bot.publish("hello from the C ABI")
        self.assertEqual(created["content"], "hello from the C ABI")
        page = self.bot.posts()
        self.assertEqual(page["posts"][0]["content"], "hello from the C ABI")

    def test_chat_round_trip(self) -> None:
        conversation_id = self.bot.create_conversation("ffi")["id"]
        sent = self.bot.send_message(conversation_id, "over the ABI")
        self.assertEqual(sent["content"], "over the ABI")
        self.assertIsNotNone(self.bot.messages(conversation_id))

    def test_upload_through_the_abi(self) -> None:
        """The mock answers a canned attachment, so the request is what is checked."""
        payload = bytes(range(256))
        attachment = self.bot.upload("abi.bin", payload, mime="application/octet-stream")
        self.assertEqual(attachment["original_name"], "chart.png")

        recorded = [
            entry
            for entry in self.sidecar.call("testkit.requests")["requests"]
            if entry["path"] == "/api/v1/attachments"
        ]
        self.assertTrue(recorded)
        self.assertIn('name="file"', recorded[-1]["body"])
        self.assertGreaterEqual(recorded[-1]["body_len"], 256)

    def test_403_maps_to_forbidden(self) -> None:
        self.sidecar.call("testkit.force_status", {"path": "/api/v1/posts", "status": 403})
        with self.assertRaises(ForbiddenError):
            self.bot.publish("denied")

    def test_429_maps_to_rate_limited(self) -> None:
        # 429 is retried, so it must keep refusing to outlast the policy.
        self.sidecar.call(
            "testkit.force_status",
            {
                "path": "/api/v1/users/me",
                "status": 429,
                "retry_after_secs": 1,
                "times": 10,
            },
        )
        with self.assertRaises(RateLimitedError) as caught:
            self.bot.me()
        self.assertTrue(caught.exception.retryable)
        self.assertEqual(caught.exception.retry_after_secs, 1)

    def test_401_maps_to_unauthorized(self) -> None:
        self.sidecar.call("testkit.force_status", {"path": "/api/v1/users/me", "status": 401})
        with self.assertRaises(UnauthorizedError):
            self.bot.me()

    def test_a_post_is_not_replayed_on_a_500(self) -> None:
        from openfield_bot import ServerError

        self.sidecar.call(
            "testkit.force_status", {"path": "/api/v1/posts", "status": 500, "times": 5}
        )
        with self.assertRaises(ServerError):
            self.bot.publish("only once")
        recorded = [
            entry
            for entry in self.sidecar.call("testkit.requests")["requests"]
            if entry["path"] == "/api/v1/posts"
        ]
        self.assertEqual(len(recorded), 1)

    def test_raw_request_reports_a_status_without_raising(self) -> None:
        """The escape hatch hands the status back instead of raising."""
        envelope = self.bot.request("GET", "/api/v1/users/me")
        self.assertTrue(envelope["ok"], envelope)
        self.assertEqual(envelope["status"], 200)
        self.assertEqual(envelope["body"]["id"], self.info["bot_id"])

        self.sidecar.call("testkit.force_status", {"path": "/api/v1/users/me", "status": 404})
        envelope = self.bot.request("GET", "/api/v1/users/me")
        self.assertFalse(envelope["ok"])
        self.assertEqual(envelope["status"], 404)

    def test_events_arrive_through_the_abi(self) -> None:
        from .support import _TIMEOUT, Puller, real_event, wait_until

        # The subscription is established by `events()`, before any iteration, so the
        # server already counts the client here.
        events = self.bot.events()
        self.addCleanup(events.close)
        self.assertTrue(
            wait_until(
                lambda: self.sidecar.call("testkit.ws_client_count")["clients"] >= 1,
                timeout=5.0,
            ),
            "the ABI subscription must reach the server",
        )

        puller = Puller(events)
        self.addCleanup(puller.close)

        self.sidecar.call(
            "testkit.broadcast",
            {"event": {"type": "post.created", "data": {"id": 4242, "content": "abi event"}}},
        )
        event = real_event(puller, timeout=5.0)
        self.assertIsNot(_TIMEOUT, event, "no event arrived through the ABI")
        self.assertEqual(event.type, "post.created")
        self.assertEqual(event.data["id"], 4242)

    def test_closing_the_abi_subscription_disconnects(self) -> None:
        from .support import wait_until

        events = self.bot.events()
        self.assertTrue(
            wait_until(
                lambda: self.sidecar.call("testkit.ws_client_count")["clients"] >= 1,
                timeout=5.0,
            )
        )
        events.close()
        self.assertTrue(
            wait_until(
                lambda: self.sidecar.call("testkit.ws_client_count")["clients"] == 0,
                timeout=5.0,
            ),
            "closing the ABI iterator must drop the connection",
        )

    def test_both_transports_agree(self) -> None:
        """The whole point of one dispatcher: identical answers from both transports.

        The sidecar is reconfigured with the same mock, so the two paths call the same
        server with the same token and must produce the same payload.
        """
        self.sidecar.call(
            "client.init", {"base_url": self.info["base_url"], "token": self.info["token"]}
        )
        via_sidecar = self.sidecar.call("capabilities")
        via_ffi = self.transport.call("capabilities")
        # The method index must be identical, because both come from the same const.
        self.assertEqual(via_sidecar["methods"], via_ffi["methods"])

        # And a real call must agree field for field.
        self.assertEqual(self.transport.call("health"), self.sidecar.call("health"))

    def test_the_extended_surface_agrees_too(self) -> None:
        """The money, task and chunked paths were added to the shared table, not to one
        transport: the C ABI must answer them exactly as the sidecar does.
        """
        self.sidecar.call(
            "client.init", {"base_url": self.info["base_url"], "token": self.info["token"]}
        )
        for method, params in [
            ("wallet.get", None),
            ("tasks.list", None),
            ("tasks.login_calendar", None),
            ("membership.get", None),
            ("moderation.punishments", {"user_id": 7}),
            ("plugins.list", None),
        ]:
            with self.subTest(method=method):
                self.assertEqual(
                    self.transport.call(method, params),
                    self.sidecar.call(method, params),
                )

        # `chunk_init` mints a fresh session id per call, so only its shape can be
        # compared; the id is exactly what must differ.
        chunked = {"filename": "a.bin", "size": 8, "total_chunks": 1}
        via_ffi = self.transport.call("storage.chunk_init", {"upload": chunked})
        via_sidecar = self.sidecar.call("storage.chunk_init", {"upload": chunked})
        self.assertEqual(via_ffi["total_chunks"], via_sidecar["total_chunks"])
        self.assertNotEqual(via_ffi["upload_id"], via_sidecar["upload_id"])

    def test_a_binary_download_crosses_the_abi_as_base64(self) -> None:
        """`plugins.download` is the only raw-bytes method, so its base64 envelope is
        worth proving over the C ABI rather than only the sidecar.
        """
        self.sidecar.call(
            "client.init", {"base_url": self.info["base_url"], "token": self.info["token"]}
        )
        via_ffi = self.bot.download_plugin("demo")
        self.assertEqual(via_ffi, b"PK\x03\x04demo-bundle")
        self.assertEqual(
            self.transport.call("plugins.download", {"plugin_id": "demo"}),
            self.sidecar.call("plugins.download", {"plugin_id": "demo"}),
        )


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
