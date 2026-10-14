"""OpenField community bot SDK for Python.

The Rust core does the work; this package drives it through one of two transports:

* **sidecar** (default) -- a child process speaking newline-delimited JSON. No compiled
  extension, no ABI to match, one dependency: a Rust binary. See
  :class:`~openfield_bot.SidecarTransport`.
* **FFI** -- the same core loaded as a shared library through :mod:`ctypes`. Faster and
  process-free. See :class:`~openfield_bot.FfiTransport`.

Both expose the identical method surface, because both dispatch through the same Rust
table, so switching backends changes nothing but the plumbing.

Quick start
-----------

.. code-block:: python

    from openfield_bot import Bot

    with Bot("https://openfield.example.com", "ofb_" + "1" * 64) as bot:
        check = bot.startup_check()          # does the token work, what is missing?
        print(check["user"]["nickname"], check["missing_administrative"])

        bot.publish("Hello from the SDK", visibility="public")

        for event in bot.watch(33, types=["chat.message.created"]):
            if "ping" in event.data["content"]:
                bot.send_message(33, "pong", reply_to_id=event.data["id"])

Long-running bots should read ``bots.md`` 15 and 16 first: the short version is that a
``401`` means alert and stop, a ``403`` means a missing permission (the exception names
it), and the event stream is not a reliable queue.

Where the binaries come from
----------------------------

Both transports find their artifact automatically when this package sits inside the SDK
checkout: build with ``cargo build`` and the ``target/debug`` files are picked up. In
another layout, set ``OPENFIELD_BOT_SIDECAR`` or ``OPENFIELD_BOT_FFI``, or pass
``sidecar_path`` / ``library_path`` to :class:`Bot`.
"""

from __future__ import annotations

from ._transport import (
    LIBRARY_ENV,
    SIDECAR_ENV,
    EventQueue,
    Transport,
    find_library,
    find_sidecar,
    repo_root,
)
from .bot import REACTIONS, VISIBILITIES, Bot, Event
from .errors import (
    ERROR_KINDS,
    BadRequestError,
    BotError,
    ClosedError,
    ConflictError,
    DecodeError,
    ForbiddenError,
    InvalidArgumentError,
    NetworkError,
    NotFoundError,
    RateLimitedError,
    ServerError,
    TimeoutError,
    TransportError,
    UnauthorizedError,
    error_from_envelope,
)
from .ffi import FfiTransport
from .sidecar import SidecarTransport

__version__ = "0.1.0"

#: The RPC protocol revision this package speaks. It must match the transport's.
PROTOCOL_VERSION = 1

__all__ = [
    # core
    "Bot",
    "Event",
    # transports
    "Transport",
    "SidecarTransport",
    "FfiTransport",
    "EventQueue",
    # discovery
    "find_sidecar",
    "find_library",
    "repo_root",
    "SIDECAR_ENV",
    "LIBRARY_ENV",
    # errors
    "BotError",
    "TransportError",
    "InvalidArgumentError",
    "NetworkError",
    "BadRequestError",
    "UnauthorizedError",
    "ForbiddenError",
    "NotFoundError",
    "ConflictError",
    "RateLimitedError",
    "ServerError",
    "DecodeError",
    "TimeoutError",
    "ClosedError",
    "ERROR_KINDS",
    "error_from_envelope",
    # constants
    "REACTIONS",
    "VISIBILITIES",
    "PROTOCOL_VERSION",
    "__version__",
]
