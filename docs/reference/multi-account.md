# OAuth Multi-Account Login

One OAuth (OIDC) identity can be bound to **several** OpenField accounts. After
an identity is linked to more than one account, logging in with that identity
shows the account picker ("您想要登录哪个账号"), lets the user choose which
account to sign in as, or add a new account — up to a per-identity quota.

## Model

A single OAuth identity is `(oauth2_provider, oauth2_id)`. The `users` table
stores the binding on each user row (`oauth2_provider`, `oauth2_id`,
`oauth2_username`); there is **no unique index** on those columns, so several
users may legitimately share one identity. Each individual account can still be
bound to only one identity (the settings page hides "绑定 OAuth" once an
account has a binding).

## Quota

`auth.MaxOAuth2Accounts = 5` (`services/account/internal/auth/user.go`) caps
how many accounts one identity may be bound to. Enforced in two places, so a
stale client can never exceed it:

- `POST /auth/oidc/pick/create` (409 `account quota reached`)
- `POST /auth/oidc/bind` (bind result page reason `quota`)

The pick info endpoint (`GET /auth/oidc/pick`) exposes `max_accounts` and the
`accounts` list; the client shows the "添加新账号" button only when the count
is below the quota.

## Login flow (multi-account)

The OAuth authorization code is **single-use**: after the callback exchanges it
with the provider it cannot be re-exchanged later for a second account choice.
So when a login identity is already bound to one or more accounts, the callback
does **not** sign anyone in. Instead:

1. `GET|POST /auth/oidc/callback` exchanges the code and finds the bound
   accounts via `Manager.ResolveIdentity` (`auth/manager.go`).
2. If zero accounts are bound → first sign-in: provision a new account
   (`needs_registration = true`), as before.
3. If one or more accounts are bound → issue a **pick ticket** (table
   `oauth2_picks`, TTL 10 minutes, single-use, created by
   `repository.IssueOAuth2Pick`) holding the identity payload, then redirect:
   - web flow → `web_redirect_url?pick=<ticket>`
   - app flow → `app_redirect_url?pick=<ticket>` (HTML deep-link page for
     `openfield://`)
   - no redirect configured → JSON `{ "pick": "<ticket>" }`
4. The client shows the account picker:
   - `GET /auth/oidc/pick?ticket=` lists the identity + bound accounts
     (does **not** consume the ticket).
   - `POST /auth/oidc/pick/select` `{ticket, user_id}` consumes the ticket,
     verifies the account is bound to the identity, runs the same
     banned/deleted checks as login, and returns the login token payload.
   - `POST /auth/oidc/pick/create` `{ticket}` consumes the ticket, enforces the
     quota, provisions a new account bound to the identity, and returns the
     login token payload.

All three endpoints are `authPublic` (the ticket is the credential); the
gateway route table in `services/gateway/cmd/main.go` must stay in sync with
the account router (`services/account/internal/handler/router.go`).

## Binding

`POST /auth/oidc/bind` (authenticated) links the identity that just authorized
to the signed-in account:

- account already bound to this identity → idempotent success;
- account already bound to a *different* identity → `ErrOAuth2AlreadyBound`
  (result page reason `taken`);
- identity already bound to `MaxOAuth2Accounts` accounts →
  `ErrOAuth2QuotaExceeded` (result page reason `quota`).

## Capabilities

`auth.oidc_multi_account: true` is advertised by the server
(`services/account/internal/handler/capabilities.go`) and mirrored in the
client's `ClientCapabilities.supported`
(`openfield/lib/data/models/client_capabilities.dart`).

## Housekeeping

`startAuthDataSweeper` (`services/account/cmd/main.go`) purges expired pick
tickets hourly via `repository.PurgeExpiredOAuth2Picks`.
