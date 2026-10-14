"""Realtime events, through the sidecar against the Rust mock server.

The mock's WebSocket is the same hand-rolled one the core's own tests use: a
``POST /api/v1/ws`` mints a single-use ticket, and ``GET /api/v1/ws?ticket=`` upgrades.

Every subscriber is greeted with ``{"type": "connected"}`` before any event, so the tests
that broadcast use :func:`~.support.real_event` to step over it. That greeting is real
behaviour, not test noise, so it is asserted on directly as well.
"""

from __future__ import annotations

import unittest

from openfield_bot import BotError

from .support import _FINISHED, _TIMEOUT, Puller, TestkitCase, real_event, wait_until


class EventStreamTests(TestkitCase):
    """Subscribing, filtering, and stopping."""

    def connected(self, timeout: float = 5.0) -> None:
        """Wait until the mock counts our subscription."""
        self.assertTrue(
            wait_until(lambda: self.ws_clients() >= 1, timeout=timeout),
            "the subscription must reach the server before broadcasting",
        )

    def test_the_greeting_arrives_first(self) -> None:
        """`ws.go` sends `{"type":"connected"}` to every subscriber, before any event."""
        events = self.bot.events()
        self.addCleanup(events.close)

        puller = Puller(events)
        self.addCleanup(puller.close)
        greeting = puller.take(timeout=5.0)
        self.assertIsNot(_TIMEOUT, greeting, "no greeting arrived")
        self.assertEqual(greeting.type, "connected")

    def test_a_subscription_receives_a_broadcast(self) -> None:
        events = self.bot.events()
        self.addCleanup(events.close)
        self.connected()

        delivered = self.broadcast(
            {"type": "post.created", "data": {"id": 99, "content": "from the mock"}}
        )
        self.assertEqual(delivered, 1)

        puller = Puller(events)
        self.addCleanup(puller.close)
        event = real_event(puller, timeout=5.0)
        self.assertIsNot(_TIMEOUT, event, "no event arrived")
        self.assertEqual(event.type, "post.created")
        self.assertEqual(event.data["id"], 99)
        self.assertTrue(event.is_broadcast())

    def test_subscribing_is_eager(self) -> None:
        """`events()` must start the subscription before returning.

        A generator that subscribed on its first ``next()`` would look identical to a
        caller but leave the server unaware, so an immediately-following broadcast would
        be lost. The socket handshake itself is asynchronous, so this waits for the server
        to count it while never iterating -- which is the actual contract.
        """
        events = self.bot.events()
        self.addCleanup(events.close)
        self.assertTrue(
            wait_until(lambda: self.ws_clients() >= 1, timeout=5.0),
            "the subscription must be established by `events()` itself",
        )

    def test_the_event_keeps_its_recipients(self) -> None:
        events = self.bot.events()
        self.addCleanup(events.close)
        self.connected()

        self.broadcast(
            {
                "type": "chat.message.created",
                "recipients": [7, self.info["bot_id"]],
                "data": {"id": 5, "conversation_id": 33, "content": "hi"},
            }
        )

        puller = Puller(events)
        self.addCleanup(puller.close)
        event = real_event(puller, timeout=5.0)
        self.assertIsNot(_TIMEOUT, event, "no event arrived")
        self.assertEqual(event.type, "chat.message.created")
        self.assertFalse(event.is_broadcast())
        self.assertIn(7, event.recipients)
        self.assertEqual(event.data["conversation_id"], 33)

    def test_a_filter_drops_other_types(self) -> None:
        events = self.bot.events(types=["chat.message.created"])
        self.addCleanup(events.close)
        self.connected()

        # Filtered out, so it must never arrive.
        self.broadcast({"type": "post.created", "data": {"id": 1}})
        # The one the filter keeps.
        self.broadcast(
            {
                "type": "chat.message.created",
                "recipients": [7],
                "data": {"id": 2, "conversation_id": 33, "content": "kept"},
            }
        )

        puller = Puller(events)
        self.addCleanup(puller.close)
        event = real_event(puller, timeout=5.0)
        self.assertIsNot(_TIMEOUT, event, "no event arrived")
        self.assertEqual(event.type, "chat.message.created")
        self.assertEqual(event.data["content"], "kept")

    def test_a_conversation_filter_drops_other_conversations(self) -> None:
        events = self.bot.events(conversation_ids=[33])
        self.addCleanup(events.close)
        self.connected()

        self.broadcast(
            {
                "type": "chat.message.created",
                "data": {"id": 1, "conversation_id": 44, "content": "wrong room"},
            }
        )
        self.broadcast(
            {
                "type": "chat.message.created",
                "data": {"id": 2, "conversation_id": 33, "content": "right room"},
            }
        )

        puller = Puller(events)
        self.addCleanup(puller.close)
        event = real_event(puller, timeout=5.0)
        self.assertIsNot(_TIMEOUT, event, "no event arrived")
        self.assertEqual(event.data["conversation_id"], 33)

    def test_stats_report_the_stream(self) -> None:
        events = self.bot.events()
        self.addCleanup(events.close)
        self.connected()
        self.broadcast({"type": "post.created", "data": {"id": 1}})

        puller = Puller(events)
        self.addCleanup(puller.close)
        self.assertIsNot(_TIMEOUT, real_event(puller, timeout=5.0), "no event arrived")

        stats = events.stats
        self.assertGreaterEqual(stats["connection_attempts"], 1)
        self.assertGreaterEqual(stats["connections_opened"], 1)
        # The greeting counts as a delivered event.
        self.assertGreaterEqual(stats["events_delivered"], 2)

    def test_unsubscribing_disconnects(self) -> None:
        events = self.bot.events()
        self.connected()
        events.close()

        self.assertTrue(
            wait_until(lambda: self.ws_clients() == 0, timeout=5.0),
            "closing the iterator must drop the subscription",
        )
        events.close()  # idempotent

    def test_a_second_subscription_replaces_the_first(self) -> None:
        """The transport keeps one stream, so a new one must not stack a second reader."""
        first = self.bot.events()
        self.addCleanup(first.close)
        self.connected()

        second = self.bot.events(types=["post.created"])
        self.addCleanup(second.close)
        self.assertTrue(
            wait_until(lambda: self.ws_clients() == 1, timeout=5.0),
            "the old subscription must have been dropped, not stacked",
        )

    def test_events_need_a_token(self) -> None:
        self.sidecar.call("client.set_token", {"token": ""})
        with self.assertRaises(BotError) as caught:
            self.bot.events()
        self.assertIn("token", str(caught.exception).lower())
        self.sidecar.call("client.set_token", {"token": self.token})


class WatchTests(TestkitCase):
    """`Bot.watch` merges the subscription with polling.

    Most tests pass an explicit ``since``. Left unset, ``watch`` aligns to the newest
    message first -- the documented restart behaviour (``bots.md`` 9.2) -- which would skip
    the traffic each test seeds beforehand. That alignment is covered on its own.
    """

    def watch(self, conversation_id: int, **options) -> Puller:
        """Start a `watch` driven by a `Puller`, cleaned up automatically."""
        options.setdefault("poll_interval", 0.15)
        puller = Puller(self.bot.watch(conversation_id, **options))
        self.addCleanup(puller.close)
        return puller

    def test_watch_yields_someone_elses_message(self) -> None:
        conversation_id = self.bot.create_conversation("watch")["id"]

        # Nobody broadcasts this: `watch` must find it by polling, which is the reconnect
        # case the event stream alone cannot cover.
        seeded = self.seed_message(conversation_id, "are you there?")

        watched = self.watch(conversation_id, max_events=1, since=0)
        event = watched.take(timeout=10.0)
        self.assertIsNot(_TIMEOUT, event, "watch produced nothing")
        self.assertEqual(event.type, "chat.message.created")
        self.assertEqual(event.data["id"], seeded["id"])
        self.assertEqual(event.data["sender_id"], 7)
        self.assertNotEqual(event.data["sender_id"], self.bot.id)

    def test_watch_drops_the_connected_greeting(self) -> None:
        """`watch` promises messages, so the greeting must not consume its budget.

        The budget matters: with `max_events=1` a leaked greeting would end the run before
        a single real message was seen.
        """
        conversation_id = self.bot.create_conversation("greeting")["id"]
        seeded = self.seed_message(conversation_id, "after the greeting", broadcast=True)

        # `since=0` so the seeded message is not skipped, max_events=1 so any extra frame
        # would be visible as a wrong first event.
        watched = self.watch(conversation_id, max_events=1, since=0)
        event = watched.take(timeout=10.0)
        self.assertIsNot(_TIMEOUT, event, "the greeting must not have used up the budget")
        self.assertEqual(event.type, "chat.message.created")
        self.assertEqual(event.data["id"], seeded["id"])

    def test_watch_aligns_to_the_newest_message_by_default(self) -> None:
        """A restart must not replay history, so the default cursor is the newest id.

        Seeding again after the alignment pins both halves of that contract: the old
        message is ignored, the new one is not.
        """
        conversation_id = self.bot.create_conversation("align")["id"]
        self.seed_message(conversation_id, "old news")

        watched = self.watch(conversation_id, max_events=1)

        # Nothing new yet: the aligned cursor is already past the seeded message.
        self.assertIs(_TIMEOUT, watched.take(timeout=1.5))

        fresh = self.seed_message(conversation_id, "breaking news")
        event = watched.take(timeout=10.0)
        self.assertIsNot(_TIMEOUT, event, "the new message must be seen")
        self.assertEqual(event.data["id"], fresh["id"])

    def test_watch_skips_the_bots_own_messages(self) -> None:
        """A bot that answers itself loops forever, so `watch` drops its own output."""
        conversation_id = self.bot.create_conversation("self")["id"]
        self.bot.send_message(conversation_id, "I said this")

        watched = self.watch(conversation_id, max_events=5, since=0)

        # Nothing foreign exists, so `watch` must stay silent rather than echo the bot.
        self.assertIs(_TIMEOUT, watched.take(timeout=1.5))

    def test_watch_de_duplicates_the_event_and_the_poll(self) -> None:
        conversation_id = self.bot.create_conversation("dedupe")["id"]
        seeded = self.seed_message(conversation_id, "hello", broadcast=True)

        watched = self.watch(conversation_id, max_events=5, since=0)

        first = watched.take(timeout=10.0)
        self.assertIsNot(_TIMEOUT, first)
        self.assertEqual(first.data["id"], seeded["id"])

        # The same id arriving on the other path must not produce a second yield.
        self.assertIs(_TIMEOUT, watched.take(timeout=1.5))

    def test_watch_yields_the_last_message_before_stopping(self) -> None:
        """A finite run must deliver its final message, not stop one short."""
        conversation_id = self.bot.create_conversation("finite")["id"]
        self.seed_message(conversation_id, "one")
        second = self.seed_message(conversation_id, "two")

        watched = self.watch(conversation_id, max_events=2, since=0)

        first_event = watched.take(timeout=10.0)
        self.assertIsNot(_TIMEOUT, first_event)
        last_event = watched.take(timeout=10.0)
        self.assertIsNot(_TIMEOUT, last_event, "the last message must still be yielded")
        self.assertEqual(last_event.data["id"], second["id"])

        # Only then does the generator finish.
        self.assertIs(_FINISHED, watched.take(timeout=10.0))

    def test_watch_honours_since(self) -> None:
        conversation_id = self.bot.create_conversation("since")["id"]
        old = self.seed_message(conversation_id, "before my time")
        self.seed_message(conversation_id, "after my time")

        watched = self.watch(conversation_id, max_events=1, since=old["id"])

        event = watched.take(timeout=10.0)
        self.assertIsNot(_TIMEOUT, event)
        self.assertGreater(event.data["id"], old["id"])
        self.assertEqual(event.data["content"], "after my time")

    def test_watch_yields_messages_in_order(self) -> None:
        conversation_id = self.bot.create_conversation("order")["id"]
        first = self.seed_message(conversation_id, "one")
        second = self.seed_message(conversation_id, "two")

        watched = self.watch(conversation_id, max_events=2, since=0)

        seen = [watched.take(timeout=10.0), watched.take(timeout=10.0)]
        self.assertEqual([event.data["id"] for event in seen], [first["id"], second["id"]])


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
