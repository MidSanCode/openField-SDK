/**
 * The `Bot` facade: one Node API over the transport.
 *
 * `Bot` adds nothing the Rust core cannot do. It maps JavaScript names onto the shared
 * method index and turns the documented bot recipes (`bots.md` 7, 9 and 12) into small
 * helpers. Everything it can do is also reachable through {@link Bot#call}, so a missing
 * wrapper is never a limitation.
 *
 * Two habits the helpers encode, because they are easy to get wrong by hand:
 *
 * - **Never act on your own messages.** The event channel echoes what the bot sends, and a
 *   bot that reacts to its own output loops. {@link Bot#watch} tracks the bot's own id and
 *   skips its messages, so a handler cannot forget;
 * - **Reconcile after a disconnect.** The server relays events best-effort and does not
 *   replay them, so {@link Bot#watch} polls as well as subscribes.
 */

import { InvalidArgumentError, UnauthorizedError, BotError } from './errors.js';
import { SidecarTransport } from './sidecar.js';

/** The reaction keys the server accepts, in its own order (`bots.md` 7.5). */
export const REACTIONS = ['like', 'dislike', 'love', 'haha', 'wow', 'sad', 'angry'];

/** The visibility values the server accepts (`bots.md` 7.4). */
export const VISIBILITIES = ['public', 'login', 'friends', 'private'];

/** The report target kinds the server accepts. */
export const REPORT_TARGETS = ['post', 'message', 'user'];

/** One realtime event, with accessors for the envelope's three fields. */
export class Event {
  /** @param {object} raw */
  constructor(raw) {
    this.raw = raw;
  }

  /** The event type, e.g. `chat.message.created`. */
  get type() {
    return this.raw.event ?? '';
  }

  /** The event payload. */
  get data() {
    return this.raw.data ?? {};
  }

  /** The addressed user ids; empty means the event was broadcast. */
  get recipients() {
    return this.raw.recipients ?? [];
  }

  /** Whether the event was sent to everyone. */
  isBroadcast() {
    return this.recipients.length === 0;
  }

  toJSON() {
    return this.raw;
  }
}

/** An OpenField bot client. */
export class Bot {
  /**
   * @param {string} [baseUrl] The instance root, e.g. `https://openfield.example.com`.
   * @param {string} [token] The bot token: `ofb_` plus 64 lowercase hex characters.
   * @param {object} [options]
   * @param {SidecarTransport} [options.transport] An already-built transport, for tests or
   *   a shared connection. When given, the other options are ignored.
   * @param {string} [options.sidecarPath] An explicit sidecar binary, overriding discovery.
   * @param {number} [options.timeoutMs] Per-request timeout.
   * @param {string} [options.userAgent] The `User-Agent` header; identify your bot, as
   *   `bots.md` 15.2 asks.
   * @param {object} [options.headers] Extra headers sent on every request.
   * @param {object} [options.retry] Retry overrides, e.g. `{ max_attempts: 5 }`. Applied by
   *   the Rust core: GET and HEAD retry on transport failures and 5xx, every verb retries on
   *   429 honouring `Retry-After`, and a mutating verb is never replayed on 5xx.
   * @param {object} [options.reconnect] Event-stream reconnect overrides.
   */
  constructor(baseUrl, token, options = {}) {
    this.me_ = null;
    this.ownId_ = null;

    if (options.transport) {
      // Sharing a transport lets several `Bot` views drive one child process -- which is
      // what the test suite does with the process that owns the mock server.
      this.transport = options.transport;
      return;
    }

    const clientOptions = {};
    if (baseUrl) clientOptions.base_url = baseUrl;
    if (token) clientOptions.token = token;
    if (options.timeoutMs !== undefined) clientOptions.timeout_ms = options.timeoutMs;
    if (options.connectTimeoutMs !== undefined) {
      clientOptions.connect_timeout_ms = options.connectTimeoutMs;
    }
    if (options.userAgent !== undefined) clientOptions.user_agent = options.userAgent;
    if (options.headers !== undefined) clientOptions.headers = options.headers;
    if (options.maxResponseBytes !== undefined) {
      clientOptions.max_response_bytes = options.maxResponseBytes;
    }
    if (options.retry !== undefined) clientOptions.retry = options.retry;
    if (options.reconnect !== undefined) clientOptions.reconnect = options.reconnect;

    this.transport = new SidecarTransport({
      binary: options.sidecarPath,
      options: clientOptions,
    });
  }

  /** Await the handshake, so a failure surfaces at an awaited point. */
  async ready() {
    await this.transport.whenReady();
    return this;
  }

  /** The active transport. */
  get backend() {
    return this.transport.name;
  }

  /**
   * Run any method from the shared index.
   *
   * This is the escape hatch for anything without a wrapper:
   *
   *     await bot.call('posts.create', { content: 'hello', visibility: 'public' });
   *
   * @param {string} method
   * @param {object} [params]
   * @returns {Promise<any>}
   */
  async call(method, params = {}) {
    return this.transport.call(method, params);
  }

  /** The SDK's protocol revision, feature flags and every method name. */
  capabilities() {
    return this.call('capabilities');
  }

  /** The SDK version and protocol revision. */
  version() {
    return this.call('version');
  }

  /** Release the transport. */
  close() {
    this.transport.close();
  }

  // -- identity ----------------------------------------------------------

  /** This bot's own user record, cached after the first call. */
  async me({ refresh = false } = {}) {
    if (this.me_ === null || refresh) {
      this.me_ = await this.call('me');
      this.ownId_ = this.me_.id ?? null;
    }
    return this.me_;
  }

  /** This bot's user id, fetching it on first use. */
  async id() {
    if (this.ownId_ === null) await this.me();
    return this.ownId_;
  }

  /** This bot's username. */
  async username() {
    return (await this.me()).username;
  }

  /** The permission keys this bot holds, plus the groups they came from. */
  permissions() {
    return this.call('permissions');
  }

  /**
   * Validate the token and report every administrative permission that is missing.
   *
   * Run this once at startup: a bot that starts without checking discovers a missing
   * permission as a 403 in the middle of a job (`bots.md` 16.1).
   */
  startupCheck() {
    return this.call('startup_check');
  }

  /**
   * Fail early when a permission is missing.
   *
   * @param {string} permission A permission key, e.g. `posts.create`.
   * @throws {UnauthorizedError} naming the missing key.
   */
  async requirePermission(permission) {
    const held = await this.permissions();
    if (!(held.permissions ?? []).includes(permission)) {
      throw new UnauthorizedError(
        `this bot lacks the ${permission} permission; ask an administrator to grant it ` +
          `to the bot's role`,
        { permission, status: 403 },
      );
    }
  }

  /**
   * Update the bot's profile. Accepts `username`, `nickname`, `bio`.
   *
   * The nickname limit is measured in Unicode code points, not bytes (`bots.md` 6.4).
   */
  updateProfile(profile) {
    return this.call('update_profile', { profile });
  }

  user(userId) {
    return this.call('user', { user_id: userId });
  }

  searchUsers(query) {
    return this.call('search_users', { query });
  }

  follow(userId) {
    return this.call('follow', { user_id: userId });
  }

  unfollow(userId) {
    return this.call('unfollow', { user_id: userId });
  }

  storageUsage() {
    return this.call('storage_usage');
  }

  storageBuckets() {
    return this.call('storage_buckets');
  }

  setStorageBucket(bucket) {
    return this.call('set_storage_bucket', { bucket });
  }

  // -- bots --------------------------------------------------------------

  /**
   * Create a new bot under this account.
   *
   * The returned token is shown **once**; store it before doing anything else. A single
   * owner may hold at most 10 bots (`bots.md` 5.3).
   */
  createBot(username, nickname) {
    return this.call('bots.create', { username, nickname });
  }

  listBots() {
    return this.call('bots.list');
  }

  /**
   * Rotate a bot's token.
   *
   * @param {number} botId
   * @param {boolean} [adopt] Switch this client to the new token immediately. Use it when
   *   the rotated bot is the one this client runs as, because the old token dies at once
   *   (`bots.md` 5.4).
   */
  regenerateToken(botId, adopt = false) {
    return this.call('bots.regenerate', { bot_id: botId, adopt });
  }

  deleteBot(botId) {
    return this.call('bots.delete', { bot_id: botId });
  }

  /** Look a bot up by username, or `null`. */
  async findBot(username) {
    const { bots } = await this.listBots();
    return (bots ?? []).find((bot) => bot.username === username) ?? null;
  }

  // -- posts -------------------------------------------------------------

  /**
   * Publish a post.
   *
   * @param {string} content The body: at most 5000 **bytes**, which for CJK text is well
   *   under 5000 characters.
   * @param {object} [options] `visibility`, `attachments`, `camp_id`, `quoted_post_id`.
   */
  publish(content, options = {}) {
    return this.call('posts.create', { content, ...options });
  }

  posts({ page, limit } = {}) {
    return this.call('posts.list', compact({ page, limit }));
  }

  /** Search posts by keyword, camp, author, and so on. */
  searchPosts(filters = {}) {
    return this.call('posts.search', { query: compact(filters) });
  }

  post(postId) {
    return this.call('posts.get', { post_id: postId });
  }

  updatePost(postId, post) {
    return this.call('posts.update', { post_id: postId, post });
  }

  deletePost(postId) {
    return this.call('posts.delete', { post_id: postId });
  }

  pinPost(postId, pinned = true) {
    return this.call('posts.pin', { post_id: postId, pinned });
  }

  /** React to a post. The server enforces one reaction per user per post. */
  react(postId, reaction) {
    if (!REACTIONS.includes(reaction)) {
      throw new InvalidArgumentError(
        `unknown reaction ${JSON.stringify(reaction)}; expected one of ` +
          REACTIONS.join(', '),
      );
    }
    return this.call('posts.react', { post_id: postId, reaction });
  }

  unreact(postId) {
    return this.call('posts.unreact', { post_id: postId });
  }

  favorite(postId, favorite = true) {
    const method = favorite ? 'posts.favorite' : 'posts.unfavorite';
    return this.call(method, { post_id: postId });
  }

  reply(postId, content, options = {}) {
    return this.call('posts.reply', { post_id: postId, content, ...options });
  }

  replies(postId, { page, limit } = {}) {
    return this.call('posts.replies', { post_id: postId, ...compact({ page, limit }) });
  }

  // -- chat --------------------------------------------------------------

  conversations() {
    return this.call('chat.conversations');
  }

  /** Create a group conversation. The server binds `title` and rejects an empty one. */
  createConversation(title) {
    return this.call('chat.create', { title });
  }

  conversation(conversationId) {
    return this.call('chat.get', { conversation_id: conversationId });
  }

  updateConversation(conversationId, { isPublic, allowJoin, encrypted } = {}) {
    const settings = compact({
      is_public: isPublic,
      allow_join: allowJoin,
      encrypted,
    });
    return this.call('chat.settings', { conversation_id: conversationId, settings });
  }

  /**
   * A page of history, **newest first**.
   *
   * Note: the bundled `bots.md` 7.3 claims this is oldest-first. It is not: the handler
   * sorts by `created_at DESC, id DESC`. Sort explicitly if you need chronological order.
   */
  messages(conversationId, { limit, before } = {}) {
    return this.call('chat.messages', {
      conversation_id: conversationId,
      ...compact({ limit, before }),
    });
  }

  latestMessages(conversationId, { limit } = {}) {
    return this.call('chat.latest', { conversation_id: conversationId, ...compact({ limit }) });
  }

  /**
   * Send a chat message.
   *
   * @param {number} conversationId
   * @param {string} content The body, at most 5000 **bytes**.
   * @param {object} [options]
   * @param {number} [options.replyToId] The message this one answers.
   * @param {number[]} [options.mentions] User ids to notify. `-1` pings everyone.
   * @param {number} [options.burnSeconds] Delete after this long: `0` (never) or `5..=86400`
   *   (`bots.md` 9.4).
   * @param {number[]} [options.attachments] Attachment ids to attach.
   */
  sendMessage(conversationId, content, options = {}) {
    const { replyToId, mentions, burnSeconds, attachments } = options;
    return this.call(
      'chat.send',
      compact({
        conversation_id: conversationId,
        content,
        reply_to_id: replyToId,
        mentions,
        burn_seconds: burnSeconds,
        attachments,
      }),
    );
  }

  editMessage(conversationId, messageId, content) {
    return this.call('chat.edit', {
      conversation_id: conversationId,
      message_id: messageId,
      content,
    });
  }

  deleteMessage(conversationId, messageId) {
    return this.call('chat.delete', { conversation_id: conversationId, message_id: messageId });
  }

  /**
   * Advance this bot's read cursor.
   *
   * @param {number} lastMessageId The newest message seen; `0` marks the whole
   *   conversation read. The server binds this field name (`bots.md` 8.7).
   */
  markRead(conversationId, lastMessageId = 0) {
    return this.call('chat.read', {
      conversation_id: conversationId,
      last_message_id: lastMessageId,
    });
  }

  typing(conversationId) {
    return this.call('chat.typing', { conversation_id: conversationId });
  }

  joinConversation(conversationId) {
    return this.call('chat.join', { conversation_id: conversationId });
  }

  /**
   * Invite a user.
   *
   * The server files a **consent request** rather than adding them directly, which the
   * invitee must accept (`bots.md` 8.5); watch for `chat.consent.requested`.
   */
  invite(conversationId, userId, message) {
    return this.call('chat.invite', compact({ conversation_id: conversationId, user_id: userId, message }));
  }

  leaveConversation(conversationId) {
    return this.call('chat.leave', { conversation_id: conversationId });
  }

  removeMember(conversationId, userId) {
    return this.call('chat.remove_member', {
      conversation_id: conversationId,
      user_id: userId,
    });
  }

  /** Open (or reuse) a direct conversation with a user. */
  startConversation(userId, { message, encrypted = false } = {}) {
    return this.call('chat.start', compact({ user_id: userId, message, encrypted }));
  }

  consentRequests() {
    return this.call('chat.consent_requests');
  }

  acceptConsent(requestId) {
    return this.call('chat.accept_consent', { request_id: requestId });
  }

  declineConsent(requestId) {
    return this.call('chat.decline_consent', { request_id: requestId });
  }

  conversationFiles(conversationId) {
    return this.call('chat.files', { conversation_id: conversationId });
  }

  // -- camps -------------------------------------------------------------

  /** List camps, with optional `q`/`page`/`limit` filters. */
  camps(filters = {}) {
    return this.call('camps.list', { query: compact(filters) });
  }

  camp(campId) {
    return this.call('camps.get', { camp_id: campId });
  }

  /**
   * Create a camp.
   *
   * @param {string} name Required, and unique across the instance.
   * @param {object} [options] `description`, `is_visible`, `direct_join`, `member_post`,
   *   `member_pin`, `parent_camp_id`.
   */
  createCamp(name, options = {}) {
    return this.call('camps.create', { camp: { name, ...options } });
  }

  updateCamp(campId, changes) {
    return this.call('camps.update', { camp_id: campId, update: changes });
  }

  deleteCamp(campId) {
    return this.call('camps.delete', { camp_id: campId });
  }

  campPosts(campId, { page, limit } = {}) {
    return this.call('camps.posts', { camp_id: campId, ...compact({ page, limit }) });
  }

  joinCamp(campId) {
    return this.call('camps.join', { camp_id: campId });
  }

  leaveCamp(campId) {
    return this.call('camps.leave', { camp_id: campId });
  }

  campMembers(campId) {
    return this.call('camps.members', { camp_id: campId });
  }

  campSubgroups(campId) {
    return this.call('camps.subgroups', { camp_id: campId });
  }

  postToCamp(campId, content, options = {}) {
    return this.call('camps.post', { camp_id: campId, content, ...options });
  }

  // -- storage -----------------------------------------------------------

  /**
   * Upload bytes and return the attachment record.
   *
   * The bytes cross as base64, so a large file does not become a million-element JSON
   * array.
   *
   * @param {string} fileName The name the server records.
   * @param {Buffer|Uint8Array} data
   * @param {string} [mime] Overrides the type guessed from the extension.
   */
  upload(fileName, data, mime) {
    const upload = {
      file_name: fileName,
      bytes: Buffer.from(data).toString('base64'),
      ...compact({ mime }),
    };
    return this.call('storage.upload', { upload });
  }

  uploads(files) {
    return this.call('storage.upload_many', { uploads: files });
  }

  attachments() {
    return this.call('storage.list');
  }

  deleteAttachment(attachmentId) {
    return this.call('storage.delete', { attachment_id: attachmentId });
  }

  // -- moderation, notifications, instance -------------------------------

  /**
   * Report a post, message or user.
   *
   * @param {string} targetType One of {@link REPORT_TARGETS}; the server binds this field.
   * @param {number} targetId
   * @param {string} reason At most 500 characters.
   */
  report(targetType, targetId, reason) {
    if (!REPORT_TARGETS.includes(targetType)) {
      throw new InvalidArgumentError(
        `unknown report target ${JSON.stringify(targetType)}; expected one of ` +
          REPORT_TARGETS.join(', '),
      );
    }
    return this.call('reports.create', {
      report: { target_type: targetType, target_id: targetId, reason },
    });
  }

  reportPost(postId, reason) {
    return this.report('post', postId, reason);
  }

  reportMessage(messageId, reason) {
    return this.report('message', messageId, reason);
  }

  reportUser(userId, reason) {
    return this.report('user', userId, reason);
  }

  myReports() {
    return this.call('reports.mine');
  }

  notifications() {
    return this.call('notifications.list');
  }

  readNotifications() {
    return this.call('notifications.read');
  }

  /** The instance health report. Requires no token. */
  health() {
    return this.call('health');
  }

  announcements({ all = false } = {}) {
    return this.call('announcements', { all });
  }

  // -- raw ---------------------------------------------------------------

  /**
   * Perform a raw request and return `{ ok, status, body }`.
   *
   * Does **not** throw on a non-2xx status: the envelope carries it, so a caller can
   * deliberately handle a 409 or read a 404 body.
   */
  request(method, path, { query, body, bytes } = {}) {
    return this.call('request', compact({ method, path, query, body, bytes }));
  }

  // -- events ------------------------------------------------------------

  /**
   * Subscribe to the realtime channel.
   *
   * The subscription is established before this resolves, so the server already counts the
   * connection.
   *
   * @param {object} [params] `types`, `conversation_ids`, `queue_capacity`, `reconnect`.
   * @returns {Promise<AsyncIterable<Event>>}
   */
  async events(params = {}) {
    const subscription = await this.transport.events(params);
    return wrap(subscription);
  }

  /**
   * Follow one conversation, merging the event stream with polling.
   *
   * Implements the reliable half of the `bots.md` 9.2/9.3 recipe:
   *
   * - events arrive immediately while the connection is up;
   * - a poll of `chat.latest` runs every `pollIntervalMs`, so a message sent while the bot
   *   was disconnected is still seen;
   * - the bot's own messages are skipped, because the event channel echoes what the bot
   *   sends and a bot that answers itself loops forever;
   * - ids are de-duplicated, so the same message delivered by both paths yields once.
   *
   * Everything that is not a `chat.message.created` for this conversation is dropped,
   * including the `connected` greeting, so a finite run cannot spend its `maxEvents` budget
   * on a connection notice. Use {@link Bot#events} for the unfiltered stream.
   *
   * @param {number} conversationId
   * @param {object} [options]
   * @param {number} [options.pollIntervalMs] Milliseconds between reconciliation polls.
   * @param {number} [options.since] The last message id already handled; anything at or
   *   below it is skipped. Defaults to the newest message, so a restart does not replay
   *   history.
   * @param {number} [options.maxEvents] Stop after this many messages.
   * @param {number} [options.settleMs] How long to sleep between checks.
   * @returns {AsyncGenerator<Event>}
   */
  async *watch(conversationId, options = {}) {
    const {
      pollIntervalMs = 5_000,
      maxEvents,
      settleMs = 200,
      types = ['chat.message.created'],
    } = options;

    let since = options.since;
    if (since === undefined) {
      const page = await this.latestMessages(conversationId, { limit: 1 });
      since = newestMessageId(page) ?? 0;
    }

    const ownId = await this.id();
    const seen = new Set();
    let highest = since;
    let emitted = 0;
    let nextPoll = 0;

    const subscription = await this.transport.events({
      types,
      conversation_ids: [conversationId],
    });

    // A background consumer drains the subscription so the loop below can interleave
    // events with polls without blocking on either.
    const inbox = [];
    const waiters = [];
    let finished = false;
    let failure = null;
    const push = (value) => {
      const waiter = waiters.shift();
      if (waiter) waiter(value);
      else inbox.push(value);
    };
    const pump = (async () => {
      try {
        for await (const event of subscription) {
          push({ event: new Event(event) });
        }
      } catch (error) {
        failure = error;
      } finally {
        finished = true;
        push({ done: true });
      }
    })();

    try {
      for (;;) {
        // 1. Everything the subscription already delivered.
        for (;;) {
          let item;
          if (inbox.length > 0) {
            item = inbox.shift();
          } else if (failure) {
            throw failure;
          } else if (finished) {
            throw new BotError('the event subscription closed');
          } else {
            break;
          }

          if (item.done) throw new BotError('the event subscription closed');
          const event = item.event;
          const accepted = acceptEvent(event, conversationId, highest, seen, ownId);
          if (!accepted) continue;
          highest = Math.max(highest, accepted.data.id);
          emitted += 1;
          yield accepted;
          if (maxEvents !== undefined && emitted >= maxEvents) return;
        }

        // 2. Reconcile: catch anything sent while the subscription was down.
        if (Date.now() >= nextPoll) {
          nextPoll = Date.now() + pollIntervalMs;
          for (const event of await this.pollNewMessages(conversationId, highest, ownId)) {
            if (seen.has(event.data.id)) continue;
            highest = Math.max(highest, event.data.id);
            seen.add(event.data.id);
            emitted += 1;
            yield event;
            if (maxEvents !== undefined && emitted >= maxEvents) return;
          }
        }

        // Wait for an event or the poll deadline, whichever comes first.
        await new Promise((resolveWait) => {
          const waiter = (value) => {
            clearTimeout(timer);
            resolveWait(value);
          };
          const timer = setTimeout(() => {
            const index = waiters.indexOf(waiter);
            if (index >= 0) waiters.splice(index, 1);
            resolveWait(undefined);
          }, settleMs);
          timer.unref?.();
          waiters.push(waiter);
        });
      }
    } finally {
      subscription.close();
      await pump.catch(() => {});
    }
  }

  /**
   * Messages newer than `since` that this bot did not send, oldest first.
   *
   * @param {number} conversationId
   * @param {number} since
   * @param {number|null} ownId
   * @returns {Promise<Event[]>}
   */
  async pollNewMessages(conversationId, since, ownId) {
    const page = await this.latestMessages(conversationId, { limit: 50 });
    const fresh = [];
    for (const message of pageItems(page)) {
      const id = messageId(message);
      if (id === null || id <= since) continue;
      if (isOwn(message, ownId)) continue;
      fresh.push(new Event({ event: 'chat.message.created', data: message, recipients: [] }));
    }
    fresh.sort((a, b) => (a.data.id ?? 0) - (b.data.id ?? 0));
    return fresh;
  }
}

/** Wrap the raw subscription so callers iterate `Event` objects. */
function wrap(subscription) {
  return {
    [Symbol.asyncIterator]() {
      return this;
    },
    async next() {
      const result = await subscription.next();
      if (result.done) return { value: undefined, done: true };
      return { value: new Event(result.value), done: false };
    },
    close() {
      subscription.close();
    },
    stats() {
      return subscription.stats();
    },
    async return() {
      subscription.close();
      return { value: undefined, done: true };
    },
  };
}

/**
 * Keep only new, actionable `chat.message.created` events for the conversation.
 *
 * @returns {Event|null}
 */
function acceptEvent(event, conversationId, since, seen, ownId) {
  if (event.type !== 'chat.message.created') return null;
  const data = event.data;
  if (data.conversation_id !== undefined && data.conversation_id !== conversationId) return null;
  const id = messageId(data);
  if (id === null || id <= since || seen.has(id) || isOwn(data, ownId)) return null;
  seen.add(id);
  return event;
}

/** Whether a message is this bot's own, by id rather than by trusting `sender_is_bot`. */
function isOwn(message, ownId) {
  const sender = message.sender_id ?? message.user_id;
  return ownId !== null && ownId !== undefined && sender === ownId;
}

/** The id of a message, tolerating an id the server marshalled as a string. */
export function messageId(message) {
  const raw = message?.id;
  if (typeof raw === 'number') return raw;
  const parsed = Number.parseInt(raw, 10);
  return Number.isNaN(parsed) ? null : parsed;
}

/** Pull the message list out of whichever envelope the endpoint returned. */
export function pageItems(page) {
  if (Array.isArray(page)) return page.filter((item) => item && typeof item === 'object');
  if (page && typeof page === 'object') {
    for (const key of ['messages', 'items', 'data', 'list']) {
      if (Array.isArray(page[key])) {
        return page[key].filter((item) => item && typeof item === 'object');
      }
    }
  }
  return [];
}

/**
 * The largest id in a page.
 *
 * The handler sorts newest-first, so this is normally the head -- but the SDK must not
 * depend on an ordering the server is free to change, so it takes the maximum.
 */
export function newestMessageId(page) {
  const ids = pageItems(page)
    .map(messageId)
    .filter((id) => id !== null);
  return ids.length > 0 ? Math.max(...ids) : null;
}

/** Drop keys whose value is `undefined`, so absent options are not sent as null. */
function compact(object) {
  const result = {};
  for (const [key, value] of Object.entries(object)) {
    if (value !== undefined && value !== null) result[key] = value;
  }
  return result;
}
