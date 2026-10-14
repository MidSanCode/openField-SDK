"""The :class:`Bot` facade: one Python API over both transports.

``Bot`` adds nothing the Rust core cannot do -- it maps Python names onto the shared
method index and turns the documented bot recipes (``bots.md`` sections 7, 9 and 12) into
small helpers. Everything it can do is also reachable through :meth:`Bot.call`, so a
missing wrapper is never a limitation.

Two habits the helpers encode, because they are easy to get wrong by hand:

* **Never act on your own messages.** The event channel echoes what the bot sends, and a
  bot that reacts to its own output loops. :meth:`Bot.watch` stops that by tracking the
  bot's own id and skipping its messages, so a handler cannot forget.
* **Reconcile after a disconnect.** The server relays events best-effort and does not
  replay them (:mod:`openfield_bot` docs, ``bots.md`` 9.3), so :meth:`Bot.watch` polls as
  well as subscribes.
"""

from __future__ import annotations

import base64
import os
import queue
import threading
import time
from pathlib import Path
from typing import Any, Iterator, Optional

from . import _transport
from ._transport import Transport
from .errors import BotError, InvalidArgumentError, UnauthorizedError

__all__ = ["Bot", "Event", "REACTIONS", "VISIBILITIES"]

#: The reaction keys the server accepts, in its own order (``bots.md`` 7.5).
REACTIONS = ("like", "dislike", "love", "haha", "wow", "sad", "angry")

#: The visibility values the server accepts (``bots.md`` 7.4).
VISIBILITIES = ("public", "login", "friends", "private")


class Event(dict):
    """One realtime event.

    A plain ``dict`` subclass, so ``event["data"]`` works and so does ``event.type``.
    """

    @property
    def type(self) -> str:
        """The event type, e.g. ``"chat.message.created"``."""
        return self.get("event", "")

    @property
    def data(self) -> dict[str, Any]:
        """The event payload."""
        return self.get("data") or {}

    @property
    def recipients(self) -> list[Any]:
        """The addressed user ids; empty means the event was broadcast."""
        return self.get("recipients") or []

    def is_broadcast(self) -> bool:
        """Whether the event was sent to everyone."""
        return not self.recipients

    def __repr__(self) -> str:  # pragma: no cover - trivial
        return f"<Event {self.type} keys={sorted(self.data)[:6]}>"


class _EventAdapter:
    """Wrap a transport's event iterator so it yields :class:`Event` objects.

    A thin adapter rather than a generator, because a generator would postpone the
    subscription to the first ``next()``, and a caller that checks the server before
    iterating would then see nothing.
    """

    def __init__(self, inner: Any) -> None:
        self._inner = inner

    def __iter__(self) -> "_EventAdapter":
        return self

    def __next__(self) -> Event:
        return Event(next(self._inner))

    def close(self) -> None:
        """Drop the subscription."""
        self._inner.close()

    @property
    def stats(self) -> dict[str, Any]:
        """The stream counters, when the backend exposes them."""
        return getattr(self._inner, "stats", {}) or {}

    def __enter__(self) -> "_EventAdapter":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    def __repr__(self) -> str:  # pragma: no cover - trivial
        return f"<BotEvents via {self._inner!r}>"


class Bot:
    """An OpenField bot client.

    Args:
        base_url: the instance root, e.g. ``"https://openfield.example.com"``. Defaults to
            the server's own default of ``http://localhost:8080``.
        token: the bot token, ``ofb_`` plus 64 lowercase hex characters. Omit it for the
            public endpoints only.
        backend: ``"sidecar"``, ``"ffi"``, or ``"auto"`` (default). ``auto`` prefers the
            sidecar, because it needs no ABI match and cannot segfault the interpreter.
        sidecar_path: an explicit sidecar binary, overriding discovery.
        library_path: an explicit shared library, overriding discovery.
        timeout_ms: per-request timeout. The default is the server's own 30 s.
        connect_timeout_ms: connection-establishment timeout.
        user_agent: the ``User-Agent`` header; identify your bot, as ``bots.md`` 15.2 asks.
        headers: extra headers sent on every request.
        max_response_bytes: refuse a response larger than this.
        retry: overrides for the retry policy, e.g. ``{"max_attempts": 5}``. Applied by
            the Rust core: GET and HEAD retry on transport failures and 5xx, every verb
            retries on 429 honouring ``Retry-After``, and a mutating verb is never
            replayed on 5xx.
        reconnect: overrides for the event-stream reconnect policy.
        transport: an already-built transport, for tests or a custom backend.

    A ``Bot`` is a context manager, so ``with Bot(...) as bot:`` closes the transport on
    exit.
    """

    def __init__(
        self,
        base_url: Optional[str] = None,
        token: Optional[str] = None,
        *,
        backend: str = "auto",
        sidecar_path: Optional[str | os.PathLike[str]] = None,
        library_path: Optional[str | os.PathLike[str]] = None,
        timeout_ms: Optional[int] = None,
        connect_timeout_ms: Optional[int] = None,
        user_agent: Optional[str] = None,
        headers: Optional[dict[str, str]] = None,
        max_response_bytes: Optional[int] = None,
        retry: Optional[dict[str, Any]] = None,
        reconnect: Optional[dict[str, Any]] = None,
        transport: Optional[Transport] = None,
    ) -> None:
        self._me: Optional[dict[str, Any]] = None
        self._own_id: Optional[int] = None

        if transport is not None:
            self._transport = transport
            return

        options: dict[str, Any] = {}
        if base_url:
            options["base_url"] = base_url
        if token:
            options["token"] = token
        if timeout_ms is not None:
            options["timeout_ms"] = timeout_ms
        if connect_timeout_ms is not None:
            options["connect_timeout_ms"] = connect_timeout_ms
        if user_agent is not None:
            options["user_agent"] = user_agent
        if headers:
            options["headers"] = headers
        if max_response_bytes is not None:
            options["max_response_bytes"] = max_response_bytes
        if retry is not None:
            options["retry"] = retry
        if reconnect is not None:
            options["reconnect"] = reconnect

        self._transport = self._build_transport(backend, options, sidecar_path, library_path)

    @staticmethod
    def _build_transport(
        backend: str,
        options: dict[str, Any],
        sidecar_path: Optional[str | os.PathLike[str]],
        library_path: Optional[str | os.PathLike[str]],
    ) -> Transport:
        """Pick a backend, honouring ``backend`` and falling back only when asked."""
        from .ffi import FfiTransport
        from .sidecar import SidecarTransport

        choice = backend.lower()
        if choice == "sidecar":
            return SidecarTransport(sidecar_path, options=options)
        if choice == "ffi":
            return FfiTransport(library_path, options=options)
        if choice != "auto":
            raise InvalidArgumentError(
                f"unknown backend {backend!r}; expected 'auto', 'sidecar' or 'ffi'"
            )

        try:
            return SidecarTransport(sidecar_path, options=options)
        except BotError:
            # Only a missing sidecar justifies the fallback; a crash or a bad
            # configuration should surface as itself rather than as a second failure.
            return FfiTransport(library_path, options=options)

    # -- plumbing ----------------------------------------------------------

    @property
    def transport(self) -> Transport:
        """The active backend, for diagnostics."""
        return self._transport

    @property
    def backend(self) -> str:
        """The active backend's name."""
        return self._transport.name

    def call(self, method: str, **params: Any) -> Any:
        """Run any method from the shared index and return its result.

        This is the escape hatch for anything without a wrapper::

            bot.call("posts.create", content="hello", visibility="public")
            bot.call("chat.settings", conversation_id=33,
                     settings={"is_public": False, "allow_join": True})

        Raises:
            openfield_bot.BotError: for any failure the SDK reports.
        """
        return self._transport.call(method, params or None)

    def capabilities(self) -> dict[str, Any]:
        """The SDK's protocol revision, feature flags and every method name."""
        return self.call("capabilities")

    def version(self) -> dict[str, Any]:
        """The SDK version and protocol revision."""
        return self.call("version")

    def close(self) -> None:
        """Release the transport. Safe to call more than once."""
        self._transport.close()

    def __enter__(self) -> "Bot":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    def __repr__(self) -> str:  # pragma: no cover - trivial
        who = self._me.get("username") if self._me else None
        return f"<Bot {who or 'unauthenticated'} via {self._transport.name}>"

    # -- identity ----------------------------------------------------------

    def me(self, *, refresh: bool = False) -> dict[str, Any]:
        """This bot's own user record, cached after the first call."""
        if self._me is None or refresh:
            self._me = self.call("me")
            self._own_id = self._me.get("id")
        return self._me

    @property
    def id(self) -> Optional[int]:
        """This bot's user id, fetching it on first use."""
        if self._own_id is None:
            self.me()
        return self._own_id

    @property
    def username(self) -> Optional[str]:
        """This bot's username."""
        return self.me().get("username")

    def permissions(self) -> dict[str, Any]:
        """The permission keys this bot holds, plus the groups they came from."""
        return self.call("permissions")

    def startup_check(self) -> dict[str, Any]:
        """Validate the token and report every administrative permission that is missing.

        Run this once at startup: a bot that starts without checking discovers a missing
        permission as a ``403`` in the middle of a job (``bots.md`` 16.1).
        """
        return self.call("startup_check")

    def require_permission(self, permission: str) -> None:
        """Fail early with :class:`UnauthorizedError` when a permission is missing.

        Args:
            permission: a permission key, e.g. ``"posts.create"``.

        Raises:
            UnauthorizedError: naming the missing key, so the operator knows what to grant.
        """
        held = self.permissions()
        if permission not in (held.get("permissions") or []):
            raise UnauthorizedError(
                f"this bot lacks the {permission!r} permission; ask an administrator to "
                f"grant it to the bot's role",
                permission=permission,
            )

    def update_profile(self, **profile: Any) -> dict[str, Any]:
        """Update the bot's profile.

        Accepts ``username``, ``nickname``, ``bio``. The nickname limit is measured in
        Unicode code points, not bytes (``bots.md`` 6.4).
        """
        return self.call("update_profile", profile=profile)

    def user(self, user_id: int) -> dict[str, Any]:
        """One user's public profile."""
        return self.call("user", user_id=user_id)

    def search_users(self, query: str) -> Any:
        """Search users by name."""
        return self.call("search_users", query=query)

    def follow(self, user_id: int) -> Any:
        """Follow a user."""
        return self.call("follow", user_id=user_id)

    def unfollow(self, user_id: int) -> Any:
        """Unfollow a user."""
        return self.call("unfollow", user_id=user_id)

    def storage_usage(self) -> dict[str, Any]:
        """Attachment storage usage for this bot."""
        return self.call("storage_usage")

    def storage_buckets(self) -> Any:
        """The storage buckets available to this bot."""
        return self.call("storage_buckets")

    def set_storage_bucket(self, bucket: str) -> Any:
        """Choose which bucket new uploads go to."""
        return self.call("set_storage_bucket", bucket=bucket)

    # -- bots --------------------------------------------------------------

    def create_bot(self, username: str, nickname: str) -> dict[str, Any]:
        """Create a new bot under this account.

        The returned token is shown **once**; store it before doing anything else. A
        single owner may hold at most 10 bots (``bots.md`` 5.3).

        Note:
            A username must be 3-32 characters of ``[A-Za-z0-9_]``.
        """
        return self.call("bots.create", username=username, nickname=nickname)

    def list_bots(self) -> dict[str, Any]:
        """Every bot this account owns, with quotas."""
        return self.call("bots.list")

    def regenerate_token(self, bot_id: int, *, adopt: bool = False) -> dict[str, Any]:
        """Rotate a bot's token.

        Args:
            bot_id: the bot whose token to rotate.
            adopt: switch this client to the new token immediately. Use it when the
                rotated bot is the one this client runs as, because the old token dies at
                once (``bots.md`` 5.4).
        """
        return self.call("bots.regenerate", bot_id=bot_id, adopt=adopt)

    def delete_bot(self, bot_id: int) -> Any:
        """Permanently delete a bot and its token."""
        return self.call("bots.delete", bot_id=bot_id)

    def find_bot(self, username: str) -> Optional[dict[str, Any]]:
        """Look a bot up by username, or ``None``."""
        for bot in self.list_bots().get("bots", []):
            if bot.get("username") == username:
                return bot
        return None

    # -- posts -------------------------------------------------------------

    def publish(self, content: str, **options: Any) -> dict[str, Any]:
        """Publish a post.

        Args:
            content: the body. At most 5000 **bytes**, which for CJK text is well under
                5000 characters.
            **options: ``visibility``, ``attachments`` (a list of attachment ids),
                ``camp_id``, ``quoted_post_id``.

        Raises:
            InvalidArgumentError: for content over the byte limit, or more than 9
                attachments.
        """
        return self.call("posts.create", content=content, **options)

    def posts(self, *, page: Optional[int] = None, limit: Optional[int] = None) -> dict[str, Any]:
        """The global post feed."""
        return self.call("posts.list", **{k: v for k, v in {"page": page, "limit": limit}.items() if v is not None})

    def search_posts(self, query: Optional[str] = None, **filters: Any) -> dict[str, Any]:
        """Search posts by keyword, camp, author, and so on."""
        params = dict(filters)
        if query is not None:
            params["q"] = query
        return self.call("posts.search", query=params)

    def post(self, post_id: int) -> dict[str, Any]:
        """One post with its aggregate counters."""
        return self.call("posts.get", post_id=post_id)

    def update_post(self, post_id: int, post: dict[str, Any]) -> dict[str, Any]:
        """Replace a post's editable fields."""
        return self.call("posts.update", post_id=post_id, post=post)

    def delete_post(self, post_id: int) -> Any:
        """Delete a post."""
        return self.call("posts.delete", post_id=post_id)

    def pin_post(self, post_id: int, pinned: bool = True) -> Any:
        """Pin or unpin a post."""
        return self.call("posts.pin", post_id=post_id, pinned=pinned)

    def react(self, post_id: int, reaction: str) -> Any:
        """React to a post.

        Args:
            reaction: one of :data:`REACTIONS`. The server rejects anything else, and
                enforces one reaction per user per post.
        """
        if reaction not in REACTIONS:
            raise InvalidArgumentError(
                f"unknown reaction {reaction!r}; expected one of {', '.join(REACTIONS)}"
            )
        return self.call("posts.react", post_id=post_id, reaction=reaction)

    def unreact(self, post_id: int) -> Any:
        """Remove this bot's reaction from a post."""
        return self.call("posts.unreact", post_id=post_id)

    def favorite(self, post_id: int, *, favorite: bool = True) -> Any:
        """Add or remove a post from the bot's favourites."""
        method = "posts.favorite" if favorite else "posts.unfavorite"
        return self.call(method, post_id=post_id)

    def reply(self, post_id: int, content: str, **options: Any) -> dict[str, Any]:
        """Reply to a post, with the same byte limit as a post."""
        return self.call("posts.reply", post_id=post_id, content=content, **options)

    def replies(
        self, post_id: int, *, page: Optional[int] = None, limit: Optional[int] = None
    ) -> dict[str, Any]:
        """A post's replies, oldest first."""
        params = {
            key: value for key, value in {"page": page, "limit": limit}.items() if value is not None
        }
        return self.call("posts.replies", post_id=post_id, **params)

    # -- chat --------------------------------------------------------------

    def conversations(self) -> Any:
        """Every conversation this bot belongs to."""
        return self.call("chat.conversations")

    def create_conversation(self, title: str) -> dict[str, Any]:
        """Create a group conversation.

        Args:
            title: required -- the server binds it and rejects an empty one (``bots.md``
                8.1).
        """
        return self.call("chat.create", title=title)

    def conversation(self, conversation_id: int) -> dict[str, Any]:
        """One conversation's metadata, including whether this bot is a member."""
        return self.call("chat.get", conversation_id=conversation_id)

    def update_conversation(
        self,
        conversation_id: int,
        *,
        is_public: Optional[bool] = None,
        allow_join: Optional[bool] = None,
        encrypted: Optional[bool] = None,
    ) -> dict[str, Any]:
        """Change a conversation's settings. Only the fields you pass are sent."""
        settings = {
            key: value
            for key, value in {
                "is_public": is_public,
                "allow_join": allow_join,
                "encrypted": encrypted,
            }.items()
            if value is not None
        }
        return self.call("chat.settings", conversation_id=conversation_id, settings=settings)

    def messages(
        self,
        conversation_id: int,
        *,
        limit: Optional[int] = None,
        before: Optional[int] = None,
    ) -> dict[str, Any]:
        """A page of history, **newest first**.

        Note:
            The bundled ``bots.md`` 7.3 claims this is oldest-first. It is not: the
            handler sorts by ``created_at DESC, id DESC``. Sort explicitly if you need
            chronological order.
        """
        params = {
            key: value
            for key, value in {"limit": limit, "before": before}.items()
            if value is not None
        }
        return self.call("chat.messages", conversation_id=conversation_id, **params)

    def latest_messages(
        self, conversation_id: int, *, limit: Optional[int] = None
    ) -> dict[str, Any]:
        """The newest messages, for a first run or after a reconnect."""
        params = {"limit": limit} if limit is not None else {}
        return self.call("chat.latest", conversation_id=conversation_id, **params)

    def send_message(
        self,
        conversation_id: int,
        content: str,
        *,
        reply_to_id: Optional[int] = None,
        mentions: Optional[list[int]] = None,
        burn_seconds: Optional[int] = None,
        attachments: Optional[list[int]] = None,
    ) -> dict[str, Any]:
        """Send a chat message.

        Args:
            conversation_id: where to send it.
            content: the body, at most 5000 **bytes**.
            reply_to_id: the message this one answers.
            mentions: user ids to notify. ``-1`` (``MENTION_EVERYONE``) pings everyone.
            burn_seconds: delete the message after this long. The server accepts ``0``
                (never) or ``5..=86400`` (``bots.md`` 9.4).
            attachments: attachment ids to attach.
        """
        params: dict[str, Any] = {"conversation_id": conversation_id, "content": content}
        if reply_to_id is not None:
            params["reply_to_id"] = reply_to_id
        if mentions:
            params["mentions"] = mentions
        if burn_seconds is not None:
            params["burn_seconds"] = burn_seconds
        if attachments:
            params["attachments"] = attachments
        return self.call("chat.send", **params)

    def edit_message(self, conversation_id: int, message_id: int, content: str) -> dict[str, Any]:
        """Edit a message this bot sent."""
        return self.call(
            "chat.edit",
            conversation_id=conversation_id,
            message_id=message_id,
            content=content,
        )

    def delete_message(self, conversation_id: int, message_id: int) -> Any:
        """Delete a message."""
        return self.call("chat.delete", conversation_id=conversation_id, message_id=message_id)

    def mark_read(self, conversation_id: int, last_message_id: int = 0) -> Any:
        """Advance this bot's read cursor.

        Args:
            last_message_id: the newest message seen; ``0`` marks the whole conversation
                read. The server binds this field name (``bots.md`` 8.7).
        """
        return self.call(
            "chat.read", conversation_id=conversation_id, last_message_id=last_message_id
        )

    def typing(self, conversation_id: int) -> Any:
        """Show a typing indicator in a conversation."""
        return self.call("chat.typing", conversation_id=conversation_id)

    def join_conversation(self, conversation_id: int) -> Any:
        """Join a public conversation."""
        return self.call("chat.join", conversation_id=conversation_id)

    def invite(
        self, conversation_id: int, user_id: int, message: Optional[str] = None
    ) -> Any:
        """Invite a user.

        The server files a **consent request** rather than adding them directly, which
        the invitee must accept (``bots.md`` 8.5); watch for the
        ``chat.consent.requested`` event.
        """
        params: dict[str, Any] = {"conversation_id": conversation_id, "user_id": user_id}
        if message is not None:
            params["message"] = message
        return self.call("chat.invite", **params)

    def leave_conversation(self, conversation_id: int) -> Any:
        """Leave a conversation."""
        return self.call("chat.leave", conversation_id=conversation_id)

    def remove_member(self, conversation_id: int, user_id: int) -> Any:
        """Remove a member."""
        return self.call(
            "chat.remove_member", conversation_id=conversation_id, user_id=user_id
        )

    def start_conversation(
        self, user_id: int, message: Optional[str] = None, *, encrypted: bool = False
    ) -> dict[str, Any]:
        """Open (or reuse) a direct conversation with a user."""
        params: dict[str, Any] = {"user_id": user_id, "encrypted": encrypted}
        if message is not None:
            params["message"] = message
        return self.call("chat.start", **params)

    def consent_requests(self) -> Any:
        """Pending consent requests addressed to this bot."""
        return self.call("chat.consent_requests")

    def accept_consent(self, request_id: int) -> Any:
        """Accept a consent request."""
        return self.call("chat.accept_consent", request_id=request_id)

    def decline_consent(self, request_id: int) -> Any:
        """Decline a consent request."""
        return self.call("chat.decline_consent", request_id=request_id)

    def conversation_files(self, conversation_id: int) -> Any:
        """Files shared in a conversation."""
        return self.call("chat.files", conversation_id=conversation_id)

    # -- camps -------------------------------------------------------------

    def camps(self, **filters: Any) -> Any:
        """List camps, with optional ``q``/``page``/``limit`` filters."""
        return self.call("camps.list", query=filters)

    def camp(self, camp_id: int) -> dict[str, Any]:
        """One camp."""
        return self.call("camps.get", camp_id=camp_id)

    def create_camp(self, name: str, **options: Any) -> dict[str, Any]:
        """Create a camp.

        Args:
            name: required, and unique across the instance.
            **options: ``description``, ``is_visible``, ``direct_join``, ``member_post``,
                ``member_pin``, ``parent_camp_id``.
        """
        return self.call("camps.create", camp={"name": name, **options})

    def update_camp(self, camp_id: int, **changes: Any) -> dict[str, Any]:
        """Change some of a camp's fields."""
        return self.call("camps.update", camp_id=camp_id, update=changes)

    def delete_camp(self, camp_id: int) -> Any:
        """Delete a camp."""
        return self.call("camps.delete", camp_id=camp_id)

    def camp_posts(
        self, camp_id: int, *, page: Optional[int] = None, limit: Optional[int] = None
    ) -> dict[str, Any]:
        """A camp's posts."""
        params = {
            key: value for key, value in {"page": page, "limit": limit}.items() if value is not None
        }
        return self.call("camps.posts", camp_id=camp_id, **params)

    def join_camp(self, camp_id: int) -> Any:
        """Join a camp."""
        return self.call("camps.join", camp_id=camp_id)

    def leave_camp(self, camp_id: int) -> Any:
        """Leave a camp."""
        return self.call("camps.leave", camp_id=camp_id)

    def camp_members(self, camp_id: int) -> Any:
        """A camp's members."""
        return self.call("camps.members", camp_id=camp_id)

    def camp_subgroups(self, camp_id: int) -> Any:
        """A camp's subgroups."""
        return self.call("camps.subgroups", camp_id=camp_id)

    def post_to_camp(self, camp_id: int, content: str, **options: Any) -> dict[str, Any]:
        """Publish a post into a camp."""
        return self.call("camps.post", camp_id=camp_id, content=content, **options)

    # -- storage -----------------------------------------------------------

    def upload(
        self, file_name: str, data: bytes, *, mime: Optional[str] = None
    ) -> dict[str, Any]:
        """Upload bytes and return the attachment record.

        Args:
            file_name: the name the server records.
            data: the raw bytes. They cross as base64, so a large file does not become a
                million-element JSON array.
            mime: overrides the type guessed from the extension.
        """
        upload: dict[str, Any] = {
            "file_name": file_name,
            "bytes": base64.b64encode(data).decode("ascii"),
        }
        if mime is not None:
            upload["mime"] = mime
        return self.call("storage.upload", upload=upload)

    def upload_file(self, path: str | os.PathLike[str], **options: Any) -> dict[str, Any]:
        """Upload a file from disk."""
        target = Path(path)
        return self.upload(target.name, target.read_bytes(), **options)

    def uploads(self, files: list[dict[str, Any]]) -> Any:
        """Upload several files in one call.

        Each entry is ``{"file_name": ..., "bytes": <base64 or list>, "mime": ...}``.
        """
        return self.call("storage.upload_many", uploads=files)

    def attachments(self) -> Any:
        """This bot's uploaded attachments."""
        return self.call("storage.list")

    def delete_attachment(self, attachment_id: int) -> Any:
        """Delete an attachment."""
        return self.call("storage.delete", attachment_id=attachment_id)

    def attachment(self, attachment_id: int) -> dict[str, Any]:
        """Fetch one attachment record by id."""
        return self.call("storage.attachment", attachment_id=attachment_id)

    def attachment_by_hash(self, hex_digest: str) -> dict[str, Any]:
        """Look an attachment up by the lowercase hex SHA-256 of its bytes.

        Deduplication is per user, so this only finds blobs this bot uploaded.

        Raises:
            InvalidArgumentError: the digest is not 64 hex characters.
            NotFoundError: nothing matches.
        """
        digest = _require_sha256(hex_digest)
        return self.call("storage.by_hash", hash=digest)

    def arm_attachment_burn(self, attachment_id: int, burn_seconds: int) -> dict[str, Any]:
        """Arm a burn-after-view countdown of at most 24 hours.

        The deadline starts on the **first view by someone else**; the uploader can
        never trigger it, and for the uploader the server answers 404 exactly as it
        does for an unknown id.

        Args:
            attachment_id: the attachment to arm.
            burn_seconds: 1..86400.

        Raises:
            InvalidArgumentError: ``burn_seconds`` is outside ``1..86400``.
        """
        seconds = _require_int("burn_seconds", burn_seconds)
        if not 1 <= seconds <= 86_400:
            raise InvalidArgumentError(f"burn_seconds must be 1..86400, got {seconds}")
        return self.call(
            "storage.burn_arm", attachment_id=attachment_id, burn_seconds=seconds
        )

    # -- chunked uploads ---------------------------------------------------

    #: Largest single chunk the server accepts.
    CHUNK_BYTES = 8 * 1024 * 1024
    #: Largest number of chunks per session.
    MAX_CHUNKS = 10_000

    def start_chunked_upload(
        self,
        filename: str,
        size: int,
        total_chunks: int,
        *,
        mime_type: Optional[str] = None,
        visibility: Optional[str] = None,
    ) -> dict[str, Any]:
        """Open a resumable upload session and return ``{"upload_id", "total_chunks"}``.

        Raises:
            InvalidArgumentError: a part count above :attr:`MAX_CHUNKS`, or a ``size``
                the declared chunk count cannot reach.
            PayloadTooLargeError: above the upload cap or the storage quota.
        """
        upload = self._chunk_layout(
            filename, size, total_chunks, mime_type=mime_type, visibility=visibility
        )
        return self.call("storage.chunk_init", upload=upload)

    def chunk_status(self, upload_id: str) -> dict[str, Any]:
        """Which 1-based chunk indexes already landed, for a resume."""
        return self.call("storage.chunk_status", upload_id=upload_id)

    def upload_chunk(self, upload_id: str, index: int, data: bytes) -> dict[str, Any]:
        """Store one chunk. Indexes are 1-based; the multipart field is ``chunk``.

        Raises:
            InvalidArgumentError: an index below 1, or a chunk above :attr:`CHUNK_BYTES`.
            NotFoundError: the session is gone or belongs to someone else.
        """
        number = _require_int("index", index)
        if number < 1:
            raise InvalidArgumentError(f"chunk index is 1-based, got {number}")
        if len(data) > self.CHUNK_BYTES:
            raise InvalidArgumentError(
                f"chunk is {len(data)} bytes, above the {self.CHUNK_BYTES} byte limit"
            )
        part = {
            "upload_id": upload_id,
            "index": number,
            "bytes": base64.b64encode(data).decode("ascii"),
        }
        return self.call("storage.chunk_upload", part=part)

    def complete_chunked_upload(
        self,
        upload_id: str,
        filename: str,
        size: int,
        total_chunks: int,
        *,
        mime_type: Optional[str] = None,
        visibility: Optional[str] = None,
    ) -> dict[str, Any]:
        """Assemble the stored chunks into an attachment.

        Raises:
            ConflictError: chunks are missing; the message names them.
        """
        upload = self._chunk_layout(
            filename, size, total_chunks, mime_type=mime_type, visibility=visibility
        )
        return self.call("storage.chunk_complete", upload_id=upload_id, upload=upload)

    def upload_chunked(
        self,
        file_name: str,
        data: bytes,
        *,
        chunk_bytes: Optional[int] = None,
        upload_id: Optional[str] = None,
        mime_type: Optional[str] = None,
        visibility: Optional[str] = None,
    ) -> dict[str, Any]:
        """Upload a large payload through the chunked path, resuming if asked.

        Args:
            file_name: the name the server records.
            data: the whole payload.
            chunk_bytes: part size; defaults to :attr:`CHUNK_BYTES` and is clamped.
            upload_id: continue an open session instead of starting one, so a retry
                re-sends only the parts the server is missing.
        """
        if not data:
            raise InvalidArgumentError("nothing to upload")
        size = chunk_bytes if chunk_bytes is not None else self.CHUNK_BYTES
        size = min(max(1, int(size)), self.CHUNK_BYTES)
        parts = [data[offset : offset + size] for offset in range(0, len(data), size)]
        upload = self._chunk_layout(
            file_name,
            len(data),
            len(parts),
            mime_type=mime_type,
            visibility=visibility,
        )
        session_id = upload_id
        if session_id is None:
            session_id = self.call("storage.chunk_init", upload=upload)["upload_id"]
        stored = set(self.call("storage.chunk_status", upload_id=session_id).get("uploaded") or ())
        for index, part in enumerate(parts, start=1):
            # Resuming skips whatever the server already holds.
            if index not in stored:
                self.upload_chunk(session_id, index, part)
        return self.call("storage.chunk_complete", upload_id=session_id, upload=upload)

    def _chunk_layout(
        self,
        filename: str,
        size: int,
        total_chunks: int,
        *,
        mime_type: Optional[str],
        visibility: Optional[str],
    ) -> dict[str, Any]:
        """Build and check the ``init``/``complete`` body, which share one shape."""
        if not filename or not filename.strip():
            raise InvalidArgumentError("filename is required")
        size = _require_int("size", size)
        total_chunks = _require_int("total_chunks", total_chunks)
        if size <= 0:
            raise InvalidArgumentError(f"size must be positive, got {size}")
        if total_chunks <= 0:
            raise InvalidArgumentError(f"total_chunks must be positive, got {total_chunks}")
        if total_chunks > self.MAX_CHUNKS:
            raise InvalidArgumentError(
                f"total_chunks must be at most {self.MAX_CHUNKS}, got {total_chunks}"
            )
        if size > total_chunks * self.CHUNK_BYTES:
            raise InvalidArgumentError(
                f"declared size {size} exceeds what {total_chunks} chunks can carry"
            )
        upload: dict[str, Any] = {
            "filename": filename,
            "size": size,
            "total_chunks": total_chunks,
        }
        if mime_type is not None:
            upload["mime_type"] = mime_type
        if visibility is not None:
            upload["visibility"] = visibility
        return upload

    # -- wallet ------------------------------------------------------------

    def wallet(self, *, page: Optional[int] = None, limit: Optional[int] = None) -> dict[str, Any]:
        """This bot's balance and a page of its transactions.

        Amounts are **decimal coins**, the shape the server's ``model.Cents`` puts on
        the wire: ``10`` is ten coins and ``12.5`` is twelve and a half. Keep cents in
        your own arithmetic if you need exactness.
        """
        params: dict[str, Any] = {}
        if page is not None:
            params["page"] = page
        if limit is not None:
            params["limit"] = limit
        return self.call("wallet.get", **params)

    def recharge_wallet(
        self, user_id: int, amount: float, *, description: Optional[str] = None
    ) -> dict[str, Any]:
        """Credit ``amount`` coins to ``user_id``. Needs ``wallet.manage``.

        The default group holds ``wallet.manage``, so any authenticated bot can move
        any user's balance until the server tightens group permissions.
        """
        return self._adjust_wallet("wallet.recharge", user_id, amount, description)

    def deduct_wallet(
        self, user_id: int, amount: float, *, description: Optional[str] = None
    ) -> dict[str, Any]:
        """Debit ``amount`` coins from ``user_id``. Needs ``wallet.manage``.

        Unlike :meth:`adjust_wallet`, the sign is handled here: the server stores the
        amount verbatim and never checks it against the transaction type, so a
        positive value would silently become a credit.

        Raises:
            BadRequestError: the deduction would drive the balance below zero.
        """
        return self._adjust_wallet("wallet.deduct", user_id, amount, description)

    def adjust_wallet(
        self,
        user_id: int,
        amount: float,
        *,
        type: Optional[str] = None,  # noqa: A002 - mirrors the API's own field name
        description: Optional[str] = None,
    ) -> dict[str, Any]:
        """Apply a signed balance change, exactly as the wire takes it.

        Args:
            user_id: whose wallet.
            amount: **signed** decimal coins. Positive credits, negative debits.
            type: ``recharge`` or ``deduct``. The server records it as a label and
                does **not** reconcile it with the sign, so a positive ``amount``
                with ``type="deduct"`` is a credit.
            description: optional operator note.
        """
        params: dict[str, Any] = {
            "user_id": _require_int("user_id", user_id),
            "amount": _require_amount("amount", amount),
        }
        if type is not None:
            params["type"] = type
        if description is not None:
            params["description"] = description
        return self.call("wallet.adjust", **params)

    def _adjust_wallet(
        self, method: str, user_id: int, amount: float, description: Optional[str]
    ) -> dict[str, Any]:
        params: dict[str, Any] = {
            "user_id": _require_int("user_id", user_id),
            "amount": _require_amount("amount", amount),
        }
        if description is not None:
            params["description"] = description
        return self.call(method, **params)

    # -- tasks, sign-in and experience -------------------------------------

    def tasks(self) -> dict[str, Any]:
        """The task catalog with this bot's progress, streak and make-up price.

        Currency figures here are **whole coins**, unlike the wallet's decimal coins.
        """
        return self.call("tasks.list")

    def claim_task(self, code: str) -> dict[str, Any]:
        """Claim a one-time or streak task: ``{"claimed", "exp", "currency"}``.

        Raises:
            NotFoundError: no such task code.
            ConflictError: not earned yet, or already claimed.
        """
        return self.call("tasks.claim", code=code)

    def claim_daily_task(self, code: str) -> dict[str, Any]:
        """Claim one of today's activity tiers. Each tier is once per local day.

        Raises:
            ConflictError: ``task requirements not met`` or ``task already claimed today``.
        """
        return self.call("tasks.claim_daily", code=code)

    def claim_daily_login(self) -> dict[str, Any]:
        """Sign in for today.

        A second call is not an error: it answers ``{"granted": False}``.
        """
        return self.call("tasks.login_claim")

    def checkin_calendar(self) -> dict[str, Any]:
        """The sign-in grid: ``registered_at``, ``today``, ``signed``, ``days``, …"""
        return self.call("tasks.login_calendar")

    def makeup_checkin(self) -> dict[str, Any]:
        """Pay to make up the most recent missed sign-in.

        A make-up renews the streak and grants experience but never the daily
        currency.

        Raises:
            ConflictError: the gap is wider than three days.
        """
        return self.call("tasks.makeup")

    def makeup_checkin_on(self, date: str) -> dict[str, Any]:
        """Pay to sign in one specific missed day (``YYYY-MM-DD``, game timezone).

        Raises:
            InvalidArgumentError: the date is not ``YYYY-MM-DD``.
            BadRequestError: the date is today, in the future, or before registration.
        """
        if len(date) != 10 or date[4] != "-" or date[7] != "-":
            raise InvalidArgumentError(f"date must be `YYYY-MM-DD`, got {date!r}")
        return self.call("tasks.makeup_date", date=date)

    def exp_history(self, *, limit: Optional[int] = None) -> Any:
        """Newest-first experience awards."""
        params: dict[str, Any] = {}
        if limit is not None:
            params["limit"] = limit
        return self.call("tasks.exp_history", **params)

    # -- membership and the payment PIN ------------------------------------

    def membership(self) -> dict[str, Any]:
        """This bot's membership state plus the purchasable catalog."""
        return self.call("membership.get")

    def purchase_membership(self, level: int, pin: str) -> dict[str, Any]:
        """Buy a membership tier with wallet coins, authorized by the payment PIN.

        Raises:
            BadRequestError: an invalid level, or a balance that cannot cover it.
            ForbiddenError: no payment PIN has been set (see :meth:`set_payment_pin`).
            UnauthorizedError: the PIN is wrong; it counts against a five-failure budget.
            RateLimitedError: that budget is exhausted (15-minute lockout).
        """
        level = _require_int("level", level)
        if not 1 <= level <= 4:
            raise InvalidArgumentError(f"membership level must be 1..4, got {level}")
        if not pin:
            raise InvalidArgumentError("pin must not be empty")
        return self.call("membership.purchase", level=level, pin=pin)

    def set_auto_renew(self, enabled: bool, *, pin: Optional[str] = None) -> dict[str, Any]:
        """Toggle automatic renewal.

        Enabling pre-authorizes future charges and needs ``pin``; disabling needs only
        the token.

        Raises:
            InvalidArgumentError: enabling without a PIN.
            ForbiddenError: enabling with no PIN configured on the account.
        """
        params: dict[str, Any] = {"enabled": bool(enabled)}
        if pin is not None:
            params["pin"] = pin
        elif enabled:
            raise InvalidArgumentError("enabling auto-renew needs the payment pin")
        return self.call("membership.auto_renew", **params)

    def membership_purchases(
        self, *, page: Optional[int] = None, limit: Optional[int] = None
    ) -> dict[str, Any]:
        """This bot's membership purchase history."""
        params: dict[str, Any] = {}
        if page is not None:
            params["page"] = page
        if limit is not None:
            params["limit"] = limit
        return self.call("membership.purchases", **params)

    def grant_membership(
        self, user_id: int, level: int, *, days: Optional[int] = None
    ) -> dict[str, Any]:
        """Grant a membership directly, skipping the wallet. Needs ``user.membership.grant``.

        ``level=0`` clears the membership.
        """
        params: dict[str, Any] = {
            "user_id": _require_int("user_id", user_id),
            "level": _require_int("level", level),
        }
        if days is not None:
            params["days"] = _require_int("days", days)
        return self.call("membership.grant", **params)

    def adjust_exp(
        self, user_id: int, delta: int, *, description: Optional[str] = None
    ) -> dict[str, Any]:
        """Adjust a user's experience. Needs ``user.adjust_exp``.

        ``delta`` is signed and the field is named ``delta``, not ``amount``. A
        positive delta is multiplied by the target's membership multiplier first.

        Returns:
            ``{"exp": ..., "level": ...}``.
        """
        params: dict[str, Any] = {
            "user_id": _require_int("user_id", user_id),
            "delta": _require_int("delta", delta),
        }
        if description is not None:
            params["description"] = description
        return self.call("membership.exp_adjust", **params)

    def set_payment_pin(self, pin: str, *, old_pin: Optional[str] = None) -> dict[str, Any]:
        """Set the six-digit payment PIN.

        Every outgoing payment -- tips, memberships and transfers -- is authorized by
        this PIN, and a freshly created bot has none, so this is what a spending bot
        must call first. Replacing an existing PIN requires ``old_pin``.

        Raises:
            InvalidArgumentError: the PIN is not exactly six digits.
            ForbiddenError: a PIN already exists and ``old_pin`` was omitted.
        """
        _require_pin(pin)
        params: dict[str, Any] = {"pin": pin}
        if old_pin is not None:
            params["old_pin"] = old_pin
        return self.call("account.pin_set", **params)

    def change_payment_pin(self, old_pin: str, pin: str) -> dict[str, Any]:
        """Replace the payment PIN, presenting the current one."""
        _require_pin(pin)
        if not old_pin:
            raise InvalidArgumentError("old_pin must not be empty")
        return self.call("account.pin_change", old_pin=old_pin, pin=pin)

    def verify_payment_pin(self, pin: str) -> bool:
        """Check a PIN without spending anything.

        A wrong PIN is **counted** against the same five-failure budget that tips and
        memberships use, so probing this can lock payments for fifteen minutes.
        """
        if not pin:
            raise InvalidArgumentError("pin must not be empty")
        return bool(self.call("account.pin_verify", pin=pin)["valid"])

    # -- moderation --------------------------------------------------------

    #: The punishment types the server accepts.
    PUNISHMENT_TYPES = (
        "warning",
        "demerit",
        "revoke",
        "temp_ban",
        "ban",
        "unban",
        "restore",
    )

    def punish(
        self,
        user_id: int,
        type: str,  # noqa: A002 - mirrors the API's own field name
        *,
        reason: Optional[str] = None,
        permission_key: Optional[str] = None,
        duration_minutes: Optional[int] = None,
    ) -> dict[str, Any]:
        """Record a moderation action. Needs ``user.punish``, which the default group lacks.

        Args:
            user_id: the sanctioned user. Not yourself: that is a 403.
            type: one of :attr:`PUNISHMENT_TYPES`.
            reason: optional free text.
            permission_key: required for ``revoke`` and ``restore``.
            duration_minutes: required for ``temp_ban``.

        Returns:
            ``{"punishment_id", "type", "expires_at"}``.
        """
        if type not in self.PUNISHMENT_TYPES:
            raise InvalidArgumentError(
                f"unknown punishment type {type!r}; expected one of "
                f"{', '.join(self.PUNISHMENT_TYPES)}"
            )
        if type in ("revoke", "restore") and not permission_key:
            raise InvalidArgumentError(f"{type} requires permission_key")
        if type == "temp_ban" and not duration_minutes:
            raise InvalidArgumentError("temp_ban requires a positive duration_minutes")
        punishment: dict[str, Any] = {"type": type}
        if reason is not None:
            punishment["reason"] = reason
        if permission_key is not None:
            punishment["permission_key"] = permission_key
        if duration_minutes is not None:
            punishment["duration_minutes"] = _require_int(
                "duration_minutes", duration_minutes
            )
        return self.call("moderation.punish", user_id=user_id, punishment=punishment)

    def punishments(self, user_id: int, *, limit: Optional[int] = None) -> dict[str, Any]:
        """A user's moderation history plus the permissions currently withheld.

        ``permission_bans`` is a list of permission *keys*, not records.
        """
        params: dict[str, Any] = {"user_id": _require_int("user_id", user_id)}
        if limit is not None:
            params["limit"] = limit
        return self.call("moderation.punishments", **params)

    # -- end-to-end encryption ---------------------------------------------

    def set_e2ee_public_key(self, public_key: str) -> dict[str, Any]:
        """Publish this bot's E2EE public key. An empty string clears it."""
        return self.call("e2ee.set_key", public_key=public_key)

    def e2ee_keys(self, conversation_id: int) -> dict[str, Any]:
        """Every group-key envelope plus this bot's own, for one conversation."""
        return self.call("e2ee.keys", conversation_id=conversation_id)

    def put_e2ee_keys(self, conversation_id: int, envelopes: dict[int, str]) -> dict[str, Any]:
        """Store a rotated batch of group-key envelopes.

        Args:
            conversation_id: the group the keys belong to. The caller must be a member.
            envelopes: ``{target_user_id: ciphertext}``. The server relays the
                ciphertext untouched and refuses an empty batch.
        """
        if not envelopes:
            raise InvalidArgumentError("at least one envelope is required")
        # Go binds `map[int64]string`, so the ids are JSON object keys.
        batch = {str(user_id): ciphertext for user_id, ciphertext in envelopes.items()}
        return self.call("e2ee.put_keys", conversation_id=conversation_id, envelopes=batch)

    # -- plugin store ------------------------------------------------------

    def plugins(self) -> dict[str, Any]:
        """The published plugin catalog. Public: no token is required."""
        return self.call("plugins.list")

    def plugin(self, plugin_id: str) -> dict[str, Any]:
        """One published plugin by id.

        Raises:
            NotFoundError: absent *or* unpublished; the two are indistinguishable.
        """
        return self.call("plugins.get", plugin_id=_require_plugin_id(plugin_id))

    def download_plugin(self, plugin_id: str) -> bytes:
        """Download a bundle. Requires a token, and increments the download counter."""
        answer = self.call("plugins.download", plugin_id=_require_plugin_id(plugin_id))
        return base64.b64decode(answer["bytes"])

    def download_plugin_to(self, plugin_id: str, path: str | os.PathLike[str]) -> int:
        """Download a bundle and write it to ``path``. Returns the byte count."""
        data = self.download_plugin(plugin_id)
        target = Path(path)
        target.write_bytes(data)
        return len(data)

    def upload_plugin(
        self, file_name: str, data: bytes, *, publish: bool = False
    ) -> dict[str, Any]:
        """Upload a bundle. Needs ``plugin.manage``.

        The service caps bundles at 5 MiB and requires a ``manifest.json`` at the zip
        root with a valid id and semantic version.
        """
        if len(data) > 5 * 1024 * 1024:
            raise InvalidArgumentError(f"bundle is {len(data)} bytes, above the 5 MiB limit")
        upload: dict[str, Any] = {
            "file_name": file_name,
            "bytes": base64.b64encode(data).decode("ascii"),
            "mime": "application/zip",
        }
        return self.call("plugins.upload", upload=upload, publish=bool(publish))

    def upload_plugin_file(
        self, path: str | os.PathLike[str], *, publish: bool = False
    ) -> dict[str, Any]:
        """Upload a bundle read from disk."""
        target = Path(path)
        return self.upload_plugin(target.name, target.read_bytes(), publish=publish)

    def publish_plugin(self, plugin_id: str) -> Any:
        """List a plugin on the store."""
        return self.call("plugins.publish", plugin_id=_require_plugin_id(plugin_id))

    def unpublish_plugin(self, plugin_id: str) -> Any:
        """Hide a plugin from the store, keeping the row and the bundle."""
        return self.call("plugins.unpublish", plugin_id=_require_plugin_id(plugin_id))

    def delete_plugin(self, plugin_id: str) -> Any:
        """Delete a plugin row and its bundle."""
        return self.call("plugins.delete", plugin_id=_require_plugin_id(plugin_id))

    # -- tips --------------------------------------------------------------

    def tip_post(self, post_id: int, amount: int, pin: str) -> dict[str, Any]:
        """Tip a post with coins, authorized by the payment PIN.

        ``amount`` is a whole **coin** count (1..100000), not the wallet's decimal
        coins. The response reports cents: a 5-coin tip records ``amount=500`` and
        ``net_amount=475``, the 5 % platform fee having been taken. The post author
        gets the rest, and a deleted post refunds the tipper.

        Raises:
            InvalidArgumentError: the amount is outside ``1..100000`` or is not an int.
            BadRequestError: tipping your own post.
        """
        coins = _require_int("amount", amount)
        if not 1 <= coins <= 100_000:
            raise InvalidArgumentError(f"tip amount must be 1..100000 coins, got {coins}")
        if not pin:
            raise InvalidArgumentError("pin must not be empty")
        return self.call("posts.tip", post_id=post_id, amount=coins, pin=pin)

    # -- moderation, notifications, instance -------------------------------

    #: The report target kinds the server accepts.
    REPORT_TARGETS = ("post", "message", "user")

    def report(self, target_type: str, target_id: int, reason: str) -> Any:
        """Report a post, message or user.

        Args:
            target_type: one of :attr:`REPORT_TARGETS`. The server binds this field and
                rejects anything else, so it is validated here as well.
            target_id: the reported resource.
            reason: why. At most 500 characters.

        Note:
            A bot may file at most 20 reports per hour (``bots.md`` 12.1); the 21st is a
            :class:`~openfield_bot.RateLimitedError`.
        """
        if target_type not in self.REPORT_TARGETS:
            raise InvalidArgumentError(
                f"unknown report target {target_type!r}; expected one of "
                f"{', '.join(self.REPORT_TARGETS)}"
            )
        report = {"target_type": target_type, "target_id": target_id, "reason": reason}
        return self.call("reports.create", report=report)

    def report_post(self, post_id: int, reason: str) -> Any:
        """Report a post."""
        return self.report("post", post_id, reason)

    def report_message(self, message_id: int, reason: str) -> Any:
        """Report a chat message."""
        return self.report("message", message_id, reason)

    def report_user(self, user_id: int, reason: str) -> Any:
        """Report a user."""
        return self.report("user", user_id, reason)

    def my_reports(self) -> Any:
        """The reports this bot has filed."""
        return self.call("reports.mine")

    def notifications(self) -> Any:
        """Unread notifications."""
        return self.call("notifications.list")

    def read_notifications(self) -> Any:
        """Mark every notification read."""
        return self.call("notifications.read")

    def health(self) -> dict[str, Any]:
        """The instance health report. Requires no token."""
        return self.call("health")

    def announcements(self, *, all: bool = False) -> Any:  # noqa: A002 - matches the API
        """Server announcements."""
        return self.call("announcements", all=all)

    # -- raw ---------------------------------------------------------------

    def request(
        self,
        method: str,
        path: str,
        *,
        query: Optional[dict[str, Any]] = None,
        body: Optional[Any] = None,
        bytes: Optional[bytes] = None,  # noqa: A002 - mirrors the payload field
    ) -> dict[str, Any]:
        """Perform a raw request and return ``{"ok", "status", "body"}``.

        Does **not** raise on a non-2xx status: the envelope carries it, so a caller can
        deliberately handle a ``409`` or read a ``404`` body.
        """
        from .ffi import FfiTransport

        params: dict[str, Any] = {"method": method, "path": path}
        if query:
            params["query"] = {k: str(v) for k, v in query.items()}
        if body is not None:
            params["body"] = body
        if bytes is not None:
            params["bytes"] = list(bytes)

        if isinstance(self._transport, FfiTransport):
            return self._transport.raw_request(params)
        return self._transport.call("request", params)

    # -- events ------------------------------------------------------------

    def events(
        self,
        *,
        types: Optional[list[str]] = None,
        conversation_ids: Optional[list[int]] = None,
        queue_capacity: Optional[int] = None,
        reconnect: Optional[dict[str, Any]] = None,
    ) -> "_EventAdapter":
        """Subscribe to the realtime channel and return an event iterator.

        The subscription is established before this returns, so the server already counts
        the connection -- and a broadcast sent immediately afterwards cannot be missed.

        Args:
            types: event types to keep; omit for all of them.
            conversation_ids: restrict chat events to these conversations.
            queue_capacity: how many events to buffer before dropping the oldest.
            reconnect: overrides for the reconnect policy.

        Returns:
            An iterator of :class:`Event` objects, which is also a context manager.

        Note:
            Delivery is best effort. The server relays through Postgres ``NOTIFY`` and
            does not replay across a disconnect, so anything polled (**votes**, tips,
            wallet changes) must be reconciled periodically rather than assumed from the
            event stream (``bots.md`` 9.3). :meth:`watch` does that for chat.
        """
        params: dict[str, Any] = {}
        if types:
            params["types"] = types
        if conversation_ids:
            params["conversation_ids"] = conversation_ids
        if queue_capacity is not None:
            params["queue_capacity"] = queue_capacity
        if reconnect is not None:
            params["reconnect"] = reconnect

        return _EventAdapter(self._transport.events(params or None))

    def watch(
        self,
        conversation_id: int,
        *,
        poll_interval: float = 5.0,
        types: Optional[list[str]] = None,
        since: Optional[int] = None,
        max_events: Optional[int] = None,
        settle: float = 0.2,
    ) -> Iterator[Event]:
        """Follow one conversation, merging the event stream with polling.

        Implements the reliable half of the ``bots.md`` 9.2/9.3 recipe:

        * events arrive immediately while the connection is up, because a background
          thread drains the subscription into a local queue;
        * a poll of ``chat.latest`` runs every `poll_interval` seconds, so a message sent
          while the bot was disconnected is still seen;
        * the bot's own messages are skipped, because the event channel echoes what the
          bot sends and a bot that answers itself loops forever;
        * ids are de-duplicated, so the same message delivered by both paths yields once.

        Args:
            conversation_id: the conversation to follow.
            poll_interval: seconds between reconciliation polls.
            types: restrict the subscription, e.g. ``["chat.message.created"]``.
            since: the last message id already handled; anything at or below it is
                skipped. Defaults to the newest message, so a restart does not replay
                history.
            max_events: stop after this many messages, for a finite job.
            settle: how long to sleep between checks, so this does not spin the CPU.

        Yields:
            :class:`Event` objects of type ``chat.message.created``, for messages that
            came from someone else. Everything else is filtered out -- including the
            ``connected`` greeting, which would otherwise consume a ``max_events`` budget
            with something that is not a message. Use :meth:`events` for the unfiltered
            stream.

        Example::

            for event in bot.watch(33):
                if "ping" in event.data["content"]:
                    bot.send_message(33, "pong", reply_to_id=event.data["id"])
        """
        if since is None:
            page = self.latest_messages(conversation_id, limit=1)
            newest = _newest_message_id(page)
            since = newest if newest is not None else 0

        own_id = self.id
        seen: set[int] = set()
        highest = since

        inbox: queue.Queue[Any] = queue.Queue()
        subscription = self.events(
            types=types or ["chat.message.created"],
            conversation_ids=[conversation_id],
        )

        def pump() -> None:
            """Drain the subscription into the local queue, off the caller's thread."""
            try:
                for event in subscription:
                    inbox.put(event)
            except BaseException as exc:  # noqa: BLE001 - re-raised on the caller thread
                inbox.put(exc)
            finally:
                inbox.put(None)

        threading.Thread(target=pump, name="openfield-watch", daemon=True).start()
        # The subscription must be released when the caller stops iterating -- otherwise
        # the connection outlives its consumer, and a backend whose handle is freed at
        # close would be read by a thread that no longer has any business reading it.
        try:
            yield from self._watch_loop(
                inbox, conversation_id, highest, seen, own_id, poll_interval, max_events, settle
            )
        finally:
            subscription.close()

    def _watch_loop(
        self,
        inbox: "queue.Queue[Any]",
        conversation_id: int,
        highest: int,
        seen: set[int],
        own_id: Optional[int],
        poll_interval: float,
        max_events: Optional[int],
        settle: float,
    ) -> Iterator[Event]:
        """Merge the subscription's queue with periodic reconciliation polls."""
        emitted = 0
        next_poll = 0.0

        while True:
            # 1. Anything the subscription already delivered.
            while True:
                try:
                    item = inbox.get_nowait()
                except queue.Empty:
                    break
                if item is None:
                    raise BotError("the event subscription closed")
                if isinstance(item, BaseException):
                    raise item
                for event in _filter_events([item], conversation_id, highest, seen, own_id):
                    highest = max(highest, event.data["id"])
                    emitted += 1
                    # Yield first, then check the budget: returning before the yield would
                    # silently drop the last message of a finite run.
                    yield event
                    if max_events is not None and emitted >= max_events:
                        return

            # 2. Reconcile: catch anything sent while the subscription was down.
            if time.monotonic() >= next_poll:
                next_poll = time.monotonic() + poll_interval
                for message in _poll_new_messages(self, conversation_id, highest, own_id):
                    if message["id"] in seen:
                        continue
                    highest = max(highest, message["id"])
                    seen.add(message["id"])
                    emitted += 1
                    yield Event(
                        {"event": "chat.message.created", "data": message, "recipients": []}
                    )
                    if max_events is not None and emitted >= max_events:
                        return

            time.sleep(settle)


def _require_amount(name: str, value: Any) -> float:
    """Accept a monetary amount as an int or a float, in decimal coins.

    ``bool`` is rejected for the same reason as in :func:`_require_int`: ``True`` is
    never an amount. A string is rejected too, even though the server's
    ``Cents.UnmarshalJSON`` would parse one, because it hides the unit.
    """
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise InvalidArgumentError(f"{name} must be a number of coins, got {value!r}")
    return value


def _require_int(name: str, value: Any) -> int:
    """Coerce an integer field, refusing the shapes Go's decoder would refuse.

    ``bool`` is an ``int`` in Python, so it is rejected explicitly: sending ``True``
    where the server binds an ``int64`` is never what the caller meant. A float is
    rejected too -- Go refuses ``5.0`` for an ``int64`` -- and so is a numeric string
    the caller should have passed as a number.
    """
    if isinstance(value, bool) or not isinstance(value, int):
        raise InvalidArgumentError(f"{name} must be an integer, got {value!r}")
    return value


def _require_sha256(hex_digest: str) -> str:
    """Normalise a SHA-256 digest to the lowercase 64-character hex the server looks up."""
    digest = hex_digest.strip().lower() if isinstance(hex_digest, str) else ""
    if len(digest) != 64 or any(character not in "0123456789abcdef" for character in digest):
        raise InvalidArgumentError("hash must be 64 hexadecimal characters (SHA-256)")
    return digest


def _require_pin(pin: str) -> str:
    """A payment PIN is exactly six digits (``security.ValidPin``)."""
    if not isinstance(pin, str) or len(pin) != 6 or not pin.isdigit():
        raise InvalidArgumentError("the payment pin must be exactly 6 digits")
    return pin


def _require_plugin_id(plugin_id: str) -> str:
    """Reject a plugin id the service would reject, and never let one act as a path."""
    candidate = plugin_id.strip() if isinstance(plugin_id, str) else ""
    allowed = "._-"
    shaped = (
        3 <= len(candidate) <= 64
        and candidate[0].isascii()
        and (candidate[0].islower() or candidate[0].isdigit())
        and all(
            character.isascii()
            and (character.islower() or character.isdigit() or character in allowed)
            for character in candidate
        )
    )
    # `.` and `..` satisfy the character class, so they are excluded by name.
    if not shaped or candidate in (".", ".."):
        raise InvalidArgumentError(
            f"plugin id {plugin_id!r} must match ^[a-z0-9][a-z0-9._-]{{2,63}}$"
        )
    return candidate


def _message_id(message: dict[str, Any]) -> Optional[int]:
    """The id of a message dict, tolerating an id the server marshalled as a string."""
    raw = message.get("id")
    if isinstance(raw, int):
        return raw
    try:
        return int(raw)
    except (TypeError, ValueError):
        return None


def _page_items(page: Any) -> list[dict[str, Any]]:
    """Pull the message list out of whichever envelope the endpoint returned."""
    if isinstance(page, list):
        return [item for item in page if isinstance(item, dict)]
    if isinstance(page, dict):
        for key in ("messages", "items", "data", "list"):
            value = page.get(key)
            if isinstance(value, list):
                return [item for item in value if isinstance(item, dict)]
    return []


def _newest_message_id(page: Any) -> Optional[int]:
    """The largest id in a page.

    The handler sorts newest-first, so this is normally the head -- but the SDK must not
    depend on an ordering the server is free to change, so it takes the maximum.
    """
    ids = [mid for mid in (_message_id(item) for item in _page_items(page)) if mid is not None]
    return max(ids) if ids else None


def _is_own(message: dict[str, Any], own_id: Optional[int]) -> bool:
    """Whether a message is this bot's own.

    Checked by id rather than by trust in ``sender_is_bot`` alone, because two bots in one
    conversation must still be able to talk to each other.
    """
    sender = message.get("sender_id")
    if sender is None:
        sender = message.get("user_id")
    return own_id is not None and sender == own_id


def _filter_events(
    events: list[Event],
    conversation_id: int,
    since: int,
    seen: set[int],
    own_id: Optional[int],
) -> list[Event]:
    """Keep only new, actionable ``chat.message.created`` events for this conversation.

    Everything else is dropped, including the ``connected`` greeting the server sends to
    every new subscriber. ``watch`` promises messages, so handing the caller a connection
    notice -- or a typing indicator -- would be a surprise, and passing the greeting
    through would consume a caller's ``max_events`` budget with something that is not a
    message at all. Use :meth:`Bot.events` for the unfiltered stream.
    """
    kept: list[Event] = []
    for event in events:
        if event.type != "chat.message.created":
            continue
        data = event.data
        if data.get("conversation_id") not in (None, conversation_id):
            continue
        mid = _message_id(data)
        if mid is None or mid <= since or mid in seen or _is_own(data, own_id):
            continue
        seen.add(mid)
        kept.append(event)
    return kept


def _poll_new_messages(
    bot: Bot, conversation_id: int, since: int, own_id: Optional[int]
) -> list[dict[str, Any]]:
    """Ask for messages newer than `since` that this bot did not send, oldest first."""
    page = bot.latest_messages(conversation_id, limit=50)
    fresh = []
    for message in _page_items(page):
        mid = _message_id(message)
        if mid is None or mid <= since or _is_own(message, own_id):
            continue
        fresh.append(message)
    fresh.sort(key=lambda item: _message_id(item) or 0)
    return fresh
