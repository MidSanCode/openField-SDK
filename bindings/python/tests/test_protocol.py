"""Tests for the parts that do not need a server: error mapping, discovery, the facade."""

from __future__ import annotations

import os
import unittest
from unittest.mock import patch

from openfield_bot import (
    ERROR_KINDS,
    REACTIONS,
    SIDECAR_ENV,
    VISIBILITIES,
    BadRequestError,
    Bot,
    BotError,
    ConflictError,
    DecodeError,
    ForbiddenError,
    InvalidArgumentError,
    NetworkError,
    NotFoundError,
    RateLimitedError,
    ServerError,
    TransportError,
    UnauthorizedError,
    error_from_envelope,
    find_library,
    find_sidecar,
    repo_root,
)


class ErrorMappingTests(unittest.TestCase):
    """An envelope becomes the exception a caller should catch."""

    def test_each_status_maps_to_its_class(self) -> None:
        expectations = {
            400: BadRequestError,
            401: UnauthorizedError,
            403: ForbiddenError,
            404: NotFoundError,
            409: ConflictError,
            429: RateLimitedError,
            500: ServerError,
            503: ServerError,
        }
        for status, expected in expectations.items():
            with self.subTest(status=status):
                error = error_from_envelope(
                    {"kind": "http", "status": status, "message": f"http {status}"}
                )
                self.assertIsInstance(error, expected)
                self.assertEqual(error.status, status)

    def test_an_sdk_kind_is_honoured_directly(self) -> None:
        error = error_from_envelope({"kind": "invalid_argument", "message": "bad field"})
        self.assertIsInstance(error, InvalidArgumentError)
        self.assertEqual(error.kind, "invalid_argument")

    def test_an_unknown_kind_falls_back_to_the_base(self) -> None:
        error = error_from_envelope({"kind": "something_new", "message": "?"})
        self.assertIs(type(error), BotError)
        self.assertEqual(error.kind, "something_new")

    def test_the_permission_and_retry_hint_survive(self) -> None:
        error = error_from_envelope(
            {
                "kind": "http",
                "status": 403,
                "message": "permission denied",
                "permission": "posts.create",
                "retryable": False,
                "api_error": "permission denied",
                "body": {"error": "permission denied"},
            }
        )
        self.assertEqual(error.permission, "posts.create")
        self.assertFalse(error.retryable)
        self.assertEqual(error.api_error, "permission denied")
        self.assertEqual(error.body["error"], "permission denied")

    def test_retry_after_is_carried(self) -> None:
        error = error_from_envelope(
            {"kind": "http", "status": 429, "message": "slow down", "retry_after_secs": 12}
        )
        self.assertEqual(error.retry_after_secs, 12)
        # An absent `retryable` must not turn a retryable 429 into a permanent failure, so
        # the class's own verdict stands.
        self.assertTrue(error.retryable)

    def test_an_explicit_retryable_overrides_the_class_default(self) -> None:
        error = error_from_envelope(
            {"kind": "http", "status": 429, "message": "no", "retryable": False}
        )
        self.assertFalse(error.retryable)

    def test_a_missing_message_still_produces_something_readable(self) -> None:
        error = error_from_envelope({"kind": "http", "status": 500})
        self.assertTrue(str(error))

    def test_a_non_dict_error_does_not_crash(self) -> None:
        error = error_from_envelope("boom")  # type: ignore[arg-type]
        self.assertIsInstance(error, BotError)
        self.assertIn("boom", str(error))

    def test_every_kind_has_a_class(self) -> None:
        self.assertIn("rate_limited", ERROR_KINDS)
        self.assertIs(ERROR_KINDS["network"], NetworkError)

    def test_network_and_server_errors_are_retryable_by_default(self) -> None:
        self.assertTrue(NetworkError("reset").retryable)
        self.assertTrue(ServerError("boom", status=500).retryable)
        self.assertFalse(ForbiddenError("nope", status=403).retryable)
        self.assertFalse(UnauthorizedError("bad token", status=401).retryable)


class DiscoveryTests(unittest.TestCase):
    """The artifact lookup, and the error when it fails."""

    def test_the_repo_root_is_found_from_the_source_tree(self) -> None:
        root = repo_root()
        self.assertIsNotNone(root, "the package should be running from the checkout")
        assert root is not None
        self.assertTrue((root / "Cargo.toml").is_file())
        self.assertTrue((root / "core").is_dir())

    def test_an_explicit_path_wins(self) -> None:
        """A path the caller passes is used as-is, whatever discovery would have found."""
        real = find_sidecar()
        self.assertEqual(find_sidecar(real), real)

    def test_discovery_reports_every_place_it_looked(self) -> None:
        """When nothing exists, the error must name the search path and the fix.

        The cargo target directory and `PATH` are hidden for the duration, so the failure
        path is reachable even on a machine where the SDK has been built.
        """
        with patch("openfield_bot._transport._candidate_dirs", return_value=[]):
            with patch("openfield_bot._transport.shutil.which", return_value=None):
                with patch.dict(os.environ, {SIDECAR_ENV: "/nonexistent/from-env"}):
                    with self.assertRaises(TransportError) as caught:
                        find_sidecar("/nonexistent/explicit")
        message = str(caught.exception)
        self.assertIn("/nonexistent/explicit", message)
        self.assertIn("/nonexistent/from-env", message)
        self.assertIn("cargo build", message)

    def test_the_environment_variable_is_honoured(self) -> None:
        """A path from the environment is used, and a broken one is reported."""
        real = find_sidecar()
        with patch.dict(os.environ, {SIDECAR_ENV: str(real)}):
            self.assertEqual(find_sidecar(), real)

        with patch.dict(os.environ, {SIDECAR_ENV: "/nonexistent/broken"}):
            with patch("openfield_bot._transport._candidate_dirs", return_value=[]):
                with patch("openfield_bot._transport.shutil.which", return_value=None):
                    with self.assertRaises(TransportError) as caught:
                        find_sidecar()
        self.assertIn("/nonexistent/broken", str(caught.exception))

    def test_a_library_that_cannot_be_found_says_how_to_build_it(self) -> None:
        with patch("openfield_bot._transport._candidate_dirs", return_value=[]):
            with patch("openfield_bot._transport.shutil.which", return_value=None):
                with patch.dict(os.environ, {}, clear=False):
                    os.environ.pop("OPENFIELD_BOT_FFI", None)
                    with self.assertRaises(TransportError) as caught:
                        find_library()
        message = str(caught.exception)
        self.assertIn("cargo build", message)
        self.assertIn("libopenfield_bot_ffi", message)


class ConstantsTests(unittest.TestCase):
    """The vocabularies the server enforces, mirrored rather than guessed."""

    def test_reactions_match_the_server(self) -> None:
        self.assertEqual(
            REACTIONS, ("like", "dislike", "love", "haha", "wow", "sad", "angry")
        )

    def test_visibilities_match_the_server(self) -> None:
        self.assertEqual(VISIBILITIES, ("public", "login", "friends", "private"))


class FacadeTests(unittest.TestCase):
    """`Bot` without a transport, using a stub."""

    class StubTransport:
        """Records calls and answers canned values, so the facade can be tested alone."""

        def __init__(self) -> None:
            self.calls: list[tuple[str, object]] = []
            self.closed = False

        def call(self, method, params=None, **_ignored):
            self.calls.append((method, params))
            if method == "me":
                return {"id": 4242, "username": "stub_bot", "is_bot": True}
            if method == "permissions":
                return {"permissions": ["posts.create"], "groups": []}
            if method == "posts.create":
                return {"id": 1, "content": (params or {}).get("content")}
            if method == "chat.send":
                return {"id": 2, "content": (params or {}).get("content")}
            if method == "chat.latest":
                return {"messages": [{"id": 9, "sender_id": 7, "content": "hi"}]}
            return {}

        def events(self, params=None, **_ignored):
            return iter(())

        def close(self):
            self.closed = True

        @property
        def name(self):
            return "stub"

    def setUp(self) -> None:
        self.stub = self.StubTransport()
        self.bot = Bot(transport=self.stub)

    def test_calls_are_forwarded_verbatim(self) -> None:
        self.bot.call("set_storage_bucket", bucket="local")
        self.assertEqual(self.stub.calls[-1], ("set_storage_bucket", {"bucket": "local"}))

    def test_me_is_cached(self) -> None:
        first = self.bot.me()
        second = self.bot.me()
        self.assertIs(first, second)
        self.assertEqual(sum(1 for method, _ in self.stub.calls if method == "me"), 1)

        self.bot.me(refresh=True)
        self.assertEqual(sum(1 for method, _ in self.stub.calls if method == "me"), 2)

    def test_the_id_and_username_come_from_me(self) -> None:
        self.assertEqual(self.bot.id, 4242)
        self.assertEqual(self.bot.username, "stub_bot")

    def test_publish_passes_content_through(self) -> None:
        created = self.bot.publish("hello", visibility="public")
        self.assertEqual(created["content"], "hello")
        method, params = self.stub.calls[-1]
        self.assertEqual(method, "posts.create")
        self.assertEqual(params["visibility"], "public")

    def test_react_validates_before_calling(self) -> None:
        with self.assertRaises(InvalidArgumentError):
            self.bot.react(1, "nope")
        self.assertFalse(
            any(method == "posts.react" for method, _ in self.stub.calls),
            "an invalid reaction must not reach the server",
        )

    def test_favorite_picks_the_right_verb(self) -> None:
        self.bot.favorite(1)
        self.assertEqual(self.stub.calls[-1][0], "posts.favorite")
        self.bot.favorite(1, favorite=False)
        self.assertEqual(self.stub.calls[-1][0], "posts.unfavorite")

    def test_optional_parameters_are_omitted_when_absent(self) -> None:
        self.bot.send_message(33, "hi")
        method, params = self.stub.calls[-1]
        self.assertEqual(method, "chat.send")
        self.assertEqual(params, {"conversation_id": 33, "content": "hi"})

        self.bot.send_message(33, "hi", reply_to_id=5, mentions=[7], burn_seconds=300)
        _, params = self.stub.calls[-1]
        self.assertEqual(params["reply_to_id"], 5)
        self.assertEqual(params["mentions"], [7])
        self.assertEqual(params["burn_seconds"], 300)

    def test_upload_encodes_bytes_as_base64(self) -> None:
        self.bot.upload("a.bin", b"\x00\x01\xff")
        method, params = self.stub.calls[-1]
        self.assertEqual(method, "storage.upload")
        self.assertEqual(params["upload"]["file_name"], "a.bin")
        self.assertEqual(params["upload"]["bytes"], "AAH/")

    def test_create_camp_wraps_the_payload(self) -> None:
        self.bot.create_camp("weather", is_visible=True)
        method, params = self.stub.calls[-1]
        self.assertEqual(method, "camps.create")
        self.assertEqual(params["camp"], {"name": "weather", "is_visible": True})

    def test_update_camp_wraps_the_changes(self) -> None:
        self.bot.update_camp(4, description="new")
        self.assertEqual(self.stub.calls[-1][1], {"camp_id": 4, "update": {"description": "new"}})

    def test_the_context_manager_closes(self) -> None:
        with Bot(transport=self.stub):
            pass
        self.assertTrue(self.stub.closed)

    def test_window_lookup_uses_the_polled_page(self) -> None:
        """`watch` reads the newest id from `chat.latest` to set its starting point."""
        from openfield_bot.bot import _newest_message_id

        self.assertEqual(_newest_message_id({"messages": [{"id": 3}, {"id": 9}]}), 9)
        self.assertEqual(_newest_message_id({"data": [{"id": "12"}, {"id": 4}]}), 12)
        self.assertIsNone(_newest_message_id({"messages": []}))
        self.assertIsNone(_newest_message_id({}))

    def test_own_message_detection(self) -> None:
        from openfield_bot.bot import _is_own

        self.assertTrue(_is_own({"sender_id": 5}, 5))
        self.assertTrue(_is_own({"user_id": 5}, 5))
        self.assertFalse(_is_own({"sender_id": 6}, 5))
        self.assertFalse(_is_own({"sender_id": 5}, None), "an unknown id must not match")

    def test_event_type_helpers(self) -> None:
        from openfield_bot import Event

        event = Event({"event": "post.created", "data": {"id": 1}})
        self.assertEqual(event.type, "post.created")
        self.assertEqual(event.data, {"id": 1})
        self.assertTrue(event.is_broadcast())
        self.assertEqual(event.recipients, [])

        addressed = Event({"event": "chat.typing", "data": {}, "recipients": [7]})
        self.assertFalse(addressed.is_broadcast())
        self.assertEqual(addressed.recipients, [7])


class DecodeErrorTests(unittest.TestCase):
    """A decode failure is a first-class error, not a crash."""

    def test_decode_error_has_its_own_kind(self) -> None:
        self.assertEqual(DecodeError("bad shape").kind, "decode")
        self.assertIs(ERROR_KINDS["decode"], DecodeError)


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
