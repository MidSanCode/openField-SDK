"""Shared test scaffolding.

The suite drives the **real** mock server, the one implemented in Rust for the core's own
tests, reached through the sidecar's ``testkit.*`` methods. A second mock written in
Python would drift from the handler it is supposed to imitate, and then it would be
proving the wrong thing.

That means the sidecar must be built with the feature::

    cargo build -p openfield-bot-sidecar --features testkit

Without it the tests that need a server are skipped with a message saying so, rather than
failing and looking like a binding bug.
"""

from __future__ import annotations

import os
import queue
import threading
import time
import unittest
from pathlib import Path

from openfield_bot import Bot, BotError, SidecarTransport

SIDECAR_FEATURE_HINT = (
    "the sidecar was built without the `testkit` feature. Rebuild it with:\n"
    "    cargo build -p openfield-bot-sidecar --features testkit\n"
    "(or point OPENFIELD_BOT_SIDECAR at such a build)"
)


def sidecar_needs_testkit() -> SidecarTransport:
    """Start a sidecar whose mock server is available.

    Raises:
        unittest.SkipTest: when this build has no `testkit` feature.
    """
    try:
        return SidecarTransport(options={})
    except BotError as exc:  # pragma: no cover - only when nothing was built
        raise unittest.SkipTest(f"no usable sidecar: {exc}") from exc


class TestkitCase(unittest.TestCase):
    """A case wired to a freshly started mock server.

    Each test gets its own server, so a test that mutates posts or conversations cannot
    affect the next one, and no test has to reason about leftover state.
    """

    #: Whether the events tests should also run. Set by `setUp`.
    supports_events = True

    def setUp(self) -> None:
        self.sidecar = sidecar_needs_testkit()
        try:
            self.info = self.sidecar.call("testkit.start")
        except BotError as exc:
            self.sidecar.close()
            raise unittest.SkipTest(f"{SIDECAR_FEATURE_HINT} ({exc})") from exc

        self.base_url: str = self.info["base_url"]
        self.token: str = self.info["token"]
        self.sidecar.call("client.init", {"base_url": self.base_url, "token": self.token})
        self.bot = Bot(transport=self.sidecar)
        self.addCleanup(self._teardown)

    def _teardown(self) -> None:
        """Stop the mock server, then the process."""
        try:
            self.sidecar.call("testkit.stop", timeout=5.0)
        except BotError:
            pass
        self.sidecar.close()

    # -- helpers the tests share ------------------------------------------

    def force_status(
        self, path: str, status: int, *, retry_after_secs: int | None = None, times: int = 1
    ) -> None:
        """Make the mock answer `status` for the next `times` calls to `path`."""
        params: dict[str, object] = {"path": path, "status": status, "times": times}
        if retry_after_secs is not None:
            params["retry_after_secs"] = retry_after_secs
        self.sidecar.call("testkit.force_status", params)

    def requests(self) -> list[dict[str, object]]:
        """Everything the mock has received."""
        return self.sidecar.call("testkit.requests")["requests"]

    def requests_to(self, path: str) -> list[dict[str, object]]:
        """The recorded calls to one path."""
        return [entry for entry in self.requests() if entry["path"] == path]

    def broadcast(self, event: dict[str, object]) -> int:
        """Push an event to every connected WebSocket client."""
        return self.sidecar.call("testkit.broadcast", {"event": event})["delivered"]

    def ws_clients(self) -> int:
        """How many WebSocket clients are connected to the mock."""
        return self.sidecar.call("testkit.ws_client_count")["clients"]

    def seed_message(
        self,
        conversation_id: int,
        content: str,
        *,
        sender_id: int = 7,
        sender_name: str = "alice",
        broadcast: bool = False,
    ) -> dict[str, object]:
        """Insert a message from *another user*, then optionally announce it.

        Every mock route speaks as the bot under test, so this is the only way to produce
        traffic the bot must react to rather than ignore.
        """
        return self.sidecar.call(
            "testkit.seed_message",
            {
                "conversation_id": conversation_id,
                "sender_id": sender_id,
                "sender_name": sender_name,
                "content": content,
                "broadcast": broadcast,
            },
        )


def python_sources() -> list[Path]:
    """Every file in this package, for the self-check tests."""
    package = Path(__file__).resolve().parent.parent / "openfield_bot"
    return sorted(path for path in package.rglob("*.py") if path.is_file())


def sidecar_binary_exists() -> bool:
    """Whether a sidecar is discoverable, without starting one."""
    try:
        from openfield_bot import find_sidecar

        find_sidecar()
    except BotError:
        return False
    return True


def env_flag(name: str) -> bool:
    """Whether an environment variable is set to something truthy."""
    return os.environ.get(name, "").lower() in {"1", "true", "yes", "on"}


class _Sentinel:
    """A distinguishable marker, with a readable repr for assertion failures."""

    def __init__(self, label: str) -> None:
        self.label = label

    def __repr__(self) -> str:
        return self.label


#: Returned by :meth:`Puller.take` when nothing arrived in time.
_TIMEOUT = _Sentinel("<no event within the timeout>")
#: Returned by :meth:`Puller.take` when the iterator ended.
_FINISHED = _Sentinel("<iterator finished>")


class Puller:
    """Drive an event iterator from one dedicated thread, so a test never hangs.

    An event iterator blocks until an event arrives, so calling ``next()`` directly would
    hang a failing test forever with no output -- and calling it once per expectation from
    a fresh thread would drive one iterator from several threads at once, which Python
    rejects. A `Puller` starts a single worker that pulls continuously into a queue, so
    each :meth:`take` is a bounded wait and the iterator is only ever entered by one
    thread.
    """

    def __init__(self, iterator) -> None:
        self._iterator = iterator
        self._inbox: "queue.Queue[object]" = queue.Queue()
        self._stopped = threading.Event()
        self._thread = threading.Thread(
            target=self._run, name="openfield-test-puller", daemon=True
        )
        self._thread.start()

    def _run(self) -> None:
        while not self._stopped.is_set():
            try:
                item = next(self._iterator)
            except StopIteration:
                self._inbox.put(_FINISHED)
                return
            except BaseException as exc:  # noqa: BLE001 - reported to the test
                self._inbox.put(exc)
                return
            self._inbox.put(item)

    def take(self, timeout: float = 5.0):
        """The next item, `_TIMEOUT` when nothing arrived, or `_FINISHED` at the end.

        Raises:
            BaseException: whatever the iterator raised.
        """
        try:
            item = self._inbox.get(timeout=timeout)
        except queue.Empty:
            return _TIMEOUT
        if isinstance(item, BaseException):
            raise item
        return item

    def drain(self, timeout: float = 0.5) -> list:
        """Everything currently available, without waiting long for more."""
        collected = []
        while True:
            item = self.take(timeout=timeout)
            if item is _TIMEOUT or item is _FINISHED:
                return collected
            collected.append(item)

    def close(self) -> None:
        """Stop the worker and close the iterator."""
        self._stopped.set()
        close = getattr(self._iterator, "close", None)
        if close is not None:
            try:
                close()
            except BaseException:  # noqa: BLE001 - teardown is best effort
                pass


def real_event(puller: Puller, timeout: float = 5.0):
    """The next event that is not the ``connected`` greeting.

    The server greets every new subscriber with ``{"type": "connected"}``, so a test that
    took the very next frame would assert against the greeting rather than the event it
    broadcast. Skipping it here keeps each test's intent visible.
    """
    while True:
        item = puller.take(timeout=timeout)
        if item is _TIMEOUT or item is _FINISHED:
            return item
        if item.type != "connected":
            return item


def wait_until(predicate, timeout: float = 5.0, interval: float = 0.05) -> bool:
    """Poll `predicate` until it is true or the deadline passes."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return predicate()
