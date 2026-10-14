/**
 * The sidecar transport: a helper process speaking newline-delimited JSON.
 *
 * Node drives the SDK through the sidecar rather than the C ABI, because loading a
 * `cdylib` from Node needs a native addon (`ffi-napi` and friends), which is a build step
 * and a Node-version coupling. A subprocess is neither: the only requirement is the Rust
 * binary, discovered automatically inside the SDK checkout.
 *
 * Protocol: one JSON object per line in both directions.
 *
 *     -> {"id":1,"method":"posts.create","params":{"content":"hello"}}
 *     <- {"id":1,"result":{"id":42,"content":"hello"}}
 *     <- {"event":"chat.message.created","data":{…},"recipients":[4242]}
 */

import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { delimiter, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createInterface } from 'node:readline';

import { ClosedError, TimeoutError, TransportError, errorFromEnvelope } from './errors.js';

/** The environment variable naming an explicit sidecar binary. */
export const SIDECAR_ENV = 'OPENFIELD_BOT_SIDECAR';

const SIDECAR_BASENAME = process.platform === 'win32' ? 'openfield-bot-sidecar.exe' : 'openfield-bot-sidecar';

/**
 * The SDK checkout containing this package, when it is being run from source.
 *
 * @returns {string|null}
 */
export function repoRoot() {
  let here = dirname(fileURLToPath(import.meta.url));
  for (;;) {
    const candidate = resolve(here, '..');
    if (candidate === here) return null;
    if (existsSync(join(candidate, 'Cargo.toml')) && existsSync(join(candidate, 'core'))) {
      return candidate;
    }
    here = candidate;
  }
}

/**
 * Locate the sidecar binary.
 *
 * Resolution order: the `explicit` argument, `OPENFIELD_BOT_SIDECAR`, the cargo target
 * directories of the checkout this package lives in, then `PATH`.
 *
 * @param {string} [explicit]
 * @param {object} [search] Overrides for the search itself, so the failure path is
 *   reachable from a test on a machine where the SDK *has* been built.
 * @param {string[]} [search.searchDirs] Candidate directories, in order.
 * @param {string} [search.pathEnv] The `PATH` to scan.
 * @returns {string}
 * @throws {TransportError} when nothing was found, naming every place that was searched.
 */
export function findSidecar(explicit, search = {}) {
  const searched = [];

  for (const candidate of [explicit, process.env[SIDECAR_ENV]]) {
    if (!candidate) continue;
    if (existsSync(candidate)) return candidate;
    searched.push(candidate);
  }

  let directories = search.searchDirs;
  if (directories === undefined) {
    directories = [];
    const root = repoRoot();
    if (root) {
      // `debug` before `release`: during development the debug build is the fresh one.
      directories.push(join(root, 'target', 'debug'), join(root, 'target', 'release'));
    }
  }
  for (const directory of directories) {
    const candidate = join(directory, SIDECAR_BASENAME);
    if (existsSync(candidate)) return candidate;
    searched.push(candidate);
  }

  const pathEnv = search.pathEnv ?? process.env.PATH ?? '';
  for (const directory of pathEnv.split(delimiter)) {
    if (!directory) continue;
    const candidate = join(directory, SIDECAR_BASENAME);
    if (existsSync(candidate)) return candidate;
  }
  searched.push(`$PATH:${SIDECAR_BASENAME}`);

  throw new TransportError(
    'could not find the openfield-bot-sidecar binary. Build it with ' +
      '`cargo build -p openfield-bot-sidecar`, or point ' +
      `${SIDECAR_ENV} at it. Searched: ${searched.join(', ')}`,
  );
}

/** One in-flight request. */
class Pending {
  constructor() {
    this.promise = new Promise((resolvePromise, rejectPromise) => {
      this.resolve = resolvePromise;
      this.reject = rejectPromise;
    });
  }
}

/**
 * Runs the SDK in a child process and talks JSON over stdin/stdout.
 */
export class SidecarTransport {
  /**
   * The process is started eagerly, and the client is configured before the constructor
   * returns, so a missing binary or a malformed token fails here rather than on the
   * caller's first real request.
   *
   * @param {object} [config]
   * @param {string} [config.binary] Explicit sidecar path.
   * @param {object} [config.options] Client options: `base_url`, `token`, `timeout_ms`…
   * @param {number} [config.startupTimeoutMs]
   */
  constructor({ binary, options = {}, startupTimeoutMs = 10_000 } = {}) {
    this.binary = findSidecar(binary);
    this.options = { ...options };
    this.closed = false;
    this.pending = new Map();
    this.nextId = 0;
    this.events_ = null;
    this.stderrTail = [];
    this.ready = null;

    this.child = spawn(this.binary, [], {
      stdio: ['pipe', 'pipe', 'pipe'],
      env: { ...process.env },
    });

    this.child.on('error', (error) => {
      this.failAll(new TransportError(`the sidecar could not be started: ${error.message}`));
    });

    this.lines = createInterface({ input: this.child.stdout });
    this.lines.on('line', (line) => this.handleLine(line));

    this.child.stderr.setEncoding('utf8');
    this.child.stderr.on('data', (chunk) => {
      for (const line of String(chunk).split('\n')) {
        if (!line) continue;
        this.stderrTail.push(line);
        if (this.stderrTail.length > 200) this.stderrTail.splice(0, 100);
      }
    });

    this.child.on('close', () => {
      this.failAll(new TransportError('the sidecar exited'));
    });

    this.ready = this.start(startupTimeoutMs);
  }

  /** Configure the client, then prove the child answers. */
  async start(timeoutMs) {
    try {
      await this.call('client.init', this.options, { timeoutMs });
      await this.call('version', { timeoutMs });
    } catch (error) {
      const tail = this.stderrTail.slice(-5).join('\n');
      throw new TransportError(
        `the sidecar at ${this.binary} did not accept its configuration` +
          (tail ? ` (stderr: ${tail})` : ''),
      );
    }
  }

  /** Await construction, for callers that want a plain async factory. */
  async whenReady() {
    return this.ready;
  }

  /** A short label for diagnostics. */
  get name() {
    return `sidecar(${this.binary})`;
  }

  handleLine(line) {
    const trimmed = line.trim();
    if (!trimmed) return;

    let message;
    try {
      message = JSON.parse(trimmed);
    } catch {
      // Not protocol, which usually means something else wrote to stdout.
      this.stderrTail.push(`non-JSON stdout: ${trimmed.slice(0, 200)}`);
      return;
    }

    if (message && typeof message === 'object' && 'event' in message) {
      this.events_?.push(message);
      return;
    }

    const id = message?.id;
    const pending = this.pending.get(id);
    if (!pending) return;
    this.pending.delete(id);

    if (message.error) {
      pending.reject(errorFromEnvelope(message.error));
    } else {
      pending.resolve(message.result);
    }
  }

  failAll(error) {
    for (const [, pending] of this.pending) pending.reject(error);
    this.pending.clear();
    this.events_?.fail(error);
  }

  /**
   * Run a named method.
   *
   * @param {string} method A name from the shared index, e.g. `posts.create`.
   * @param {object} [params] The parameter object; omit for a method that takes none.
   * @param {object} [options]
   * @param {number} [options.timeoutMs] How long to wait; 0 waits indefinitely.
   * @returns {Promise<any>}
   */
  async call(method, params = {}, { timeoutMs = 60_000 } = {}) {
    if (this.closed) throw new ClosedError('the transport is closed');

    this.nextId += 1;
    const id = this.nextId;
    const pending = new Pending();
    this.pending.set(id, pending);

    let timer = null;
    const payload = `${JSON.stringify({ id, method, params: params ?? {} })}\n`;
    try {
      this.child.stdin.write(payload);
    } catch (error) {
      this.pending.delete(id);
      throw new TransportError(`the sidecar pipe is broken: ${error.message}`);
    }

    if (timeoutMs > 0) {
      timer = setTimeout(() => {
        if (this.pending.delete(id)) {
          pending.reject(new TimeoutError(`\`${method}\` did not answer within ${timeoutMs}ms`));
        }
      }, timeoutMs);
      timer.unref?.();
    }

    try {
      return await pending.promise;
    } finally {
      if (timer) clearTimeout(timer);
    }
  }

  /**
   * Subscribe to the realtime channel. The subscription is established before this
   * returns, so a caller can check the server's own client count and already see it.
   *
   * @param {object} [params] `{ types, conversation_ids, queue_capacity, reconnect }`
   * @returns {Promise<SidecarEvents>}
   */
  async events(params = {}) {
    return SidecarEvents.create(this, params);
  }

  /** The stream counters from the child. */
  async eventStats() {
    return this.call('events.stats');
  }

  /** Stop the child. Safe to call more than once. */
  close() {
    if (this.closed) return;
    this.closed = true;

    this.events_?.fail(new ClosedError('the transport was closed'));
    this.events_ = null;

    try {
      this.child.stdin.end();
    } catch {
      /* already gone */
    }
    this.lines?.close();

    const killTimer = setTimeout(() => {
      if (this.child.exitCode === null) this.child.kill('SIGTERM');
    }, 1_000);
    killTimer.unref?.();

    this.child.once('close', () => clearTimeout(killTimer));
  }
}

/**
 * An established subscription.
 *
 * An async iterator rather than an async generator, because a generator would not issue
 * the `events.subscribe` call until its first `next()` -- a difference the caller cannot
 * see, but which means a broadcast sent right after subscribing is missed.
 */
export class SidecarEvents {
  static async create(transport, params) {
    const subscription = new SidecarEvents(transport);
    try {
      await transport.call('events.subscribe', params);
    } catch (error) {
      subscription.closed = true;
      throw error;
    }
    return subscription;
  }

  constructor(transport) {
    this.transport = transport;
    this.closed = false;
    this.buffer = [];
    this.waiters = [];
    this.failure = null;

    // The transport routes event frames here; only one subscription may be active, so a
    // new one replaces the old.
    transport.events_?.fail(new ClosedError('replaced by a newer subscription'));
    transport.events_ = this;
  }

  /** @param {object} event */
  push(event) {
    const waiter = this.waiters.shift();
    if (waiter) {
      waiter.resolve({ value: event, done: false });
    } else {
      this.buffer.push(event);
    }
  }

  /** @param {Error} error */
  fail(error) {
    this.failure = error;
    for (const waiter of this.waiters.splice(0)) waiter.resolve({ value: undefined, done: true });
  }

  [Symbol.asyncIterator]() {
    return this;
  }

  /** @returns {Promise<IteratorResult<object>>} */
  next() {
    if (this.closed) return Promise.resolve({ value: undefined, done: true });
    if (this.buffer.length > 0) {
      return Promise.resolve({ value: this.buffer.shift(), done: false });
    }
    if (this.failure) {
      const error = this.failure;
      this.failure = null;
      return Promise.reject(error);
    }
    const pending = new Pending();
    this.waiters.push(pending);
    return pending.promise;
  }

  /** Drop the subscription. Safe to call more than once. */
  close() {
    if (this.closed) return;
    this.closed = true;
    if (this.transport.events_ === this) {
      this.transport.events_ = null;
      void this.transport.call('events.unsubscribe').catch(() => {});
    }
    for (const waiter of this.waiters.splice(0)) waiter.resolve({ value: undefined, done: true });
  }

  /** Release without the round trip, used when the transport is going away. */
  abort() {
    this.closed = true;
    for (const waiter of this.waiters.splice(0)) waiter.resolve({ value: undefined, done: true });
  }

  /** The stream counters, or `{}` once closed. */
  async stats() {
    if (this.closed) return {};
    return this.transport.eventStats();
  }
}
