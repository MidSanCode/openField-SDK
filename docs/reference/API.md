# OpenField Server API Documentation

## Base URL
```
http://localhost:8080/api/v1
```

All requests are proxied through the **gateway**, which validates JWTs and enforces
permissions before forwarding to the internal services.

## Authentication

OpenField uses OIDC (OpenID Connect) for authentication, plus password login for
admin-created local accounts. **Self-registration is not allowed.** New OAuth users
are created with `needs_registration = true` and must call `POST /auth/register`
to set a unique username and a nickname before using the app.

### Roles & Permissions

- Roles are granted via **groups**; the first user created in the system (via OAuth)
  is automatically granted the `admin` role.
- Permissions are checked at the gateway. A permission is granted either by the
  user's role or by an explicit permission grant. Permission keys are dot-namespaced,
  e.g. `posts.view`, `chat.send`.

### Get Available Providers
```
GET /auth/providers
```

Response:
```json
{
  "providers": ["oidc", "password"]
}
```

### Password Login (local accounts created by admins)
```
POST /auth/login
Body: { "username": "alice", "password": "secret" }
```

### Register (complete OAuth signup)
```
POST /auth/register
Authorization: Bearer <access_token>
Body: { "username": "alice", "nickname": "Alice" }
```

### OIDC Login Flow
1. Get the authorization URL:
```
GET /auth/oidc/login
```

Response:
```json
{
  "auth_url": "https://auth.yun/oidc/auth?...",
  "provider": "oidc"
}
```

Pass `?flow=web` when the sign-in starts from a browser client, so step 4
bounces back to the web app instead of firing the `openfield://` deep link
(browsers refuse to open custom schemes silently):

```
GET /auth/oidc/login?flow=web
```

The response is unchanged; `flow` is echoed back in the payload.

2. Redirect user to the auth_url.

3. After authorization, the provider redirects to the callback URL with an authorization code.

4. The server exchanges the code for tokens:

```
GET /auth/oidc/callback?code=...
```

When `oidc.app_redirect_url` is configured (e.g. `openfield://oauth/callback`), the server responds with a
`302 Found` redirect to that deep link carrying the access token and user info, so desktop/mobile apps can
complete the login via the OS URI handler:

```
openfield://oauth/callback?access_token=eyJ...&username=openfield&email=user@example.com&avatar_url=...
```

Otherwise (e.g. API-only or web clients) it returns JSON:

```json
{
  "access_token": "eyJ...",
  "refresh_token": "...",
  "token_type": "Bearer",
  "expires_in": 86400,
  "user": {
    "id": 1,
    "username": "openfield",
    "email": "user@example.com",
    "avatar_url": "https://..."
  }
}
```

#### Browser (web) callbacks

A login started with `flow=web` never reaches the JSON branch: when
`oidc.web_redirect_url` is set, the server redirects the browser to that URL
with the same token payload in the query string, so the web client can pick it
up on its own callback route and store the tokens itself.

```
302 Found
Location: https://horizon.example.com/oauth/callback
  ?access_token=eyJ...
  &refresh_token=...
  &expires_in=86400
  &refresh_expires_in=2592000
  &username=openfield
  &email=user@example.com
  &avatar_url=https%3A%2F%2F...
  &needs_registration=false
```

If the identity is bound to several accounts the callback redirects with
`?pick=<ticket>` instead (see *Multi-Account Login* below), and the client
resolves it through `GET /auth/oidc/pick`.

Three things worth designing for:

- The tokens arrive in the query string, so the client should strip them from
  the URL (and from browser history) as soon as it has stored them, and the
  route must be served over HTTPS in production.
- `web_redirect_url` is read by the gateway instance the OIDC provider
  redirects to. Set it on that deployment, not only on the one the web client
  talks to. When it is empty, `flow=web` falls back to `app_redirect_url` and
  then to the JSON response.
- The web app itself needs two things outside this endpoint: the host serving
  it must rewrite unknown paths to `index.html` (history-mode SPA) so the
  callback route resolves, and — when the app and the gateway are on different
  hosts — its origin must be allowed by `server.allowed_origins` for the
  `GET /auth/providers` / `GET /auth/oidc/login` calls that start the flow.

### Multi-Account Login (OAuth identity → several OpenField accounts)

One OAuth identity may be bound to up to `5` OpenField accounts (see
`docs/multi-account.md`). When the identity is already bound to at least one
account, the callback does **not** auto-login: it issues a short-lived
single-use **pick ticket** and redirects with `?pick=<ticket>` (web:
`web_redirect_url?pick=...`, app: `app_redirect_url?pick=...`, no redirect
configured: JSON `{ "pick": "..." }`). The client then uses the ticket to
choose an account or add a new one:

#### Get bound accounts for a pick ticket
```
GET /auth/oidc/pick?ticket=<ticket>
```
Does **not** consume the ticket (safe to re-fetch). Returns the identity and
the accounts bound to it:
```json
{
  "provider": "oidc",
  "oauth2_username": "alice",
  "email": "alice@example.com",
  "avatar_url": "https://...",
  "max_accounts": 5,
  "accounts": [
    { "id": 1, "username": "openfield", "nickname": "Alice", ... }
  ]
}
```
`404` when the ticket is unknown or expired.

#### Select an account and sign in
```
POST /auth/oidc/pick/select
Body: { "ticket": "...", "user_id": 1 }
```
Consumes the ticket. Verifies the account really belongs to the ticket's
identity, then returns the same token payload as a normal login
(`access_token`, `refresh_token`, `user`). `403` when the account is not bound
to the identity or is banned/scheduled for deletion.

#### Add a new account and sign in
```
POST /auth/oidc/pick/create
Body: { "ticket": "..." }
```
Consumes the ticket. Enforces the per-identity quota server-side (`409
{"error":"account quota reached"}` when the identity already holds
`max_accounts` accounts), provisions a new `needs_registration = true` account
bound to the identity, and returns the login token payload.

### Refresh Token
```
POST /auth/refresh
Body: { "refresh_token": "..." }
```
Returns a new access token and a rotated refresh token (single-use):
```json
{
  "access_token": "jwt",
  "refresh_token": "rotated",
  "expires_in": 3600,
  "refresh_expires_in": 2592000,
  "user": { ... }
}
```
`401` when the refresh token is missing, revoked, expired, or unknown. See
`docs/token-refresh.md` for the full flow.

## Wallet

### Get My Wallet (Authenticated, `wallet.view`)
```
GET /wallet
Authorization: Bearer <access_token>
```
```json
{
  "balance": 10000,
  "transactions": [
    {
      "id": 1,
      "amount": 10000,
      "balance_after": 10000,
      "type": "recharge",
      "description": "Admin top-up",
      "created_at": "2026-08-03T10:00:00Z"
    }
  ]
}
```
Amounts are in cents. See `docs/wallet.md` for details.

### Adjust Wallet (Authenticated, `wallet.manage`)
```
POST /wallet/adjust
Authorization: Bearer <access_token>
Body: { "user_id": 42, "amount": -500, "type": "deduct", "description": "..." }
```
Positive `amount` recharges, negative deducts; a deduction below zero balance
is rejected. `type` is `recharge` / `deduct`.

## Users

### Get Current User (Authenticated)
```
GET /users/me
Authorization: Bearer <access_token>
```
Response includes the current storage usage and quota:
```json
{
  "id": 1,
  "username": "alice",
  "nickname": "Alice",
  "avatar_url": "https://...",
  "storage_quota": 104857600,
  "storage_used": 2048000
}
```

### Get My Permissions (Authenticated)
```
GET /users/me/permissions
Authorization: Bearer <access_token>
```
Response:
```json
{
  "permissions": ["posts.view", "posts.create", "chat.send"],
  "groups": ["admin", "beta-users"]
}
```

### Search Users (Authenticated)
```
GET /users/search?q=alice&limit=20
Authorization: Bearer <access_token>
```
Searches by username or nickname (substring match). Response:
```json
{
  "users": [
    { "id": 2, "username": "alice", "nickname": "Alice", "avatar_url": null }
  ]
}
```

### Update Profile (Authenticated)
```
PUT /users/me
Authorization: Bearer <access_token>
Body: { "username": "alice", "nickname": "Alice" }
```
Both fields optional; at least one required. Re-uses the same uniqueness rules as
registration. Clears `needs_registration`.

### Upload Avatar (Authenticated)
```
POST /users/me/avatar
Authorization: Bearer <access_token>
Content-Type: multipart/form-data
Form field: file=<image>
```

### Upload Banner / Header Image (Authenticated)
```
POST /users/me/banner
Authorization: Bearer <access_token>
Content-Type: multipart/form-data
Form field: file=<image>
```

### Get User by ID (Authenticated)
```
GET /users/:user_id
Authorization: Bearer <access_token>
```

## Posts

All posts endpoints require the `posts.*` permissions.

### List Posts
```
GET /posts?page=1&limit=20
Authorization: Bearer <access_token>
```

### Get Single Post
```
GET /posts/:id
Authorization: Bearer <access_token>
```

### Create Post
```
POST /posts
Authorization: Bearer <access_token>
Body: { "content": "Hello World!", "attachment_ids": [1, 2] }
```
`attachment_ids` is optional (max 9 per post). Attachments must be uploaded first via
`POST /attachments`.

### Update Post (Owner Only)
```
PUT /posts/:id
Authorization: Bearer <access_token>
Body: { "content": "Updated text!", "attachment_ids": [1, 3] }
```
`attachment_ids` is optional (max 9 per post) and **replaces** the post's attachments.

### Delete Post (Owner Only)
```
DELETE /posts/:id
Authorization: Bearer <access_token>
```

### List Replies
```
GET /posts/:id/replies?limit=50
Authorization: Bearer <access_token>
```
Response: `{ "replies": [...] }`

### Create Reply
```
POST /posts/:id/replies
Authorization: Bearer <access_token>
Body: { "content": "Nice post!" }
```

### Update Reply (Owner Only)
```
PUT /posts/:id/replies/:reply_id
Authorization: Bearer <access_token>
Body: { "content": "Updated reply!" }
```

### Delete Reply (Owner Only)
```
DELETE /posts/:id/replies/:reply_id
Authorization: Bearer <access_token>
```

### List User Posts
```
GET /users/:user_id/posts?page=1&limit=20
Authorization: Bearer <access_token>
```

## Attachments

Files are stored in the local RustFS (S3-compatible) object storage.

Each user has a total storage quota (default 100 MB). Uploads that would exceed
the quota are rejected with `413 storage quota exceeded`. Admins can adjust a
user's quota from the Flask admin panel. Avatar and banner uploads also count
toward the quota.

### Upload Attachment (Authenticated)
```
POST /attachments
Authorization: Bearer <access_token>
Content-Type: multipart/form-data
Form field: file=<file>
```
Response:
```json
{
  "id": 1,
  "user_id": 1,
  "original_name": "photo.jpg",
  "mime_type": "image/jpeg",
  "size_bytes": 12345,
  "url": "http://localhost:9000/openfield/2026/01/01/uuid.jpg",
  "created_at": "..."
}
```

### List My Attachments
```
GET /attachments?limit=50
Authorization: Bearer <access_token>
```
Response: `{ "attachments": [...] }`

### Get Attachment
```
GET /attachments/:id
Authorization: Bearer <access_token>
```

### Delete Attachment (Owner Only)
```
DELETE /attachments/:id
Authorization: Bearer <access_token>
```

### Storage Usage
```
GET /storage/usage
Authorization: Bearer <access_token>
```

## Chat

The chat feature is consent-based: starting a private chat or inviting to a group
creates a **consent request** that the target user must accept.

### List Consent Requests
```
GET /consent-requests
Authorization: Bearer <access_token>
```
Lists requests targeting the current user. Response: `{ "requests": [...] }`

### Accept Consent Request
```
POST /consent-requests/:id/accept
Authorization: Bearer <access_token>
```
Response:
```json
{
  "message": "request accepted",
  "conversation": { "id": 5, "type": "private", "title": null }
}
```

### Decline Consent Request
```
POST /consent-requests/:id/decline
Authorization: Bearer <access_token>
```
Responds `204 No Content`.

### List Conversations
```
GET /conversations
Authorization: Bearer <access_token>
```
Response: `{ "conversations": [...] }` (each includes `last_message` and `unread_count`).

### Create Group
```
POST /conversations
Authorization: Bearer <access_token>
Body: { "title": "Family" }
```
Creates a group with the caller as owner. Responds `201 Created` with the conversation.

### Start Private Chat (sends a consent request)
```
POST /conversations/start
Authorization: Bearer <access_token>
Body: { "user_id": 2, "message": "Hi!" }
```
Responds `201 Created` with `{ "message": "chat request sent" }`. Errors:
`409` if a request is already pending, `400` if `user_id` is the caller.

### Get Conversation
```
GET /conversations/:id
Authorization: Bearer <access_token>
```
Response:
```json
{
  "conversation": { "id": 5, "type": "private", "title": null },
  "members": [...],
  "my_membership": { "user_id": 1, "role": "member", "group_nickname": null, "note": null, "last_read_message_id": 0 }
}
```

### Invite to Group (sends a consent request)
```
POST /conversations/:id/invite
Authorization: Bearer <access_token>
Body: { "user_id": 3, "message": "Join us!" }
```
Responds `201 Created` with `{ "message": "invite sent" }`.

### Update Note (private chat remark)
```
PUT /conversations/:id/note
Authorization: Bearer <access_token>
Body: { "note": "my boss" }
```

### Update Group Nickname
```
PUT /conversations/:id/group-nickname
Authorization: Bearer <access_token>
Body: { "group_nickname": "Bob (work)" }
```

### Mark Conversation Read
```
POST /conversations/:id/read
Authorization: Bearer <access_token>
Body: { "last_message_id": 42 }
```
Responds `204 No Content`.

### Leave Group
```
POST /conversations/:id/leave
Authorization: Bearer <access_token>
```
The owner cannot leave. Responds `204 No Content`.

### Remove Group Member (owner/admin only)
```
DELETE /conversations/:id/members/:user_id
Authorization: Bearer <access_token>
```
Responds `204 No Content`.

### List Messages
```
GET /conversations/:id/messages?before=0&limit=50
Authorization: Bearer <access_token>
```
`before` is a message ID (paginate backwards); `limit` defaults to 50.
Returns newest-to-oldest from the cursor; response: `{ "messages": [...] }`.
Each message may include `reply_to_id`, `edited_at`, `deleted_at`.

### Send Message
```
POST /conversations/:id/messages
Authorization: Bearer <access_token>
Body: { "content": "Hello!", "reply_to_id": 12 }
```
`reply_to_id` is optional. Responds `201 Created` with the message.

### Edit Message (Owner Only)
```
PUT /conversations/:id/messages/:message_id
Authorization: Bearer <access_token>
Body: { "content": "Edited text!" }
```

### Delete Message (Owner Only, soft delete)
```
DELETE /conversations/:id/messages/:message_id
Authorization: Bearer <access_token>
```
Responds `204 No Content`.

## Health Check
```
GET /health
```
Response: `{"status": "ok"}`
