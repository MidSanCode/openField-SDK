/**
 * OpenField community bot SDK for Node.
 *
 * The Rust core does the work; this package drives it through the **sidecar**, a child
 * process speaking newline-delimited JSON. Node is the one language of the three where the
 * C ABI would mean a native addon (`ffi-napi` and friends) — a build step and a Node-version
 * coupling — so the subprocess transport is the supported one here. The method surface is
 * identical to the Python bindings' and to `libopenfield_bot_ffi`, because all three
 * dispatch through the same Rust table.
 *
 * Quick start:
 *
 *     import { Bot } from 'openfield-bot';
 *
 *     const bot = await new Bot('https://openfield.example.com', 'ofb_' + '1'.repeat(64))
 *       .ready();
 *
 *     const check = await bot.startupCheck();   // does the token work, what is missing?
 *     console.log(check.user.nickname, check.missing_administrative);
 *
 *     await bot.publish('Hello from the SDK', { visibility: 'public' });
 *
 *     for await (const event of bot.watch(33)) {
 *       if (event.data.content?.includes('ping')) {
 *         await bot.sendMessage(33, 'pong', { replyToId: event.data.id });
 *       }
 *     }
 *
 * Long-running bots should read `bots.md` 15 and 16 first: the short version is that a 401
 * means alert and stop, a 403 means a missing permission (the error names it), and the event
 * stream is not a reliable queue.
 *
 * Where the binary comes from: it is found automatically when this package sits inside the
 * SDK checkout — build with `cargo build` and `target/debug` is picked up. Elsewhere, set
 * `OPENFIELD_BOT_SIDECAR` or pass `sidecarPath`.
 */

export {
  BotError,
  TransportError,
  InvalidArgumentError,
  NetworkError,
  BadRequestError,
  UnauthorizedError,
  ForbiddenError,
  NotFoundError,
  ConflictError,
  RateLimitedError,
  ServerError,
  DecodeError,
  TimeoutError,
  ClosedError,
  ERROR_KINDS,
  errorFromEnvelope,
} from './errors.js';

export { SidecarTransport, SIDECAR_ENV, findSidecar, repoRoot } from './sidecar.js';

export {
  Bot,
  Event,
  REACTIONS,
  VISIBILITIES,
  REPORT_TARGETS,
  messageId,
  newestMessageId,
  pageItems,
} from './bot.js';

/** The RPC protocol revision this package speaks. It must match the transport's. */
export const PROTOCOL_VERSION = 1;
