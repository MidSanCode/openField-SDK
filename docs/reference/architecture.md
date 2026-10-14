# OpenField Server Architecture

## Overview

OpenField is a microservices monorepo with a single API gateway in front of four
internal services plus a Flask admin panel. The Flutter client only ever talks to
the gateway (`:8080`).

```
                  ┌────────────────────┐
 Flutter App ────►│  Gateway  (:8080)  │
                  │  auth + routing    │
                  └───┬────┬────┬──────┘
                      │    │    │
           ┌──────────┘    │    └──────────┐
           ▼               ▼               ▼
   Account (:8081)    Storage (:8082)  Chat (:8083)
   JWT/register       attachments/      conversations/
   profile/users      usage             consent/msgs
   Posts (:8084)
   posts/replies
   Flask Admin (:5001)  manages users, groups, permissions
   RustFS  (:9000)      S3-compatible object storage
```

## Ports

| Component            | Port  | Protocol |
|----------------------|-------|----------|
| Gateway              | 8080  | HTTP     |
| Account service      | 8081  | HTTP     |
| Storage service      | 8082  | HTTP     |
| Chat service         | 8083  | HTTP     |
| Posts service        | 8084  | HTTP     |
| Flask admin panel    | 5001  | HTTP     |
| RustFS (S3)          | 9000  | S3 API   |

## Request Flow

1. The client calls `http://localhost:8080/api/v1/<endpoint>` with an
   `Authorization: Bearer <jwt>` header when authenticated.
2. The gateway matches the method + path against its route table.
3. Depending on the route's auth level, the gateway:
   - `authPublic` — forwards without *requiring* a token, but still resolves a
     valid Bearer token when one is present and injects `X-User-ID`, so public
     reads can be personalized (e.g. `is_following`, `my_role`). An absent or
     invalid token simply leaves the request anonymous.
   - `authRequired` — validates the JWT and injects `X-User-ID`.
   - `authPermission` — validates the JWT and checks the route's permission key.
4. If the permission check fails, the gateway returns `403` and never contacts
   the upstream service.
5. The gateway forwards the request to the target service via `httputil.ReverseProxy`,
   preserving the full `/api/v1` path.

Internal services run `GatewayAuthMiddleware()`, which trusts the `X-User-ID`
header set by the gateway. Internal services are **not exposed directly**: they
bind to `127.0.0.1` only, so the sole externally reachable entry point is the
gateway on `:8080`.

> **Public routes that personalize.** `middleware.GetUserID(c)` only reads the
> gin context, which `GatewayAuthMiddleware` populates — so on a public route
> it always returns `0`/false. A public handler that needs the caller's
> identity must use the package's `requesterID(c)` helper (context first, then
> the raw `X-User-ID` header). Getting this wrong is silent: authenticated
> users are treated as anonymous, which surfaces as spurious `401`s
> (`GET /camps?mine=1`), missing `my_role`/`is_member` fields, or an owner
> being locked out of their own resource by a `403`.

## Route Matching Rules

The gateway route table uses gin-style `:param` segments (see `matchRoute` in
`services/gateway/cmd/main.go`):

- Each route has a method and a pattern, e.g. `PUT /api/v1/posts/:id/replies/:reply_id`.
- A pattern segment starting with `:` matches any single path segment.
- A literal segment matches only the identical path segment.
- The number of segments must match exactly.
- When several routes match, the one with the **most literal segments** wins
  (ties broken by first definition). This ensures parameterized routes such as
  `/conversations/start` do not get captured by `/conversations/:id`.

## Permission Model

The gateway checks permissions command-style via
`permFactory.Permit(userID, permissionKey)`:

- Every user belongs to zero or more groups; groups grant permissions.
- The system `admin` role grants all permissions.
- Permission keys are dot-namespaced by domain:

| Domain   | Keys                                                                  |
|----------|-----------------------------------------------------------------------|
| account  | `account.view`, `account.profile.edit`, `account.avatar.edit`, `account.banner.edit` |
| storage  | `storage.upload`, `storage.list`, `storage.get`, `storage.delete`     |
| posts    | `posts.view`, `posts.create`, `posts.edit`, `posts.delete`, `posts.reply.create`, `posts.reply.edit`, `posts.reply.delete` |
| chat     | `chat.view`, `chat.send`, `chat.edit`, `chat.delete`, `chat.request.send`, `chat.request.approve`, `chat.group.create`, `chat.group.invite`, `chat.group.manage`, `chat.group.nickname`, `chat.note.edit` |

The `POST /auth/register` route and `GET /users/me/permissions` require only a
valid JWT (so a user can finish registration and inspect their own grants without
needing any pre-existing permission).

## Services

### Gateway (`services/gateway`)
- Owns the single route table and all auth/permission logic.
- Depends on `pkg/middleware` (JWT parse, `X-User-ID`, permission factory) and
  `pkg/config`.

### Account (`services/account`)
- JWT issuance/refresh, password login, OIDC login, profile updates, avatar/banner
  upload, user search, permissions lookup.

### Posts (`services/posts`)
- Posts CRUD, replies CRUD, per-user post listing. Attachments are referenced by ID
  and rendered from the Storage service's URLs.

### Storage (`services/storage`)
- Attachment upload/list/get/delete and storage usage. Stores bytes in RustFS.

### Chat (`services/chat`)
- Consent-request based conversations: private chat start, group invite, accept/decline.
- Group membership (owner/admin/member), notes, group nicknames, read state.
- Messages with quote-replies (`reply_to_id`), edit (`edited_at`) and soft-delete (`deleted_at`).

## Admin Panel

The Flask admin panel (`admin/`) manages users, groups, group memberships, and
permission grants. It is the source of truth for role/permission changes.

## Configuration

The gateway reads a YAML config (default `config/config.local.yaml`, overridable
via the `OPENFIELD_CONFIG` env var). Service base URLs and the JWT secret must be
consistent across the gateway and all internal services.
