//! Documentation conflicts found while building this SDK, and how the source
//! resolved them.
//!
//! The bot guide (`bots.md`) was written by reading the server source; the platform
//! reference (`API.md`, `features.md` and the feature notes) was written separately.
//! **They disagree in a number of places.** This module records every conflict
//! together with what the code in `midSanCode/openField-server` actually does, so a
//! reader can see which document to trust for a given question and which SDK
//! behaviour was chosen because of it.
//!
//! Where the source was authoritative, the SDK follows the source and still accepts
//! the other spelling so a bot does not break against an older or diverged build.

/// Which document the source agreed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The server source matches `bots.md`.
    BotsMd,
    /// The server source matches `API.md` / `features.md`.
    ApiMd,
    /// The source matches neither document; the SDK follows the code.
    Neither,
    /// The documents describe different things and both are true.
    Both,
}

impl Verdict {
    /// Short label for reports.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BotsMd => "bots.md",
            Self::ApiMd => "API.md",
            Self::Neither => "neither",
            Self::Both => "both",
        }
    }
}

/// One place where two documents disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divergence {
    /// Short topic, e.g. `attachment field names`.
    pub topic: &'static str,
    /// What `bots.md` says, or "not specified".
    pub bots_md: &'static str,
    /// What `API.md` / `features.md` say, or "not specified".
    pub api_md: &'static str,
    /// What the server source actually does.
    pub source: &'static str,
    /// Which document the source agreed with.
    pub verdict: Verdict,
    /// What this SDK does about it.
    pub decision: &'static str,
}

/// Every documented conflict known to this SDK version.
pub const KNOWN_DIVERGENCES: &[Divergence] = &[
    Divergence {
        topic: "attachment field names",
        bots_md: "not specified",
        api_md: "`original_name`, `mime_type`, `size_bytes`, `thumb_url`, `preview_url`, `bucket`",
        source: "`pkg/model/model.go` `Attachment` uses exactly those tags, plus `visibility` and `burn_at`",
        verdict: Verdict::ApiMd,
        decision: "the source names are used verbatim; `file_name()`/`content_type()`/`size()` accessors are provided for readers who expect the other spelling",
    },
    Divergence {
        topic: "message list ordering",
        bots_md: "§7.3: the array is ascending (old to new), so the newest is `messages[-1]`",
        api_md: "the array is newest-to-oldest",
        source: "`pkg/repository/message.go` `ListByConversation` ends `ORDER BY m.created_at DESC, m.id DESC` — newest first. The Go doc comment on that function still says \"(newest last)\", so the comment and the SQL disagree too",
        verdict: Verdict::ApiMd,
        decision: "`MessagePage::newest()` selects by maximum id rather than by position, so it is correct under either ordering; `ascending()` returns processing order",
    },
    Divergence {
        topic: "conversation discriminator and label",
        bots_md: "`kind` / `name`",
        api_md: "`type` / `title`",
        source: "`model.Conversation` tags are `type` and `title`",
        verdict: Verdict::ApiMd,
        decision: "`type`/`title` are primary and emitted; `kind`/`name` accepted as aliases, with a `name()` accessor",
    },
    Divergence {
        topic: "conversation unread counter",
        bots_md: "not specified",
        api_md: "`unread_count`",
        source: "`model.Conversation` tags the field `unread`",
        verdict: Verdict::Neither,
        decision: "the field is `unread`; `unread_count` is accepted as an alias",
    },
    Divergence {
        topic: "create group request body",
        bots_md: "not specified",
        api_md: "`{\"title\": \"Family\"}`",
        source: "`conversation.go` binds `Title string json:\"title\" binding:\"required\"` — the title is mandatory",
        verdict: Verdict::ApiMd,
        decision: "`NewConversation` sends `title` and refuses to build a request without one",
    },
    Divergence {
        topic: "invite request body",
        bots_md: "`{\"user_ids\": [...]}`",
        api_md: "`{\"user_id\": 3, \"message\": \"...\"}` — one target, and it files a consent request",
        source: "`conversation.go` binds `user_id` (required), `message` and `encrypted`; there is no bulk form",
        verdict: Verdict::ApiMd,
        decision: "single-target `invite(user_id, message)`; `invite_many` loops so callers still get the bulk convenience",
    },
    Divergence {
        topic: "mark read request body",
        bots_md: "POST with no body",
        api_md: "`{\"last_message_id\": 42}`, answers 204",
        source: "`conversation.go` binds `LastMessageID int64 json:\"last_message_id\"`, and there is also a per-message `POST .../messages/:message_id/read`",
        verdict: Verdict::ApiMd,
        decision: "`mark_read(conversation_id, last_message_id)` sends the field; omitting it sends 0, which the server treats as \"up to nothing\"",
    },
    Divergence {
        topic: "avatar and banner multipart field name",
        bots_md: "not specified",
        api_md: "`file`",
        source: "`user.go` calls `c.Request.FormFile(\"file\")` for both avatar and banner",
        verdict: Verdict::ApiMd,
        decision: "`file` is the default for avatar and banner uploads; the field name stays overridable",
    },
    Divergence {
        topic: "camp field names",
        bots_md: "implies `owner_id`, `parent_id`, `members_can_post`",
        api_md: "`creator_id`, `parent_camp_id`, `is_visible`, `direct_join`, `member_post`, `member_pin`, `my_role`",
        source: "`model.Camp` uses the `features.md` names and also carries `creator_name` and `announcement`",
        verdict: Verdict::ApiMd,
        decision: "the source names are used; `is_hidden()` is derived as `!is_visible`, and `members_can_post()` reads `member_post`",
    },
    Divergence {
        topic: "post aggregate field names",
        bots_md: "`replies_count`, `reactions_count`",
        api_md: "`reply_count`, `reactions`, `favorite_count`, `pinned`, `camp_pinned`",
        source: "`model.Post` uses `reply_count`, a `reactions` map, `favorite_count`, `my_reaction`, `pinned` and `camp_pinned`; there is no `reactions_count`",
        verdict: Verdict::ApiMd,
        decision: "the source names are used; the `_s_count` spellings survive only in `extra`",
    },
    Divergence {
        topic: "post attachment and tag output",
        bots_md: "§7.1 lists `attachment_ids` and `tags` as fields",
        api_md: "not specified for the response",
        source: "`model.Post` has neither: attachments come back as populated `attachments` objects, and `tags` only ever appears in the create/update request body",
        verdict: Verdict::Neither,
        decision: "`Post` exposes `attachments`; `NewPost` still sends `attachment_ids` and `tags` so the documented request shape keeps working",
    },
    Divergence {
        topic: "message attachment output",
        bots_md: "§7.4 implies an `attachment_ids` field",
        api_md: "not specified for the response",
        source: "`model.Message` has no `attachment_ids`; it has `attachments []Attachment`. The ids are request-only",
        verdict: Verdict::Neither,
        decision: "`Message::attachment_ids()` derives the ids from `attachments`; `NewMessage` still sends `attachment_ids`",
    },
    Divergence {
        topic: "reaction request body",
        bots_md: "§6.2 shows `PUT/DELETE /posts/:id/reactions` with no body",
        api_md: "`features.md` says \"with a reaction key\"",
        source: "`post.go` `SetPostReaction` binds `reaction` as required and rejects anything outside `allowedReactions`",
        verdict: Verdict::ApiMd,
        decision: "`react(post_id, Reaction)` requires one of the seven allowed keys; `unreact` still sends no body",
    },
    Divergence {
        topic: "content length limit unit",
        bots_md: "§7.1 says content is capped at 5000 characters",
        api_md: "says the same",
        source: "`post.go` checks `len(req.Content) > 5000`, and Go's `len` counts **bytes**, not runes. The bot nickname check in `bot.go` uses `len([]rune(...))`, so the two limits are not in the same unit",
        verdict: Verdict::Neither,
        decision: "the SDK validates content by **byte** length to match the server, so a CJK post is rejected locally at the same point the server would reject it",
    },
    Divergence {
        topic: "whether a general rate limit exists",
        bots_md: "§12: the server has no general request rate limit on the request path",
        api_md: "`features.md`: the gateway applies a per-IP token bucket, 120 req/min with burst 240, answering 429",
        source: "`services/gateway/cmd/gateway_middleware.go` `newRateLimiter()` sets `perMin = 120.0`, `burst = 240`, and the middleware is installed with `r.Use(...)` before the routes are registered",
        verdict: Verdict::ApiMd,
        decision: "429 is treated as retryable and `Retry-After` is honoured; the README documents the bucket, because it binds bot traffic even though no per-bot quota exists",
    },
    Divergence {
        topic: "health endpoint and rate limiting",
        bots_md: "not specified",
        api_md: "`features.md` says `/api/v1/health` is exempt from rate limiting",
        source: "`main.go` registers the limiter with `r.Use(...)` and then `r.GET(\"/api/v1/health\", ...)`; gin applies middleware registered with `Use` to routes added afterwards, so the aggregate health route is **inside** the limiter. The `(not /healthz)` comment refers to the per-service probe, which is a different path and is not proxied",
        verdict: Verdict::Neither,
        decision: "the SDK accepts both 200 and 503 from `/api/v1/health` and does not assume the call is free; poll at health.md's suggested 30s floor or slower",
    },
    Divergence {
        topic: "profile route auth level",
        bots_md: "§6.1 marks `GET /users/:user_id` as `authPublic`",
        api_md: "marks it `authRequired`",
        source: "the gateway table entry is `authPublic`, with `GET /users/me` as `authRequired`",
        verdict: Verdict::BotsMd,
        decision: "the SDK always sends the token when it has one, which satisfies both readings",
    },
    Divergence {
        topic: "attachment listing permission",
        bots_md: "§6.5 marks `GET /attachments` as `storage.list`",
        api_md: "does not give the level",
        source: "the route table confirms `storage.list`, and `GET /attachments/:id` is `authPublic`",
        verdict: Verdict::Both,
        decision: "both rely on the default group holding `storage.*`, which `bots.md` appendix B confirms",
    },
];

/// Look up a divergence by topic.
pub fn divergence(topic: &str) -> Option<&'static Divergence> {
    KNOWN_DIVERGENCES.iter().find(|entry| entry.topic == topic)
}

/// Every divergence the source resolved in favour of `bots.md`.
pub fn resolved_for_bots_md() -> Vec<&'static Divergence> {
    by_verdict(Verdict::BotsMd)
}

/// Every divergence the source resolved in favour of `API.md` / `features.md`.
pub fn resolved_for_api_md() -> Vec<&'static Divergence> {
    by_verdict(Verdict::ApiMd)
}

/// Every divergence where neither document matched the code.
pub fn resolved_for_neither() -> Vec<&'static Divergence> {
    by_verdict(Verdict::Neither)
}

fn by_verdict(verdict: Verdict) -> Vec<&'static Divergence> {
    KNOWN_DIVERGENCES
        .iter()
        .filter(|entry| entry.verdict == verdict)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_divergence_records_a_source_finding_and_a_decision() {
        assert!(
            KNOWN_DIVERGENCES.len() >= 15,
            "the documents disagree in at least fifteen places"
        );
        for entry in KNOWN_DIVERGENCES {
            assert!(!entry.topic.is_empty(), "topic must be set");
            assert!(
                !entry.bots_md.is_empty(),
                "{}: bots_md must be set",
                entry.topic
            );
            assert!(
                !entry.api_md.is_empty(),
                "{}: api_md must be set",
                entry.topic
            );
            assert!(
                entry.source.len() > 20,
                "{}: the source finding must name what the code does",
                entry.topic
            );
            assert!(
                entry.decision.len() > 15,
                "{}: the decision must explain the resolution",
                entry.topic
            );
        }
    }

    #[test]
    fn topics_are_unique_and_lookupable() {
        for (index, entry) in KNOWN_DIVERGENCES.iter().enumerate() {
            for other in KNOWN_DIVERGENCES.iter().skip(index + 1) {
                assert_ne!(entry.topic, other.topic, "duplicate topic {}", entry.topic);
            }
            assert_eq!(
                divergence(entry.topic).map(|found| found.topic),
                Some(entry.topic)
            );
        }
        assert!(divergence("no such topic").is_none());
    }

    #[test]
    fn the_headline_conflicts_are_recorded() {
        for topic in [
            "message list ordering",
            "attachment field names",
            "conversation discriminator and label",
            "reaction request body",
            "content length limit unit",
            "whether a general rate limit exists",
        ] {
            assert!(divergence(topic).is_some(), "{topic} must be recorded");
        }
    }

    #[test]
    fn verdicts_partition_the_list() {
        let total = resolved_for_bots_md().len()
            + resolved_for_api_md().len()
            + resolved_for_neither().len()
            + by_verdict(Verdict::Both).len();
        assert_eq!(total, KNOWN_DIVERGENCES.len());
        assert!(
            !resolved_for_api_md().is_empty(),
            "the API reference turned out to be right in several places"
        );
        assert!(
            !resolved_for_neither().is_empty(),
            "some behaviour is described by neither document"
        );
    }

    #[test]
    fn the_message_ordering_verdict_matches_the_sql() {
        let entry = divergence("message list ordering").unwrap();
        assert_eq!(entry.verdict, Verdict::ApiMd);
        assert!(entry.source.contains("DESC"));
    }

    #[test]
    fn verdict_labels_are_stable() {
        assert_eq!(Verdict::BotsMd.as_str(), "bots.md");
        assert_eq!(Verdict::ApiMd.as_str(), "API.md");
        assert_eq!(Verdict::Neither.as_str(), "neither");
        assert_eq!(Verdict::Both.as_str(), "both");
    }
}
