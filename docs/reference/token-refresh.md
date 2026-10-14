# Token Refresh

## Overview

OpenField uses a **short-lived access token + long-lived refresh token** pair to
keep sessions secure without forcing the user to log in frequently.

- **Access token** (JWT) — valid for 1 hour by default (`jwt.expiry_hours: 1`).
  Sent as `Authorization: Bearer <jwt>` on every API call.
- **Refresh token** — a random 256-bit opaque string stored server-side in the
  `refresh_tokens` table. Valid for 30 days by default
  (`jwt.refresh_expiry_days: 30`). Rotated on every use (single-use).

## Flow

```
Login / OIDC callback
        │
        ▼
 ┌─────────────────┐   access_token + refresh_token + expires_in
 │  Account svc    │ ───────────────────────────────────────────►  Client
 └─────────────────┘                (refresh_expires_in)
        │
        │  POST /api/v1/auth/refresh { "refresh_token": "..." }
        ▼
 ┌─────────────────┐   new access_token + new refresh_token (rotated)
 │  Account svc    │ ◄────────────────────────────────────────────  Client
 └─────────────────┘
```

1. On login (password or OIDC) the account service generates a new access token
   and refresh token, stores the refresh token, and returns both to the client.
2. The Flutter client persists both plus `access_expires_at` (computed from
   `expires_in`) in SharedPreferences.
3. A `Timer.periodic` (60 s) checks the remaining access-token lifetime. When
   less than 5 minutes remain, the client calls `/auth/refresh`.
4. The server validates the presented refresh token against `refresh_tokens`,
   revokes it, and issues a **new** refresh token + access token (rotation).
5. On startup, if the stored access token has already expired, the client tries
   to refresh immediately.
6. If refresh fails (invalid/expired/revoked token), the client clears its
   stored credentials and returns to the login screen (automatic logout).

## Endpoints

### `POST /api/v1/auth/refresh`

Public endpoint (gateway: `authPublic`). No access token required.

Request body:

```json
{ "refresh_token": "..." }
```

Response `200`:

```json
{
  "access_token": "jwt",
  "refresh_token": "rotated",
  "expires_in": 3600,
  "refresh_expires_in": 2592000,
  "user": { ... }
}
```

Response `401` — refresh token missing, revoked, expired, or unknown.

## Server implementation notes

| Concern          | Location                                                                                      |
|------------------|-----------------------------------------------------------------------------------------------|
| Config           | `pkg/config/config.go` — `JWTConfig.RefreshExpiryDays` (env `JWT_REFRESH_EXPIRY_DAYS`)        |
| Access expiry    | `pkg/middleware/token.go` — `ExpirySeconds()`                                                 |
| Refresh storage  | `pkg/repository/errors.go` — `StoreRefreshToken`, `RotateRefreshToken`, `RevokeRefreshTokens` |
| Handlers         | `services/account/internal/handler/auth.go` — Login, OIDC callback, RefreshToken              |

`RotateRefreshToken(old, new, userID, seconds)` runs in a transaction: it
deletes the old token (erroring with `repository.ErrNotFound` if the old token
was already revoked/unknown, which the handler maps to a `401`) and inserts the
new one. This makes each refresh token single-use — replaying an old token fails.

`RevokeRefreshTokens(userID)` deletes all refresh tokens for a user (used on
logout / password change).

## Client implementation notes

- `openfield/lib/data/services/auth_service.dart` persists `refresh_token` and
  `access_expires_at`, runs the auto-refresh loop, and exposes
  `refreshAccessToken()`.
- `openfield/lib/data/services/api_service.dart` — `refreshAccessToken(refreshToken)`.
- Deep links / pasted OIDC tokens carry `refresh_token` as a query parameter;
  the client stores it when present.

## Security notes

- Refresh tokens are random 256-bit opaque strings persisted in the
  `refresh_tokens` table, never embedded in JWTs.
- Access token expiry is intentionally short; combined with refresh rotation a
  stolen token is hard to replay.
- The default `everyone` group does not gate refresh — it is tied to the refresh
  token itself, not a permission.
