/**
 * Typed calls against the Rust mock server.
 */

import assert from 'node:assert/strict';
import { describe, it } from 'node:test';

import {
  BadRequestError,
  ConflictError,
  ForbiddenError,
  InvalidArgumentError,
  NotFoundError,
  RateLimitedError,
  ServerError,
  UnauthorizedError,
} from '../src/index.js';
import { Puller, TIMEOUT, rejectsWith, requireKit, useTestkit, waitUntil } from './support.js';

const kit = useTestkit();

describe('requests', () => {
  it('reaches the unauthenticated endpoints', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const envelope = await bot.request('GET', '/api/v1/capabilities');
    assert.equal(envelope.ok, true);
    assert.ok(envelope.body.features.includes('chat.e2ee'));

    const health = await bot.health();
    assert.equal(health.all_healthy, true);

    const announcements = await bot.announcements();
    assert.equal(announcements[0].title, 'welcome');
  });

  it('sends the token in the Authorization header', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    await current.bot.me();
    const recorded = await current.requestsTo('/api/v1/users/me');
    assert.ok(recorded.length > 0);
    assert.equal(recorded[0].authorization, `Bearer ${current.info.token}`);
  });

  it('reports the bot identity and permissions', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const me = await current.bot.me();
    assert.equal(me.is_bot, true);
    assert.equal(me.id, current.info.bot_id);
    assert.equal(await current.bot.username(), 'weather_watcher');
    assert.equal(await current.bot.id(), current.info.bot_id);

    const held = await current.bot.permissions();
    assert.ok(held.permissions.includes('posts.create'));

    const check = await current.bot.startupCheck();
    assert.equal(check.is_bot, true);
    assert.equal(check.bot_id, current.info.bot_id);
  });

  it('names the missing permission in requirePermission', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const error = await rejectsWith(UnauthorizedError, () =>
      bot.requirePermission('admin.everything'),
    );
    assert.equal(error.permission, 'admin.everything');
    assert.match(error.message, /admin\.everything/);

    await bot.requirePermission('posts.create'); // must not throw
  });

  it('publishes and reads back a post', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const created = await bot.publish('hello from node', { visibility: 'public' });
    assert.equal(created.content, 'hello from node');

    const page = await bot.posts();
    assert.equal(page.posts[0].content, 'hello from node');

    const fetched = await bot.post(created.id);
    assert.equal(fetched.id, created.id);
  });

  it('refuses a post over the byte limit without sending it', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const before = (await current.requestsTo('/api/v1/posts')).length;
    await rejectsWith(InvalidArgumentError, () => current.bot.publish('测'.repeat(2000)));
    assert.equal(
      (await current.requestsTo('/api/v1/posts')).length,
      before,
      'the server must never see an over-long body',
    );
  });

  it('validates reactions locally', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    await rejectsWith(InvalidArgumentError, () => bot.react(1, 'thumbsup'));
    const post = await bot.publish('react to me');
    assert.ok(await bot.react(post.id, 'haha'));
  });

  it('walks a conversation', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const conversation = await bot.createConversation('alerts');
    const sent = await bot.sendMessage(conversation.id, 'first');
    assert.equal(sent.content, 'first');

    const page = await bot.messages(conversation.id, { limit: 10 });
    assert.ok(page.messages.length > 0);
  });

  it('returns messages newest first', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const conversation = await bot.createConversation('ordering');
    const first = await bot.sendMessage(conversation.id, 'one');
    const second = await bot.sendMessage(conversation.id, 'two');
    assert.ok(second.id > first.id);

    const page = await bot.messages(conversation.id, { limit: 10 });
    const ids = page.messages.map((message) => message.id);
    assert.deepEqual(ids, [...ids].sort((a, b) => b - a), `expected DESC, got ${ids}`);
  });

  it('requires a title to create a conversation', async (t) => {
    if (!requireKit(t, kit.current)) return;
    await rejectsWith(InvalidArgumentError, () => kit.current.bot.call('chat.create', { title: '' }));
  });

  it('uploads through the multipart field the handler reads', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const payload = Buffer.from(Array.from({ length: 256 }, (_, index) => index));
    const attachment = await current.bot.upload('report.bin', payload, 'application/octet-stream');
    // The mock answers a canned record, so the request is what matters.
    assert.equal(attachment.original_name, 'chart.png');

    const recorded = await current.requestsTo('/api/v1/attachments');
    assert.ok(recorded.length > 0);
    assert.match(recorded.at(-1).body, /name="file"/);
    assert.match(recorded.at(-1).body, /filename="report.bin"/);
    assert.ok(recorded.at(-1).body_len >= 256, 'all 256 octets must cross');
  });

  it('lists and creates bots', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const listed = await current.bot.listBots();
    assert.ok(Array.isArray(listed.bots));
    assert.ok(listed.bots.length > 0, 'the mock seeds a roster');
    assert.equal((await current.bot.findBot(listed.bots[0].username)).username, listed.bots[0].username);
    assert.equal(await current.bot.findBot('nobody'), null);

    const created = await current.bot.createBot('a_new_bot', 'A New Bot');
    assert.equal(created.user.username, 'a_new_bot');
    assert.equal(created.user.nickname, 'A New Bot');
    assert.match(created.token, /^ofb_[0-9a-f]{64}$/);
  });

  it('refuses a too-short bot username locally', async (t) => {
    if (!requireKit(t, kit.current)) return;
    await rejectsWith(InvalidArgumentError, () => kit.current.bot.createBot('no', 'Short'));
  });

  it('maps a duplicate bot username to a conflict', async (t) => {
    if (!requireKit(t, kit.current)) return;
    await rejectsWith(ConflictError, () => kit.current.bot.createBot('taken', 'Already There'));
  });

  it('files reports with the fields the handler binds', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const post = await current.bot.publish('report me');
    assert.ok(await current.bot.reportPost(post.id, 'spam'));
    assert.ok(await current.bot.myReports());

    const recorded = await current.requestsTo('/api/v1/reports');
    assert.match(recorded.at(-1).body, /"target_type"/);
    assert.match(recorded.at(-1).body, /"target_id"/);

    await rejectsWith(InvalidArgumentError, () => current.bot.report('comment', 1, 'nope'));
    await rejectsWith(BadRequestError, () =>
      current.bot.reportUser(current.info.bot_id, 'me'),
    );
  });

  it('reads notifications', async (t) => {
    if (!requireKit(t, kit.current)) return;
    assert.ok(await kit.current.bot.notifications());
    assert.ok(await kit.current.bot.readNotifications());
  });
});

describe('error mapping', () => {
  it('carries the status and permission of a 403', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;
    await current.forceStatus('/api/v1/posts', 403);

    const error = await rejectsWith(ForbiddenError, () => current.bot.publish('denied'));
    assert.equal(error.status, 403);
    assert.equal(error.retryable, false);
    assert.equal(typeof error.body, 'object');
  });

  it('distinguishes 404 and 409', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    await current.forceStatus('/api/v1/users/me', 404);
    await rejectsWith(NotFoundError, () => current.bot.me());

    await current.forceStatus('/api/v1/posts', 409);
    await rejectsWith(ConflictError, () => current.bot.publish('duplicate'));
  });

  it('retries a 429 and reports Retry-After', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    // 429 is retried, so the refusal must outlast the policy.
    await current.forceStatus('/api/v1/users/me', 429, { retryAfterSecs: 1, times: 10 });
    const error = await rejectsWith(RateLimitedError, () => current.bot.me());
    assert.equal(error.retryAfterSecs, 1);
    assert.equal(error.retryable, true);
    assert.ok(
      (await current.requestsTo('/api/v1/users/me')).length > 1,
      'the 429 must have been retried',
    );
  });

  it('retries a GET on a 500 but never replays a POST', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    await current.forceStatus('/api/v1/users/me', 500, { times: 10 });
    await rejectsWith(ServerError, () => current.bot.me());
    assert.ok((await current.requestsTo('/api/v1/users/me')).length > 1);

    await current.forceStatus('/api/v1/posts', 500, { times: 5 });
    await rejectsWith(ServerError, () => current.bot.publish('only once'));
    assert.equal(
      (await current.requestsTo('/api/v1/posts')).length,
      1,
      'replaying a mutation would duplicate content',
    );
  });

  it('retries a GET after a transport failure', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    await current.testkit('testkit.fail_next', { path: '/api/v1/users/me', times: 1 });
    const me = await current.bot.me();
    assert.equal(me.id, current.info.bot_id);
    assert.ok((await current.requestsTo('/api/v1/users/me')).length >= 2);
  });

  it('treats a wrong token as a 401 that must not be retried', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    await current.transport.call('client.set_token', { token: `ofb_${'f'.repeat(64)}` });
    const error = await rejectsWith(UnauthorizedError, () => current.bot.me({ refresh: true }));
    assert.equal(error.status, 401);
    assert.equal(error.retryable, false);
    assert.match(error.message, /invalid bot token/);
    assert.equal((await current.requestsTo('/api/v1/users/me')).length, 1);

    await current.transport.call('client.set_token', { token: current.info.token });
  });
});

describe('raw requests', () => {
  it('hands the status back instead of throwing', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const current = kit.current;

    const ok = await current.bot.request('GET', '/api/v1/users/me');
    assert.equal(ok.ok, true);
    assert.equal(ok.status, 200);
    assert.equal(ok.body.id, current.info.bot_id);

    await current.forceStatus('/api/v1/users/me', 404);
    const missing = await current.bot.request('GET', '/api/v1/users/me');
    assert.equal(missing.ok, false);
    assert.equal(missing.status, 404);
  });

  it('sends a body and query parameters', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const created = await bot.request('POST', '/api/v1/posts', {
      body: { content: 'raw', visibility: 'public' },
    });
    assert.equal(created.ok, true, JSON.stringify(created));
    assert.equal(created.body.content, 'raw');

    const paged = await bot.request('GET', '/api/v1/posts', { query: { page: 3 } });
    assert.equal(paged.body.page, 3);
  });

  it('refuses an absolute path', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const error = await rejectsWith(InvalidArgumentError, () =>
      kit.current.bot.request('GET', 'https://evil.example/steal'),
    );
    assert.match(error.message, /server-relative/);
  });
});

describe('the escape hatch', () => {
  it('runs any named method', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const scheduled = await bot.call('posts.create', { content: 'via call' });
    assert.equal(scheduled.content, 'via call');

    await rejectsWith(InvalidArgumentError, () => bot.call('posts.create', {}));
  });
});
