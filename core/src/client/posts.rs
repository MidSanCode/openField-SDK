//! Post endpoints (`bots.md` §6.2 and §7.1).
//!
//! Verified against `services/posts/internal/handler/post.go`:
//!
//! * `POST /posts` binds `content`, `visibility`, `attachment_ids`, `check_id`,
//!   `tags`, `quoted_post_id`, `camp_id` — exactly the documented request shape.
//! * Content is capped at **5000 bytes**, not characters: the handler checks
//!   `len(req.Content) > 5000` and Go's `len` counts bytes. Validation here matches
//!   that, so a CJK post is refused locally at the same point the server would.
//! * `PUT /posts/:id/reactions` requires a body of `{"reaction": "..."}` drawn from
//!   the seven keys in `allowedReactions`, despite `bots.md` showing no body.
//! * `attachment_ids` replaces the post's attachments on update.

use serde::{Deserialize, Serialize};

use crate::error::{Result, SdkError};
use crate::models::{Id, JsonObject, Post, PostTip, Reaction, Reply, ReplyPage, Visibility};

use super::{pagination, AuthenticatedClient, API_PREFIX};

/// Maximum post or reply body length **in bytes** (`post.go`).
pub const MAX_CONTENT_BYTES: usize = 5000;
/// Maximum attachments per post or message (`post.go`, `attachment.go`).
pub const MAX_ATTACHMENTS: usize = 9;

/// A post to create.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewPost {
    /// Body text, up to [`MAX_CONTENT_BYTES`] bytes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    /// Visibility, defaulting to `public` server-side when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
    /// Attachment ids from `POST /attachments`, at most [`MAX_ATTACHMENTS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_ids: Vec<Id>,
    /// Tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Post to quote/repost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quoted_post_id: Option<Id>,
    /// Camp to post into; the bot must already be a member. `0` means global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camp_id: Option<Id>,
    /// Red packet / lottery id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_id: Option<Id>,
}

impl NewPost {
    /// A post with the given body.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ..Self::default()
        }
    }

    /// A post that only quotes another one (`bots.md` §7.1 allows content *or*
    /// `quoted_post_id`).
    pub fn quote(post_id: Id) -> Self {
        Self {
            quoted_post_id: Some(post_id),
            ..Self::default()
        }
    }

    /// Builder: set the visibility.
    pub fn visibility(mut self, visibility: Visibility) -> Self {
        self.visibility = Some(visibility);
        self
    }

    /// Builder: attach one attachment id.
    pub fn attachment(mut self, attachment_id: Id) -> Self {
        self.attachment_ids.push(attachment_id);
        self
    }

    /// Builder: attach several attachment ids.
    pub fn attachments(mut self, ids: impl IntoIterator<Item = Id>) -> Self {
        self.attachment_ids.extend(ids);
        self
    }

    /// Builder: add one tag.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }

    /// Builder: post into a camp. The bot must have joined it first (`bots.md` §6.4).
    pub fn in_camp(mut self, camp_id: Id) -> Self {
        self.camp_id = Some(camp_id);
        self
    }

    /// Builder: attach a red packet / lottery.
    pub fn with_check(mut self, check_id: Id) -> Self {
        self.check_id = Some(check_id);
        self
    }

    /// Reject a post the server would reject.
    pub fn validate(&self) -> Result<()> {
        if self.content.is_empty() && self.quoted_post_id.is_none() {
            return Err(SdkError::InvalidArgument {
                message: "a post needs content or quoted_post_id (`bots.md` §7.1)".into(),
            });
        }
        if self.content.len() > MAX_CONTENT_BYTES {
            return Err(SdkError::InvalidArgument {
                message: format!(
                    "content exceeds maximum length of {MAX_CONTENT_BYTES} bytes; the server \
                     counts bytes, not characters"
                ),
            });
        }
        if self.attachment_ids.len() > MAX_ATTACHMENTS {
            return Err(SdkError::InvalidArgument {
                message: format!("too many attachments (max {MAX_ATTACHMENTS})"),
            });
        }
        Ok(())
    }
}

/// A reply to create.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewReply {
    /// Reply body, required and up to [`MAX_CONTENT_BYTES`] bytes.
    pub content: String,
    /// Parent reply, which must belong to the same post (`bots.md` §7.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Id>,
    /// Attachment ids, at most [`MAX_ATTACHMENTS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_ids: Vec<Id>,
}

impl NewReply {
    /// A plain reply.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ..Self::default()
        }
    }

    /// Builder: thread this reply under another reply on the same post.
    pub fn replying_to(mut self, parent_id: Id) -> Self {
        self.parent_id = Some(parent_id);
        self
    }

    /// Builder: attach one attachment id.
    pub fn attachment(mut self, attachment_id: Id) -> Self {
        self.attachment_ids.push(attachment_id);
        self
    }

    /// Reject a reply the server would reject.
    pub fn validate(&self) -> Result<()> {
        if self.content.is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "content cannot be empty".into(),
            });
        }
        if self.content.len() > MAX_CONTENT_BYTES {
            return Err(SdkError::InvalidArgument {
                message: format!("content exceeds maximum length of {MAX_CONTENT_BYTES} bytes"),
            });
        }
        if self.attachment_ids.len() > MAX_ATTACHMENTS {
            return Err(SdkError::InvalidArgument {
                message: format!("too many attachments (max {MAX_ATTACHMENTS})"),
            });
        }
        Ok(())
    }
}

impl AuthenticatedClient {
    /// `POST /api/v1/posts` (`posts.create`) — returns `201` plus the post itself.
    pub fn create_post(&self, post: &NewPost) -> Result<Post> {
        post.validate()?;
        let body = serde_json::to_value(post).map_err(super::json_error)?;
        self.post_json(&format!("{API_PREFIX}/posts"), &body)
    }

    /// Convenience wrapper for a plain public text post.
    pub fn publish(&self, content: &str) -> Result<Post> {
        self.create_post(&NewPost::text(content))
    }

    /// `GET /api/v1/posts` — timeline with the documented filters.
    ///
    /// Filters are `page`, `limit`, `q`, `author`, `tag`, `author_id`, `from`, `to`.
    /// Page-numbered: an empty `posts` array means the end (`bots.md` §7.3`).
    pub fn posts(&self, page: Option<i64>, limit: Option<i64>) -> Result<crate::models::PostPage> {
        self.get_json_query(&format!("{API_PREFIX}/posts"), &pagination(page, limit))
    }

    /// `GET /api/v1/posts` with arbitrary filters.
    pub fn search_posts(&self, query: &[(&str, &str)]) -> Result<crate::models::PostPage> {
        self.get_json_query(&format!("{API_PREFIX}/posts"), &super::owned(query))
    }

    /// `GET /api/v1/posts/:id` — one post. Records a view server-side.
    pub fn post(&self, post_id: Id) -> Result<Post> {
        self.get_json(&format!("{API_PREFIX}/posts/{post_id}"))
    }

    /// `PUT /api/v1/posts/:id` (`posts.edit`) — edit one of the bot's own posts.
    ///
    /// `attachment_ids` **replaces** the post's attachments rather than adding to
    /// them, so send the full list the post should end up with.
    pub fn update_post(&self, post_id: Id, post: &NewPost) -> Result<Post> {
        post.validate()?;
        let body = serde_json::to_value(post).map_err(super::json_error)?;
        self.put_json(&format!("{API_PREFIX}/posts/{post_id}"), &body)
    }

    /// `DELETE /api/v1/posts/:id` (`posts.delete`).
    pub fn delete_post(&self, post_id: Id) -> Result<JsonObject> {
        self.ok_object("DELETE", &format!("{API_PREFIX}/posts/{post_id}"))
    }

    /// `PUT /api/v1/posts/:id/pin` — pin or unpin one of the bot's own posts.
    ///
    /// The handler binds `{"pinned": true|false}`; author only.
    pub fn pin_post(&self, post_id: Id, pinned: bool) -> Result<Post> {
        let body = serde_json::json!({ "pinned": pinned });
        self.put_json(&format!("{API_PREFIX}/posts/{post_id}/pin"), &body)
    }

    /// `PUT /api/v1/camps/:id/posts/:post_id/pin` — pin a post inside its camp.
    pub fn pin_camp_post(&self, camp_id: Id, post_id: Id, pinned: bool) -> Result<JsonObject> {
        let body = serde_json::json!({ "pinned": pinned });
        let response = self.base.http.json_raw(
            "PUT",
            &format!("{API_PREFIX}/camps/{camp_id}/posts/{post_id}/pin"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// `GET /api/v1/posts/:id/replies` — replies to a post.
    pub fn replies(&self, post_id: Id, page: Option<i64>, limit: Option<i64>) -> Result<ReplyPage> {
        self.get_json_query(
            &format!("{API_PREFIX}/posts/{post_id}/replies"),
            &pagination(page, limit),
        )
    }

    /// `POST /api/v1/posts/:id/replies` (`posts.reply.create`).
    pub fn create_reply(&self, post_id: Id, reply: &NewReply) -> Result<Reply> {
        reply.validate()?;
        let body = serde_json::to_value(reply).map_err(super::json_error)?;
        self.post_json(&format!("{API_PREFIX}/posts/{post_id}/replies"), &body)
    }

    /// `PUT /api/v1/posts/:id/replies/:reply_id` (`posts.reply.edit`).
    pub fn update_reply(&self, post_id: Id, reply_id: Id, content: &str) -> Result<Reply> {
        if content.is_empty() || content.len() > MAX_CONTENT_BYTES {
            return Err(SdkError::InvalidArgument {
                message: format!("content must be 1..={MAX_CONTENT_BYTES} bytes"),
            });
        }
        let body = serde_json::json!({ "content": content });
        self.put_json(
            &format!("{API_PREFIX}/posts/{post_id}/replies/{reply_id}"),
            &body,
        )
    }

    /// `DELETE /api/v1/posts/:id/replies/:reply_id` (`posts.reply.delete`).
    pub fn delete_reply(&self, post_id: Id, reply_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "DELETE",
            &format!("{API_PREFIX}/posts/{post_id}/replies/{reply_id}"),
        )
    }

    /// `PUT /api/v1/posts/:id/reactions` (`posts.react`) — set a reaction.
    ///
    /// The server requires one of the seven keys in [`Reaction`]; anything else is a
    /// `400 unknown reaction`. `bots.md` §6.2 shows this call without a body, which the
    /// handler contradicts.
    pub fn react(&self, post_id: Id, reaction: Reaction) -> Result<Post> {
        let body = serde_json::json!({ "reaction": reaction.as_str() });
        self.put_json(&format!("{API_PREFIX}/posts/{post_id}/reactions"), &body)
    }

    /// `PUT /api/v1/posts/:id/reactions` with a raw key.
    ///
    /// Returns [`crate::ErrorKind::InvalidArgument`] for a key outside
    /// [`Reaction::ALL`]; use this only if a future build adds a reaction.
    pub fn react_with(&self, post_id: Id, reaction: &str) -> Result<Post> {
        let body = serde_json::json!({ "reaction": reaction });
        let response = self.base.http.json_raw(
            "PUT",
            &format!("{API_PREFIX}/posts/{post_id}/reactions"),
            Some(&body),
            &[],
        )?;
        response.decode()
    }

    /// `DELETE /api/v1/posts/:id/reactions` — clear the bot's reaction.
    pub fn unreact(&self, post_id: Id) -> Result<JsonObject> {
        self.ok_object("DELETE", &format!("{API_PREFIX}/posts/{post_id}/reactions"))
    }

    /// `POST /api/v1/posts/:id/favorite` (`posts.favorite`) — favourite a post.
    pub fn favorite(&self, post_id: Id) -> Result<JsonObject> {
        self.ok_object("POST", &format!("{API_PREFIX}/posts/{post_id}/favorite"))
    }

    /// `DELETE /api/v1/posts/:id/favorite` — remove the favourite.
    pub fn unfavorite(&self, post_id: Id) -> Result<JsonObject> {
        self.ok_object("DELETE", &format!("{API_PREFIX}/posts/{post_id}/favorite"))
    }

    /// `POST /api/v1/posts/:id/replies/:reply_id/favorite` — favourite a reply.
    pub fn favorite_reply(&self, post_id: Id, reply_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "POST",
            &format!("{API_PREFIX}/posts/{post_id}/replies/{reply_id}/favorite"),
        )
    }

    /// `DELETE /api/v1/posts/:id/replies/:reply_id/favorite`.
    pub fn unfavorite_reply(&self, post_id: Id, reply_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "DELETE",
            &format!("{API_PREFIX}/posts/{post_id}/replies/{reply_id}/favorite"),
        )
    }

    /// `GET /api/v1/users/:user_id/favorites/posts` — a user's favourite posts.
    pub fn favorite_posts(&self, user_id: Id) -> Result<crate::models::PostPage> {
        self.get_json(&format!("{API_PREFIX}/users/{user_id}/favorites/posts"))
    }

    /// `GET /api/v1/users/:user_id/favorites/replies` — a user's favourite replies.
    pub fn favorite_replies(&self, user_id: Id) -> Result<ReplyPage> {
        self.get_json(&format!("{API_PREFIX}/users/{user_id}/favorites/replies"))
    }

    /// `POST /api/v1/posts/:post_id/tips` — tip a post with coins.
    ///
    /// `amount` is a whole **coin** count (`int64` on the wire), not the decimal
    /// coin value the wallet uses; the recorded tip reports it back in cents.
    /// `MAX_TIP_COINS` is the ceiling and `1` the floor. The post author
    /// receives 95 % and the platform keeps 5 %; if the post is deleted the tips
    /// are refunded to the tipper.
    ///
    /// The call is authorized by the **payment PIN**, which is why `pin` is part
    /// of the body rather than the token alone. Wrong entries share the per-user
    /// budget with every other payment check, and a bot that has never set a PIN
    /// gets `403 payment pin not set` — see
    /// [`set_payment_pin`](AuthenticatedClient::set_payment_pin).
    ///
    /// # Errors
    ///
    /// * `400 amount and pin are required`;
    /// * `400 invalid tip amount` — below 1 or above [`MAX_TIP_COINS`];
    /// * `400 cannot tip your own post`;
    /// * `400 insufficient balance`;
    /// * `403 payment pin not set`;
    /// * `401 invalid payment pin`;
    /// * `404 post not found` — absent, or not visible to the caller;
    /// * `429 尝试次数过多，请稍后再试` — the PIN budget is exhausted.
    pub fn tip_post(&self, post_id: Id, amount_coins: i64, pin: &str) -> Result<PostTip> {
        let request = NewTip::new(post_id, amount_coins, pin)?;
        let body = serde_json::to_value(&request).map_err(super::json_error)?;
        self.post_json(&format!("{API_PREFIX}/posts/{post_id}/tips"), &body)
    }
}

/// Largest tip, in whole coins (`post.go`: `maxTipCoins`).
pub const MAX_TIP_COINS: i64 = 100_000;

/// Share of each tip that reaches the author, in percent (`tipPlatformFeePercent`
/// leaves 5 % with the platform).
pub const TIP_AUTHOR_PERCENT: i64 = 95;

/// A tip to submit.
///
/// The amount is a whole coin count, matching `post.go`'s `amount int64` —
/// unlike the wallet, which speaks decimal coins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewTip {
    /// Coins to pay, `1..=MAX_TIP_COINS`.
    pub amount: i64,
    /// The caller's six-digit payment PIN.
    pub pin: String,
}

impl NewTip {
    /// A tip of `amount_coins`, authorized by `pin`.
    ///
    /// # Errors
    ///
    /// Rejects an amount outside `1..=MAX_TIP_COINS` and an empty PIN.
    pub fn new(post_id: Id, amount_coins: i64, pin: impl Into<String>) -> Result<Self> {
        if post_id <= 0 {
            return Err(SdkError::InvalidArgument {
                message: "post_id must be a positive id".into(),
            });
        }
        if !(1..=MAX_TIP_COINS).contains(&amount_coins) {
            return Err(SdkError::InvalidArgument {
                message: format!("tip amount must be 1..={MAX_TIP_COINS} coins"),
            });
        }
        let pin = pin.into();
        if pin.trim().is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "pin must not be empty".into(),
            });
        }
        Ok(Self {
            amount: amount_coins,
            pin,
        })
    }

    /// What the author receives for this tip, in **cents**.
    ///
    /// Mirrors `netTipAmount`: 95 % of `amount * 100`, rounded down.
    pub fn author_cents(&self) -> i64 {
        self.amount * crate::models::MONEY_SCALE * TIP_AUTHOR_PERCENT / 100
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_post_requires_content_or_quote() {
        assert!(NewPost::default().validate().is_err());
        assert!(NewPost::text("hello").validate().is_ok());
        assert!(NewPost::quote(7).validate().is_ok());
        // Unlike the reply and message paths, the post handler accepts a whitespace
        // body as long as it is non-empty; the SDK matches that rather than guessing.
        assert!(NewPost::text("   ").validate().is_ok());
    }

    #[test]
    fn new_post_counts_content_in_bytes_like_the_server() {
        assert!(NewPost::text("a".repeat(5000)).validate().is_ok());
        assert!(NewPost::text("a".repeat(5001)).validate().is_err());
        // 2000 CJK characters are 6000 bytes, so the server rejects them.
        let cjk = "测".repeat(2000);
        assert_eq!(cjk.len(), 6000);
        assert!(
            NewPost::text(cjk).validate().is_err(),
            "the byte limit must bite before the 5000-character mark"
        );
        assert!(NewPost::text("测".repeat(1600)).validate().is_ok());
    }

    #[test]
    fn new_post_enforces_the_attachment_ceiling() {
        let too_many = NewPost::text("x").attachments((0..10).map(|i| i as Id));
        assert!(too_many.validate().is_err());
        let at_limit = NewPost::text("x").attachments((0..9).map(|i| i as Id));
        assert!(at_limit.validate().is_ok());
    }

    #[test]
    fn new_post_serialises_the_handler_field_names() {
        let json = serde_json::to_value(
            NewPost::text("hi")
                .visibility(Visibility::Private)
                .in_camp(66)
                .tag("market")
                .attachment(9),
        )
        .unwrap();
        assert_eq!(json["content"], "hi");
        assert_eq!(json["visibility"], "private");
        assert_eq!(json["camp_id"], 66);
        assert_eq!(json["tags"][0], "market");
        assert_eq!(json["attachment_ids"][0], 9);

        let bare = serde_json::to_value(NewPost::text("hi")).unwrap();
        assert!(bare.get("visibility").is_none(), "unset fields are omitted");
        assert!(bare.get("attachment_ids").is_none());
    }

    #[test]
    fn reaction_body_matches_the_handler() {
        // The handler binds {"reaction": "..."} and validates against allowedReactions.
        let body = serde_json::json!({ "reaction": Reaction::Like.as_str() });
        assert_eq!(body, serde_json::json!({ "reaction": "like" }));
        for reaction in Reaction::ALL {
            let body = serde_json::json!({ "reaction": reaction.as_str() });
            assert!(body["reaction"].is_string());
        }
    }

    #[test]
    fn new_reply_checks_content_and_attachments() {
        assert!(NewReply::text("ok").validate().is_ok());
        assert!(NewReply::default().validate().is_err());
        assert!(
            NewReply::text("   ").validate().is_ok(),
            "the reply handler only rejects an empty string"
        );
        assert!(NewReply::text("x".repeat(5001)).validate().is_err());
        assert!(NewReply::text("x".repeat(5000)).validate().is_ok());
        assert!(NewReply::text("ok")
            .replying_to(5)
            .attachment(1)
            .validate()
            .is_ok());

        let json = serde_json::to_value(NewReply::text("ok").replying_to(5)).unwrap();
        assert_eq!(json["parent_id"], 5);
        assert!(json.get("attachment_ids").is_none());
    }

    #[test]
    fn documented_limits_are_exported() {
        assert_eq!(MAX_CONTENT_BYTES, 5000);
        assert_eq!(MAX_ATTACHMENTS, 9);
    }
}
