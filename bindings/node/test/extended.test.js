/**
 * The money, task, moderation, E2EE, tip, chunked-upload and plugin methods.
 *
 * Each case runs against the Rust mock server through the sidecar, so what is being checked
 * is the whole path: the JavaScript facade, the JSON method table, the Rust client and the
 * endpoint contract the mock reproduces from the real handlers.
 */

import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, it } from 'node:test';

import {
  BadRequestError,
  ConflictError,
  ForbiddenError,
  InvalidArgumentError,
  NotFoundError,
  PUNISHMENT_TYPES,
  UnauthorizedError,
} from '../src/index.js';
import { requireKit, rejectsWith, useTestkit } from './support.js';

const kit = useTestkit();

describe('wallet', () => {
  it('reports a decimal coin balance', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const wallet = await bot.wallet({ page: 1, limit: 5 });
    // The mock opens with 1000 coins; Cents(100000) marshals as 1000.
    assert.equal(wallet.balance, 1000);
    assert.equal(wallet.page, 1);
    assert.equal(wallet.limit, 5);
  });

  it('recharges and deducts with the sign handled for you', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const credited = await bot.rechargeWallet(42, 12.5, { description: '补偿' });
    assert.equal(credited.balance, 1012.5);
    assert.equal(credited.transaction.amount, 12.5);
    assert.equal(credited.transaction.type, 'recharge');
    assert.equal(credited.transaction.description, '补偿');

    const debited = await bot.deductWallet(42, 2.5);
    assert.equal(debited.balance, 1010);
    assert.equal(debited.transaction.amount, -2.5);
  });

  it('keeps the sign the caller gave on the raw form', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    // A positive amount with type 'deduct' is a credit: the handler stores it verbatim
    // and never reconciles the two.
    const adjusted = await bot.adjustWallet(42, 1, { type: 'deduct' });
    assert.equal(adjusted.balance, 1001);
    assert.equal(adjusted.transaction.type, 'deduct');
  });

  it('refuses a deduction beyond the balance', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;
    await rejectsWith(BadRequestError, () => bot.deductWallet(42, 5000));
  });

  it('refuses an amount that is not a number', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;
    await rejectsWith(InvalidArgumentError, () => bot.rechargeWallet(42, '10'));
    await rejectsWith(InvalidArgumentError, () => bot.rechargeWallet(42, NaN));
  });
});

describe('tasks', () => {
  it('exposes progress and claimability', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const catalog = await bot.tasks();
    assert.equal(catalog.streak, 4);
    assert.equal(catalog.makeup_cost, 100);
    const byCode = Object.fromEntries(catalog.tasks.map((task) => [task.code, task]));
    assert.equal(byCode.daily_posts_1.claimable, true);
    assert.equal(byCode.daily_chat_100.claimable, false);
    assert.equal(byCode.daily_posts_1.reward_currency, 5);
  });

  it('refuses a second claim of the same task', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const claimed = await bot.claimTask('daily_posts_1');
    assert.equal(claimed.claimed, true);
    assert.equal(claimed.currency, 5);
    await rejectsWith(ConflictError, () => bot.claimTask('daily_posts_1'));
  });

  it('reports an unknown code as not found', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;
    await rejectsWith(NotFoundError, () => bot.claimTask('nope'));
  });

  it('keeps the daily route and the task code spelling apart', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    // Wrong spelling: the generic :code route answers instead of the sign-in.
    await rejectsWith(ConflictError, () => bot.claimTask('daily_login'));
    const signin = await bot.claimDailyLogin();
    assert.equal(signin.granted, true);
    assert.equal(signin.currency, 20);
    const again = await bot.claimDailyLogin();
    assert.equal(again.granted, false);
  });

  it('lists the missed day in the calendar', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const calendar = await bot.checkinCalendar();
    assert.equal(calendar.today, '2026-08-04');
    const missed = calendar.days.filter((day) => !day.signed).map((day) => day.date);
    assert.deepEqual(missed, ['2026-08-03']);
  });

  it('charges coins for a make-up and never the currency', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const result = await bot.makeupCheckin();
    assert.equal(result.granted, true);
    assert.equal(result.cost, 100);
    assert.equal(result.currency, undefined);
  });

  it('shape-checks a make-up date locally', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    await rejectsWith(InvalidArgumentError, () => bot.makeupCheckinOn('03/08/2026'));
    const dated = await bot.makeupCheckinOn('2026-08-03');
    assert.equal(dated.date, '2026-08-03');
  });

  it('reads the experience history', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const history = await bot.expHistory({ limit: 5 });
    assert.equal(history.entries.length, 2);
    assert.equal(history.entries[0].reason, 'daily_bonus');
  });
});

describe('membership and the payment pin', () => {
  it('forbids payments while no pin is set', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const membership = await bot.membership();
    assert.equal(membership.level, 0);
    assert.equal(membership.tiers.length, 4);
    await rejectsWith(ForbiddenError, () => bot.purchaseMembership(1, '123456'));
  });

  it('unlocks a purchase once the pin is set', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const set = await bot.setPaymentPin('123456');
    assert.equal(set.has_pin, true);
    const result = await bot.purchaseMembership(1, '123456');
    assert.equal(result.paid, 100);
    assert.equal(result.status.level, 1);
  });

  it('requires exactly six digits', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;
    await rejectsWith(InvalidArgumentError, () => bot.setPaymentPin('12'));
    await rejectsWith(InvalidArgumentError, () => bot.setPaymentPin('abcdef'));
  });

  it('rejects a wrong pin', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    await bot.setPaymentPin('123456');
    await rejectsWith(UnauthorizedError, () => bot.purchaseMembership(1, '999999'));
    await rejectsWith(UnauthorizedError, () => bot.verifyPaymentPin('000000'));
    assert.equal(await bot.verifyPaymentPin('123456'), true);
  });

  it('needs the current pin to replace it', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    await bot.setPaymentPin('123456');
    await rejectsWith(ForbiddenError, () => bot.setPaymentPin('654321'));
    await bot.setPaymentPin('654321', { oldPin: '123456' });
    assert.equal(await bot.verifyPaymentPin('654321'), true);
    await bot.changePaymentPin('654321', '111111');
    assert.equal(await bot.verifyPaymentPin('111111'), true);
  });

  it('needs the pin only to enable auto-renew', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    await bot.setPaymentPin('123456');
    await rejectsWith(InvalidArgumentError, () => bot.setAutoRenew(true));
    const enabled = await bot.setAutoRenew(true, { pin: '123456' });
    assert.equal(enabled.auto_renew, true);
    const disabled = await bot.setAutoRenew(false);
    assert.equal(disabled.auto_renew, false);
  });

  it('reads purchase history and grants directly', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    await bot.setPaymentPin('123456');
    await bot.purchaseMembership(2, '123456');
    const history = await bot.membershipPurchases();
    assert.equal(history.purchases[0].price_coins, 200);

    const cleared = await bot.grantMembership(9, 0);
    assert.equal(cleared.level, 0);
    const granted = await bot.grantMembership(9, 3, { days: 30 });
    assert.equal(granted.level, 3);
  });

  it('refuses a level outside the catalog', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;
    await rejectsWith(InvalidArgumentError, () => bot.purchaseMembership(5, '123456'));
    await rejectsWith(InvalidArgumentError, () => bot.purchaseMembership(1, ''));
  });

  it('adjusts experience through delta', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const result = await bot.adjustExp(9, -25, { description: '回滚' });
    assert.equal(result.level, 5);
  });
});

describe('punishments', () => {
  it('records a warning and lists it', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const receipt = await bot.punish(7, 'warning', { reason: '刷屏' });
    assert.ok(receipt.punishment_id > 0);
    assert.equal(receipt.type, 'warning');

    const history = await bot.punishments(7, { limit: 10 });
    assert.equal(history.punishments.length, 1);
    assert.equal(history.punishments[0].reason, '刷屏');
    assert.deepEqual(history.permission_bans, ['posts.create']);
  });

  it('sends what each type requires', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const ban = await bot.punish(7, 'temp_ban', { reason: '冷却', durationMinutes: 60 });
    assert.ok(ban.expires_at);
    const revoked = await bot.punish(8, 'revoke', {
      reason: '滥用',
      permissionKey: 'posts.create',
    });
    assert.equal(revoked.type, 'revoke');
    const restored = await bot.punish(8, 'restore', { permissionKey: 'posts.create' });
    assert.equal(restored.type, 'restore');
  });

  it('refuses missing parameters and unknown types locally', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    await rejectsWith(InvalidArgumentError, () => bot.punish(7, 'revoke'));
    await rejectsWith(InvalidArgumentError, () =>
      bot.punish(7, 'temp_ban', { reason: 'x', durationMinutes: 0 }),
    );
    await rejectsWith(InvalidArgumentError, () => bot.punish(7, 'exile'));
    for (const type of PUNISHMENT_TYPES) {
      assert.equal(typeof type, 'string');
    }
  });

  it('applies the rank and self guards', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const me = (await bot.id()) ?? 4242;
    await rejectsWith(ForbiddenError, () => bot.punish(me, 'ban', { reason: 'no' }));
    await rejectsWith(ForbiddenError, () => bot.punish(1, 'warning', { reason: 'admin' }));
    await rejectsWith(ForbiddenError, () => bot.punish(2, 'ban', { reason: 'peer' }));
  });
});

describe('tips', () => {
  it('reports cents while the request carries coins', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    await bot.setPaymentPin('123456');
    const tip = await bot.tipPost(555, 5, '123456');
    assert.equal(tip.amount, 500);
    assert.equal(tip.net_amount, 475);
  });

  it('forbids a tip while no pin is set', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;
    await rejectsWith(ForbiddenError, () => bot.tipPost(555, 5, '123456'));
  });

  it('refuses tipping your own post', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    await bot.setPaymentPin('123456');
    await rejectsWith(BadRequestError, () => bot.tipPost(777, 5, '123456'));
  });

  it('requires whole coins', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    await bot.setPaymentPin('123456');
    await rejectsWith(InvalidArgumentError, () => bot.tipPost(555, 0, '123456'));
    await rejectsWith(InvalidArgumentError, () => bot.tipPost(555, 100001, '123456'));
    // Go will not decode 1.5 into an int64, so a fractional coin count is refused here.
    await rejectsWith(InvalidArgumentError, () => bot.tipPost(555, 1.5, '123456'));
  });
});

describe('end-to-end encryption', () => {
  it('keys the envelopes by user id', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const put = await bot.putE2eeKeys(33, { 7: 'AAA', 9: 'BBB' });
    assert.equal(put.version, 1);
    const keys = await bot.e2eeKeys(33);
    assert.equal(keys.version, 1);
    assert.equal(keys.envelopes.length, 2);
    assert.deepEqual(
      keys.envelopes.map((envelope) => envelope.target_user_id).sort((a, b) => a - b),
      [7, 9],
    );
  });

  it('accepts a Map and bumps the version on rotation', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    await bot.putE2eeKeys(33, new Map([[7, 'AAA']]));
    const second = await bot.putE2eeKeys(33, { 7: 'BBB', 9: 'CCC' });
    assert.equal(second.version, 2);
  });

  it('refuses an empty batch locally', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;
    await rejectsWith(InvalidArgumentError, () => bot.putE2eeKeys(33, {}));
  });

  it('sets and clears the public key', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;
    await bot.setE2eePublicKey('ed25519:AAAA');
    await bot.setE2eePublicKey('');
  });
});

describe('chunked uploads', () => {
  it('runs the whole flow', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const session = await bot.startChunkedUpload('big.bin', 200, 2);
    assert.equal(session.total_chunks, 2);
    const uploadId = session.upload_id;

    assert.deepEqual((await bot.chunkStatus(uploadId)).uploaded, []);
    await bot.uploadChunk(uploadId, 1, Buffer.alloc(100, 7));
    assert.deepEqual((await bot.chunkStatus(uploadId)).uploaded, [1]);
    await bot.uploadChunk(uploadId, 2, Buffer.alloc(100, 8));

    const attachment = await bot.completeChunkedUpload(uploadId, 'big.bin', 200, 2);
    assert.equal(attachment.original_name, 'big.bin');
    assert.equal(attachment.size_bytes, 200);
  });

  it('reports a gap as a conflict', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const session = await bot.startChunkedUpload('big.bin', 300, 3);
    await bot.uploadChunk(session.upload_id, 1, Buffer.alloc(100, 1));
    await rejectsWith(ConflictError, () =>
      bot.completeChunkedUpload(session.upload_id, 'big.bin', 300, 3),
    );
  });

  it('splits the payload in the driver', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const attachment = await bot.uploadChunked('report.bin', Buffer.alloc(250, 9), {
      chunkBytes: 100,
    });
    assert.equal(attachment.original_name, 'report.bin');
    assert.equal(attachment.size_bytes, 250);
  });

  it('resumes and re-sends only the missing parts', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;
    const { current } = kit;

    const session = await bot.startChunkedUpload('resume.bin', 200, 2);
    const uploadId = session.upload_id;
    await bot.uploadChunk(uploadId, 1, Buffer.alloc(100, 1));
    await current.clearRequests();

    const attachment = await bot.uploadChunked(
      'resume.bin',
      Buffer.concat([Buffer.alloc(100, 1), Buffer.alloc(100, 2)]),
      { chunkBytes: 100, uploadId },
    );
    assert.equal(attachment.original_name, 'resume.bin');

    const parts = (await current.requests()).filter(
      (entry) =>
        entry.path.startsWith('/api/v1/attachments/chunk/') && entry.path.endsWith('/2'),
    );
    assert.equal(parts.length, 1, 'only part 2 should have been re-sent');
  });

  it('checks declarations locally', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    await rejectsWith(InvalidArgumentError, () => bot.startChunkedUpload('a.bin', 0, 1));
    await rejectsWith(InvalidArgumentError, () =>
      bot.startChunkedUpload('a.bin', 100, bot.constructor.MAX_CHUNKS + 1),
    );
    await rejectsWith(InvalidArgumentError, () =>
      bot.startChunkedUpload('a.bin', bot.constructor.CHUNK_BYTES + 1, 1),
    );
    await rejectsWith(InvalidArgumentError, () => bot.uploadChunked('a.bin', Buffer.alloc(0)));
  });

  it('checks the chunk index and size locally', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const session = await bot.startChunkedUpload('a.bin', 10, 1);
    await rejectsWith(InvalidArgumentError, () => bot.uploadChunk(session.upload_id, 0, Buffer.alloc(1)));
    await rejectsWith(InvalidArgumentError, () =>
      bot.uploadChunk(
        session.upload_id,
        1,
        Buffer.alloc(bot.constructor.CHUNK_BYTES + 1),
      ),
    );
  });

  it('reports an unknown session as not found', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;
    await rejectsWith(NotFoundError, () => bot.chunkStatus('chunk-does-not-exist'));
  });
});

describe('reuse and burn', () => {
  it('returns the existing attachment for a known hash', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const reused = await bot.attachmentByHash('A'.repeat(64));
    assert.equal(reused.id, 9001);
    assert.equal(reused.original_name, 'chart.png');
  });

  it('reports an unknown hash as not found', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;
    await rejectsWith(NotFoundError, () => bot.attachmentByHash('b'.repeat(64)));
  });

  it('refuses a malformed hash locally', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    for (const digest of ['nope', 'z'.repeat(64), 'a'.repeat(63)]) {
      await rejectsWith(InvalidArgumentError, () => bot.attachmentByHash(digest));
    }
  });

  it('arms a burn and caps the window', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const armed = await bot.armAttachmentBurn(9001, 300);
    assert.equal(armed.id, 9001);
    assert.ok(armed.burn_at);
    for (const seconds of [0, -1, 86401]) {
      await rejectsWith(InvalidArgumentError, () => bot.armAttachmentBurn(9001, seconds));
    }
  });
});

describe('the plugin store', () => {
  it('reads the catalog and one plugin', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const catalog = await bot.plugins();
    assert.equal(catalog.plugins.length, 1);
    assert.equal(catalog.plugins[0].id, 'demo');
    const plugin = await bot.plugin('demo');
    assert.equal(plugin.name, 'Demo');
    assert.equal(plugin.verified, true);
  });

  it('rejects an unknown or malformed id', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    await rejectsWith(NotFoundError, () => bot.plugin('missing'));
    for (const pluginId of ['..', '.', 'AB', 'a/b', '']) {
      await rejectsWith(InvalidArgumentError, () => bot.plugin(pluginId));
    }
  });

  it('downloads a bundle as a Buffer', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const bundle = await bot.downloadPlugin('demo');
    assert.ok(Buffer.isBuffer(bundle));
    assert.deepEqual(bundle, Buffer.from('PK\x03\x04demo-bundle', 'binary'));
  });

  it('writes a bundle to disk', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const directory = mkdtempSync(join(tmpdir(), 'openfield-node-'));
    try {
      const target = join(directory, 'bundle.zip');
      const written = await bot.downloadPluginTo('demo', target);
      const bytes = readFileSync(target);
      assert.equal(written, bytes.length);
      assert.deepEqual(bytes, Buffer.from('PK\x03\x04demo-bundle', 'binary'));
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });

  it('uploads, publishes and deletes', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const uploaded = await bot.uploadPlugin('bundle.zip', Buffer.from([1, 2, 3]), {
      publish: true,
    });
    assert.equal(uploaded.message, 'uploaded');
    assert.equal(uploaded.plugin.published, true);

    await bot.unpublishPlugin('demo');
    assert.deepEqual((await bot.plugins()).plugins, []);
    await rejectsWith(NotFoundError, () => bot.plugin('demo'));

    await bot.publishPlugin('demo');
    assert.equal((await bot.plugins()).plugins.length, 1);
    await bot.deletePlugin('demo');
    await rejectsWith(NotFoundError, () => bot.deletePlugin('demo'));
  });

  it('uploads a bundle from a path', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    const directory = mkdtempSync(join(tmpdir(), 'openfield-node-'));
    try {
      const source = join(directory, 'bundle.zip');
      writeFileSync(source, Buffer.from('PK\x03\x04payload', 'binary'));
      const uploaded = await bot.uploadPluginFile(source);
      assert.equal(uploaded.plugin.id, 'demo');
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });

  it('refuses an oversize bundle locally', async (t) => {
    if (!requireKit(t, kit.current)) return;
    const { bot } = kit.current;

    await rejectsWith(InvalidArgumentError, () =>
      bot.uploadPlugin('bundle.zip', Buffer.alloc(5 * 1024 * 1024 + 1)),
    );
  });
});
