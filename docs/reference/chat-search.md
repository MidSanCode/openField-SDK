# Chat History Search

## Overview

Searchable chat history per conversation. All criteria are optional and
combined with AND, so a client can filter by any combination of:

- **Content keyword** (`q`): case-insensitive substring match on the message body.
- **Sender** (`sender_id`): only messages authored by this user.
- **Time range** (`from` / `to`): inclusive bounds on `created_at`.
- **Attachments** (`has_attachment`): only messages carrying at least one file.
- **File name** (`file_name`): case-insensitive substring match on attachment
  original names (implies `has_attachment`).

Only regular text messages are considered; system notices
(`system.join`, `system.leave`, ...) and soft-deleted messages never match.

> Note: in end-to-end-encrypted conversations the server stores ciphertext,
> so `q` / `file_name` cannot match plaintext there — clients should search
> their locally decrypted cache for those conversations instead.

## Endpoint

### `GET /api/v1/conversations/:id/messages/search`

Auth: gateway auth (same as all chat endpoints). The caller must be an active
member of the conversation; non-members may search public groups (same
visibility rule as message listing).

Query parameters (all optional):

| Parameter        | Type               | Description                                        |
|------------------|--------------------|----------------------------------------------------|
| `q`              | string             | Content keyword (substring, case-insensitive).     |
| `sender_id`      | int64              | Filter by sender user ID.                          |
| `from`           | RFC3339 / unix s   | Only messages created at or after this time.       |
| `to`             | RFC3339 / unix s   | Only messages created at or before this time.      |
| `has_attachment` | true/false/1/0     | When true, only messages with attachments match.   |
| `file_name`      | string             | Match against attachment original names.           |
| `limit`          | int (default 50)   | Max results (clamped to 1..100).                   |

Response `200`:

```json
{
  "messages": [
    {
      "id": 123,
      "conversation_id": 7,
      "sender_id": 42,
      "kind": "text",
      "content": "quarterly report",
      "created_at": "2026-08-01T09:30:00Z",
      "sender_name": "alice",
      "attachments": [ ... ]
    }
  ]
}
```

Results are ordered newest first (unlike chronological listing) because the
common flow is scanning recent matches backwards. Messages carry the same
shape as `GET .../messages`, including sender styling fields, reply previews
and populated attachments.

Errors: `400` invalid conversation id / sender id / time format / boolean,
`403` not a member (and conversation is not a public group), `500` internal.
