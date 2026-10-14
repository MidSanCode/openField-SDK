"""Exception types raised by :mod:`openfield_bot`.

Every failure the SDK can produce arrives as a :class:`BotError` subclass, carrying the
same fields the Rust core reports, so callers never have to parse a message string to
decide what to do. In particular:

* :class:`UnauthorizedError` means the token was reset, never existed, or the bot is
  banned -- all three answer ``401``, and the documented response is to alert and stop,
  never to retry (``bots.md`` 15.2);
* :class:`ForbiddenError` carries :attr:`~BotError.permission`, the permission key the
  bot is missing, because the server reports it explicitly;
* :class:`RateLimitedError` carries :attr:`~BotError.retry_after_secs`, so a caller can
  wait exactly as long as the server asked instead of guessing;
* :attr:`BotError.retryable` is the SDK's own verdict, computed from the status.
"""

from __future__ import annotations

from typing import Any, Optional

__all__ = [
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
]


class BotError(Exception):
    """Base class for every SDK failure.

    Attributes:
        kind: the SDK's coarse category, e.g. ``"forbidden"``.
        message: the server's own error text when there is one, wrapped otherwise.
        status: the HTTP status, when the failure came from a response.
        permission: the missing permission key, on a ``403`` permission failure.
        retryable: whether repeating the call could plausibly succeed.
        retry_after_secs: the server's ``Retry-After`` hint, when it sent one.
        body: the server's parsed error envelope.
        api_error: the raw ``error`` string from the envelope.
    """

    #: The SDK's category for this failure. Overridden per subclass.
    kind: str = "other"

    def __init__(
        self,
        message: str,
        *,
        status: Optional[int] = None,
        permission: Optional[str] = None,
        retryable: bool = False,
        retry_after_secs: Optional[int] = None,
        body: Optional[dict[str, Any]] = None,
        api_error: Optional[str] = None,
        kind: Optional[str] = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.status = status
        self.permission = permission
        self.retryable = retryable
        self.retry_after_secs = retry_after_secs
        self.body = body or {}
        self.api_error = api_error
        if kind:
            self.kind = kind

    def __str__(self) -> str:  # pragma: no cover - trivial
        parts = [self.message]
        if self.status is not None:
            parts.append(f"status={self.status}")
        if self.permission:
            parts.append(f"permission={self.permission}")
        return " ".join(parts)

    def __repr__(self) -> str:  # pragma: no cover - trivial
        return f"{type(self).__name__}({self.message!r}, status={self.status!r}, kind={self.kind!r})"


class TransportError(BotError):
    """The transport itself failed: no sidecar binary, no shared library, a dead pipe."""

    kind = "transport"


class InvalidArgumentError(BotError):
    """A caller-supplied value was rejected before any request was made."""

    kind = "invalid_argument"


class NetworkError(BotError):
    """The request never reached a usable response: DNS, TLS, reset, timeout."""

    kind = "network"

    def __init__(self, message: str, **kwargs: Any) -> None:
        kwargs.setdefault("retryable", True)
        super().__init__(message, **kwargs)


class BadRequestError(BotError):
    """The server rejected the request as malformed (``400``)."""

    kind = "bad_request"


class UnauthorizedError(BotError):
    """The token is unknown, reset, expired, or the bot is banned (``401``).

    Do not retry: surface it and stop the bot (``bots.md`` 15.2).
    """

    kind = "unauthorized"


class ForbiddenError(BotError):
    """Authenticated but not permitted (``403``).

    Inspect :attr:`BotError.permission` for the key the bot lacks.
    """

    kind = "forbidden"


class NotFoundError(BotError):
    """The target does not exist, or is invisible to this bot (``404``)."""

    kind = "not_found"


class ConflictError(BotError):
    """A duplicate or an already-pending operation (``409``)."""

    kind = "conflict"


class RateLimitedError(BotError):
    """A quota was exhausted (``429``). Honour :attr:`BotError.retry_after_secs`."""

    kind = "rate_limited"

    def __init__(self, message: str, **kwargs: Any) -> None:
        kwargs.setdefault("retryable", True)
        super().__init__(message, **kwargs)


class ServerError(BotError):
    """The server failed (``5xx``); safe to retry with backoff."""

    kind = "server_error"

    def __init__(self, message: str, **kwargs: Any) -> None:
        kwargs.setdefault("retryable", True)
        super().__init__(message, **kwargs)


class DecodeError(BotError):
    """A response body did not match the expected shape."""

    kind = "decode"


class TimeoutError(BotError):  # noqa: A001 - shadowing the builtin is intentional here
    """The sidecar or shared library did not answer in time."""

    kind = "timeout"

    def __init__(self, message: str, **kwargs: Any) -> None:
        kwargs.setdefault("retryable", True)
        super().__init__(message, **kwargs)


class ClosedError(BotError):
    """The client or event stream has already been closed."""

    kind = "closed"


#: Every category the SDK can report, mapped to its class.
ERROR_KINDS: dict[str, type[BotError]] = {
    "transport": TransportError,
    "invalid_argument": InvalidArgumentError,
    "network": NetworkError,
    "bad_request": BadRequestError,
    "unauthorized": UnauthorizedError,
    "forbidden": ForbiddenError,
    "not_found": NotFoundError,
    "conflict": ConflictError,
    "rate_limited": RateLimitedError,
    "server_error": ServerError,
    "decode": DecodeError,
    "timeout": TimeoutError,
    "closed": ClosedError,
    "other": BotError,
}


def error_from_envelope(error: dict[str, Any]) -> BotError:
    """Build the right exception from an error object.

    Accepts the shape both transports use::

        {"kind": "http", "status": 403, "message": "http 403: permission denied",
         "permission": "posts.create", "retryable": false, "body": {…}}

    ``kind`` may be the SDK's own category (``forbidden``) or the Rust enum tag
    (``http``); when it is the latter the status decides the class.
    """
    if not isinstance(error, dict):
        return BotError(str(error))

    status = error.get("status")
    kind = error.get("kind") or "other"

    # The Rust error enum tags an HTTP failure as `http`; the class then follows the
    # status, which is what callers actually branch on.
    if kind == "http":
        kind = _kind_for_status(status)

    cls = ERROR_KINDS.get(kind, BotError)
    details: dict[str, Any] = {
        "status": status,
        "permission": error.get("permission"),
        "retry_after_secs": error.get("retry_after_secs"),
        "body": error.get("body") or {},
        "api_error": error.get("api_error"),
        "kind": kind,
    }
    # Only override the class's own verdict when the server actually expressed one: an
    # absent field must not turn a retryable 429 into a permanent failure.
    if error.get("retryable") is not None:
        details["retryable"] = bool(error["retryable"])

    return cls(error.get("message") or f"request failed ({kind})", **details)


def _kind_for_status(status: Any) -> str:
    """Map an HTTP status onto an SDK category."""
    if not isinstance(status, int):
        return "other"
    if status == 400:
        return "bad_request"
    if status == 401:
        return "unauthorized"
    if status == 403:
        return "forbidden"
    if status == 404:
        return "not_found"
    if status == 409:
        return "conflict"
    if status == 429:
        return "rate_limited"
    if 500 <= status < 600:
        return "server_error"
    return "other"
