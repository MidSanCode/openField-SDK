# OpenField Server Documentation

## Guides

- [Features](features.md) — feature guide for the whole server (auth,
  membership, wallet, tasks, chat, storage, admin).
- [Daily tasks](daily-tasks.md) — per-day activity milestones (posts 1/5/10/20,
  chat 25/50/100) and how their progress and cycle keys work.
- [Architecture](architecture.md) — service layout, ports, routes and the
  permission model.
- [API](API.md) — full REST API reference.
- [Wallet](wallet.md) — wallet, transfers and currency model.
- [Token refresh](token-refresh.md) — refresh-token rotation flow.
- [Storage buckets](storage-buckets.md) — physical multi-bucket object storage.
- [Punishments](punishments.md) — moderation actions and ban history.
- [Moderation reports](reports.md) — the 举报 queue for posts, chat messages and
  users.
- [Bots](bots.md) — bot development guide: creating automated accounts,
  authenticating with an `ofb_` token, the permission model, the callable API
  surface, realtime events and the limits that apply.

## Configuration

The server uses YAML configuration files located in the `config/` directory.

### Configuration Files

- `config/config.example.yaml` - Example configuration (committed to git)
- `config/config.local.yaml` - Local configuration (git-ignored, contains your actual settings)

### Setup

1. Copy the example config:
   ```bash
   cp config/config.example.yaml config/config.local.yaml
   ```

2. Edit `config/config.local.yaml` with your actual values:
   ```yaml
   database:
     user: "postgres"
     password: "your_password"
   oidc:
     client_id: "your_client_id"
     client_secret: "your_client_secret"
   jwt:
     secret_key: "your-secret-key"
   ```

### Environment Variables

You can override any config value using environment variables:

| Variable | Description | Default |
|----------|-------------|---------|
| `OPENFIELD_CONFIG` | Path to config file | `config/config.local.yaml` |
| `SERVER_PORT` | Server port | `8080` |
| `ALLOW_ALL_ORIGINS` | Set `true` to allow API calls from any browser origin | - |
| `ALLOWED_ORIGINS` | CSV of allowed browser origins (e.g. `https://openfield.eu.cc,http://localhost:8090`). Local `localhost`/`127.0.0.1` origins are always allowed regardless of this setting | - |
| `DATABASE_URL` | PostgreSQL connection URL | - |
| `REDIS_ADDR` | Redis address | `localhost:6379` |
| `REDIS_PASSWORD` | Redis password | - |
| `OIDC_ISSUER_URL` | OIDC issuer URL | `https://auth.yun/oidc` |
| `OIDC_CLIENT_ID` | OIDC client ID | - |
| `OIDC_CLIENT_SECRET` | OIDC client secret | - |
| `OIDC_REDIRECT_URL` | OIDC redirect URL | `http://localhost:8080/api/v1/auth/oidc/callback` |
| `OIDC_APP_REDIRECT_URL` | Callback URL after login: the web app URL for browser logins (e.g. `https://openfield.eu.cc/`), or a custom-scheme deep link for desktop/mobile (e.g. `openfield://oauth/callback`) | empty (JSON response) |
| `JWT_SECRET_KEY` | JWT signing secret | - |
| `STORAGE_ENDPOINT` | S3-compatible endpoint (AWS S3, MinIO, RustFS) | `localhost:9000` |
| `STORAGE_ACCESS_KEY` | S3 access key | `minioadmin` |
| `STORAGE_SECRET_KEY` | S3 secret key | `minioadmin` |
| `STORAGE_BUCKET` | S3 bucket name | `openfield` |
| `STORAGE_REGION` | S3 region (required for AWS S3) | empty |
Stored objects are served from this prefix.|

## Features

The full feature guide lives in [features.md](features.md). Highlights:

- Authentication via OIDC plus password login for local accounts.
- JWT access + rotating refresh tokens, permission-checked gateway routes.
- Wallet (integer cents), transfers, payment PIN and membership tiers.
- Experience, levels, daily sign-in calendar and exp history.
- Membership name styling (color / gradient / animated) and storage bonus.
- Posts, replies, reactions, favorites and scoped visibility.
- RustFS-backed attachments with per-user storage quotas.
- Consent-based chat with E2E encryption, groups and mentions.
- A Flask admin panel managing users, groups, permissions and wallets.

## Database

The server uses PostgreSQL. Set up your database:

```sql
CREATE DATABASE openfield;
```

The server will automatically create tables on first run via migrations.

## Redis

Redis is optional. To enable:

1. Set `redis.enabled: true` in config
2. Ensure Redis is running at the configured address

Redis is used for:
- Caching frequently accessed data
- Session storage
- Rate limiting

## Running

```bash
# Install dependencies
go mod download

# Run with default config
go run ./cmd/main.go

# Run with custom config path
OPENFIELD_CONFIG=/path/to/config.yaml go run ./cmd/main.go
```

## API Documentation

See [API.md](API.md) for full API documentation.
