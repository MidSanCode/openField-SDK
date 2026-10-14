/**
 * The handshake and error mapping, without needing a server.
 */

import assert from 'node:assert/strict';
import { describe, it } from 'node:test';

import {
  BadRequestError,
  BotError,
  ClosedError,
  ConflictError,
  ERROR_KINDS,
  ForbiddenError,
  InvalidArgumentError,
  NetworkError,
  NotFoundError,
  RateLimitedError,
  REACTIONS,
  REPORT_TARGETS,
  SIDECAR_ENV,
  ServerError,
  SidecarTransport,
  UnauthorizedError,
  VISIBILITIES,
  errorFromEnvelope,
  findSidecar,
  repoRoot,
} from '../src/index.js';
import { rejectsWith } from './support.js';

describe('errorFromEnvelope', () => {
  it('maps each status onto its class', () => {
    const expectations = new Map([
      [400, BadRequestError],
      [401, UnauthorizedError],
      [403, ForbiddenError],
      [404, NotFoundError],
      [409, ConflictError],
      [429, RateLimitedError],
      [500, ServerError],
      [503, ServerError],
    ]);
    for (const [status, Class] of expectations) {
      const error = errorFromEnvelope({ kind: 'http', status, message: `http ${status}` });
      assert.ok(error instanceof Class, `expected ${Class.name} for ${status}`);
      assert.equal(error.status, status);
    }
  });

  it('honours an SDK kind directly', () => {
    const error = errorFromEnvelope({ kind: 'invalid_argument', message: 'bad field' });
    assert.ok(error instanceof InvalidArgumentError);
    assert.equal(error.kind, 'invalid_argument');
  });

  it('falls back to the base class for an unknown kind', () => {
    const error = errorFromEnvelope({ kind: 'something_new', message: '?' });
    assert.equal(Object.getPrototypeOf(error), BotError.prototype);
    assert.equal(error.kind, 'something_new');
  });

  it('keeps the permission and the retry hint', () => {
    const error = errorFromEnvelope({
      kind: 'http',
      status: 403,
      message: 'permission denied',
      permission: 'posts.create',
      retryable: false,
      api_error: 'permission denied',
      body: { error: 'permission denied' },
    });
    assert.equal(error.permission, 'posts.create');
    assert.equal(error.retryable, false);
    assert.equal(error.apiError, 'permission denied');
    assert.equal(error.body.error, 'permission denied');
  });

  it('carries retry_after', () => {
    const error = errorFromEnvelope({
      kind: 'http',
      status: 429,
      message: 'slow down',
      retry_after_secs: 12,
    });
    assert.equal(error.retryAfterSecs, 12);
    assert.equal(error.retryable, true);
  });

  it('survives a non-object error', () => {
    const error = errorFromEnvelope('boom');
    assert.ok(error instanceof BotError);
    assert.match(error.message, /boom/);
  });

  it('has a class for every kind', () => {
    assert.equal(ERROR_KINDS.network, NetworkError);
    assert.equal(ERROR_KINDS.rate_limited, RateLimitedError);
    assert.equal(ERROR_KINDS.closed, ClosedError);
  });

  it('marks network and server errors retryable, and auth errors not', () => {
    assert.equal(new NetworkError('reset').retryable, true);
    assert.equal(new ServerError('boom', { status: 500 }).retryable, true);
    assert.equal(new ForbiddenError('nope', { status: 403 }).retryable, false);
    assert.equal(new UnauthorizedError('bad token', { status: 401 }).retryable, false);
  });
});

describe('discovery', () => {
  it('finds the checkout this package lives in', () => {
    const root = repoRoot();
    assert.ok(root, 'the package should be running from the SDK checkout');
  });

  it('uses an explicit path when it exists', () => {
    const real = findSidecar();
    assert.equal(findSidecar(real), real);
  });

  it('reports every place it looked when nothing is found', () => {
    const searched = [];
    assert.throws(
      () => findSidecar('/nonexistent/explicit', { searchDirs: searched, pathEnv: '' }),
      (error) => {
        assert.match(error.message, /cargo build/);
        assert.match(error.message, /\/nonexistent\/explicit/);
        return true;
      },
    );
  });

  it('honours the environment variable', () => {
    const real = findSidecar();
    const previous = process.env[SIDECAR_ENV];
    try {
      process.env[SIDECAR_ENV] = real;
      assert.equal(findSidecar(), real);

      process.env[SIDECAR_ENV] = '/nonexistent/from-env';
      assert.throws(
        () => findSidecar(undefined, { searchDirs: [], pathEnv: '' }),
        (error) => {
          assert.match(error.message, /\/nonexistent\/from-env/);
          return true;
        },
      );
    } finally {
      if (previous === undefined) delete process.env[SIDECAR_ENV];
      else process.env[SIDECAR_ENV] = previous;
    }
  });
});

describe('vocabularies', () => {
  it('mirrors the reactions the server accepts', () => {
    assert.deepEqual(REACTIONS, ['like', 'dislike', 'love', 'haha', 'wow', 'sad', 'angry']);
  });

  it('mirrors the visibilities the server accepts', () => {
    assert.deepEqual(VISIBILITIES, ['public', 'login', 'friends', 'private']);
  });

  it('mirrors the report targets the server accepts', () => {
    assert.deepEqual(REPORT_TARGETS, ['post', 'message', 'user']);
  });
});

describe('SidecarTransport handshake', () => {
  it('reports the version, the protocol and every method', async () => {
    const transport = new SidecarTransport();
    try {
      await transport.whenReady();

      const version = await transport.call('version');
      assert.equal(version.name, 'openfield-bot-sdk');
      assert.equal(version.protocol, 1);

      const caps = await transport.call('capabilities');
      assert.equal(caps.protocol, 1);
      assert.equal(caps.features.ffi, true);
      assert.ok(caps.methods.length > 80);
      assert.equal(new Set(caps.methods).size, caps.methods.length, 'names must be unique');
      for (const expected of ['posts.create', 'chat.send', 'camps.list', 'events.subscribe']) {
        assert.ok(caps.methods.includes(expected), `missing ${expected}`);
      }
    } finally {
      transport.close();
    }
  });

  it('rejects an unknown method', async () => {
    const transport = new SidecarTransport();
    try {
      await transport.whenReady();
      await rejectsWith(InvalidArgumentError, () => transport.call('nope.nope'));
    } finally {
      transport.close();
    }
  });

  it('never exposes the token in the config', async () => {
    const transport = new SidecarTransport();
    try {
      await transport.whenReady();
      const token = `ofb_${'a'.repeat(64)}`;
      await transport.call('client.init', { base_url: 'http://127.0.0.1:9', token });

      const config = await transport.call('client.config');
      assert.equal(config.has_token, true);
      assert.ok(!JSON.stringify(config).includes('a'.repeat(64)));
      assert.match(config.token, /ofb_/);
    } finally {
      transport.close();
    }
  });

  it('switches between bot and anonymous', async () => {
    const transport = new SidecarTransport();
    try {
      await transport.whenReady();
      assert.equal((await transport.call('client.has_token')).has_token, false);
      await rejectsWith(InvalidArgumentError, () => transport.call('me'));

      await transport.call('client.set_token', { token: `ofb_${'b'.repeat(64)}` });
      assert.equal((await transport.call('client.has_token')).has_token, true);

      await transport.call('client.set_token', { token: '' });
      assert.equal((await transport.call('client.has_token')).has_token, false);
    } finally {
      transport.close();
    }
  });

  it('rejects a malformed token without a request', async () => {
    const transport = new SidecarTransport();
    try {
      await transport.whenReady();
      const error = await rejectsWith(BotError, () =>
        transport.call('client.init', { token: 'not-a-token' }),
      );
      assert.match(error.message, /ofb_/);
    } finally {
      transport.close();
    }
  });

  it('refuses to work once closed', async () => {
    const transport = new SidecarTransport();
    await transport.whenReady();
    transport.close();
    await rejectsWith(ClosedError, () => transport.call('version'));
    transport.close(); // idempotent
  });
});
