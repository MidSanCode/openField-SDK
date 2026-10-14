# Service Health Checks

## Overview

Every backend service exposes a lightweight liveness/readiness probe, and the
gateway aggregates them into one public endpoint so clients (and monitoring)
can see at a glance which parts of the platform are currently working.

## Per-service probe

### `GET /healthz` (internal port, unauthenticated)

Mounted by account, chat, posts, push and storage. Returns HTTP 200 with a
JSON body (the body carries the state, so simple TCP/HTTP probes can also use
the status code of the aggregate endpoint):

```json
{
  "status": "up",
  "details": { "database": "up" },
  "checked_at": "2026-08-24T12:00:00Z"
}
```

- `status`: `up` | `down` | `degraded`.
- `details.database`: result of a `PingContext` against the shared PostgreSQL
  pool within a 3s timeout (`n/a` for services without a pooled connection,
  e.g. push). A failed ping makes the whole report `down`.
- The storage service adds `details.storage`: `up` when object storage is
  configured and enabled, otherwise `disabled`, which degrades the overall
  status to `degraded`.

## Aggregate endpoint

### `GET /api/v1/health` (public)

Served by the gateway itself; fans out concurrently to every configured
backend's `/healthz` and measures per-service latency.

Response `200` when everything is healthy, `503` otherwise — the body is
always parseable:

```json
{
  "all_healthy": false,
  "checked_at": "2026-08-24T12:00:00Z",
  "services": {
    "gateway": { "status": "up", "latency_ms": 0, "details": { "database": "up" } },
    "account": { "status": "up", "latency_ms": 3, "details": { "database": "up" } },
    "chat":    { "status": "up", "latency_ms": 2, "details": { "database": "up" } },
    "posts":   { "status": "down", "latency_ms": 3001, "error": "context deadline exceeded" }
  }
}
```

Each entry carries `status`, `latency_ms` and either upstream `details` or an
`error`. Unconfigured service URLs report `unknown`.

The endpoint requires no authentication and is safe to poll (each call costs
one DB ping plus five local HTTP GETs); keep polling intervals modest
(≥ 30 seconds).
