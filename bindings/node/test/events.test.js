/**
 * Realtime events and `Bot.watch`, against the Rust mock server.
 */

import assert from 'node:assert/strict';
import { describe, it } from 'node:test';

import { BotError } from '../src/index.js';
import { FINISHED, Puller, TIMEOUT, requireKit, useTestkit, waitUntil } from './support.js';

const kit = useTestkit();

describe('event streams', () => {
  it('greets every subscriber before any event', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const events = await bot.events();
    const puller = new Puller(events);
    t.after(() => puller.close());

    const greeting = await puller.take();
    assert.notEqual(greeting, TIMEOUT, 'no greeting arrived');
    assert.equal(greeting.type, 'connected');
  });

  it('subscribes eagerly', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const events = await current.bot.events();
    t.after(() => events.close());

    assert.ok(
      await waitUntil(async () => (await current.wsClients()) >= 1),
      'the subscription must be established by `events()` itself',
    );
  });

  it('receives a broadcast with its recipients', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const events = await current.bot.events();
    t.after(() => events.close());
    assert.ok(await waitUntil(async () => (await current.wsClients()) >= 1));

    const delivered = await current.broadcast({
      type: 'chat.message.created',
      recipients: [7, current.info.bot_id],
      data: { id: 5, conversation_id: 33, content: 'hi' },
    });
    assert.equal(delivered, 1);

    const puller = new Puller(events);
    t.after(() => puller.close());
    const event = await puller.realEvent();
    assert.notEqual(event, TIMEOUT, 'no event arrived');
    assert.equal(event.type, 'chat.message.created');
    assert.deepEqual(event.recipients, [7, current.info.bot_id]);
    assert.equal(event.isBroadcast(), false);
    assert.equal(event.data.conversation_id, 33);
  });

  it('drops events the filter excludes', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const events = await current.bot.events({ types: ['chat.message.created'] });
    t.after(() => events.close());
    assert.ok(await waitUntil(async () => (await current.wsClients()) >= 1));

    await current.broadcast({ type: 'post.created', data: { id: 1 } });
    await current.broadcast({
      type: 'chat.message.created',
      recipients: [7],
      data: { id: 2, conversation_id: 33, content: 'kept' },
    });

    const puller = new Puller(events);
    t.after(() => puller.close());
    const event = await puller.realEvent();
    assert.notEqual(event, TIMEOUT, 'no event arrived');
    assert.equal(event.type, 'chat.message.created');
    assert.equal(event.data.content, 'kept');
  });

  it('reflects the greeting in the stream counters', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const events = await current.bot.events();
    t.after(() => events.close());
    assert.ok(await waitUntil(async () => (await current.wsClients()) >= 1));

    const puller = new Puller(events);
    t.after(() => puller.close());
    assert.notEqual(await puller.take(), TIMEOUT);

    const stats = await events.stats();
    assert.ok(stats.connection_attempts >= 1);
    assert.ok(stats.connections_opened >= 1);
    assert.ok(stats.events_delivered >= 1);
  });

  it('disconnects when the subscription closes', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const events = await current.bot.events();
    assert.ok(await waitUntil(async () => (await current.wsClients()) >= 1));

    events.close();
    assert.ok(
      await waitUntil(async () => (await current.wsClients()) === 0),
      'closing the iterator must drop the subscription',
    );
    events.close(); // idempotent
  });

  it('needs a token', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    await current.transport.call('client.set_token', { token: '' });
    await assert.rejects(() => current.bot.events(), (error) => {
      assert.match(error.message.toLowerCase(), /token/);
      return true;
    });
    await current.transport.call('client.set_token', { token: current.info.token });
  });
});

describe('watch', () => {
  /**
   * Start a `watch` driven by a `Puller`.
   *
   * Every test passes an explicit `since`. Left unset, `watch` aligns to the newest message
   * first -- the documented restart behaviour -- which would skip the traffic each test
   * seeds beforehand.
   */
  function watch(t, conversationId, options) {
    const puller = new Puller(
      kit.current.bot.watch(conversationId, { pollIntervalMs: 150, ...options }),
    );
    t.after(() => puller.close());
    return puller;
  }

  it('yields a message from someone else, found by polling', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const conversation = await current.bot.createConversation('watch');
    // Nobody broadcasts this: `watch` must find it by polling, which is the reconnect case.
    const seeded = await current.seedMessage(conversation.id, 'are you there?');

    const watched = watch(t, conversation.id, { maxEvents: 1, since: 0 });
    const event = await watched.take(10_000);
    assert.notEqual(event, TIMEOUT, 'watch produced nothing');
    assert.equal(event.type, 'chat.message.created');
    assert.equal(event.data.id, seeded.id);
    assert.equal(event.data.sender_id, 7);
    assert.notEqual(event.data.sender_id, await current.bot.id());
  });

  it('does not spend its budget on the connected greeting', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const conversation = await current.bot.createConversation('greeting');
    const seeded = await current.seedMessage(conversation.id, 'after the greeting', {
      broadcast: true,
    });

    const watched = watch(t, conversation.id, { maxEvents: 1, since: 0 });
    const event = await watched.take(10_000);
    assert.notEqual(event, TIMEOUT, 'the greeting must not have used up the budget');
    assert.equal(event.type, 'chat.message.created');
    assert.equal(event.data.id, seeded.id);
  });

  it('aligns to the newest message by default', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const conversation = await current.bot.createConversation('align');
    await current.seedMessage(conversation.id, 'old news');

    const watched = watch(t, conversation.id, { maxEvents: 1 });

    // Nothing new yet: the aligned cursor is already past the seeded message.
    assert.equal(await watched.take(1_500), TIMEOUT);

    const fresh = await current.seedMessage(conversation.id, 'breaking news');
    const event = await watched.take(10_000);
    assert.notEqual(event, TIMEOUT, 'the new message must be seen');
    assert.equal(event.data.id, fresh.id);
  });

  it('skips the bot\'s own messages', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const conversation = await current.bot.createConversation('self');
    await current.bot.sendMessage(conversation.id, 'I said this');

    const watched = watch(t, conversation.id, { maxEvents: 5, since: 0 });
    assert.equal(await watched.take(1_500), TIMEOUT, 'a bot must not answer itself');
  });

  it('de-duplicates the event and the poll', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const conversation = await current.bot.createConversation('dedupe');
    const seeded = await current.seedMessage(conversation.id, 'hello', { broadcast: true });

    const watched = watch(t, conversation.id, { maxEvents: 5, since: 0 });
    const first = await watched.take(10_000);
    assert.notEqual(first, TIMEOUT);
    assert.equal(first.data.id, seeded.id);

    assert.equal(await watched.take(1_500), TIMEOUT, 'the same id must not yield twice');
  });

  it('yields the last message before stopping', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const conversation = await current.bot.createConversation('finite');
    await current.seedMessage(conversation.id, 'one');
    const second = await current.seedMessage(conversation.id, 'two');

    const watched = watch(t, conversation.id, { maxEvents: 2, since: 0 });
    assert.notEqual(await watched.take(10_000), TIMEOUT);
    const last = await watched.take(10_000);
    assert.notEqual(last, TIMEOUT, 'the last message must still be yielded');
    assert.equal(last.data.id, second.id);

    assert.equal(await watched.take(10_000), FINISHED);
  });

  it('honours since', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const conversation = await current.bot.createConversation('since');
    const old = await current.seedMessage(conversation.id, 'before my time');
    await current.seedMessage(conversation.id, 'after my time');

    const watched = watch(t, conversation.id, { maxEvents: 1, since: old.id });
    const event = await watched.take(10_000);
    assert.notEqual(event, TIMEOUT);
    assert.ok(event.data.id > old.id);
    assert.equal(event.data.content, 'after my time');
  });
});
