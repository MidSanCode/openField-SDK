# OpenField Server Features

This document is a feature guide for the OpenField server. It complements the
[API reference](API.md) by explaining what each feature does, how it is built,
and where the responsible code lives. See [architecture.md](architecture.md)
for the service layout and [wallet.md](wallet.md) for the wallet in detail.

## Feature Map

| Feature                    | Location                                                     | Notes |
|----------------------------|--------------------------------------------------------------|-------|
| Authentication & OIDC      | `services/account/internal/handler/auth.go`                  | OIDC + password login |
| OAuth multi-account        | `services/account/internal/handler/auth.go`, `pkg/repository/oauth_pick.go` | one identity → up to 5 accounts |
| QR login                   | `services/account/internal/handler/auth.go`                  | 5-minute handshake, scanner approves |
| Profile & users            | `services/account/internal/handler/user.go`                  | edit, avatar/banner, locale |
| Permissions (groups)       | gateway auth + `pkg/repository/permission.go`                | route-level checks |
| Wallet & transfers          | `services/account/internal/handler/wallet.go`, `transfer.go` | cents storage |
| Payment PIN                | `services/account/internal/handler/pin.go`                   | authorizes payments |
| Tasks & experience         | `services/account/internal/handler/task.go`                  | daily login, daily activity milestones, one-time codes |
| Exp history                | `services/account/internal/handler/task.go`                  | level derivation |
| Membership                 | `services/account/internal/handler/membership.go`            | tiers, multiplier, storage, quotas |
| Name styling               | `services/account/internal/handler/user.go`                  | color / gradient / animation |
| Follow & friends           | `services/account/internal/handler/user.go`                  | mutual follows |
| Posts & replies            | `services/posts`                                             | reactions, favorites, views, pins |
| Camps                      | `services/posts/internal/handler/camp.go`                    | 贴吧-style communities, quotas |
| Camp subgroups             | `services/posts/internal/handler/camp.go`, `pkg/repository/camp.go` | private nested camps, cascade delete |
| Moderation reports         | `services/account/internal/handler/report.go`                | report posts / messages / users |
| Bots (automated accounts)  | `services/account/internal/handler/bot.go`, `pkg/repository/bot.go` | `ofb_` static token, inherits the default group |
| App announcements          | `services/account/internal/handler/app_announcement.go`      | startup notices, admin-published |
| Attachments / storage      | `services/storage`                                           | RustFS, quota, previews |
| Chat & consent             | `services/chat`                                              | E2E, groups, mentions |
| Group extras               | `services/chat/internal/handler/group_features.go`           | announcements, todos, files |
| Realtime push              | `services/push`                                              | WebSocket gateway |
| API gateway                | `services/gateway/cmd/main.go`                               | routing, auth, rate limiting |
| Admin panel                | `admin/app.py`                                               | users, groups, grants |

## Authentication

- **OIDC login** exchanges an authorization code for JWT access + refresh
  tokens. When `oidc.app_redirect_url` is configured, the callback responds
  with a `302` to a deep link (`openfield://oauth/callback?...`) carrying the
  tokens and user info; otherwise it returns JSON.
- **Multi-account login**: one OAuth identity may be bound to several OpenField
  accounts (default max `5`, see [multi-account.md](multi-account.md)). When
  the identity is already bound to one or more accounts, the callback parks the
  identity behind a short-lived single-use **pick ticket** and redirects with
  `?pick=<ticket>`; the client lists the bound accounts and selects one, or
  adds a new account (quota permitting) via `/auth/oidc/pick/{select,create}`.
- **Password login** exists for admin-created local accounts.
- **Self-registration is not allowed.** New OAuth users are created with
  `needs_registration = true` and must call `POST /auth/register` to pick a
  unique username + nickname before using the app.
- **Refresh tokens** are single-use and rotated on every refresh. See
  [token-refresh.md](token-refresh.md).
- **Roles** come from groups; the first OAuth user is granted `admin`. The
  gateway enforces `authPublic` / `authRequired` / `authPermission` per route,
  injecting `X-User-ID` for downstream services.

Endpoint: `POST /auth/login`, `POST /auth/refresh`, `GET /auth/oidc/login`,
`GET|POST /auth/oidc/callback`, `GET /auth/oidc/pick`,
`POST /auth/oidc/pick/select`, `POST /auth/oidc/pick/create`,
`POST /auth/oidc/bind`, `POST /auth/register`.

## Profile & Users

- `GET /users/me` returns the full profile including `storage_quota`,
  `storage_used`, membership fields and name-styling fields.
- `PUT /users/me` updates `username` / `nickname` / `bio` with the same
  uniqueness rules as registration and clears `needs_registration`.
- `POST /users/me/avatar` and `POST /users/me/banner` accept multipart uploads
  and count toward the storage quota.
- `PUT /users/me/locale` stores the region/language used for day boundaries
  and server-pushed notifications.
- `PUT /users/me/e2ee-key` publishes the user's X25519 public key for
  end-to-end-encrypted conversations.
- `GET /users/:user_id` is the public profile lookup; `GET /users/search`
  finds users by username/nickname substring.

## Permissions

Permissions are dot-namespaced keys granted through groups or the `admin`
role. The gateway performs the only check; a `403` is returned before the
request ever reaches an internal service.

| Domain   | Keys |
|----------|------|
| account  | `account.view`, `account.profile.edit`, `account.avatar.edit`, `account.banner.edit`, `account.follow` |
| storage  | `storage.upload`, `storage.list`, `storage.get`, `storage.delete` |
| posts    | `posts.create`, `posts.edit`, `posts.delete`, `posts.reply.create`, `posts.reply.edit`, `posts.reply.delete`, `posts.react`, `posts.favorite` |
| chat     | `chat.view`, `chat.send`, `chat.edit`, `chat.delete`, `chat.request.send`, `chat.request.approve`, `chat.group.create`, `chat.group.invite`, `chat.group.manage`, `chat.group.nickname`, `chat.note.edit` |
| wallet   | `wallet.view`, `wallet.manage` |
| admin    | `user.adjust_exp`, `user.membership.grant` |
| app      | `app.announcements.manage` |

Keys are seeded by `seedPermissions()` in `pkg/database/migration.go`. Today
the default `everyone` group holds the full set; tightening group grants is a
follow-up.

`GET /users/me/permissions` returns the caller's effective permissions and
groups without requiring any permission itself (so registration can finish).

## Wallet, Transfers & Currency

Money is stored as integer cents (`BIGINT`) to avoid float drift. See
[wallet.md](wallet.md) for the transaction model.

- **Wallet**: `GET /wallet` returns balance + recent `wallet_transactions`.
  `POST /wallet/adjust` (permission `wallet.manage`) recharges/deducts a target
  user inside a transaction that locks the wallet row and refuses negative
  balances.
- **Transfers**: a user may transfer coins to any other user. Transfers are
  created with `POST /transfers`, then `accept`ed or `decline`d by the
  recipient using their payment PIN. Pending transfers are listed per user.
- **Payment PIN**: a 6-digit PIN (`POST /users/me/pin`) that authorizes all
  outbound payments. `POST /users/me/pin/verify` validates a candidate PIN
  against the stored hash. It never replaces the login password. Changing an
  existing PIN goes through `PUT /users/me/pin`, which requires the current
  PIN (`old_pin`) and shares the same per-user failure budget as verify.
  Admins can reset any user's PIN directly from the admin panel (bcrypt hash
  written to `users.pin_hash`). (These routes are registered in the gateway
  account block.)

## Tasks & Experience

Experience fuels the account level; every exp grant is routed through
`GrantExp`, which applies the active membership multiplier.

- **Daily login**: `POST /tasks/daily-login/claim` grants daily exp; the
  sign-in calendar is at `GET /tasks/daily-login/calendar`. Missed days can be
  made up with coins via `POST /tasks/daily-login/makeup` (auto) or
  `POST /tasks/daily-login/makeup-date` (per date).
- **One-time tasks**: `POST /tasks/:code/claim` completes a task by its
  achievement code (e.g. profile completion) for a one-time reward.
- **Daily activity milestones** (kind `daily`):
  `POST /tasks/daily/:code/claim` grants a per-day reward for reaching an
  activity count **today**. Progress is the user's count for the current
  server day (timezone from the game config) and each tier is claimable once
  per day, keyed in `task_completions.cycle_key` by the date. Tiers are
  cumulative, so reaching the highest tier of a group unlocks every lower one
  the same day.
  - Posts: `daily_posts_1` / `daily_posts_5` / `daily_posts_10` /
    `daily_posts_20` (1, 5, 10, 20 posts published today).
  - Chat: `daily_chat_25` / `daily_chat_50` / `daily_chat_100` (25, 50, 100
    chat messages sent today; soft-deleted messages do not count).
  - The catalog is seeded by `seedTasks()` (upsert by code), so existing
    installs gain the tiers on the next boot without a schema migration.
- **Exp history**: `GET /exp/history` returns a paginated ledger of every exp
  grant with the level applied, which the client uses to render the timeline.
- **Levels**: derived from cumulative total exp with geometric cost growth
  (level 2 costs 100 exp, each level 5% more, capped at level 200). See
  `pkg/model/level.go` for `LevelForExp` / `ExpProgress`.

## Membership

Membership is a purchasable tier that boosts experience and grants perks. It
lives in `pkg/model/membership.go`.

- **Tiers** (fixed catalog):

  | Level | Name   | Price (coins) | Exp multiplier | Storage bonus |
  |-------|--------|---------------|----------------|---------------|
  | 1     | 薄雾   | 200           | x2             | +100 MB       |
  | 2     | 篝火   | 400           | x2.5           | +200 MB       |
  | 3     | 明月   | 800           | x3             | +400 MB       |
  | 4     | 孤星   | 1600          | x3.5           | +400 MB       |

- Membership lasts 30 days (`MemberDurationDays`); the level is kept after
  expiry so a renewal extends from where the user left off (`MembershipActive`
  decides whether the multiplier/bonus applies at a given moment).
- **Purchase**: `POST /membership/purchase` charges the wallet and requires the
  payment PIN. `GET /membership` returns the user's status plus the catalog
  with each tier's multiplier, storage bonus and name-style caps.
- **Admin grant**: `PUT /users/:user_id/membership` (permission
  `user.membership.grant`) assigns a tier directly, or clears it with level 0;
  an optional `days` overrides the default duration.
- **Exp multiplier**: applied inside `GrantExp` while active, so positive
  adjustments scale up automatically.
- **Storage bonus**: upload quotas use `MemberStorageBonusAt`, which adds the
  tier bonus while active and reverts to base the moment membership expires.
  When membership expires above the base quota, further uploads are rejected
  until usage drops back under the base (see `checkQuota` in the storage
  service and `uploadImage` in the account service).
- **Creation quotas**: the same +25%-per-level bonus scales the number of
  groups a user may own (base 1000, joined groups don't count) and the number
  of camps a user may create (base 100). Quotas are enforced at creation time
  in `pkg/repository/quotas.go`; exceeding them returns a 403 with the
  computed limit.

## Name Styling

Members can customize the color of their display name, rendered identically
across posts, replies, chat and profiles via denormalized columns on every
user JOIN.

- Columns on `users`: `name_color`, `name_color_to`, `name_dynamic`
  (animated flag) plus the reserved `avatar_frame`.
- **Tier caps** (`MemberNameStyleAllowed`):
  - Lv.1: fixed preset palette only (`MemberPresetNameColors`).
  - Lv.2: any valid hex color.
  - Lv.3: also allowed a second gradient color (`name_color_to`).
  - Lv.4: also allowed the animated gradient (`name_dynamic`).
- `PUT /users/me/name-style` validates against the caps (presets-only is
  enforced server-side for Lv.1) and persists the styling. The gateway routes
  it with the `account.profile.edit` permission.
- The member tier badge next to the name (tier name shown between display name
  and verified badge) is driven by the denormalized `member_level` /
  `member_active` fields on posts, replies, chat members and messages.

## Follow & Friends

`POST/DELETE /users/:user_id/follow` (permission `account.follow`) track
followers. When two users follow each other they become **friends**; lists are
available at `GET /users/:user_id/followers|following|friends`. Follow
counts and `is_following` / `is_friend` are populated on the user profile.

Users may opt out of sharing all three lists via `PUT /users/me/privacy`
(`{"hide_follow_lists": true}`). While enabled, the lists return 403 to
everyone but the account owner, and `follower_count` / `following_count` are
suppressed from other users' profile reads.

## Posts & Replies

Posts support optional attachments, visibility scopes (public / private /
friends), reactions, favorites and read-tracking.

- **CRUD**: `GET|POST /posts`, `PUT|DELETE /posts/:id` (owner only), plus
  per-user listing `GET /users/:user_id/posts`.
- **Pinning**: `PUT /posts/:id/pin` (`{"pinned": true|false}`, author only)
  toggles the pinned flag; pinned posts float to the top of the author's
  profile and the global feed and carry a `pinned` field in payloads.
- **Replies**: `GET|POST /posts/:id/replies`, edit/delete per reply, and
  per-reply favorites (`POST .../replies/:reply_id/favorite`).
- **Reactions**: `PUT/DELETE /posts/:id/reactions` with a reaction key;
  aggregates included in the post payload.
- **Favorites**: `POST/DELETE /posts/:id/favorite`; the caller's favorite
  lists are `GET /users/:user_id/favorites/posts|replies`.
- **Reading**: post payloads include `reply_count`, `favorite_count`, reaction
  map and the author's denormalized membership/name-style fields. Anonymous
  viewers only see public posts (`visibilityCondition`).

## Camps

Camps are 贴吧-style communities: a named space whose posts live in the camp
rather than the global feed. Code: `services/posts/internal/handler/camp.go`
+ `pkg/repository/camp.go`.

- **Create**: `POST /camps` with `name`, optional `description`,
  `is_visible` (hidden camps are only listed to members), `direct_join`
  (when false, self-joining is rejected) and the permission switches
  `member_post` (default true) / `member_pin` (default false). The creator
  is added as owner and counts against their creation quota. `camps.name` is
  UNIQUE: a duplicate answers **409** (`camp name already taken`), never 500.
  The same holds for `PUT /camps/:id` renames.
- **Browse**: `GET /camps` lists visible camps (search via `?q=`, the
  caller's camps via `?mine=1`, which includes hidden ones);
  `GET /camps/:id` hides invisible camps from non-members. Camp payloads
  carry `my_role` ("owner"/"admin"/"member") for the caller.
  These three reads are **public routes that still personalize**: the gateway
  forwards `X-User-ID` for a valid Bearer token, and the handlers must read it
  through the `requesterID(c)` helper (context first, then the header).
  Reading `middleware.GetUserID(c)` alone always yields 0 on a public route,
  which would answer 401 to `?mine=1`, hide a hidden camp from its own owner,
  and 403 the owner out of their own camp feed.
  **The creator always counts as a member/owner**, even when their
  `camp_members` row is missing (e.g. camps predating role semantics): both
  `GET /camps` and `GET /camps?mine=1` fall back to `creator_id`, so an owner
  is never reported as a non-member and never sees a join action.
- **Membership**: `POST /camps/:id/join` (requires `direct_join`),
  `DELETE /camps/:id/members/me` (the creator cannot leave).
- **Roles**: `owner` (creator) > `admin` > `member`, stored in
  `camp_members.role` (v26 backfills creator rows to owner).
- **Members**: `GET /camps/:id/members` lists the roster (members only,
  highest roles first); `POST /camps/:id/members/:user_id` directly
  enrolls a user as member (admin+, bypasses `direct_join`);
  `PUT /camps/:id/members/:user_id/role` promotes/demotes between
  admin/member (owner only, the owner's own role is immutable);
  `DELETE /camps/:id/members/:user_id` kicks (admin+ may remove members,
  only the owner may remove an admin, nobody removes the owner).
- **Camp settings**: `PUT /camps/:id` — admins may change
  name/description/visibility/direct-join; `member_post`/`member_pin` are
  owner-only.
- **Camp feed**: `GET /camps/:id/posts` is **members-only** (hidden camps
  answer 404 to outsiders, visible camps 403 with a join hint). The camp's
  **owner and admins** receive every post in their camp regardless of each
  post's own `visibility` (they moderate it), so the creator always sees the
  camp's content; plain members only see posts whose visibility admits them.
  Posting into a camp happens through the normal `POST /posts` with `camp_id`
  set; the author must be a member and, when `member_post` is off, hold admin
  or owner. Camp posts are excluded from the global feed and masked from
  profiles/favorites/quotes of non-members. Camp-pinned posts
  (`camp_pinned`, v26) float to the top of their camp feed only; pinning
  via `PUT /posts/:id/pin` or `PUT /camps/:id/posts/:post_id/pin` —
  admins/owner may pin any camp post, plain members only their own when
  `member_pin` is on.
- **Choosing a camp when posting**: `POST /posts` accepts an optional
  `camp_id` (0/omitted = the global feed), so the composer can publish either
  to the square or into any camp the author joined. Editing a post never moves
  it between camps.
- **Edit / delete**: `PUT /camps/:id` edits the basics (admins) plus the
  permission switches (owner); `DELETE /camps/:id` removes a camp and is
  **creator-only**. Clients surface both from the camp directory row menu and
  from the camp feed's settings sheet.
- **Subgroups (子群)**: `camps.parent_camp_id` (v31) nests a camp under another
  camp. A subgroup is **private by construction**, not by configuration:
  creation forces `is_visible=false` and `direct_join=false`, and `PUT /camps/:id`
  re-forces both, so nobody can publish one or open it to self-service joining.
  The existing hidden-camp rules then do the rest — non-members get 404 from
  `GET /camps/:id`, and `POST /camps/:id/join` refuses hidden camps outright.
  Subgroups are also excluded from `GET /camps` (which filters on
  `parent_camp_id IS NULL`).
  - **Create**: `POST /camps/:id/subgroups` with `name` + optional
    `description`; **owner/admin of the parent camp only** (the announcement
    rule). The parent must be top-level, so nesting is exactly one level deep
    and a subgroup can never acquire subgroups. The creator becomes the
    subgroup's owner. Camp creation quotas do not apply — a subgroup is bounded
    by its parent's roster instead.
  - **List**: `GET /camps/:id/subgroups` (the parent camp's members only;
    anyone else gets the same 404 a hidden camp answers). Payloads carry
    `my_role` for the caller.
  - **Members are pulled from the parent**: `POST /camps/:id/members/:user_id`
    on a subgroup refuses a target who is not already a member of the parent
    camp (**403**). That is what keeps a subgroup a sub-community of the main
    group rather than a second, unrelated group. Every other member rule
    (roles, kick, leave) is unchanged and inherited from camps.
  - **Cascade delete**: `parent_camp_id` is `ON DELETE CASCADE`, so deleting
    the main camp deletes its subgroups. `DELETE` additionally runs in a
    transaction that **removes the posts of a private camp and of every
    subgroup** before the cascade fires: `posts.camp_id` is
    `ON DELETE SET NULL`, so a bare delete would republish a private camp's
    posts into the global feed. A *public* camp keeps its existing behaviour
    (its posts become global feed posts).

## App Announcements

Server-wide notices shown by clients on startup. Code:
`services/account/internal/handler/app_announcement.go` +
`pkg/repository/app_announcement.go`.

- `GET /announcements` is public and returns active announcements (newest
  first); `?all=1` includes retired entries for management views.
- `POST /announcements` publishes (permission
  `app.announcements.manage`); `PUT /announcements/:id` with
  `{"active": false}` retires a notice without deleting the history entry.
- Clients remember dismissed announcement ids locally, so a notice shows
  once per device until a new one is published.

## Attachments & Storage

Files are stored in RustFS (an S3-compatible store); `pkg/storage` wraps
upload/delete/URL building.

- **Quota**: each user has `storage_quota` (MB default; adjustable by admin or
  via membership bonus). `checkQuota` in the storage service and
  `uploadImage` in the account service both enforce the *effective* quota
  (base + active member bonus). Exceeding it returns `413 storage quota
  exceeded`; avatar/banner uploads count toward the same quota.
- **Previews**: image uploads generate a 1440px JPEG (quality 82) preview
  alongside the 512px thumbnail, stored as `<key>.preview.jpg` and exposed as
  `attachments.preview_url` (migration v23). Clients render the preview for
  quick viewing and fetch the original only on demand. Previews are deleted
  together with the attachment (manual delete, chunk-assembly dedupe discard,
  burn-after-view sweeper).
- **Chunked uploads**: large files go through `POST /attachments/chunk/init`,
  per-chunk `POST .../chunk/:upload_id/:index`, and
  `POST .../chunk/:upload_id/complete`.
- **Usage & management**: `GET /storage/usage` reports used bytes; attachments
  are listed via `GET /attachments` and fetched/deleted via
  `GET|DELETE /attachments/:id`.

## Chat

Chat uses a **consent-request** model: starting a private chat or inviting to
a group creates a request the target must accept or decline.

- **Private chats**: `POST /conversations/start` (sends a request),
  `PUT /conversations/:id/note` (private remark).
- **Groups**: `POST /conversations` creates a group; `POST /conversations/:id/invite`
  sends invites; membership management covers owner/admin/member roles, mute,
  titles, `group_nickname` and `leave` / `remove` / `join`.
- **Settings**: `PUT /conversations/:id/settings|title|avatar` and
  `POST/DELETE .../mute-all`.
- **Messages**: `GET|POST /conversations/:id/messages` (backward pagination via
  `before`), edit (`PUT`), soft-delete (`DELETE`), quote-replies
  (`reply_to_id`), `@mentions` (including `@everyone`), message forwarding
  (`POST .../messages/forward`) and read-state (`POST .../read`).
- **Group extras** (`services/chat/internal/handler/group_features.go`):
  - **Announcements**: `GET|POST /conversations/:id/announcements`,
    `DELETE .../announcements/:announcement_id` — member reads, manager
    writes.
  - **Todos**: `GET|POST /conversations/:id/todos`, `PUT .../todos/:todo_id`
    (`{"done": bool}`, any member checks items off), `DELETE .../todos/:todo_id`.
  - **Files**: `GET /conversations/:id/files` surfaces the attachments
    already shared into the conversation (joined from message_attachments),
    newest first — no duplicate storage.
- **QR login**: the signing-in device (anonymous) creates a five-minute
  handshake via `POST /auth/qr`, polls `GET /auth/qr/:code` until it returns
  `confirmed` with tokens; an already-signed-in device approves through
  `POST /auth/qr/:code/approve`. Signed-in devices never display a login QR.
- **E2E encryption**: conversations may be `encrypted`; group keys are
  distributed as per-member envelopes (`GET|POST /conversations/:id/e2ee-keys`).
  Uncrypted system messages carry kinds like `system.join` / `system.leave`
  for localized rendering.
- **Realtime**: the chat service emits events pushed through the push service;
  the WebSocket endpoint is `GET /api/v1/ws`.

## Realtime Push

The push service (`services/push`) holds WebSocket connections (authed via
`GET /api/v1/ws`) and fans out chat events, typing indicators and notification
messages to connected clients. It is the single bidirectional channel; all
other traffic is plain REST.

## API Gateway

The gateway (`services/gateway/cmd/main.go`) is the only public entry point.
Every internal route must be registered in its explicit route table with an
auth level (`authPublic` / `authRequired` / `authPermission` + permission key).

- **Rate limiting**: a per-IP token bucket (120 req/min, burst 240) runs
  ahead of auth and covers every proxied route; sustained flooding is
  rejected with `429`. `/api/v1/health` is exempt.
- **503 on dead backends**: when a proxied backend is unreachable
  (connection refused, timeout, DNS), the proxy's `ErrorHandler` answers
  `503 {"error":"service temporarily unavailable"}` and logs the failure —
  a down service no longer masquerades as a missing route (404).
- **Unmatched routes** still return a real `404`, so client routing bugs and
  backend outages are distinguishable at a glance.

## Moderation Reports

Any signed-in user can flag a post, a chat message or another user for review.
Details in [reports.md](reports.md); code:
`services/account/internal/handler/report.go` + `pkg/repository/report.go`.

- **File**: `POST /reports` with `target_type` (`post`/`message`/`user`),
  `target_id` and a required `reason` (1–500 chars). The target must exist
  (**404** otherwise) and reporting yourself is rejected (**400**).
- **Read back**: `GET /reports/mine` lists the caller's own reports.
- **One open report per target**: a partial unique index admits a single
  `pending` report per reporter+target (**409** on a duplicate); resolving it
  frees the slot.
- **Rate limit**: at most 20 reports per user per hour (**429**).
- **Review**: operators work the queue in the admin panel (`/reports`,
  capability `reports.manage`) and mark each report `reviewed` or `dismissed`
  with an optional note. Resolving is bookkeeping and does not itself delete
  content or punish anyone — see [punishments.md](punishments.md).

## Bots (automated accounts)

An owner-facing way to run an automated account without a password: create it
once, keep the static `ofb_` token and drive the ordinary API with it.

- **Model**: a bot is an ordinary `users` row with `is_bot` (drives the robot
  badge) and `bot_owner_id` (cascades its deletion with the owner). Its
  password hash is random garbage, so it can never log in interactively
  (`pkg/database/migration.go`, v17 `bots`).
- **API**: `POST /bots` (create, returns the plaintext token exactly once),
  `GET /bots`, `POST /bots/:id/regenerate`, `DELETE /bots/:id` — all with the
  **owner's** credentials; at most 10 bots per owner, and a bot may not create
  bots.
- **Token**: `ofb_` + 32 random bytes as hex; only the SHA-256 hash is stored
  (`bot_tokens`, one row per bot). The gateway recognises the prefix, resolves
  it to the bot's own user id and forwards that as `X-User-ID`, so no service
  needs bot-specific handling. Regenerating revokes the old token immediately;
  a banned bot fails resolution and gets `401`.
- **Permissions**: a new bot joins the default "everyone" group, so it holds
  every non-administrative permission — ordinary use, no authority over other
  accounts or server-wide state.
- **Client**: managed from the account page ("我的机器人"), which shows the
  token once and warns that it cannot be recovered.

The full developer guide — authentication, the callable API surface, realtime
events, the E2EE caveat and the limits — is [bots.md](bots.md).

## Admin Panel

The Flask admin panel (`admin/`) is the operational console:

- **Users**: list/search, edit role, verify users (writes `is_verified`,
  `verified_by`, `verified_note`; writing verification detail now implies
  `is_verified = true` so the badge renders), reset passwords, adjust exp,
  grant membership.
- **Groups & permissions**: manage groups, group memberships and permission
  grants; also manages verifier admins.
- **Wallet**: adjusts any user's balance through the same transactional
  `wallets` tables.

The panel shares the PostgreSQL database with the services and runs on its own
port (default `5001`).

## Database & Migrations

- Schema is created and upgraded by **versioned migrations** in
  `pkg/database/migration.go`, applied on boot inside a transaction protected
  by an advisory lock (see commit `89c0c2c` for the initial versioned
  framework).
- New feature columns (e.g. membership, name styling) are additive
  `ADD COLUMN IF NOT EXISTS` steps, so the server is safe to roll forward
  against older databases.

## Known Operation Notes

- New permission/gateway routes must be added in BOTH the internal service
  router and the gateway route table (`services/gateway/cmd/main.go`);
  forgetting the gateway half silently yields `404` for the route (this is
  exactly what happened with the payment-PIN routes, fixed in `3f03453`).
- Internal services bind to `127.0.0.1` and trust `X-User-ID`; they must not
  be exposed publicly.
- Camp/group creation quotas and the app-announcements admin permission are
  seeded by migration v24 + `seedPermissions()`; grants are managed per group.