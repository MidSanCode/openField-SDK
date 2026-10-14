"""The FFI backend: the same Rust core, loaded as a shared library via :mod:`ctypes`.

The sidecar backend spawns a process; this one calls straight into ``cdylib``
``libopenfield_bot_ffi``. That is faster and uses no extra process, at the cost of having
to match the platform's ABI -- which, for Python, only means matching the architecture,
since the C surface is plain ``char *`` in and out.

The method surface is identical to the sidecar's, because both dispatch through the same
Rust table. Events differ: they come from ``ofbot_events_next`` rather than from a pipe.
"""

from __future__ import annotations

import ctypes
import json
import os
import threading
from ctypes import c_char_p, c_int, c_int64, c_void_p
from pathlib import Path
from typing import Any, Iterator, Optional

from ._transport import EventIterator, EventQueue, Transport, find_library, python_bitness_note
from .errors import ClosedError, TransportError, error_from_envelope

__all__ = ["FfiTransport", "FfiEvents"]


class FfiTransport(Transport):
    """Drives the SDK through the C ABI.

    Args:
        library: path to the shared library; discovered when omitted.
        options: the client options object -- ``base_url``, ``token``, ``timeout_ms``,
            ``retry``, ... The same shape the sidecar's ``client.init`` takes.

    Raises:
        TransportError: when the library cannot be found or loaded, or the handle cannot
            be created (a malformed token, for instance).
    """

    def __init__(
        self,
        library: Optional[str | os.PathLike[str]] = None,
        *,
        options: Optional[dict[str, Any]] = None,
    ) -> None:
        self.library_path = find_library(library)
        try:
            self._lib = ctypes.CDLL(str(self.library_path))
        except OSError as exc:
            raise TransportError(
                f"could not load {self.library_path}: {exc}. {python_bitness_note()}"
            ) from exc

        self._declare()
        self._closed = False
        self._events: Optional[EventQueue] = None
        self._event_thread: Optional[threading.Thread] = None
        self._event_handle: Optional[int] = None
        self._lock = threading.Lock()

        payload = json.dumps(options or {}, ensure_ascii=False)
        self._handle = self._lib.ofbot_client_new(payload.encode("utf-8"))
        if not self._handle:
            raise TransportError(
                "ofbot_client_new returned NULL: " + self._last_error()
            )
        self._options = dict(options or {})

    def _declare(self) -> None:
        """Pin the signature of every entry point.

        Without these ctypes assumes ``int``, which truncates pointers on 64-bit hosts --
        the classic way an FFI binding corrupts memory instead of failing.
        """
        lib = self._lib
        lib.ofbot_version.restype = c_char_p
        lib.ofbot_version.argtypes = []
        lib.ofbot_protocol_version.restype = c_char_p
        lib.ofbot_protocol_version.argtypes = []
        lib.ofbot_last_error.restype = c_char_p
        lib.ofbot_last_error.argtypes = []
        lib.ofbot_string_free.restype = None
        lib.ofbot_string_free.argtypes = [c_void_p]

        lib.ofbot_client_new.restype = c_void_p
        lib.ofbot_client_new.argtypes = [c_char_p]
        lib.ofbot_client_free.restype = None
        lib.ofbot_client_free.argtypes = [c_void_p]
        lib.ofbot_client_set_token.restype = c_int
        lib.ofbot_client_set_token.argtypes = [c_void_p, c_char_p]
        lib.ofbot_client_config.restype = c_void_p
        lib.ofbot_client_config.argtypes = [c_void_p]
        lib.ofbot_client_has_token.restype = c_int
        lib.ofbot_client_has_token.argtypes = [c_void_p]

        lib.ofbot_call.restype = c_void_p
        lib.ofbot_call.argtypes = [c_void_p, c_char_p, c_char_p]
        lib.ofbot_client_request.restype = c_void_p
        lib.ofbot_client_request.argtypes = [c_void_p, c_char_p]

        lib.ofbot_events_subscribe.restype = c_void_p
        lib.ofbot_events_subscribe.argtypes = [c_void_p, c_char_p]
        lib.ofbot_events_next.restype = c_void_p
        lib.ofbot_events_next.argtypes = [c_void_p, c_int64]
        lib.ofbot_events_stats.restype = c_void_p
        lib.ofbot_events_stats.argtypes = [c_void_p]
        lib.ofbot_events_close.restype = None
        lib.ofbot_events_close.argtypes = [c_void_p]
        lib.ofbot_events_free.restype = None
        lib.ofbot_events_free.argtypes = [c_void_p]

    # -- helpers -----------------------------------------------------------

    def _last_error(self) -> str:
        """The library's thread-local error message."""
        raw = self._lib.ofbot_last_error()
        if not raw:
            return "<no message>"
        return raw.decode("utf-8", "replace")

    def _take_string(self, pointer: int) -> str:
        """Copy an owned C string out and free it."""
        if not pointer:
            raise TransportError("the library returned NULL: " + self._last_error())
        try:
            return ctypes.string_at(pointer).decode("utf-8", "replace")
        finally:
            self._lib.ofbot_string_free(pointer)

    def _call_json(self, pointer: int) -> dict[str, Any]:
        """Decode a JSON envelope the library returned."""
        text = self._take_string(pointer)
        try:
            value = json.loads(text)
        except json.JSONDecodeError as exc:  # pragma: no cover - would be a library bug
            raise TransportError(f"the library returned non-JSON: {text[:200]}") from exc
        if not isinstance(value, dict):  # pragma: no cover - would be a library bug
            raise TransportError(f"the library returned a non-object: {text[:200]}")
        return value

    # -- metadata ----------------------------------------------------------

    @property
    def version(self) -> str:
        """The loaded library's SDK version."""
        raw = self._lib.ofbot_version()
        return raw.decode("utf-8") if raw else "unknown"

    @property
    def protocol_version(self) -> str:
        """The loaded library's RPC protocol revision."""
        raw = self._lib.ofbot_protocol_version()
        return raw.decode("utf-8") if raw else "unknown"

    @property
    def name(self) -> str:
        """A short label including the library path."""
        return f"ffi({self.library_path.name})"

    def has_token(self) -> bool:
        """Whether the client holds a token."""
        return bool(self._lib.ofbot_client_has_token(self._handle))

    def set_token(self, token: Optional[str]) -> None:
        """Replace the token, or clear it with ``None``/``""``.

        Raises:
            openfield_bot.InvalidArgumentError: when the token is malformed. The previous
                token keeps working, which matters mid-``regenerate``.
        """
        raw = (token or "").encode("utf-8")
        if self._lib.ofbot_client_set_token(self._handle, raw) != 1:
            raise TransportError("ofbot_client_set_token failed: " + self._last_error())

    def config(self) -> dict[str, Any]:
        """The effective configuration, with the token redacted."""
        envelope = self._call_json(self._lib.ofbot_client_config(self._handle))
        if not envelope.get("ok"):
            raise error_from_envelope(envelope.get("error") or {})
        return envelope.get("config") or {}

    # -- calls -------------------------------------------------------------

    def call(
        self,
        method: str,
        params: Optional[dict[str, Any]] = None,
        **_ignored: Any,
    ) -> Any:
        """Run `method` and return its result.

        Args:
            method: a name from the shared index, e.g. ``"posts.create"``.

        Raises:
            openfield_bot.BotError: for any failure the SDK reports.
            openfield_bot.InvalidArgumentError: for the event methods, which this backend
                serves through :meth:`events` instead.
        """
        if self._closed:
            raise ClosedError("the transport is closed")

        with self._lock:
            handle = self._handle
        if not handle:
            raise ClosedError("the client handle was released")

        payload = json.dumps(params or {}, ensure_ascii=False).encode("utf-8")
        pointer = self._lib.ofbot_call(handle, method.encode("utf-8"), payload)
        envelope = self._call_json(pointer)
        if not envelope.get("ok"):
            raise error_from_envelope(envelope.get("error") or {})
        return envelope.get("result")

    def raw_request(self, request: dict[str, Any]) -> dict[str, Any]:
        """Perform a raw request and return the full envelope.

        Unlike :meth:`call`, a non-2xx status is **not** raised: the envelope carries
        ``status`` and ``body`` so the caller can inspect an error deliberately.
        """
        if self._closed:
            raise ClosedError("the transport is closed")
        payload = json.dumps(request, ensure_ascii=False).encode("utf-8")
        return self._call_json(self._lib.ofbot_client_request(self._handle, payload))

    # -- events ------------------------------------------------------------

    def events(
        self,
        params: Optional[dict[str, Any]] = None,
        *,
        poll_ms: int = 200,
    ) -> FfiEvents:
        """Subscribe and return an iterator over the events.

        The subscription is established before this returns, so a caller can already see
        the connection on the server.

        Args:
            params: the filter, e.g. ``{"types": ["chat.message.created"]}``.
            poll_ms: how long each blocking read may take, which bounds how quickly
                :meth:`FfiEvents.close` takes effect.

        Returns:
            An :class:`FfiEvents`, which is also usable as a context manager.
        """
        return FfiEvents(self, params, poll_ms)

    def _teardown_events(self) -> None:
        """Stop and release the current subscription, if any.

        The order matters. ``ofbot_events_free`` drops the Rust ``Box``, and the pump
        thread is inside ``ofbot_events_next`` holding a raw pointer to it, so freeing
        before joining the thread is a use-after-free -- which shows up as a segfault in
        the host interpreter, not as an exception. Hence: ask the stream to stop
        (thread-safe), wake the local queue, **join**, and only then free.
        """
        handle, self._event_handle = self._event_handle, None
        queue, self._events = self._events, None

        if handle:
            # Safe to call concurrently with `next`: both take a shared reference to the
            # same stream, whose internals are atomics and a mutex.
            self._lib.ofbot_events_close(handle)
        if queue is not None:
            # Wakes anything blocked on the Python-side queue so it stops asking.
            queue.close()

        thread, self._event_thread = self._event_thread, None
        if thread is not None and thread.is_alive():
            thread.join(timeout=5.0)

        if handle:
            # Unreferenced now, so this is safe.
            self._lib.ofbot_events_free(handle)

    def event_stats(self) -> dict[str, Any]:
        """The stream counters, or an empty dict when nothing is subscribed."""
        if not self._event_handle:
            return {}
        envelope = self._call_json(self._lib.ofbot_events_stats(self._event_handle))
        if not envelope.get("ok"):
            raise error_from_envelope(envelope.get("error") or {})
        return envelope.get("stats") or {}

    # -- lifecycle ---------------------------------------------------------

    def close(self) -> None:
        """Stop the subscription and release the client handle."""
        if self._closed:
            return
        self._closed = True
        self._teardown_events()
        handle, self._handle = self._handle, None
        if handle:
            self._lib.ofbot_client_free(handle)

    def __del__(self) -> None:  # pragma: no cover - depends on GC timing
        try:
            self.close()
        except BaseException:  # noqa: BLE001 - a destructor must not raise
            pass

    def __repr__(self) -> str:  # pragma: no cover - trivial
        state = "closed" if self._closed else "running"
        return f"<FfiTransport {self.library_path.name} {state}>"


class FfiEvents(EventIterator):
    """A live subscription held by an :class:`FfiTransport`.

    The library keeps one stream per client, so creating one of these replaces any
    previous subscription. A background thread pumps ``ofbot_events_next`` into a local
    queue, so :meth:`close` takes effect within one `poll_ms` rather than blocking until
    the next event happens to arrive.
    """

    def __init__(
        self,
        transport: FfiTransport,
        params: Optional[dict[str, Any]] = None,
        poll_ms: int = 200,
    ) -> None:
        if transport._closed:
            raise ClosedError("the transport is closed")

        self._transport = transport
        self._closed = False
        self._queue = EventQueue()
        self._poll_ms = poll_ms

        # Replace any previous subscription: the library serves one stream.
        transport._teardown_events()

        payload = json.dumps(params or {}, ensure_ascii=False).encode("utf-8")
        self._handle = transport._lib.ofbot_events_subscribe(transport._handle, payload)
        if not self._handle:
            self._closed = True
            self._queue.close()
            raise TransportError(
                "ofbot_events_subscribe returned NULL: " + transport._last_error()
            )

        transport._events = self._queue
        transport._event_handle = self._handle

        thread = threading.Thread(
            target=self._pump, name="openfield-ffi-events", daemon=True
        )
        transport._event_thread = thread
        thread.start()

    def _pump(self) -> None:
        """Move events from the library into the queue until the stream ends."""
        transport = self._transport
        try:
            while not self._queue.closed:
                pointer = transport._lib.ofbot_events_next(self._handle, self._poll_ms)
                message = transport._call_json(pointer)
                kind = message.get("kind")
                if kind == "event":
                    self._queue.push(message["event"])
                elif kind == "closed":
                    return
                elif kind == "error":
                    self._queue.fail(error_from_envelope(message.get("error") or {}))
                    return
                # `timeout` just loops around.
        except BaseException as exc:  # noqa: BLE001 - surfaced through the queue
            self._queue.fail(exc)
        finally:
            self._queue.close()

    def __next__(self) -> dict[str, Any]:
        if self._closed:
            raise StopIteration
        return self._queue.pop(timeout=max(self._poll_ms / 1000.0, 0.05) * 2)

    def close(self) -> None:
        """Drop the subscription. Safe to call more than once."""
        if self._closed:
            return
        self._closed = True
        self._queue.close()
        transport = self._transport
        if transport._event_handle == self._handle:
            transport._teardown_events()
        else:
            transport._lib.ofbot_events_close(self._handle)
            transport._lib.ofbot_events_free(self._handle)

    @property
    def stats(self) -> dict[str, Any]:
        """The stream counters, or an empty dict once closed."""
        if self._closed:
            return {}
        envelope = self._transport._call_json(
            self._transport._lib.ofbot_events_stats(self._handle)
        )
        if not envelope.get("ok"):
            raise error_from_envelope(envelope.get("error") or {})
        return envelope.get("stats") or {}

    def __repr__(self) -> str:  # pragma: no cover - trivial
        state = "closed" if self._closed else "open"
        return f"<FfiEvents {state}>"
