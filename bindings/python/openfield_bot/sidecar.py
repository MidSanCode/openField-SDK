"""The sidecar backend: a helper process speaking newline-delimited JSON.

This is the default backend because it needs no compiled Python extension and cannot
suffer an ABI mismatch: the only requirement is a Rust binary that the SDK can find (see
:func:`openfield_bot.find_sidecar`).

Protocol
--------

Requests and responses are one JSON object per line::

    -> {"id": 1, "method": "posts.create", "params": {"content": "hello"}}
    <- {"id": 1, "result": {"id": 42, "content": "hello"}}
    <- {"id": 2, "error": {"kind": "forbidden", "status": 403}}

Events arrive unsolicited while a subscription is active::

    <- {"event": "chat.message.created", "data": {…}, "recipients": [4242]}

A reader thread owns stdout and routes each line either to the waiting caller or to the
event queue, because both share the one pipe.
"""

from __future__ import annotations

import json
import os
import subprocess
import threading
from pathlib import Path
from typing import Any, Iterator, Optional

from ._transport import EventIterator, EventQueue, Transport, find_sidecar
from .errors import ClosedError, TimeoutError as BotTimeout, TransportError, error_from_envelope

__all__ = ["SidecarTransport", "SidecarEvents"]


class _Pending:
    """One in-flight request, waiting for its response line."""

    __slots__ = ("event", "result", "error")

    def __init__(self) -> None:
        self.event = threading.Event()
        self.result: Any = None
        self.error: Optional[BaseException] = None


class SidecarTransport(Transport):
    """Runs the SDK in a child process and talks JSON over stdin/stdout.

    Args:
        binary: path to the sidecar; discovered when omitted.
        extra_args: appended to the command line, for a wrapper that needs flags.
        env: extra environment variables for the child.
        stderr: ``"pipe"`` captures diagnostics for :meth:`stderr_tail`, ``"inherit"``
            sends them to this process, ``"null"`` discards them.
        startup_timeout: how long to wait for the process to accept input.

    The process is started eagerly so a missing or broken binary fails here rather than
    on the first request.
    """

    def __init__(
        self,
        binary: Optional[str | os.PathLike[str]] = None,
        *,
        options: Optional[dict[str, Any]] = None,
        extra_args: Optional[list[str]] = None,
        env: Optional[dict[str, str]] = None,
        stderr: str = "pipe",
        startup_timeout: float = 10.0,
    ) -> None:
        self.binary = find_sidecar(binary)
        self._options = dict(options or {})
        self._closed = False
        self._lock = threading.Lock()
        self._pending: dict[int, _Pending] = {}
        self._next_id = 0
        self._events: Optional[EventQueue] = None
        self._stderr_lines: list[str] = []
        self._stderr_drain: Optional[threading.Thread] = None

        child_env = dict(os.environ)
        if env:
            child_env.update(env)

        stderr_target: Any
        if stderr == "pipe":
            stderr_target = subprocess.PIPE
        elif stderr == "inherit":
            stderr_target = None
        else:
            stderr_target = subprocess.DEVNULL

        try:
            self._process = subprocess.Popen(  # noqa: S603 - the binary path is resolved above
                [str(self.binary), *(extra_args or [])],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=stderr_target,
                env=child_env,
                text=True,
                encoding="utf-8",
                bufsize=1,  # line buffered, so a request is not held back
            )
        except OSError as exc:
            raise TransportError(
                f"could not start the sidecar at {self.binary}: {exc}"
            ) from exc

        if self._process.stdin is None or self._process.stdout is None:
            raise TransportError("the sidecar was started without usable pipes")

        self._reader = threading.Thread(
            target=self._read_loop, name="openfield-sidecar-reader", daemon=True
        )
        self._reader.start()

        if self._process.stderr is not None:
            self._stderr_drain = threading.Thread(
                target=self._drain_stderr, name="openfield-sidecar-stderr", daemon=True
            )
            self._stderr_drain.start()

        self._wait_until_ready(startup_timeout)

    # -- lifecycle ---------------------------------------------------------

    def _wait_until_ready(self, timeout: float) -> None:
        """Configure the client and prove the child answers.

        Doing this here means a bad base URL or a malformed token fails at construction
        rather than on the caller's first real request.
        """
        try:
            self.call("client.init", self._options, timeout=timeout)
            self.call("version", timeout=timeout)
        except BaseException as exc:  # noqa: BLE001 - re-raised with the tail attached
            tail = self.stderr_tail()
            detail = f" (stderr: {tail})" if tail else ""
            raise TransportError(
                f"the sidecar at {self.binary} did not accept its configuration{detail}"
            ) from exc

    def _drain_stderr(self) -> None:
        """Keep stderr flowing so a chatty child cannot block on a full pipe."""
        stream = self._process.stderr
        if stream is None:
            return
        try:
            for line in stream:
                self._stderr_lines.append(line.rstrip("\n"))
                if len(self._stderr_lines) > 200:
                    del self._stderr_lines[:100]
        except (ValueError, OSError):
            return

    def _read_loop(self) -> None:
        """Route every stdout line to its waiter, or to the event queue."""
        stream = self._process.stdout
        if stream is None:
            return
        try:
            for line in stream:
                line = line.strip()
                if not line:
                    continue
                try:
                    message = json.loads(line)
                except json.JSONDecodeError:
                    # Not protocol. Surface it rather than swallowing it, because it
                    # usually means something else printed to stdout.
                    self._stderr_lines.append(f"non-JSON stdout: {line[:200]}")
                    continue

                if isinstance(message, dict) and "event" in message:
                    if self._events is not None:
                        self._events.push(message)
                    continue

                request_id = message.get("id") if isinstance(message, dict) else None
                if request_id is None:
                    if isinstance(message, dict) and "error" in message:
                        self._stderr_lines.append(
                            f"unsolicited error: {message['error']}"
                        )
                    continue

                with self._lock:
                    pending = self._pending.pop(request_id, None)
                if pending is None:
                    continue
                if "error" in message:
                    pending.error = error_from_envelope(message["error"])
                else:
                    pending.result = message.get("result")
                pending.event.set()
        except (ValueError, OSError):
            # The pipe closed: fail every waiter rather than leaving them blocked.
            pass
        finally:
            self._fail_all(TransportError("the sidecar exited"))

    def _fail_all(self, error: BaseException) -> None:
        """Release every waiter with `error`."""
        with self._lock:
            pending, self._pending = self._pending, {}
        for waiter in pending.values():
            waiter.error = error
            waiter.event.set()
        if self._events is not None:
            self._events.close()

    def close(self) -> None:
        """Stop the child. Safe to call more than once."""
        if self._closed:
            return
        self._closed = True

        if self._events is not None:
            self._events.close()

        process = self._process
        # Ask politely first, so the sidecar can close its event stream.
        if process.poll() is None:
            try:
                self.call("shutdown", timeout=2.0)
            except BaseException:  # noqa: BLE001 - shutdown is best effort
                pass
        try:
            if process.stdin is not None:
                process.stdin.close()
        except OSError:
            pass
        try:
            process.wait(timeout=5.0)
        except subprocess.TimeoutExpired:
            process.terminate()
            try:
                process.wait(timeout=5.0)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5.0)

        self._fail_all(TransportError("the transport was closed"))

        # Close the read ends too. Leaving them open leaks two file descriptors per
        # transport, which a long-running process that reconnects would accumulate.
        for stream in (process.stdout, process.stderr):
            try:
                if stream is not None:
                    stream.close()
            except OSError:
                pass
        reader, self._reader = getattr(self, "_reader", None), None
        if reader is not None and reader.is_alive():
            reader.join(timeout=2.0)
        drain, self._stderr_drain = self._stderr_drain, None
        if drain is not None and drain.is_alive():
            drain.join(timeout=2.0)

    @property
    def name(self) -> str:
        """A short label including the binary path."""
        return f"sidecar({self.binary})"

    @property
    def pid(self) -> int:
        """The child's process id, for logging."""
        return self._process.pid

    def stderr_tail(self, limit: int = 20) -> str:
        """The last few stderr lines, useful when a call fails unexpectedly."""
        return "\n".join(self._stderr_lines[-limit:])

    # -- calls -------------------------------------------------------------

    def call(
        self,
        method: str,
        params: Optional[dict[str, Any]] = None,
        *,
        timeout: Optional[float] = 60.0,
    ) -> Any:
        """Run `method` and return its result.

        Args:
            method: a name from the shared index, e.g. ``"posts.create"``. Ask for
                ``"capabilities"`` to list them all.
            params: the parameter object; omit for a method that takes none.
            timeout: seconds to wait, ``None`` to wait indefinitely.

        Raises:
            openfield_bot.BotError: for any failure the SDK reports. The concrete class
                follows the error category.
            openfield_bot.TransportError: when the child died or never answered.
        """
        if self._closed:
            raise ClosedError("the transport is closed")

        with self._lock:
            self._next_id += 1
            request_id = self._next_id
            pending = _Pending()
            self._pending[request_id] = pending

        payload = json.dumps(
            {"id": request_id, "method": method, "params": params or {}},
            ensure_ascii=False,
        )
        try:
            assert self._process.stdin is not None
            self._process.stdin.write(payload + "\n")
            self._process.stdin.flush()
        except (BrokenPipeError, OSError, ValueError) as exc:
            with self._lock:
                self._pending.pop(request_id, None)
            tail = self.stderr_tail()
            detail = f" (stderr: {tail})" if tail else ""
            raise TransportError(f"the sidecar pipe is broken{detail}") from exc

        if not pending.event.wait(timeout):
            with self._lock:
                self._pending.pop(request_id, None)
            raise BotTimeout(f"`{method}` did not answer within {timeout}s")

        if pending.error is not None:
            raise pending.error
        return pending.result

    # -- events ------------------------------------------------------------

    def events(
        self,
        params: Optional[dict[str, Any]] = None,
        *,
        timeout: Optional[float] = 60.0,
    ) -> SidecarEvents:
        """Subscribe and return an iterator over the events.

        The subscription is established before this returns, so a caller can check
        :meth:`SidecarTransport.event_stats` -- or the server's own client count -- and
        already see the connection.

        Args:
            params: the filter, e.g. ``{"types": ["chat.message.created"]}``.
            timeout: how long the subscribe call itself may take.

        Returns:
            A :class:`SidecarEvents`, which is also usable as a context manager.
        """
        return SidecarEvents(self, params, timeout)

    def event_stats(self, *, timeout: float = 30.0) -> dict[str, Any]:
        """The stream counters from the child."""
        return self.call("events.stats", timeout=timeout)

    def __del__(self) -> None:  # pragma: no cover - depends on GC timing
        try:
            self.close()
        except BaseException:  # noqa: BLE001 - a destructor must not raise
            pass

    def __repr__(self) -> str:  # pragma: no cover - trivial
        state = "closed" if self._closed else "running"
        return f"<SidecarTransport {self.binary.name} pid={self._process.pid} {state}>"


class SidecarEvents(EventIterator):
    """A live subscription held by a :class:`SidecarTransport`.

    The sidecar keeps exactly one active stream, so creating one of these replaces any
    previous subscription. Iterating blocks until an event arrives; :meth:`close` drops
    the subscription on the child as well as stopping the iterator.
    """

    def __init__(
        self,
        transport: SidecarTransport,
        params: Optional[dict[str, Any]] = None,
        timeout: Optional[float] = 60.0,
    ) -> None:
        self._transport = transport
        self._timeout = timeout
        self._closed = False
        self._queue = EventQueue()

        # Replace any previous subscription: a stale queue would hold events nobody
        # reads, and the child only serves one stream at a time.
        previous = transport._events
        if previous is not None:
            previous.close()
        transport._events = self._queue

        try:
            transport.call("events.subscribe", params or {}, timeout=timeout)
        except BaseException:
            if transport._events is self._queue:
                transport._events = None
            self._queue.close()
            self._closed = True
            raise

    def __next__(self) -> dict[str, Any]:
        if self._closed:
            raise StopIteration
        return self._queue.pop()

    def close(self) -> None:
        """Drop the subscription. Safe to call more than once."""
        if self._closed:
            return
        self._closed = True
        self._queue.close()
        if self._transport._events is self._queue:
            self._transport._events = None
        if not self._transport._closed:
            try:
                self._transport.call("events.unsubscribe", timeout=self._timeout)
            except BaseException:  # noqa: BLE001 - teardown is best effort
                pass

    @property
    def stats(self) -> dict[str, Any]:
        """The stream counters, or an empty dict once closed."""
        if self._closed:
            return {}
        return self._transport.event_stats(timeout=self._timeout or 30.0)

    def __repr__(self) -> str:  # pragma: no cover - trivial
        state = "closed" if self._closed else "open"
        return f"<SidecarEvents {state}>"
