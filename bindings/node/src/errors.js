/**
 * Error types for the OpenField bot SDK.
 *
 * Every failure the SDK reports arrives as a `BotError` subclass carrying the same fields
 * the Rust core produces, so a caller never has to match on a message string to decide
 * what to do:
 *
 * - `UnauthorizedError` means the token was reset, never existed, or the bot is banned --
 *   all three answer 401, and the documented response is to alert and stop, never to retry
 *   (`bots.md` 15.2);
 * - `ForbiddenError` carries `permission`, the key the bot is missing;
 * - `RateLimitedError` carries `retryAfterSecs`, so a caller can wait exactly as long as
 *   the server asked.
 */

/** Base class for every SDK failure. */
export class BotError extends Error {
  /**
   * @param {string} message
   * @param {object} [details]
   * @param {number} [details.status] HTTP status, when the failure came from a response.
   * @param {string} [details.permission] The missing permission key, on a 403.
   * @param {boolean} [details.retryable] Whether repeating the call could succeed.
   * @param {number} [details.retryAfterSecs] The server's `Retry-After` hint.
   * @param {object} [details.body] The server's parsed error envelope.
   * @param {string} [details.apiError] The raw `error` string from the envelope.
   * @param {string} [details.kind] Overrides the category.
   */
  constructor(message, details = {}) {
    super(message);
    this.name = new.target.name;
    this.kind = details.kind ?? /** @type {string} */ (new.target.kind);
    this.status = details.status;
    this.permission = details.permission;
    this.retryable = details.retryable ?? false;
    this.retryAfterSecs = details.retryAfterSecs;
    this.body = details.body ?? {};
    this.apiError = details.apiError;
  }
}
BotError.kind = 'other';

/** The transport itself failed: no sidecar binary, a dead pipe. */
export class TransportError extends BotError {}
TransportError.kind = 'transport';

/** A caller-supplied value was rejected before any request was made. */
export class InvalidArgumentError extends BotError {}
InvalidArgumentError.kind = 'invalid_argument';

/** The request never reached a usable response. */
export class NetworkError extends BotError {
  constructor(message, details = {}) {
    super(message, { retryable: true, ...details });
  }
}
NetworkError.kind = 'network';

/** The server rejected the request as malformed (400). */
export class BadRequestError extends BotError {}
BadRequestError.kind = 'bad_request';

/** The token is unknown, reset, expired, or the bot is banned (401). Do not retry. */
export class UnauthorizedError extends BotError {}
UnauthorizedError.kind = 'unauthorized';

/** Authenticated but not permitted (403). Inspect `permission`. */
export class ForbiddenError extends BotError {}
ForbiddenError.kind = 'forbidden';

/** The target does not exist, or is invisible to this bot (404). */
export class NotFoundError extends BotError {}
NotFoundError.kind = 'not_found';

/** A duplicate or an already-pending operation (409). */
export class ConflictError extends BotError {}
ConflictError.kind = 'conflict';

/** A quota was exhausted (429). Honour `retryAfterSecs`. */
export class RateLimitedError extends BotError {
  constructor(message, details = {}) {
    super(message, { retryable: true, ...details });
  }
}
RateLimitedError.kind = 'rate_limited';

/** The server failed (5xx); safe to retry with backoff. */
export class ServerError extends BotError {
  constructor(message, details = {}) {
    super(message, { retryable: true, ...details });
  }
}
ServerError.kind = 'server_error';

/** A response body did not match the expected shape. */
export class DecodeError extends BotError {}
DecodeError.kind = 'decode';

/** The sidecar did not answer in time. */
export class TimeoutError extends BotError {
  constructor(message, details = {}) {
    super(message, { retryable: true, ...details });
  }
}
TimeoutError.kind = 'timeout';

/** The client or event stream has already been closed. */
export class ClosedError extends BotError {}
ClosedError.kind = 'closed';

/** Every category the SDK can report. */
export const ERROR_KINDS = {
  transport: TransportError,
  invalid_argument: InvalidArgumentError,
  network: NetworkError,
  bad_request: BadRequestError,
  unauthorized: UnauthorizedError,
  forbidden: ForbiddenError,
  not_found: NotFoundError,
  conflict: ConflictError,
  rate_limited: RateLimitedError,
  server_error: ServerError,
  decode: DecodeError,
  timeout: TimeoutError,
  closed: ClosedError,
  other: BotError,
};

/**
 * Build the right error from an envelope.
 *
 * `kind` may be the SDK's own category (`forbidden`) or the Rust enum tag (`http`); in the
 * latter case the status decides the class.
 *
 * @param {object|string} error
 * @returns {BotError}
 */
export function errorFromEnvelope(error) {
  if (error === null || typeof error !== 'object') {
    return new BotError(String(error));
  }

  let kind = error.kind ?? 'other';
  if (kind === 'http') kind = kindForStatus(error.status);

  const Class = ERROR_KINDS[kind] ?? BotError;
  const details = {
    kind,
    status: error.status,
    permission: error.permission,
    retryAfterSecs: error.retry_after_secs,
    body: error.body ?? {},
    apiError: error.api_error,
  };
  // Only override the class's own verdict when the server actually expressed one: an
  // absent field must not turn a retryable 429 into a permanent failure.
  if (error.retryable !== undefined) details.retryable = Boolean(error.retryable);

  return new Class(error.message ?? `request failed (${kind})`, details);
}

/** @param {unknown} status */
function kindForStatus(status) {
  if (!Number.isInteger(status)) return 'other';
  if (status === 400) return 'bad_request';
  if (status === 401) return 'unauthorized';
  if (status === 403) return 'forbidden';
  if (status === 404) return 'not_found';
  if (status === 409) return 'conflict';
  if (status === 429) return 'rate_limited';
  if (status >= 500 && status < 600) return 'server_error';
  return 'other';
}
