# Checks (Red Packets)

## Overview

A **check** (支票) is a divisible amount of money escrowed from the creator's
wallet and claimable by other users — similar to a red packet:

- Creating a check debits the full amount from the wallet immediately
  (`check_send` transaction). Money sits in escrow, not in any user's balance.
- Each check carries `total` (cents), `shares` (how many users may claim),
  `mode` (`random` | `average`) and `expires_at`.
- **random** splits WeChat-style: every claim draws a random share bounded by
  twice the remaining average; the final claim takes what is left.
- **average** gives everyone an equal cut; the last claimer absorbs rounding.
- One claim per user per check (`UNIQUE (check_id, user_id)`).
- When every share is claimed the check becomes `settled`. When it expires,
  the unclaimed remainder is refunded to the creator (`check_refund`).
- Expiry refunds run lazily on read/claim **and** via a background sweeper in
  the account service (every 30 minutes), so money always comes back.

Checks can be attached to a post or sent as a chat message.

Limits: total 0.01–10000 coins, 1–500 shares, lifetime 1 hour – 30 days.
Creation requires the payment PIN (same budget as transfers).

## Endpoints

### `POST /api/v1/checks`

Request body:

```json
{
  "amount": 10.5,
  "shares": 3,
  "mode": "random",
  "expires_in_hours": 24,
  "pin": "123456"
}
```

Response `201`:

```json
{ "check": { "id": 1, "creator_id": 7, "total": 10.5, "shares": 3,
             "mode": "random", "status": "active", "expires_at": "..." } }
```

### `GET /api/v1/checks/:id`

Returns the check plus denormalized display fields: `claims` (user name/avatar
+ amount each), `claimed_total`, and viewer-specific `claimed_by_me` /
`my_amount`.

### `POST /api/v1/checks/:id/claim`

Pays the caller one share. Response `200` `{ "claim": { ... } }`.
Errors: `404` unknown id, `409` already fully claimed / refunded / expired, or
already claimed by this user.

## Attaching to posts

`POST /api/v1/posts` accepts an optional `check_id`. The check must be active,
unexpired, owned by the caller and not yet attached to another post. The post
payload embeds the current state as `"check": {...}` (without claims).

Deleting a post does **not** cancel its check: the money stays claimable until
expiry and unclaimed shares still refund to the creator.

## Sending as a chat message

`POST /api/v1/conversations/:id/messages` accepts an optional `check_id`; such
messages get `kind = "check"` with `check_id` set and empty content. The sender
must own the check and the check must be active/unexpired. Clients render a
claim card and use the endpoints above to fetch details and claim.

> Encrypted conversations: a check message carries no plaintext content, so it
> works the same in E2EE chats.

## Schema

```sql
checks(id, creator_id → users, total, shares, mode, status, post_id → posts NULL,
       expires_at, refunded_at, created_at)
check_claims(id, check_id → checks, user_id → users, amount, created_at,
       UNIQUE(check_id, user_id))
messages.check_id → checks NULL
```
