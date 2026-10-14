# Wallet

## Overview

Every user has a wallet with a **balance** (in cents / 分, stored as `BIGINT`)
and a list of **transactions**. Money only changes hands through an admin
(recharge / deduction) — there is no peer-to-peer tipping.

- Balances are managed in `wallet_transactions` so every movement is auditable:
  each transaction records `amount`, `balance_after`, `type`, `description` and
  the `operator_id` (admin) who made the change.
- Adjustments are transactional and reject negative balances.

## Endpoints

All wallet endpoints require authentication (`authPermission`). The gateway
enforces the permission keys before forwarding.

### `GET /api/v1/wallet`

Permission: `wallet.view`

Returns the caller's wallet and recent transactions.

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

Amounts are in cents: `10000` = `100.00` (display formatting is the client's
job: `amount / 100`).

### `POST /api/v1/wallet/adjust`

Permission: `wallet.manage`

Admin-only adjustment. The caller must hold `wallet.manage`; the `user_id`
field selects the target user.

Request body:

```json
{
  "user_id": 42,
  "amount": -500,
  "type": "deduct",
  "description": "Refund reversal"
}
```

- `amount` is in cents; positive = recharge, negative = deduction.
- `type` is one of `recharge` / `deduct` (informational; consistency with
  `amount` is recommended but the transaction is stored as given).
- `description` is optional (defaults to empty string).
- Returns `409` (or `400`) when a deduction would drive the balance below zero.

## Data model

### `wallets`

| Column      | Type    | Notes                     |
|-------------|---------|---------------------------|
| user_id     | PK      | FK to `users.id`          |
| balance     | BIGINT  | in cents, `>= 0`          |
| created_at  | timestamptz |                       |
| updated_at  | timestamptz |                       |

### `wallet_transactions`

| Column        | Type    | Notes                        |
|---------------|---------|------------------------------|
| id            | PK      |                              |
| user_id       | FK      | indexed                      |
| amount        | BIGINT  | signed cents                 |
| balance_after | BIGINT  | balance after this change    |
| type          | TEXT    | `recharge` / `deduct`        |
| description   | TEXT    | optional admin note          |
| operator_id   | FK      | admin who performed it       |
| created_at    | timestamptz | indexed                |

## Permissions

| Key            | Purpose                    |
|----------------|----------------------------|
| `wallet.view`  | View own wallet + history  |
| `wallet.manage`| Adjust any user's balance  |

New permissions are seeded automatically by `seedPermissions()` in
`pkg/database/migration.go`; the default `everyone` group holds all permissions
today, so `wallet.manage` is currently available to every authenticated user
until group permissions are tightened.

## Server implementation notes

| Concern           | Location                                          |
|-------------------|---------------------------------------------------|
| Tables            | `pkg/database/migration.go`                       |
| Models            | `pkg/model/model.go` — `Wallet`, `WalletTransaction` |
| Repository        | `pkg/repository/wallet.go`                        |
| Handlers          | `services/account/internal/handler/wallet.go`     |
| Routes            | `services/account/internal/handler/router.go`, `services/gateway/cmd/main.go` |

`AdjustBalance` runs inside a transaction: it locks the wallet row with
`SELECT ... FOR UPDATE`, computes the new balance, rejects it if negative
(`repository.ErrInsufficientBalance`), writes the transaction, and updates the
balance.

## Admin panel

`admin/app.py` shows each user's balance in the users list and exposes
`POST /users/<user_id>/wallet` (admin-session guarded) which recharges or
deducts via the same `wallets` / `wallet_transactions` tables. It also runs
inside a transaction and refuses balances below zero.

## Client implementation notes

- `openfield/lib/data/models/wallet.dart` — `Wallet` / `WalletTransaction`.
- `openfield/lib/data/services/api_service.dart` — `getWallet`, `adjustWallet`.
- `openfield/lib/pages/account/wallet_page.dart` — balance card + transaction
  list, entry point on the account page.
