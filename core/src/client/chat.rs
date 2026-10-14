//! Chat endpoints (`bots.md` §6.3, §7.1, §9).
//!
//! Three protocol details shape this module, all of them verified against
//! `services/chat/internal/handler/`:
//!
//! * **Direct messages are consent-based.** A bot cannot message a stranger; it calls
//!   [`AuthenticatedClient::start_conversation`] and the human accepts or declines
//!   (`bots.md` §9.4). Group invites work the same way.
//! * **Message listing has no `after` cursor.** Only `before`, and the query is
//!   ordered **newest first**, so "new messages" is either a poll of the newest page
//!   plus id deduplication or the WebSocket channel. [`MessageCursor`] does the dedupe.
//! * **Content limits are counted in bytes**, not characters: the handler checks
//!   `len(req.Content) > 5000` and Go's `len` is bytes. Validation here matches that,
//!   so a CJK message is rejected locally at exactly the point the server would.

use serde::{Deserialize, Serialize};

use crate::error::{Result, SdkError};
use crate::models::{
    Conversation, ConversationDetail, ConversationList, Id, JsonObject, Message, MessagePage,
};

use super::{message_query, AuthenticatedClient, API_PREFIX};

pub use crate::models::MENTION_EVERYONE;

/// Maximum message body length **in bytes** (`message.go`).
pub const MAX_CONTENT_BYTES: usize = 5000;
/// Maximum attachments per message.
pub const MAX_ATTACHMENTS: usize = 9;
/// Maximum `limit` the message endpoint honours; anything outside `1..=100` is reset
/// to [`DEFAULT_MESSAGE_LIMIT`] by the repository.
pub const MAX_MESSAGE_LIMIT: i64 = 100;
/// Page size the server falls back to when `limit` is out of range.
pub const DEFAULT_MESSAGE_LIMIT: i64 = 50;

/// A message to send.
///
/// Field names match the `Send` handler's anonymous request struct exactly:
/// `content`, `reply_to_id`, `attachment_ids`, `mentions`, `check_id`,
/// `burn_seconds`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewMessage {
    /// Body text, up to [`MAX_CONTENT_BYTES`] bytes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    /// Message to quote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_id: Option<Id>,
    /// Attachment ids, at most [`MAX_ATTACHMENTS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_ids: Vec<Id>,
    /// Mentioned user ids; [`MENTION_EVERYONE`] means everyone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<Id>,
    /// Red packet / lottery id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_id: Option<Id>,
    /// Burn-after-reading window: `0`, or `5..=86400` seconds.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub burn_seconds: i64,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

impl NewMessage {
    /// A plain text message.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ..Self::default()
        }
    }

    /// Builder: quote another message (the usual way to answer a command).
    pub fn replying_to(mut self, message_id: Id) -> Self {
        self.reply_to_id = Some(message_id);
        self
    }

    /// Builder: attach one attachment id.
    pub fn attachment(mut self, attachment_id: Id) -> Self {
        self.attachment_ids.push(attachment_id);
        self
    }

    /// Builder: mention one user.
    pub fn mention(mut self, user_id: Id) -> Self {
        self.mentions.push(user_id);
        self
    }

    /// Builder: mention `@everyone`.
    pub fn mention_everyone(mut self) -> Self {
        self.mentions.push(MENTION_EVERYONE);
        self
    }

    /// Builder: set a burn-after-reading window, validated against the documented range.
    pub fn burn_after(mut self, seconds: i64) -> Result<Self> {
        self.burn_seconds = seconds;
        self.validate()?;
        Ok(self)
    }

    /// Reject a message the server would reject.
    pub fn validate(&self) -> Result<()> {
        if self.content.is_empty() && self.attachment_ids.is_empty() && self.check_id.is_none() {
            return Err(SdkError::InvalidArgument {
                message: "message cannot be empty: set content, attachment_ids or check_id".into(),
            });
        }
        if self.content.len() > MAX_CONTENT_BYTES {
            return Err(SdkError::InvalidArgument {
                message: format!(
                    "message too long (max {MAX_CONTENT_BYTES} bytes); the server counts bytes, \
                     not characters"
                ),
            });
        }
        if self.attachment_ids.len() > MAX_ATTACHMENTS {
            return Err(SdkError::InvalidArgument {
                message: format!("too many attachments (max {MAX_ATTACHMENTS})"),
            });
        }
        if !burn_seconds_allowed(self.burn_seconds) {
            return Err(SdkError::InvalidArgument {
                message: "invalid burn_seconds (allowed: 0, or 5..86400)".into(),
            });
        }
        Ok(())
    }
}

/// Whether a `burn_seconds` value is one the server accepts.
pub fn burn_seconds_allowed(seconds: i64) -> bool {
    seconds == 0 || (5..=86_400).contains(&seconds)
}

/// Payload for creating a group conversation.
///
/// The handler binds exactly one field, `title`, and marks it required, so the SDK
/// refuses to build a titleless request. Settings such as `is_public` go through
/// [`AuthenticatedClient::update_conversation_settings`] afterwards.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewConversation {
    /// Group title. Required by the server.
    pub title: String,
}

impl NewConversation {
    /// A group with the given title.
    pub fn named(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
        }
    }

    /// Reject a request the server would reject with `400`.
    pub fn validate(&self) -> Result<()> {
        if self.title.trim().is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "a group needs a non-empty title".into(),
            });
        }
        Ok(())
    }
}

/// Payload for `PUT /conversations/:id/settings`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConversationSettings {
    /// Whether the group is searchable and viewable by non-members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_public: Option<bool>,
    /// Whether public groups may be joined without an invitation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_join: Option<bool>,
    /// Whether the conversation is end-to-end encrypted.
    ///
    /// Turning this on hands the bot the group-key problem described in
    /// `bots.md` §11; the server will not encrypt anything on the bot's behalf.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted: Option<bool>,
}

impl ConversationSettings {
    /// Builder: set public visibility.
    pub fn public(mut self, value: bool) -> Self {
        self.is_public = Some(value);
        self
    }

    /// Builder: allow joining without an invitation.
    pub fn allow_join(mut self, value: bool) -> Self {
        self.allow_join = Some(value);
        self
    }

    /// Builder: toggle end-to-end encryption.
    pub fn encrypted(mut self, value: bool) -> Self {
        self.encrypted = Some(value);
        self
    }

    /// Reject an empty settings patch.
    pub fn validate(&self) -> Result<()> {
        if self.is_public.is_none() && self.allow_join.is_none() && self.encrypted.is_none() {
            return Err(SdkError::InvalidArgument {
                message: "conversation settings patch must set at least one field".into(),
            });
        }
        Ok(())
    }
}

/// Payload for `POST /conversations/start` — a consent request, not a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// The user being asked.
    pub user_id: Id,
    /// Opening message shown to them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Whether the bot intends to speak E2EE. See [`ChatRequest::encrypted`].
    #[serde(default)]
    pub encrypted: bool,
}

impl ChatRequest {
    /// A consent request to `user_id`.
    pub fn new(user_id: Id) -> Self {
        Self {
            user_id,
            message: None,
            encrypted: false,
        }
    }

    /// Builder: include an opening message explaining what the bot does.
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Builder: request an end-to-end encrypted conversation.
    ///
    /// `bots.md` §11 is explicit that this is a trap unless the bot implements the
    /// whole group-key flow: message `content` arrives as ciphertext, and the server
    /// never encrypts on the bot's behalf. Prefer the plaintext default.
    pub fn encrypted(mut self) -> Self {
        self.encrypted = true;
        self
    }

    /// Reject locally invalid requests before spending a round trip.
    ///
    /// `self_id` lets the documented "cannot chat with yourself" rule be applied
    /// (`bots.md` §9.4); pass `None` when the bot's id is not known yet.
    pub fn validate(&self, self_id: Option<Id>) -> Result<()> {
        if self.user_id <= 0 {
            return Err(SdkError::InvalidArgument {
                message: "user_id must be a positive id".into(),
            });
        }
        if let Some(self_id) = self_id {
            if self_id == self.user_id {
                return Err(SdkError::InvalidArgument {
                    message: "cannot start a chat with yourself (`bots.md` §9.4)".into(),
                });
            }
        }
        if let Some(message) = &self.message {
            if message.len() > MAX_CONTENT_BYTES {
                return Err(SdkError::InvalidArgument {
                    message: format!("message too long (max {MAX_CONTENT_BYTES} bytes)"),
                });
            }
        }
        Ok(())
    }
}

/// The client half of the "no `after` cursor" workaround.
///
/// Seed it with the newest id already seen, then feed it each page: it returns only
/// messages newer than the cursor, in ascending id order, and advances itself. All
/// three anti-self-loop filters from `bots.md` §9.2 are applied by
/// [`MessageCursor::drain_actionable`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageCursor {
    last_seen: Id,
}

impl MessageCursor {
    /// A cursor positioned before the given id (usually `0`).
    pub fn new(last_seen: Id) -> Self {
        Self { last_seen }
    }

    /// The highest id observed so far.
    pub fn last_seen(&self) -> Id {
        self.last_seen
    }

    /// Take every message strictly newer than the cursor, in ascending id order,
    /// advancing it.
    ///
    /// Works regardless of whether the page arrived newest-first (which is what the
    /// repository returns) or oldest-first (which `bots.md` §7.3 claims).
    pub fn drain_new(&mut self, page: &MessagePage) -> Vec<Message> {
        let mut fresh: Vec<Message> = page
            .messages
            .iter()
            .filter(|message| message.id > self.last_seen)
            .cloned()
            .collect();
        fresh.sort_by_key(|message| message.id);
        if let Some(last) = fresh.last() {
            self.last_seen = last.id;
        }
        fresh
    }

    /// Like [`MessageCursor::drain_new`], but keeping only messages the bot should
    /// answer: not its own, not another bot's, and not a system notice.
    pub fn drain_actionable(&mut self, page: &MessagePage, bot_id: Id) -> Vec<Message> {
        self.drain_new(page)
            .into_iter()
            .filter(|message| message.is_actionable_for(bot_id))
            .collect()
    }

    /// Advance the cursor past every message in the page without returning them.
    ///
    /// Used on startup to avoid replaying history (`bots.md` §9.2).
    pub fn skip(&mut self, page: &MessagePage) {
        let max = page.max_id();
        if max > self.last_seen {
            self.last_seen = max;
        }
    }

    /// Whether any id in the page is newer than the cursor.
    pub fn has_new(&self, page: &MessagePage) -> bool {
        page.max_id() > self.last_seen
    }
}

impl AuthenticatedClient {
    /// `GET /api/v1/conversations` (`chat.view`).
    pub fn conversations(&self) -> Result<ConversationList> {
        self.get_json(&format!("{API_PREFIX}/conversations"))
    }

    /// `POST /api/v1/conversations` (`chat.group.create`) — create a group.
    ///
    /// The caller becomes the owner.
    pub fn create_conversation(&self, request: &NewConversation) -> Result<Conversation> {
        request.validate()?;
        let body = serde_json::to_value(request).map_err(super::json_error)?;
        self.post_json(&format!("{API_PREFIX}/conversations"), &body)
    }

    /// `PUT /api/v1/conversations/:id/settings` (`chat.group.manage`).
    pub fn update_conversation_settings(
        &self,
        conversation_id: Id,
        settings: &ConversationSettings,
    ) -> Result<JsonObject> {
        settings.validate()?;
        let body = serde_json::to_value(settings).map_err(super::json_error)?;
        let response = self.base.http.json_raw(
            "PUT",
            &format!("{API_PREFIX}/conversations/{conversation_id}/settings"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// `POST /api/v1/conversations/start` (`chat.request.send`) — request a direct chat.
    ///
    /// This only *asks*: no conversation exists until the human accepts (`bots.md` §9.4).
    /// A repeat request while one is pending answers `409`, and targeting yourself is a
    /// `400`.
    pub fn start_conversation(&self, request: &ChatRequest) -> Result<JsonObject> {
        request.validate(None)?;
        let body = serde_json::to_value(request).map_err(super::json_error)?;
        let response = self.base.http.json_raw(
            "POST",
            &format!("{API_PREFIX}/conversations/start"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// `GET /api/v1/conversations/public` — public groups the bot may join.
    pub fn public_conversations(&self) -> Result<ConversationList> {
        self.get_json(&format!("{API_PREFIX}/conversations/public"))
    }

    /// `GET /api/v1/conversations/:id` — the `{conversation, members, my_membership}`
    /// envelope.
    ///
    /// Check `encrypted` here before treating `content` as text (`bots.md` §11).
    pub fn conversation(&self, conversation_id: Id) -> Result<ConversationDetail> {
        self.get_json(&format!("{API_PREFIX}/conversations/{conversation_id}"))
    }

    /// `DELETE /api/v1/conversations/:id` (`chat.group.manage`).
    pub fn delete_conversation(&self, conversation_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "DELETE",
            &format!("{API_PREFIX}/conversations/{conversation_id}"),
        )
    }

    /// `GET /api/v1/conversations/:id/messages` — one page, **newest first**.
    ///
    /// `before` walks backwards in time; there is no forward cursor (`bots.md` §7.3).
    /// A `limit` outside `1..=100` makes the server fall back to 50, so
    /// [`AuthenticatedClient::latest_messages`] clamps it locally instead.
    pub fn messages(
        &self,
        conversation_id: Id,
        limit: Option<i64>,
        before: Option<Id>,
    ) -> Result<MessagePage> {
        self.get_json_query(
            &format!("{API_PREFIX}/conversations/{conversation_id}/messages"),
            &message_query(limit, before),
        )
    }

    /// The newest page of messages, positioned for polling (`bots.md` §9.2).
    ///
    /// `limit` is clamped to the documented `1..=100` window so the caller gets the
    /// page size they asked for instead of the server silently resetting it.
    pub fn latest_messages(&self, conversation_id: Id, limit: Option<i64>) -> Result<MessagePage> {
        let limit = limit.map(|value| value.clamp(1, MAX_MESSAGE_LIMIT));
        self.messages(conversation_id, limit, None)
    }

    /// Seed a [`MessageCursor`] at the newest message, so history is not replayed.
    ///
    /// This is the alignment step in `bots.md` §9.2.
    pub fn latest_cursor(&self, conversation_id: Id) -> Result<MessageCursor> {
        let page = self.latest_messages(conversation_id, Some(1))?;
        Ok(MessageCursor::new(page.max_id()))
    }

    /// `GET /api/v1/conversations/:id/messages/search` — filter chat history.
    ///
    /// Recognised filters are `q`, `sender_id`, `from`, `to`, `has_attachment`,
    /// `file_name` and `limit`. Results are **newest first**, and in an encrypted
    /// conversation `q` cannot match plaintext because the server stores ciphertext.
    pub fn search_messages(
        &self,
        conversation_id: Id,
        query: &[(&str, &str)],
    ) -> Result<MessagePage> {
        self.get_json_query(
            &format!("{API_PREFIX}/conversations/{conversation_id}/messages/search"),
            &super::owned(query),
        )
    }

    /// `POST /api/v1/conversations/:id/messages` (`chat.send`) — `201` plus the message.
    pub fn send_message(&self, conversation_id: Id, message: &NewMessage) -> Result<Message> {
        message.validate()?;
        let body = serde_json::to_value(message).map_err(super::json_error)?;
        self.post_json(
            &format!("{API_PREFIX}/conversations/{conversation_id}/messages"),
            &body,
        )
    }

    /// Convenience: send plain text.
    pub fn say(&self, conversation_id: Id, content: &str) -> Result<Message> {
        self.send_message(conversation_id, &NewMessage::text(content))
    }

    /// Convenience: reply to a specific message (the usual command response).
    pub fn reply(&self, conversation_id: Id, message_id: Id, content: &str) -> Result<Message> {
        self.send_message(
            conversation_id,
            &NewMessage::text(content).replying_to(message_id),
        )
    }

    /// `POST /api/v1/conversations/:id/messages/forward` (`chat.send`).
    ///
    /// Forwards messages by id into one or more target conversations.
    pub fn forward_messages(
        &self,
        conversation_id: Id,
        message_ids: &[Id],
        target_conversation_ids: &[Id],
    ) -> Result<JsonObject> {
        if message_ids.is_empty() || target_conversation_ids.is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "forward needs at least one message id and one target conversation".into(),
            });
        }
        let body = serde_json::json!({
            "message_ids": message_ids,
            "target_conversation_ids": target_conversation_ids,
        });
        let response = self.base.http.json_raw(
            "POST",
            &format!("{API_PREFIX}/conversations/{conversation_id}/messages/forward"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// `PUT /api/v1/conversations/:id/messages/:message_id` (`chat.edit`).
    pub fn edit_message(
        &self,
        conversation_id: Id,
        message_id: Id,
        content: &str,
    ) -> Result<Message> {
        if content.is_empty() || content.len() > MAX_CONTENT_BYTES {
            return Err(SdkError::InvalidArgument {
                message: format!("content must be 1..={MAX_CONTENT_BYTES} bytes"),
            });
        }
        let body = serde_json::json!({ "content": content });
        self.put_json(
            &format!("{API_PREFIX}/conversations/{conversation_id}/messages/{message_id}"),
            &body,
        )
    }

    /// `DELETE /api/v1/conversations/:id/messages/:message_id` (`chat.delete`).
    ///
    /// Soft delete: the row stays and list endpoints filter it out.
    pub fn delete_message(&self, conversation_id: Id, message_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "DELETE",
            &format!("{API_PREFIX}/conversations/{conversation_id}/messages/{message_id}"),
        )
    }

    /// `POST /api/v1/conversations/:id/read` (`chat.view`) — mark the conversation read.
    ///
    /// The handler binds `last_message_id`; pass the newest id the bot has processed.
    /// `0` is accepted and means "nothing yet".
    pub fn mark_read(&self, conversation_id: Id, last_message_id: Id) -> Result<JsonObject> {
        let body = serde_json::json!({ "last_message_id": last_message_id });
        let response = self.base.http.json_raw(
            "POST",
            &format!("{API_PREFIX}/conversations/{conversation_id}/read"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// `POST /api/v1/conversations/:id/messages/:message_id/read` (`chat.view`).
    ///
    /// Per-message read receipt; also arms burn-after-reading on that message.
    pub fn mark_message_read(&self, conversation_id: Id, message_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "POST",
            &format!("{API_PREFIX}/conversations/{conversation_id}/messages/{message_id}/read"),
        )
    }

    /// `POST /api/v1/conversations/:id/typing` (`chat.view`) — emit a typing indicator.
    pub fn send_typing(&self, conversation_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "POST",
            &format!("{API_PREFIX}/conversations/{conversation_id}/typing"),
        )
    }

    /// `POST /api/v1/conversations/:id/join` (`chat.group.manage`) — join a public group.
    ///
    /// The group must have `allow_join` set.
    pub fn join_conversation(&self, conversation_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "POST",
            &format!("{API_PREFIX}/conversations/{conversation_id}/join"),
        )
    }

    /// `POST /api/v1/conversations/:id/invite` (`chat.group.invite`) — invite **one** user.
    ///
    /// Despite `bots.md`, the handler binds a single `user_id` plus an optional
    /// `message`; it files a consent request rather than adding the member directly.
    /// Use [`AuthenticatedClient::invite_many`] for a list.
    pub fn invite(
        &self,
        conversation_id: Id,
        user_id: Id,
        message: Option<&str>,
    ) -> Result<JsonObject> {
        if user_id <= 0 {
            return Err(SdkError::InvalidArgument {
                message: "user_id must be a positive id".into(),
            });
        }
        let mut body = serde_json::json!({ "user_id": user_id });
        if let Some(message) = message {
            body["message"] = serde_json::Value::String(message.to_string());
        }
        let response = self.base.http.json_raw(
            "POST",
            &format!("{API_PREFIX}/conversations/{conversation_id}/invite"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// Invite several users, one request each.
    ///
    /// The server has no bulk invite, so this is a convenience loop. It stops at the
    /// first failure and reports both what succeeded and the error.
    pub fn invite_many(
        &self,
        conversation_id: Id,
        user_ids: &[Id],
        message: Option<&str>,
    ) -> Result<Vec<JsonObject>> {
        if user_ids.is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "invite needs at least one user id".into(),
            });
        }
        let mut results = Vec::with_capacity(user_ids.len());
        for user_id in user_ids {
            results.push(self.invite(conversation_id, *user_id, message)?);
        }
        Ok(results)
    }

    /// `POST /api/v1/conversations/:id/leave` (`chat.group.manage`) — leave a group.
    ///
    /// The owner cannot leave; the server answers with an error rather than silently
    /// transferring ownership.
    pub fn leave_conversation(&self, conversation_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "POST",
            &format!("{API_PREFIX}/conversations/{conversation_id}/leave"),
        )
    }

    /// `DELETE /api/v1/conversations/:id/members/:user_id` (`chat.group.manage`).
    ///
    /// Owner/admin only; only the owner may remove an admin.
    pub fn remove_member(&self, conversation_id: Id, user_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "DELETE",
            &format!("{API_PREFIX}/conversations/{conversation_id}/members/{user_id}"),
        )
    }

    /// `PUT /api/v1/conversations/:id/members/:user_id/role` (`chat.group.manage`).
    ///
    /// `role` is required by the handler; pass `owner`, `admin` or `member`.
    pub fn set_member_role(
        &self,
        conversation_id: Id,
        user_id: Id,
        role: &str,
    ) -> Result<JsonObject> {
        let body = serde_json::json!({ "role": role });
        let response = self.base.http.json_raw(
            "PUT",
            &format!("{API_PREFIX}/conversations/{conversation_id}/members/{user_id}/role"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// `PUT /api/v1/conversations/:id/title` (`chat.group.manage`).
    pub fn set_conversation_title(&self, conversation_id: Id, title: &str) -> Result<JsonObject> {
        if title.trim().is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "title must not be empty".into(),
            });
        }
        let body = serde_json::json!({ "title": title });
        let response = self.base.http.json_raw(
            "PUT",
            &format!("{API_PREFIX}/conversations/{conversation_id}/title"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// `PUT /api/v1/conversations/:id/group-nickname` (`chat.group.nickname`).
    pub fn set_group_nickname(&self, conversation_id: Id, nickname: &str) -> Result<JsonObject> {
        let body = serde_json::json!({ "group_nickname": nickname });
        let response = self.base.http.json_raw(
            "PUT",
            &format!("{API_PREFIX}/conversations/{conversation_id}/group-nickname"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// `PUT /api/v1/conversations/:id/note` (`chat.note.edit`) — private remark on a chat.
    pub fn set_note(&self, conversation_id: Id, note: &str) -> Result<JsonObject> {
        let body = serde_json::json!({ "note": note });
        let response = self.base.http.json_raw(
            "PUT",
            &format!("{API_PREFIX}/conversations/{conversation_id}/note"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// `POST /api/v1/conversations/:id/mute-all` (`chat.group.manage`).
    pub fn mute_all(&self, conversation_id: Id, duration_minutes: i64) -> Result<JsonObject> {
        if duration_minutes <= 0 {
            return Err(SdkError::InvalidArgument {
                message: "duration_minutes must be positive; use unmute_all to lift a mute".into(),
            });
        }
        let body = serde_json::json!({ "duration_minutes": duration_minutes });
        let response = self.base.http.json_raw(
            "POST",
            &format!("{API_PREFIX}/conversations/{conversation_id}/mute-all"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// `DELETE /api/v1/conversations/:id/mute-all` — lift a group-wide mute.
    pub fn unmute_all(&self, conversation_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "DELETE",
            &format!("{API_PREFIX}/conversations/{conversation_id}/mute-all"),
        )
    }

    /// Whether a conversation is end-to-end encrypted (`bots.md` §11).
    ///
    /// Every bot loop should call this — or check the event payload — and skip
    /// encrypted conversations rather than treat ciphertext as text.
    pub fn is_encrypted(&self, conversation_id: Id) -> Result<bool> {
        Ok(self.conversation(conversation_id)?.is_encrypted())
    }

    // --- consent requests (`API.md`, `features.md`) --------------------------

    /// `GET /api/v1/consent-requests` (`chat.request.approve`).
    ///
    /// Lists the requests **targeting this bot**. A bot that wants a human's attention
    /// has to wait for them to accept; this is how the bot answers requests aimed at it.
    pub fn consent_requests(&self) -> Result<crate::models::ConsentRequestList> {
        self.get_json(&format!("{API_PREFIX}/consent-requests"))
    }

    /// `POST /api/v1/consent-requests/:id/accept` (`chat.request.approve`).
    pub fn accept_consent_request(&self, request_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "POST",
            &format!("{API_PREFIX}/consent-requests/{request_id}/accept"),
        )
    }

    /// `POST /api/v1/consent-requests/:id/decline` (`chat.request.approve`).
    ///
    /// The handler answers `204`.
    pub fn decline_consent_request(&self, request_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "POST",
            &format!("{API_PREFIX}/consent-requests/{request_id}/decline"),
        )
    }

    // --- group extras (`group_features.go`) ---------------------------------

    /// `GET /api/v1/conversations/:id/files` (`chat.view`) — attachments already
    /// shared into the conversation, newest first.
    pub fn conversation_files(&self, conversation_id: Id) -> Result<crate::models::AttachmentList> {
        self.get_json(&format!(
            "{API_PREFIX}/conversations/{conversation_id}/files"
        ))
    }

    /// `GET /api/v1/conversations/:id/announcements` (`chat.view`).
    pub fn conversation_announcements(&self, conversation_id: Id) -> Result<serde_json::Value> {
        let response = self.base.http.json_raw(
            "GET",
            &format!("{API_PREFIX}/conversations/{conversation_id}/announcements"),
            None,
            &[],
        )?;
        response.json()
    }

    /// `POST /api/v1/conversations/:id/announcements` (`chat.group.manage`).
    pub fn create_conversation_announcement(
        &self,
        conversation_id: Id,
        title: Option<&str>,
        content: &str,
    ) -> Result<JsonObject> {
        if content.trim().is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "announcement content is required".into(),
            });
        }
        let mut body = serde_json::json!({ "content": content });
        if let Some(title) = title {
            body["title"] = serde_json::Value::String(title.to_string());
        }
        let response = self.base.http.json_raw(
            "POST",
            &format!("{API_PREFIX}/conversations/{conversation_id}/announcements"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// `DELETE /api/v1/conversations/:id/announcements/:announcement_id`.
    pub fn delete_conversation_announcement(
        &self,
        conversation_id: Id,
        announcement_id: Id,
    ) -> Result<JsonObject> {
        self.ok_object(
            "DELETE",
            &format!(
                "{API_PREFIX}/conversations/{conversation_id}/announcements/{announcement_id}"
            ),
        )
    }

    /// `GET /api/v1/conversations/:id/todos` (`chat.view`).
    pub fn conversation_todos(&self, conversation_id: Id) -> Result<serde_json::Value> {
        let response = self.base.http.json_raw(
            "GET",
            &format!("{API_PREFIX}/conversations/{conversation_id}/todos"),
            None,
            &[],
        )?;
        response.json()
    }

    /// `POST /api/v1/conversations/:id/todos` (`chat.view`).
    pub fn create_conversation_todo(&self, conversation_id: Id, title: &str) -> Result<JsonObject> {
        if title.trim().is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "todo title is required".into(),
            });
        }
        let body = serde_json::json!({ "title": title });
        let response = self.base.http.json_raw(
            "POST",
            &format!("{API_PREFIX}/conversations/{conversation_id}/todos"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// `PUT /api/v1/conversations/:id/todos/:todo_id` — tick a todo off.
    pub fn set_conversation_todo_done(
        &self,
        conversation_id: Id,
        todo_id: Id,
        done: bool,
    ) -> Result<JsonObject> {
        let body = serde_json::json!({ "done": done });
        let response = self.base.http.json_raw(
            "PUT",
            &format!("{API_PREFIX}/conversations/{conversation_id}/todos/{todo_id}"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// `DELETE /api/v1/conversations/:id/todos/:todo_id`.
    pub fn delete_conversation_todo(&self, conversation_id: Id, todo_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "DELETE",
            &format!("{API_PREFIX}/conversations/{conversation_id}/todos/{todo_id}"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MessageKind;

    fn message(id: Id, sender: Id, kind: MessageKind, is_bot: bool) -> Message {
        Message {
            id,
            conversation_id: 1,
            sender_id: sender,
            sender_is_bot: is_bot,
            kind,
            content: format!("m{id}"),
            ..Default::default()
        }
    }

    fn page(messages: Vec<Message>) -> MessagePage {
        MessagePage {
            messages,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn new_message_requires_some_payload() {
        assert!(NewMessage::default().validate().is_err());
        assert!(NewMessage::text("hi").validate().is_ok());
        let with_attachment = NewMessage {
            attachment_ids: vec![1],
            ..Default::default()
        };
        assert!(
            with_attachment.validate().is_ok(),
            "attachments alone are a valid message"
        );
        let with_check = NewMessage {
            check_id: Some(5),
            ..Default::default()
        };
        assert!(with_check.validate().is_ok(), "a check alone is valid");
    }

    #[test]
    fn new_message_counts_content_in_bytes_like_the_server() {
        // The handler checks `len(req.Content) > 5000`, which is bytes in Go.
        assert!(NewMessage::text("a".repeat(5000)).validate().is_ok());
        assert!(NewMessage::text("a".repeat(5001)).validate().is_err());
        // 2000 CJK characters are 6000 bytes, so the server rejects them.
        let cjk = "测".repeat(2000);
        assert_eq!(cjk.len(), 6000);
        assert!(
            NewMessage::text(cjk).validate().is_err(),
            "a 6000-byte body must be rejected locally, matching the server"
        );
        let cjk_ok = "测".repeat(1500);
        assert_eq!(cjk_ok.len(), 4500);
        assert!(NewMessage::text(cjk_ok).validate().is_ok());
    }

    #[test]
    fn burn_seconds_matches_the_documented_range() {
        for allowed in [0, 5, 100, 86_400] {
            assert!(burn_seconds_allowed(allowed), "{allowed} should be allowed");
        }
        for rejected in [-1, 1, 4, 86_401] {
            assert!(
                !burn_seconds_allowed(rejected),
                "{rejected} should be rejected"
            );
        }
        assert!(NewMessage::text("x").burn_after(5).is_ok());
        assert!(NewMessage::text("x").burn_after(4).is_err());
    }

    #[test]
    fn new_message_omits_zero_burn_and_builds_mentions() {
        let json = serde_json::to_value(NewMessage::text("hi")).unwrap();
        assert!(json.get("burn_seconds").is_none(), "zero burn is omitted");
        assert!(json.get("mentions").is_none());

        let json = serde_json::to_value(
            NewMessage::text("hi")
                .mention(7)
                .mention_everyone()
                .replying_to(99),
        )
        .unwrap();
        assert_eq!(json["mentions"], serde_json::json!([7, -1]));
        assert_eq!(json["reply_to_id"], 99);
    }

    #[test]
    fn new_message_serialises_the_handler_field_names() {
        let json =
            serde_json::to_value(NewMessage::text("hi").attachment(3).replying_to(4)).unwrap();
        // Exactly the keys the Send handler binds.
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort();
        assert_eq!(keys, vec!["attachment_ids", "content", "reply_to_id"]);
    }

    #[test]
    fn new_conversation_requires_a_title() {
        assert!(NewConversation::default().validate().is_err());
        assert!(NewConversation::named("   ").validate().is_err());
        assert!(NewConversation::named("Family").validate().is_ok());
        let json = serde_json::to_value(NewConversation::named("Family")).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "title": "Family" }),
            "the handler binds only `title`"
        );
    }

    #[test]
    fn conversation_settings_serialise_the_handler_field_names() {
        let settings = ConversationSettings::default()
            .public(true)
            .allow_join(false)
            .encrypted(false);
        settings.validate().unwrap();
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(json["is_public"], true);
        assert_eq!(json["allow_join"], false);
        assert_eq!(json["encrypted"], false);

        assert!(
            ConversationSettings::default().validate().is_err(),
            "an empty settings patch is refused locally"
        );
    }

    #[test]
    fn chat_request_rejects_self_targeting_and_bad_ids() {
        let request = ChatRequest::new(42).with_message("hello");
        assert!(request.validate(Some(42)).is_err());
        assert!(request.validate(Some(7)).is_ok());
        assert!(request.validate(None).is_ok());
        assert!(!request.encrypted);
        assert!(ChatRequest::new(1).encrypted().encrypted);
        assert!(ChatRequest::new(0).validate(None).is_err());
    }

    #[test]
    fn cursor_returns_only_new_messages_in_ascending_order() {
        let mut cursor = MessageCursor::new(10);
        // Feed a newest-first page, as the repository returns.
        let page = page(vec![
            message(12, 1, MessageKind::Text, false),
            message(11, 1, MessageKind::Text, false),
            message(9, 1, MessageKind::Text, false),
        ]);
        let fresh = cursor.drain_new(&page);
        assert_eq!(
            fresh.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![11, 12],
            "results are ascending even when the page is descending"
        );
        assert_eq!(cursor.last_seen(), 12);
        assert!(cursor.drain_new(&page).is_empty());
    }

    #[test]
    fn cursor_applies_all_three_anti_loop_filters() {
        let mut cursor = MessageCursor::new(0);
        let page = page(vec![
            message(1, 42, MessageKind::Text, true), // the bot itself
            message(2, 99, MessageKind::Text, true), // another bot
            message(3, 7, MessageKind::SystemJoin, false), // system notice
            message(4, 7, MessageKind::Text, false), // actionable
        ]);
        let actionable = cursor.drain_actionable(&page, 42);
        assert_eq!(actionable.len(), 1);
        assert_eq!(actionable[0].id, 4);
        assert_eq!(
            cursor.last_seen(),
            4,
            "the cursor advances past skipped messages so they are not re-examined"
        );
    }

    #[test]
    fn skip_aligns_without_replaying_history() {
        let mut cursor = MessageCursor::new(0);
        let history = page(vec![
            message(100, 1, MessageKind::Text, false),
            message(101, 1, MessageKind::Text, false),
        ]);
        cursor.skip(&history);
        assert_eq!(cursor.last_seen(), 101);
        assert!(cursor.drain_new(&history).is_empty());
        assert!(!cursor.has_new(&history));

        let newer = page(vec![message(102, 1, MessageKind::Text, false)]);
        assert!(cursor.has_new(&newer));
    }

    #[test]
    fn empty_pages_leave_the_cursor_alone() {
        let mut cursor = MessageCursor::new(7);
        cursor.skip(&page(vec![]));
        assert_eq!(cursor.last_seen(), 7);
        assert!(cursor.drain_new(&page(vec![])).is_empty());
    }

    #[test]
    fn documented_limits_are_exported() {
        assert_eq!(MAX_MESSAGE_LIMIT, 100);
        assert_eq!(DEFAULT_MESSAGE_LIMIT, 50);
        assert_eq!(MENTION_EVERYONE, -1);
        assert_eq!(MAX_CONTENT_BYTES, 5000);
        assert_eq!(MAX_ATTACHMENTS, 9);
    }
}
