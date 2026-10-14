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

import { readFileSync, writeFileSync } from 'node:fs';
import { basename } from 'node:path';

import { InvalidArgumentError, UnauthorizedError, BotError } from './errors.js';
import { SidecarTransport } from './sidecar.js';

/** The reaction keys the server accepts, in its own order (`bots.md` 7.5). */
export const REACTIONS = ['like', 'dislike', 'love', 'haha', 'wow', 'sad', 'angry'];

/** The visibility values the server accepts (`bots.md` 7.4). */
export const VISIBILITIES = ['public', 'login', 'friends', 'private'];

/** The report target kinds the server accepts. */
export const REPORT_TARGETS = ['post', 'message', 'user'];

/** The punishment types the server accepts (`model.PunishmentType`). */
export const PUNISHMENT_TYPES = [
  'warning',
  'demerit',
  'revoke',
  'temp_ban',
  'ban',
  'unban',
  'restore',
];

/** Largest plugin bundle the service accepts (5 MiB). */
export const MAX_BUNDLE_BYTES = 5 * 1024 * 1024;

/** Longest burn-after-view countdown the handler's binding allows. */
export const MAX_BURN_SECONDS = 86400;

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
  /** Largest single chunk the server accepts (8 MiB). */
  static CHUNK_BYTES = 8 * 1024 * 1024;

  /** Largest number of chunks per session. */
  static MAX_CHUNKS = 10000;

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

  /** Fetch one attachment record by id. */
  attachment(attachmentId) {
    return this.call('storage.attachment', { attachment_id: attachmentId });
  }

  /**
   * Look an attachment up by the lowercase hex SHA-256 of its bytes, so an upload the cloud
   * already holds does not have to be repeated.
   *
   * Deduplication is **per user**, so this only finds blobs this bot uploaded.
   */
  attachmentByHash(hexDigest) {
    return this.call('storage.by_hash', { hash: requireSha256(hexDigest) });
  }

  /**
   * Arm a burn-after-view countdown of at most 24 hours.
   *
   * The deadline starts on the **first view by someone else**; the uploader can never
   * trigger it, and for the uploader the server answers 404 exactly as for an unknown id.
   */
  armAttachmentBurn(attachmentId, burnSeconds) {
    const seconds = requireInt('burn_seconds', burnSeconds);
    if (seconds < 1 || seconds > 86400) {
      throw new InvalidArgumentError(`burn_seconds must be 1..86400, got ${seconds}`);
    }
    return this.call('storage.burn_arm', {
      attachment_id: attachmentId,
      burn_seconds: seconds,
    });
  }

  // -- chunked uploads ----------------------------------------------------

  /**
   * Open a resumable upload session.
   *
   * @param {string} filename
   * @param {number} size Total byte length.
   * @param {number} totalChunks At most {@link Bot.MAX_CHUNKS}.
   * @returns {Promise<{upload_id: string, total_chunks: number}>}
   */
  startChunkedUpload(filename, size, totalChunks, { mimeType, visibility } = {}) {
    const upload = chunkLayout(filename, size, totalChunks, mimeType, visibility);
    return this.call('storage.chunk_init', { upload });
  }

  /** Which 1-based chunk indexes already landed, for a resume. */
  chunkStatus(uploadId) {
    return this.call('storage.chunk_status', { upload_id: uploadId });
  }

  /**
   * Store one chunk. Indexes are 1-based and the multipart field is `chunk`.
   *
   * @param {string} uploadId
   * @param {number} index
   * @param {Buffer|Uint8Array} data At most {@link Bot.CHUNK_BYTES}.
   */
  uploadChunk(uploadId, index, data) {
    const number = requireInt('index', index);
    if (number < 1) {
      throw new InvalidArgumentError(`chunk index is 1-based, got ${number}`);
    }
    if (data.length > Bot.CHUNK_BYTES) {
      throw new InvalidArgumentError(
        `chunk is ${data.length} bytes, above the ${Bot.CHUNK_BYTES} byte limit`,
      );
    }
    const part = {
      upload_id: uploadId,
      index: number,
      bytes: Buffer.from(data).toString('base64'),
    };
    return this.call('storage.chunk_upload', { part });
  }

  /** Assemble the stored chunks into an attachment. */
  completeChunkedUpload(uploadId, filename, size, totalChunks, { mimeType, visibility } = {}) {
    const upload = chunkLayout(filename, size, totalChunks, mimeType, visibility);
    return this.call('storage.chunk_complete', { upload_id: uploadId, upload });
  }

  /**
   * Upload a large payload through the chunked path, resuming if asked.
   *
   * @param {string} fileName
   * @param {Buffer|Uint8Array} data The whole payload.
   * @param {object} [options]
   * @param {number} [options.chunkBytes] Part size; defaults to {@link Bot.CHUNK_BYTES}.
   * @param {string} [options.uploadId] Continue an open session, re-sending only what the
   *   server is missing.
   */
  async uploadChunked(fileName, data, { chunkBytes, uploadId, mimeType, visibility } = {}) {
    const buffer = Buffer.from(data);
    if (buffer.length === 0) throw new InvalidArgumentError('nothing to upload');
    const size = Math.min(Math.max(1, chunkBytes ?? Bot.CHUNK_BYTES), Bot.CHUNK_BYTES);
    const parts = [];
    for (let offset = 0; offset < buffer.length; offset += size) {
      parts.push(buffer.subarray(offset, offset + size));
    }
    const upload = chunkLayout(
      fileName,
      buffer.length,
      parts.length,
      mimeType,
      visibility,
    );
    let sessionId = uploadId;
    if (sessionId === undefined) {
      const session = await this.call('storage.chunk_init', { upload });
      sessionId = session.upload_id;
    }
    // Resume: only the parts the server does not already hold are sent.
    const status = await this.call('storage.chunk_status', { upload_id: sessionId });
    const stored = new Set(status.uploaded ?? []);
    for (const [offset, part] of parts.entries()) {
      const index = offset + 1;
      if (!stored.has(index)) await this.uploadChunk(sessionId, index, part);
    }
    return this.call('storage.chunk_complete', { upload_id: sessionId, upload });
  }

  // -- wallet -------------------------------------------------------------

  /**
   * This bot's balance and a page of its transactions.
   *
   * Amounts are **decimal coins**, the shape the server's `model.Cents` puts on the wire:
   * `10` is ten coins, `12.5` is twelve and a half. Keep cents in your own arithmetic if
   * you need exactness.
   */
  wallet({ page, limit } = {}) {
    return this.call('wallet.get', compact({ page, limit }));
  }

  /**
   * Credit `amount` coins to a user. Needs `wallet.manage`, which the default group holds —
   * so any authenticated bot can currently move any balance.
   */
  rechargeWallet(userId, amount, { description } = {}) {
    return this.call(
      'wallet.recharge',
      compact({
        user_id: requireInt('user_id', userId),
        amount: requireAmount('amount', amount),
        description,
      }),
    );
  }

  /**
   * Debit `amount` coins from a user. Needs `wallet.manage`.
   *
   * The sign is handled here: the server stores the amount verbatim and never checks it
   * against the transaction type, so a positive value would silently become a credit.
   */
  deductWallet(userId, amount, { description } = {}) {
    return this.call(
      'wallet.deduct',
      compact({
        user_id: requireInt('user_id', userId),
        amount: requireAmount('amount', amount),
        description,
      }),
    );
  }

  /**
   * Apply a signed balance change, exactly as the wire takes it.
   *
   * @param {number} userId
   * @param {number} amount **Signed** decimal coins: positive credits, negative debits.
   * @param {object} [options]
   * @param {string} [options.type] `recharge` or `deduct`. It is recorded as a label and
   *   is **not** reconciled with the sign, so a positive amount with `type: 'deduct'` is
   *   a credit.
   * @param {string} [options.description]
   */
  adjustWallet(userId, amount, { type, description } = {}) {
    return this.call(
      'wallet.adjust',
      compact({
        user_id: requireInt('user_id', userId),
        amount: requireAmount('amount', amount),
        type,
        description,
      }),
    );
  }

  // -- tasks, sign-in and experience -------------------------------------

  /**
   * The task catalog with this bot's progress, streak and make-up price.
   *
   * Currency figures here are **whole coins**, unlike the wallet's decimal coins.
   */
  tasks() {
    return this.call('tasks.list');
  }

  /** Claim a one-time or streak task. */
  claimTask(code) {
    return this.call('tasks.claim', { code });
  }

  /** Claim one of today's activity tiers; each is once per local day. */
  claimDailyTask(code) {
    return this.call('tasks.claim_daily', { code });
  }

  /**
   * Sign in for today.
   *
   * A second call is not an error: it answers `{granted: false}`.
   */
  claimDailyLogin() {
    return this.call('tasks.login_claim');
  }

  /** The sign-in grid: `registered_at`, `today`, `signed`, `days`, and so on. */
  checkinCalendar() {
    return this.call('tasks.login_calendar');
  }

  /**
   * Pay to make up the most recent missed sign-in.
   *
   * A make-up renews the streak and grants experience but never the daily currency.
   */
  makeupCheckin() {
    return this.call('tasks.makeup');
  }

  /**
   * Pay to sign in one specific missed day.
   *
   * @param {string} date `YYYY-MM-DD`, in the game's configured timezone.
   */
  makeupCheckinOn(date) {
    if (typeof date !== 'string' || !/^\d{4}-\d{2}-\d{2}$/.test(date)) {
      throw new InvalidArgumentError(`date must be \`YYYY-MM-DD\`, got ${String(date)}`);
    }
    return this.call('tasks.makeup_date', { date });
  }

  /** Newest-first experience awards. */
  expHistory({ limit } = {}) {
    return this.call('tasks.exp_history', compact({ limit }));
  }

  // -- membership and the payment PIN ------------------------------------

  /** This bot's membership state plus the purchasable catalog. */
  membership() {
    return this.call('membership.get');
  }

  /**
   * Buy a membership tier with wallet coins, authorized by the payment PIN.
   *
   * A fresh bot has no PIN, so this answers 403 until {@link Bot#setPaymentPin} is called.
   */
  purchaseMembership(level, pin) {
    const tier = requireInt('level', level);
    if (tier < 1 || tier > 4) {
      throw new InvalidArgumentError(`membership level must be 1..4, got ${tier}`);
    }
    if (!pin) throw new InvalidArgumentError('pin must not be empty');
    return this.call('membership.purchase', { level: tier, pin });
  }

  /**
   * Toggle automatic renewal.
   *
   * Enabling pre-authorizes future charges and needs `pin`; disabling needs only the token.
   */
  setAutoRenew(enabled, { pin } = {}) {
    const options = { enabled: Boolean(enabled) };
    if (pin !== undefined) options.pin = pin;
    else if (enabled) {
      throw new InvalidArgumentError('enabling auto-renew needs the payment pin');
    }
    return this.call('membership.auto_renew', options);
  }

  /** This bot's membership purchase history. */
  membershipPurchases({ page, limit } = {}) {
    return this.call('membership.purchases', compact({ page, limit }));
  }

  /**
   * Grant a membership directly, skipping the wallet. Needs `user.membership.grant`.
   *
   * `level: 0` clears the membership.
   */
  grantMembership(userId, level, { days } = {}) {
    return this.call(
      'membership.grant',
      compact({
        user_id: requireInt('user_id', userId),
        level: requireInt('level', level),
        days: days === undefined ? undefined : requireInt('days', days),
      }),
    );
  }

  /**
   * Adjust a user's experience. Needs `user.adjust_exp`.
   *
   * `delta` is signed and the field is named `delta`, not `amount`. A positive delta is
   * multiplied by the target's membership multiplier before it is applied.
   */
  adjustExp(userId, delta, { description } = {}) {
    return this.call(
      'membership.exp_adjust',
      compact({
        user_id: requireInt('user_id', userId),
        delta: requireInt('delta', delta),
        description,
      }),
    );
  }

  /**
   * Set the six-digit payment PIN.
   *
   * Every outgoing payment — tips, memberships and transfers — is authorized by this PIN,
   * and a freshly created bot has none, so this is what a spending bot must call first.
   * Replacing an existing PIN requires `oldPin`.
   */
  setPaymentPin(pin, { oldPin } = {}) {
    requirePin(pin);
    return this.call('account.pin_set', compact({ pin, old_pin: oldPin }));
  }

  /** Replace the payment PIN, presenting the current one. */
  changePaymentPin(oldPin, pin) {
    requirePin(pin);
    if (!oldPin) throw new InvalidArgumentError('old_pin must not be empty');
    return this.call('account.pin_change', { old_pin: oldPin, pin });
  }

  /**
   * Check a PIN without spending anything.
   *
   * A wrong PIN is **counted** against the same five-failure budget that tips and
   * memberships use, so probing this can lock payments out for fifteen minutes.
   *
   * @returns {Promise<boolean>}
   */
  async verifyPaymentPin(pin) {
    if (!pin) throw new InvalidArgumentError('pin must not be empty');
    const answer = await this.call('account.pin_verify', { pin });
    return Boolean(answer.valid);
  }

  // -- moderation ---------------------------------------------------------

  /**
   * Record a moderation action. Needs `user.punish`, which the default group lacks.
   *
   * @param {number} userId The sanctioned user. Not yourself: that is a 403.
   * @param {string} type One of {@link PUNISHMENT_TYPES}.
   * @param {object} [options]
   * @param {string} [options.reason]
   * @param {string} [options.permissionKey] Required for `revoke` and `restore`.
   * @param {number} [options.durationMinutes] Required for `temp_ban`.
   */
  punish(userId, type, { reason, permissionKey, durationMinutes } = {}) {
    if (!PUNISHMENT_TYPES.includes(type)) {
      throw new InvalidArgumentError(
        `unknown punishment type ${type}; expected one of ${PUNISHMENT_TYPES.join(', ')}`,
      );
    }
    if ((type === 'revoke' || type === 'restore') && !permissionKey) {
      throw new InvalidArgumentError(`${type} requires permission_key`);
    }
    if (type === 'temp_ban' && !durationMinutes) {
      throw new InvalidArgumentError('temp_ban requires a positive duration_minutes');
    }
    const punishment = compact({
      type,
      reason,
      permission_key: permissionKey,
      duration_minutes:
        durationMinutes === undefined ? undefined : requireInt('duration_minutes', durationMinutes),
    });
    return this.call('moderation.punish', { user_id: userId, punishment });
  }

  /**
   * A user's moderation history plus the permissions currently withheld.
   *
   * `permission_bans` is a list of permission **keys**, not records.
   */
  punishments(userId, { limit } = {}) {
    return this.call(
      'moderation.punishments',
      compact({ user_id: requireInt('user_id', userId), limit }),
    );
  }

  // -- end-to-end encryption ---------------------------------------------

  /** Publish this bot's E2EE public key. An empty string clears it. */
  setE2eePublicKey(publicKey) {
    return this.call('e2ee.set_key', { public_key: publicKey });
  }

  /** Every group-key envelope plus this bot's own, for one conversation. */
  e2eeKeys(conversationId) {
    return this.call('e2ee.keys', { conversation_id: conversationId });
  }

  /**
   * Store a rotated batch of group-key envelopes.
   *
   * @param {number} conversationId The group; the caller must be a member.
   * @param {object|Map<number, string>} envelopes `{targetUserId: ciphertext}`. The server
   *   relays the ciphertext untouched and refuses an empty batch. Go binds
   *   `map[int64]string`, so the ids are JSON object keys.
   */
  putE2eeKeys(conversationId, envelopes) {
    const pairs = envelopes instanceof Map ? [...envelopes.entries()] : Object.entries(envelopes);
    if (pairs.length === 0) {
      throw new InvalidArgumentError('at least one envelope is required');
    }
    const batch = {};
    for (const [userId, ciphertext] of pairs) batch[String(userId)] = ciphertext;
    return this.call('e2ee.put_keys', { conversation_id: conversationId, envelopes: batch });
  }

  // -- plugin store -------------------------------------------------------

  /** The published plugin catalog. Public: no token is required. */
  plugins() {
    return this.call('plugins.list');
  }

  /** One published plugin by id. Absent and unpublished both answer 404. */
  plugin(pluginId) {
    return this.call('plugins.get', { plugin_id: requirePluginId(pluginId) });
  }

  /**
   * Download a bundle. Requires a token, and increments the download counter.
   *
   * @returns {Promise<Buffer>}
   */
  async downloadPlugin(pluginId) {
    const answer = await this.call('plugins.download', {
      plugin_id: requirePluginId(pluginId),
    });
    return Buffer.from(answer.bytes, 'base64');
  }

  /** Download a bundle and write it to `path`. Returns the byte count. */
  async downloadPluginTo(pluginId, path) {
    const data = await this.downloadPlugin(pluginId);
    writeFileSync(path, data);
    return data.length;
  }

  /**
   * Upload a bundle. Needs `plugin.manage`.
   *
   * The service caps bundles at 5 MiB and requires a `manifest.json` at the zip root with a
   * valid id and semantic version.
   */
  uploadPlugin(fileName, data, { publish = false } = {}) {
    if (data.length > MAX_BUNDLE_BYTES) {
      throw new InvalidArgumentError(
        `bundle is ${data.length} bytes, above the ${MAX_BUNDLE_BYTES} byte service limit`,
      );
    }
    const upload = {
      file_name: fileName,
      bytes: Buffer.from(data).toString('base64'),
      mime: 'application/zip',
    };
    return this.call('plugins.upload', { upload, publish: Boolean(publish) });
  }

  /** Upload a bundle read from disk. */
  uploadPluginFile(path, options = {}) {
    return this.uploadPlugin(basename(path), readFileSync(path), options);
  }

  /** List a plugin on the store. */
  publishPlugin(pluginId) {
    return this.call('plugins.publish', { plugin_id: requirePluginId(pluginId) });
  }

  /** Hide a plugin from the store, keeping the row and the bundle. */
  unpublishPlugin(pluginId) {
    return this.call('plugins.unpublish', { plugin_id: requirePluginId(pluginId) });
  }

  /** Delete a plugin row and its bundle. */
  deletePlugin(pluginId) {
    return this.call('plugins.delete', { plugin_id: requirePluginId(pluginId) });
  }

  // -- tips ---------------------------------------------------------------

  /**
   * Tip a post with coins, authorized by the payment PIN.
   *
   * `amount` is a whole **coin** count (1..100000), not the wallet's decimal coins. The
   * response reports cents: a 5-coin tip records `amount: 500` and `net_amount: 475`, the
   * 5 % platform fee having been taken. A deleted post refunds the tipper.
   */
  tipPost(postId, amount, pin) {
    const coins = requireInt('amount', amount);
    if (coins < 1 || coins > 100000) {
      throw new InvalidArgumentError(`tip amount must be 1..100000 coins, got ${coins}`);
    }
    if (!pin) throw new InvalidArgumentError('pin must not be empty');
    return this.call('posts.tip', { post_id: postId, amount: coins, pin });
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

/**
 * Coerce an integer field, refusing the shapes Go's decoder would refuse.
 *
 * A non-integer number is rejected because Go will not decode `5.0` into an `int64`, and a
 * string is rejected because the caller should have passed a number.
 */
function requireInt(name, value) {
  if (typeof value !== 'number' || !Number.isInteger(value)) {
    throw new InvalidArgumentError(`${name} must be an integer, got ${String(value)}`);
  }
  return value;
}

/** Accept a monetary amount in decimal coins: any finite number. */
function requireAmount(name, value) {
  if (typeof value !== 'number' || !Number.isFinite(value)) {
    throw new InvalidArgumentError(`${name} must be a number of coins, got ${String(value)}`);
  }
  return value;
}

/** A payment PIN is exactly six digits (`security.ValidPin`). */
function requirePin(pin) {
  if (typeof pin !== 'string' || !/^\d{6}$/.test(pin)) {
    throw new InvalidArgumentError('the payment pin must be exactly 6 digits');
  }
  return pin;
}

/** Normalise a SHA-256 digest to the lowercase 64-character hex the server looks up. */
function requireSha256(hexDigest) {
  const digest = typeof hexDigest === 'string' ? hexDigest.trim().toLowerCase() : '';
  if (!/^[0-9a-f]{64}$/.test(digest)) {
    throw new InvalidArgumentError('hash must be 64 hexadecimal characters (SHA-256)');
  }
  return digest;
}

/**
 * Reject a plugin id the service would reject, and never let one act as a path.
 *
 * `.` and `..` satisfy the id grammar, so they are excluded by name.
 */
function requirePluginId(pluginId) {
  const candidate = typeof pluginId === 'string' ? pluginId.trim() : '';
  if (!/^[a-z0-9][a-z0-9._-]{2,63}$/.test(candidate) || candidate === '.' || candidate === '..') {
    throw new InvalidArgumentError(
      `plugin id ${JSON.stringify(pluginId)} must match ^[a-z0-9][a-z0-9._-]{2,63}$`,
    );
  }
  return candidate;
}

/** Build and check the `chunk/init` and `chunk/complete` body, which share one shape. */
function chunkLayout(filename, size, totalChunks, mimeType, visibility) {
  if (typeof filename !== 'string' || filename.trim() === '') {
    throw new InvalidArgumentError('filename is required');
  }
  const bytes = requireInt('size', size);
  const chunks = requireInt('total_chunks', totalChunks);
  if (bytes <= 0) throw new InvalidArgumentError(`size must be positive, got ${bytes}`);
  if (chunks <= 0) {
    throw new InvalidArgumentError(`total_chunks must be positive, got ${chunks}`);
  }
  if (chunks > Bot.MAX_CHUNKS) {
    throw new InvalidArgumentError(
      `total_chunks must be at most ${Bot.MAX_CHUNKS}, got ${chunks}`,
    );
  }
  if (bytes > chunks * Bot.CHUNK_BYTES) {
    throw new InvalidArgumentError(
      `declared size ${bytes} exceeds what ${chunks} chunks can carry`,
    );
  }
  return compact({
    filename,
    size: bytes,
    total_chunks: chunks,
    mime_type: mimeType,
    visibility,
  });
}
