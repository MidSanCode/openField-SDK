# User Punishments

OpenField moderation actions are recorded as permanent history records in the
database. Every action has a type, an optional reason, the operator and a
timestamp, so re-runs are fully auditable.

## Punishment types

| Type       | Meaning                        | Side effect                                    |
|------------|--------------------------------|------------------------------------------------|
| `warning`  | 警告 (warning)                 | record only                                    |
| `demerit`  | 记过 (demerit)                 | record only                                    |
| `revoke`   | 剥夺权限 (revoke permission)   | writes `user_permission_bans`                  |
| `temp_ban` | 暂时封禁 (temporary ban)       | `users.status = 'banned'` + `banned_until`     |
| `ban`      | 永久封禁 (permanent ban)       | `users.status = 'banned'`, `banned_until = NULL` |
| `unban`    | 解除封禁 (unban)               | `users.status = 'active'`, clears all `user_permission_bans` |
| `restore`  | 恢复权限 (restore permission)  | deletes the matching `user_permission_bans` row |

Records live in the `user_punishments` table:

- `operator_id` — admin or system user; shown as the operator.
- `permission_key` — relevant for `revoke` / `restore`.
- `expires_at` — for `temp_ban`, the auto-unban deadline.
- `reason` — free text.

`user_permission_bans` holds the *currently effective* removed permissions.
It is the table `GetEffectivePermissions` excludes when resolving an account's
permissions, so a revoked key is gone everywhere until restored.

## API

### Apply a punishment

`POST /api/v1/users/:id/punishments` (admin, requires `user.punish`)

Body (JSON):

| Field             | Type   | Required | Meaning                                         |
|-------------------|--------|----------|-------------------------------------------------|
| `type`            | string | yes      | one of the types above                          |
| `reason`          | string | no       | reason text                                     |
| `permission_key`  | string | req. for `revoke`/`restore` | permission key to remove/restore |
| `duration_minutes`| int    | req. for `temp_ban` | ban duration in minutes            |

The record insert and its side effect run inside a database transaction.

### List a user's punishments

`GET /api/v1/users/:id/punishments` (admin, requires `user.punish`)

Returns the full history (`user_punishments`) plus the currently effective
permission bans.

## Auth integration

`users.status` / `users.banned_until` gate login:

- Login, OIDC callback and refresh-token flows reject users whose status is
  `banned`.
- For `temp_ban`, the ban is lifted automatically once `banned_until` is in
  the past (the banned check compares against `NOW()`).
- An active token does not stop working on ban (the ban targets new logins);
  revoking a session token is out of scope for this feature.