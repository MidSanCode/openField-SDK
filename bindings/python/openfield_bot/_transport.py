"""Transport plumbing shared by the sidecar and FFI backends.

Both backends expose the same two things:

* :meth:`Transport.call` -- run a named method and return its result;
* :meth:`Transport.events` -- iterate realtime events.

Everything else in this package is built on those, which is why ``Bot`` behaves
identically whichever backend is active.
"""

from __future__ import annotations

import abc
import os
import platform
import queue
import shutil
import sys
from pathlib import Path
from typing import Any, Iterator, Optional

from .errors import TransportError

__all__ = [
    "Transport",
    "EventIterator",
    "EventQueue",
    "find_sidecar",
    "find_library",
    "repo_root",
    "SIDECAR_ENV",
    "LIBRARY_ENV",
]

#: Environment variable naming an explicit sidecar binary.
SIDECAR_ENV = "OPENFIELD_BOT_SIDECAR"
#: Environment variable naming an explicit shared library.
LIBRARY_ENV = "OPENFIELD_BOT_FFI"

SIDECAR_NAME = "openfield-bot-sidecar"
LIBRARY_STEM = "openfield_bot_ffi"


class EventQueue:
    """A thread-safe queue that reports its closed state.

    The reader thread pushes events; the consumer iterates. ``close`` wakes a blocked
    consumer so a shutdown is immediate rather than waiting for the next event.
    """

    def __init__(self) -> None:
        self._queue: queue.Queue[Any] = queue.Queue()
        self._closed = False
        self._error: Optional[BaseException] = None

    def push(self, item: Any) -> None:
        """Enqueue one event."""
        self._queue.put(item)

    def close(self) -> None:
        """Mark the queue closed and wake any consumer."""
        self._closed = True
        self._queue.put(None)

    def fail(self, error: BaseException) -> None:
        """Close the queue and make the consumer raise `error`.

        Used when the producing thread hits a failure that the consumer has no other way
        of learning about, so a broken stream surfaces instead of looking like a quiet
        stream that simply ended.
        """
        self._error = error
        self.close()

    @property
    def error(self) -> Optional[BaseException]:
        """The failure that stopped the producer, if any."""
        return self._error

    @property
    def closed(self) -> bool:
        """Whether :meth:`close` has been called."""
        return self._closed

    def get(self, timeout: Optional[float] = None) -> Any:
        """Pop one item, or raise :class:`queue.Empty` on timeout."""
        return self._queue.get(timeout=timeout)

    def pop(self, timeout: float = 0.5) -> Any:
        """Block for the next event.

        Raises:
            StopIteration: once the queue is closed and drained, which is what lets an
                :class:`EventIterator` end a ``for`` loop normally.
            BaseException: whatever a failing producer passed to :meth:`fail`.
        """
        while True:
            try:
                item = self._queue.get(timeout=timeout)
            except queue.Empty:
                if self._closed:
                    if self._error is not None:
                        raise self._error
                    raise StopIteration from None
                continue
            if item is None:
                if self._error is not None:
                    raise self._error
                raise StopIteration from None
            return item

    def iterate(self, timeout: float = 0.5) -> Iterator[Any]:
        """Yield events until the queue is closed and drained.

        Raises:
            BaseException: whatever was passed to :meth:`fail`, once the buffered events
                have been delivered.
        """
        while True:
            try:
                yield self.pop(timeout)
            except StopIteration:
                return


class EventIterator(abc.ABC):
    """An already-established subscription.

    Note the "already": a generator that subscribes on its first ``next()`` looks
    identical to a caller but is not, and the difference shows up as a subscription that
    does not exist yet when the caller checks for it, or a broadcast sent before the
    ``for`` loop reaches its first iteration. Every transport therefore subscribes in the
    constructor and returns one of these.
    """

    @abc.abstractmethod
    def __next__(self) -> dict[str, Any]:
        """The next event, or raise :class:`StopIteration` when the stream ends."""

    def __iter__(self) -> "EventIterator":
        return self

    @abc.abstractmethod
    def close(self) -> None:
        """Drop the subscription. Safe to call more than once."""

    def __enter__(self) -> "EventIterator":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()


class Transport(abc.ABC):
    """The contract both backends satisfy."""

    @abc.abstractmethod
    def call(self, method: str, params: Optional[dict[str, Any]] = None) -> Any:
        """Run `method` and return its result, raising a :class:`BotError` on failure."""

    @abc.abstractmethod
    def events(self, params: Optional[dict[str, Any]] = None) -> EventIterator:
        """Subscribe and return an iterator over the events.

        The subscription is established before this returns.
        """

    @abc.abstractmethod
    def close(self) -> None:
        """Release the backend. Safe to call more than once."""

    @property
    def name(self) -> str:
        """A short label for diagnostics."""
        return type(self).__name__

    def __enter__(self) -> "Transport":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()


def repo_root() -> Optional[Path]:
    """The SDK checkout containing this package, when it is being run from source.

    Walks up looking for the cargo workspace manifest that sits beside ``core/``,
    ``ffi/`` and ``sidecar/``. Returns ``None`` for an installed package.
    """
    for candidate in Path(__file__).resolve().parents:
        manifest = candidate / "Cargo.toml"
        if manifest.is_file():
            try:
                text = manifest.read_text(encoding="utf-8")
            except OSError:
                continue
            if "[workspace]" in text and (candidate / "core").is_dir():
                return candidate
    return None


def _candidate_dirs() -> list[Path]:
    """Where a built artifact is likely to live, most specific first."""
    dirs: list[Path] = []
    root = repo_root()
    if root is not None:
        # `debug` before `release`: during development the debug build is the fresh one.
        dirs.extend([root / "target" / "debug", root / "target" / "release"])
    # A vendored copy next to the package, for a wheel that ships prebuilt binaries.
    here = Path(__file__).resolve().parent
    dirs.extend([here / "bin", here / "_native"])
    return dirs


def find_sidecar(explicit: Optional[str | os.PathLike[str]] = None) -> Path:
    """Locate the sidecar binary.

    Resolution order: the `explicit` argument, ``OPENFIELD_BOT_SIDECAR``, the cargo
    target directories of the checkout this package lives in, then ``PATH``.

    Raises:
        TransportError: when nothing was found, naming every place that was searched.
    """
    searched: list[str] = []

    def name() -> str:
        return f"{SIDECAR_NAME}.exe" if platform.system() == "Windows" else SIDECAR_NAME

    for candidate in (explicit, os.environ.get(SIDECAR_ENV)):
        if candidate:
            path = Path(candidate)
            if path.is_file():
                return path
            searched.append(str(path))

    for directory in _candidate_dirs():
        path = directory / name()
        searched.append(str(path))
        if path.is_file():
            return path

    found = shutil.which(name())
    if found:
        return Path(found)
    searched.append(f"$PATH:{name()}")

    raise TransportError(
        "could not find the openfield-bot-sidecar binary. Build it with "
        "`cargo build -p openfield-bot-sidecar`, or point "
        f"{SIDECAR_ENV} at it. Searched: " + ", ".join(searched)
    )


def find_library(explicit: Optional[str | os.PathLike[str]] = None) -> Path:
    """Locate the shared library.

    Resolution order: the `explicit` argument, ``OPENFIELD_BOT_FFI``, the cargo target
    directories of the checkout this package lives in, then the system loader path.

    Raises:
        TransportError: when nothing was found, naming every place that was searched.
    """
    system = platform.system()
    if system == "Windows":
        names = [f"{LIBRARY_STEM}.dll"]
    elif system == "Darwin":
        names = [f"lib{LIBRARY_STEM}.dylib", f"lib{LIBRARY_STEM}.so"]
    else:
        names = [f"lib{LIBRARY_STEM}.so"]

    searched: list[str] = []

    for candidate in (explicit, os.environ.get(LIBRARY_ENV)):
        if candidate:
            path = Path(candidate)
            if path.is_file():
                return path
            searched.append(str(path))

    for directory in _candidate_dirs():
        for filename in names:
            path = directory / filename
            searched.append(str(path))
            if path.is_file():
                return path

    # A system-wide install, which ctypes can load by bare name.
    for filename in names:
        found = shutil.which(filename)
        if found:
            return Path(found)
        searched.append(f"$PATH:{filename}")

    raise TransportError(
        "could not find the OpenField bot shared library. Build it with "
        "`cargo build -p openfield-bot-ffi`, or point "
        f"{LIBRARY_ENV} at it. Searched: " + ", ".join(searched)
    )


def python_bitness_note() -> str:
    """A one-line hint for the common bitness/ABI mismatch after loading fails."""
    return (
        f"interpreter: {sys.version.split()[0]} on {platform.machine()}/{platform.system()}"
    )
