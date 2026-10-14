/**
 * Shared test scaffolding.
 *
 * The suite drives the **real** mock server, the one implemented in Rust for the core's own
 * tests, reached through the sidecar's `testkit.*` methods. A second mock written in
 * JavaScript would drift from the handler it is supposed to imitate.
 *
 * That means the sidecar must be built with the feature:
 *
 *     cargo build -p openfield-bot-sidecar --features testkit
 */

import { afterEach, beforeEach } from 'node:test';

import { Bot, BotError, SidecarTransport } from '../src/index.js';

export const SIDECAR_FEATURE_HINT =
  'the sidecar was built without the `testkit` feature. Rebuild it with:\n' +
  '    cargo build -p openfield-bot-sidecar --features testkit\n' +
  '(or point OPENFIELD_BOT_SIDECAR at such a build)';

/** A mock server plus an authenticated bot, torn down after each test. */
export class Testkit {
  constructor() {
    this.transport = null;
    this.bot = null;
    this.info = null;
  }

  /**
   * Start a sidecar, start its mock server, and point a bot at it.
   *
   * @returns {Promise<Testkit>}
   * @throws {Error} with a rebuild instruction when the feature is missing.
   */
  async start() {
    this.transport = new SidecarTransport();
    await this.transport.whenReady();

    try {
      this.info = await this.transport.call('testkit.start');
    } catch (error) {
      this.transport.close();
      const hint = new Error(`${SIDECAR_FEATURE_HINT} (${error.message})`);
      hint.skip = true;
      throw hint;
    }

    await this.transport.call('client.init', {
      base_url: this.info.base_url,
      token: this.info.token,
    });
    // One child process owns the mock server; the `Bot` drives that same transport rather
    // than spawning a second one.
    this.bot = new Bot(null, null, { transport: this.transport });
    await this.bot.ready();
    return this;
  }

  /** Run `method` against the sidecar's testkit surface. */
  testkit(method, params) {
    return this.transport.call(method, params);
  }

  forceStatus(path, status, { retryAfterSecs, times = 1 } = {}) {
    return this.testkit('testkit.force_status', {
      path,
      status,
      times,
      ...(retryAfterSecs === undefined ? {} : { retry_after_secs: retryAfterSecs }),
    });
  }

  async requests() {
    return (await this.testkit('testkit.requests')).requests;
  }

  async requestsTo(path) {
    return (await this.requests()).filter((entry) => entry.path === path);
  }

  async broadcast(event) {
    const result = await this.testkit('testkit.broadcast', { event });
    return result.delivered;
  }

  async wsClients() {
    return (await this.testkit('testkit.ws_client_count')).clients;
  }

  /**
   * Insert a message from **another user**.
   *
   * Every mock route speaks as the bot under test, so this is the only way to produce
   * traffic the bot must react to rather than ignore.
   */
  seedMessage(conversationId, content, { senderId = 7, senderName = 'alice', broadcast = false } = {}) {
    return this.testkit('testkit.seed_message', {
      conversation_id: conversationId,
      sender_id: senderId,
      sender_name: senderName,
      content,
      broadcast,
    });
  }

  async stop() {
    try {
      await this.testkit('testkit.stop');
    } catch {
      /* already gone */
    }
    this.transport?.close();
  }
}

/**
 * Wire a `Testkit` into the current test, skipping it when the sidecar lacks `testkit`.
 *
 * @returns {{ current: () => Testkit }}
 */
export function useTestkit() {
  const holder = { current: null };

  beforeEach(async () => {
    const kit = new Testkit();
    try {
      await kit.start();
    } catch (error) {
      if (error.skip) {
        // node:test has no runtime `skip()`; the caller checks `current` for null.
        return;
      }
      throw error;
    }
    holder.current = kit;
  });

  afterEach(async () => {
    await holder.current?.stop();
    holder.current = null;
  });

  return holder;
}

/** Whether a sidecar without the testkit feature was encountered. */
export function requireKit(t, kit) {
  if (kit === null) {
    t.skip(SIDECAR_FEATURE_HINT);
    return false;
  }
  return true;
}

/**
 * Pull events from an async iterable with deadlines, so a failing test reports instead of
 * hanging forever.
 */
export class Puller {
  constructor(iterable) {
    this.iterator = iterable[Symbol.asyncIterator]();
    this.pending = this.iterator.next();
  }

  /**
   * @param {number} timeoutMs
   * @returns {Promise<object|symbol>}
   */
  async take(timeoutMs = 5_000) {
    const timeout = Symbol('timeout');
    const result = await Promise.race([
      this.pending,
      new Promise((resolve) => {
        const timer = setTimeout(() => resolve(timeout), timeoutMs);
        timer.unref?.();
      }),
    ]);
    if (result === timeout) return TIMEOUT;
    this.pending = this.iterator.next();
    if (result.done) return FINISHED;
    return result.value;
  }

  /** Skip the `connected` greeting the server sends every subscriber. */
  async realEvent(timeoutMs = 5_000) {
    for (;;) {
      const item = await this.take(timeoutMs);
      if (item === TIMEOUT || item === FINISHED) return item;
      if (item.type !== 'connected') return item;
    }
  }

  close() {
    this.iterator.return?.().catch(() => {});
  }
}

/** Returned by `Puller.take` when nothing arrived in time. */
export const TIMEOUT = Symbol('no event within the timeout');
/** Returned by `Puller.take` when the iterator ended. */
export const FINISHED = Symbol('iterator finished');

/** Poll `predicate` until it is true or the deadline passes. */
export async function waitUntil(predicate, timeoutMs = 5_000, intervalMs = 50) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    if (await predicate()) return true;
    if (Date.now() >= deadline) return false;
    await new Promise((resolve) => setTimeout(resolve, intervalMs));
  }
}

/** Assert that a call rejects with a particular error class. */
export async function rejectsWith(Class, body) {
  try {
    await body();
  } catch (error) {
    if (error instanceof Class) return error;
    throw new Error(`expected ${Class.name}, got ${error?.name}: ${error?.message}`);
  }
  throw new Error(`expected ${Class.name}, but the call resolved`);
}

export { Bot, BotError, SidecarTransport };
