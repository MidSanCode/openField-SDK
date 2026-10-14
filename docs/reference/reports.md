# Moderation Reports

OpenField lets any signed-in user flag content for review. A report names one
target — a post, a chat message or another user — and carries the reporter's
reason. Reports are reviewed by an operator in the Flask admin panel; the
server only files and lists them.

Code: `services/account/internal/handler/report.go` (endpoints) +
`pkg/repository/report.go` (storage and validation) + the `reports` table
(migration v32).

## The report record

| Column              | Meaning                                                        |
|---------------------|----------------------------------------------------------------|
| `reporter_id`       | who filed it (FK `users`, `ON DELETE CASCADE`)                 |
| `target_type`       | `post` / `message` / `user` (CHECK constraint)                 |
| `target_id`         | id of the reported row                                         |
| `reason`            | free text, required, max 500 runes                             |
| `status`            | `pending` / `reviewed` / `dismissed` (CHECK constraint)        |
| `reviewer_id`       | panel account id that handled it, `0` while pending            |
| `reviewer_username` | denormalized operator name, so history survives account changes |
| `review_note`       | optional note written by the operator                          |
| `created_at`        | when the report was filed                                      |
| `reviewed_at`       | when it was resolved, `NULL` while pending                     |

`(target_type, target_id)` is deliberately **not** a foreign key: the three
targets live in three different tables, and a target that is deleted after the
fact must not erase the record that it was reported. The id is validated at
write time instead, and the admin panel renders a deleted target as such.

## API

### File a report

`POST /api/v1/reports` (authenticated)

| Field         | Type   | Required | Meaning                          |
|---------------|--------|----------|----------------------------------|
| `target_type` | string | yes      | `post`, `message` or `user`      |
| `target_id`   | int    | yes      | id of the reported content       |
| `reason`      | string | yes      | 1–500 characters                 |

Answers `201` with the created report.

### List my reports

`GET /api/v1/reports/mine` (authenticated, optional `?limit=`, default 50,
capped at 100) — the caller's own reports, newest first, with the reporter name
resolved.

## Guards

| Condition                                   | Answer |
|---------------------------------------------|--------|
| unknown `target_type`                       | `400`  |
| `target_id` ≤ 0, empty or oversized reason  | `400`  |
| report yourself (`target_type=user` and `target_id` == caller) | `400` |
| target row does not exist                   | `404`  |
| caller already has a pending report on the same target | `409`  |
| caller filed more than 20 reports in the last hour | `429`  |

The `409` comes from `idx_reports_one_open`, a partial unique index on
`(reporter_id, target_type, target_id) WHERE status = 'pending'`; the unique
violation is translated into a sentinel rather than surfacing as a 500. Resolving
the report frees the slot, so the same target can be reported again afterwards.

## Reviewing

Reviewing happens in the admin panel (`/reports`, capability `reports.manage`):

- The queue lists pending reports by default and can be filtered by status and
  by target kind.
- Resolving writes `status`, `reviewer_id`, `reviewer_username`, `review_note`
  and `reviewed_at`, and records an `admin_audit_log` entry.
- A resolved report can be reopened (back to `pending`, review fields cleared),
  unless the same reporter has since filed a newer open report on the same
  target — that would collide with the partial unique index, so it is refused
  with an explanation.

Resolving a report is a bookkeeping action: it does not itself punish anyone or
remove content. Operators act on the target separately (delete the post, punish
the user) — see [punishments.md](punishments.md).

## Deletion semantics

Deleting a user removes their reports as a consequence of
`reporter_id ... ON DELETE CASCADE`. The reports other people filed *against*
that user are kept: their `target_id` simply stops resolving, which is why the
panel shows a missing target explicitly instead of a blank cell.
