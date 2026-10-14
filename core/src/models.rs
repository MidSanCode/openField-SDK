//! Data models mirroring the OpenField API.
//!
//! Field names follow the server's own JSON tags — these were read from
//! `pkg/model/model.go` and the repository scanners in `midSanCode/openField-server`,
//! not inferred. Where the bundled documentation disagreed with the source, the
//! source won; see [`crate::docs::KNOWN_DIVERGENCES`] for the conflicts and how each
//! was resolved.
//!
//! Two conventions make the models resilient:
//!
//! * every object keeps unnamed fields in `extra`, so a server that grows a field
//!   neither breaks a bot nor loses data on a round trip;
//! * display strings tolerate `null` ([`de_string_or_null`]), because several
//!   handlers marshal a nullable Go `*string` where the model has a plain `string`.

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Free-form bag of fields the model did not name explicitly.
pub type Extra = Map<String, Value>;

/// Identifiers are `int64` throughout the API.
pub type Id = i64;

/// Deserialize a string, mapping `null` to an empty string.
///
/// Used on display fields: the Go model has several plain `string` fields that some
/// endpoints fill from a nullable `*string`, so `null` really does appear on the
/// wire (for example `avatar_url` in a search result).
pub fn de_string_or_null<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

/// A user object. Bots are ordinary users carrying `is_bot` (`bots.md` §1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct User {
    /// User id.
    pub id: Id,
    /// Unique handle.
    #[serde(deserialize_with = "de_string_or_null")]
    pub username: String,
    /// Display name.
    #[serde(deserialize_with = "de_string_or_null")]
    pub nickname: String,
    /// Contact email.
    #[serde(deserialize_with = "de_string_or_null")]
    pub email: String,
    /// Avatar URL.
    #[serde(deserialize_with = "de_string_or_null")]
    pub avatar_url: String,
    /// Profile banner URL.
    #[serde(deserialize_with = "de_string_or_null")]
    pub banner_url: String,
    /// Account role, e.g. `user` or `admin`.
    #[serde(deserialize_with = "de_string_or_null")]
    pub role: String,
    /// `true` while a registration is unfinished.
    pub needs_registration: bool,
    /// Profile bio.
    #[serde(deserialize_with = "de_string_or_null")]
    pub bio: String,
    /// `true` when the account carries the verified badge.
    pub is_verified: bool,
    /// Storage quota in bytes.
    pub storage_quota: Option<i64>,
    /// Bytes currently stored.
    pub storage_used: Option<i64>,
    /// Logical storage bucket id (`storage-buckets.md`).
    pub storage_bucket: Option<String>,
    /// RFC 3339 creation timestamp.
    pub created_at: Option<String>,
    /// RFC 3339 last-update timestamp.
    pub updated_at: Option<String>,
    /// Lifetime accumulated experience.
    pub exp: Option<i64>,
    /// Display level derived from `exp`.
    pub level: Option<i64>,
    /// Follower count, suppressed for other viewers by the privacy switch.
    pub follower_count: Option<i64>,
    /// Following count, suppressed for other viewers by the privacy switch.
    pub following_count: Option<i64>,
    /// Whether this user hides their follow lists.
    pub hide_follow_lists: bool,
    /// Whether the caller follows this user.
    pub is_following: bool,
    /// Whether the caller and this user follow each other.
    pub is_friend: bool,
    /// Permission keys, populated on `/users/me` when requested.
    pub permissions: Option<Vec<String>>,
    /// Published X25519 public key for end-to-end encrypted chats.
    pub e2ee_public_key: Option<String>,
    /// Current consecutive daily sign-in streak.
    pub checkin_streak: Option<i64>,
    /// Whether a payment PIN is set.
    pub has_pin: bool,
    /// Membership tier level (`0` = none); kept after expiry.
    pub member_level: Option<i64>,
    /// When the membership expires.
    pub member_expires_at: Option<String>,
    /// Whether membership auto-renews.
    pub auto_renew: bool,
    /// Locale region (e.g. `中国`), used for day boundaries.
    pub region: Option<String>,
    /// Display language override.
    pub lang: Option<String>,
    /// Display-name colour.
    pub name_color: Option<String>,
    /// Gradient end colour (Lv.3+).
    pub name_color_to: Option<String>,
    /// Animated gradient flag (Lv.4+).
    pub name_dynamic: bool,
    /// Full gradient colour list (1–6 hex colours).
    pub name_colors: Vec<String>,
    /// Linear-gradient orientation for multi-colour names.
    pub name_gradient_direction: Option<String>,
    /// Avatar frame key, reserved for a future feature.
    pub avatar_frame: Option<String>,
    /// `active` or `banned`.
    pub status: Option<String>,
    /// When a temporary ban lifts.
    pub banned_until: Option<String>,
    /// `true` for bot accounts (`bots.md` §1).
    pub is_bot: bool,
    /// The human owner of a bot account (`bots.md` §12).
    pub bot_owner_id: Option<Id>,
    /// Set when the account requested deletion.
    pub deleted_at: Option<String>,
    /// Last heartbeat timestamp.
    pub last_seen_at: Option<String>,
    /// Whether the user is currently online.
    pub online: bool,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

/// One of the caller's bots, as returned by `GET /api/v1/bots` (`bots.md` §4).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bot {
    /// Bot user id.
    pub id: Id,
    /// Bot handle.
    #[serde(deserialize_with = "de_string_or_null")]
    pub username: String,
    /// Bot display name.
    #[serde(deserialize_with = "de_string_or_null")]
    pub nickname: String,
    /// Bot avatar URL.
    #[serde(deserialize_with = "de_string_or_null")]
    pub avatar_url: String,
    /// When the bot account was created.
    pub created_at: Option<String>,
    /// When the current token was issued; falls back to `created_at` server-side.
    pub token_created_at: Option<String>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

/// The `POST /api/v1/bots` response.
///
/// The token appears here exactly once (`bots.md` §2.1); the server keeps only its
/// SHA-256 hash, so it can never be retrieved again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatedBot {
    /// The freshly created bot.
    pub user: User,
    /// `ofb_…` credential. Store it in a secret manager immediately.
    pub token: String,
}

impl CreatedBot {
    /// Parse `token` into a validated [`crate::config::BotToken`].
    pub fn bot_token(&self) -> crate::error::Result<crate::config::BotToken> {
        crate::config::BotToken::new(self.token.clone())
    }
}

/// The `GET /api/v1/bots` response envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BotList {
    /// The caller's bots.
    pub bots: Vec<Bot>,
    /// Server-side ceiling on bots per owner, `repository.MaxBotsPerOwner`.
    pub limit: i64,
}

impl Default for BotList {
    fn default() -> Self {
        Self {
            bots: Vec::new(),
            limit: 10,
        }
    }
}

/// Payload returned by `POST /bots/:id/regenerate`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenResponse {
    /// The new `ofb_…` token.
    pub token: String,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

/// Post visibility. The server accepts exactly these four (`allowedVisibilities`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum Visibility {
    /// Visible to everyone, including anonymous readers. The default.
    #[default]
    Public,
    /// Visible to any authenticated user.
    Login,
    /// Visible to mutual followers and the author.
    Friends,
    /// Visible only to the author.
    Private,
    /// A value this SDK version does not name.
    Unknown(String),
}

impl Visibility {
    /// Parse a wire value.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "public" => Self::Public,
            "login" => Self::Login,
            "friends" => Self::Friends,
            "private" => Self::Private,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// The wire value.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Public => "public",
            Self::Login => "login",
            Self::Friends => "friends",
            Self::Private => "private",
            Self::Unknown(other) => other,
        }
    }
}

impl std::fmt::Display for Visibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Visibility {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Visibility {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::parse(&raw))
    }
}

/// Message kind (`model.Message.Kind`).
///
/// The server additionally emits `system.unmute`, `system.mute.all` and
/// `system.unmute.all`, which [`MessageKind::is_system`] catches through the
/// `system.` prefix rather than by enumerating every value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum MessageKind {
    /// A normal user-authored message. The default.
    #[default]
    Text,
    /// A red packet / lottery message.
    Check,
    /// A member joined.
    SystemJoin,
    /// A member left.
    SystemLeave,
    /// A member was muted.
    SystemMute,
    /// Any other kind, raw value retained.
    Other(String),
}

impl MessageKind {
    /// Parse a wire value.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "text" => Self::Text,
            "check" => Self::Check,
            "system.join" => Self::SystemJoin,
            "system.leave" => Self::SystemLeave,
            "system.mute" => Self::SystemMute,
            other => Self::Other(other.to_string()),
        }
    }

    /// The wire value.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Text => "text",
            Self::Check => "check",
            Self::SystemJoin => "system.join",
            Self::SystemLeave => "system.leave",
            Self::SystemMute => "system.mute",
            Self::Other(other) => other,
        }
    }

    /// Whether this is a system notice that must never be treated as user input.
    pub fn is_system(&self) -> bool {
        match self {
            Self::SystemJoin | Self::SystemLeave | Self::SystemMute => true,
            Self::Other(raw) => raw.starts_with("system."),
            Self::Text | Self::Check => false,
        }
    }
}

impl std::fmt::Display for MessageKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for MessageKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MessageKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::parse(&raw))
    }
}

/// An attachment record (`model.Attachment`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Attachment {
    /// Attachment id, referenced from `attachment_ids` on posts and messages.
    pub id: Id,
    /// Owner user id.
    pub user_id: Id,
    /// Original file name as uploaded.
    #[serde(deserialize_with = "de_string_or_null")]
    pub original_name: String,
    /// MIME type reported by the server.
    #[serde(deserialize_with = "de_string_or_null")]
    pub mime_type: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Public download URL.
    #[serde(deserialize_with = "de_string_or_null")]
    pub url: String,
    /// 512px thumbnail URL, when generated.
    pub thumb_url: Option<String>,
    /// 1440px JPEG preview URL, when generated.
    pub preview_url: Option<String>,
    /// Attachment visibility; posts refuse non-public attachments.
    pub visibility: Option<String>,
    /// Logical storage bucket the file lives in (`storage-buckets.md`).
    pub bucket: Option<String>,
    /// Burn-after-view deadline, set the first time someone else views it.
    pub burn_at: Option<String>,
    /// RFC 3339 creation timestamp.
    pub created_at: Option<String>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Attachment {
    /// Original file name.
    pub fn file_name(&self) -> &str {
        &self.original_name
    }

    /// MIME type.
    pub fn content_type(&self) -> &str {
        &self.mime_type
    }

    /// Size in bytes.
    pub fn size(&self) -> i64 {
        self.size_bytes
    }
}

/// A post (`model.Post`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Post {
    /// Post id.
    pub id: Id,
    /// Author user id.
    pub user_id: Id,
    /// Post body.
    #[serde(deserialize_with = "de_string_or_null")]
    pub content: String,
    /// Visibility setting.
    pub visibility: Option<Visibility>,
    /// RFC 3339 creation timestamp.
    pub created_at: Option<String>,
    /// RFC 3339 last-edit timestamp.
    pub updated_at: Option<String>,
    /// Reply count.
    pub reply_count: Option<i64>,
    /// Total views.
    pub view_count: Option<i64>,
    /// Distinct viewers.
    pub unique_views: Option<i64>,
    /// Sum of non-refunded tips in cents.
    pub tip_total: Option<i64>,
    /// Reaction counts keyed by reaction name (`like`, `love`, ……).
    pub reactions: Extra,
    /// The caller's own reaction, when present.
    pub my_reaction: Option<String>,
    /// Whether the caller favourited this post.
    pub favorited: bool,
    /// Total favourites.
    pub favorite_count: Option<i64>,
    /// Author display name.
    pub nickname: Option<String>,
    /// Author avatar URL.
    pub avatar_url: Option<String>,
    /// Whether the author is verified.
    pub is_verified: bool,
    /// Whether the author is a bot — lets a bot filter its own content out.
    pub is_bot: bool,
    /// Author membership tier.
    pub member_level: Option<i64>,
    /// Whether the author's membership is active.
    pub member_active: bool,
    /// Author display-name colour.
    pub name_color: Option<String>,
    /// Author gradient end colour.
    pub name_color_to: Option<String>,
    /// Author animated-gradient flag.
    pub name_dynamic: bool,
    /// Author gradient colour list.
    pub name_colors: Vec<String>,
    /// Author gradient orientation.
    pub name_gradient_direction: Option<String>,
    /// Author avatar frame.
    pub avatar_frame: Option<String>,
    /// Populated attachments.
    pub attachments: Vec<Attachment>,
    /// Quoted post id; `0` when this is not a quote.
    pub quoted_post_id: Option<Id>,
    /// Nested quoted post, resolved one level deep.
    pub quoted_post: Option<Box<Post>>,
    /// Author-pinned flag.
    pub pinned: bool,
    /// Camp this post belongs to; `0` means the global feed.
    pub camp_id: Id,
    /// Whether a camp admin pinned this post inside its camp.
    pub camp_pinned: bool,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

/// A reply to a post (`model.PostReply`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reply {
    /// Reply id.
    pub id: Id,
    /// Post this reply belongs to.
    pub post_id: Id,
    /// Author user id.
    pub user_id: Id,
    /// Reply body.
    #[serde(deserialize_with = "de_string_or_null")]
    pub content: String,
    /// Parent reply id for threaded replies.
    pub parent_id: Option<Id>,
    /// RFC 3339 creation timestamp.
    pub created_at: Option<String>,
    /// RFC 3339 last-edit timestamp.
    pub updated_at: Option<String>,
    /// Soft-delete timestamp.
    pub deleted_at: Option<String>,
    /// Author handle.
    pub username: Option<String>,
    /// Author display name.
    pub nickname: Option<String>,
    /// Author avatar URL.
    pub avatar_url: Option<String>,
    /// Whether the author is verified.
    pub is_verified: bool,
    /// Whether the author is a bot.
    pub is_bot: bool,
    /// Author membership tier.
    pub member_level: Option<i64>,
    /// Whether the author's membership is active.
    pub member_active: bool,
    /// Preview of the parent reply's content.
    pub parent_content: Option<String>,
    /// Display name of the parent reply's author.
    pub parent_name: Option<String>,
    /// Whether the caller favourited this reply.
    pub favorited: bool,
    /// Total favourites on this reply.
    pub favorite_count: Option<i64>,
    /// Populated attachments.
    pub attachments: Vec<Attachment>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

/// A chat message (`model.Message`).
///
/// The fields a bot must branch on — `sender_id`, `sender_is_bot`, `kind`,
/// `mentions` — are typed and defaulted so the anti-self-loop checks in
/// `bots.md` §9.2 cannot be skipped by accident.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Message {
    /// Message id, monotonically increasing. Use it as the "is this new" cursor.
    pub id: Id,
    /// Conversation the message belongs to.
    pub conversation_id: Id,
    /// Sender user id. Compare against the bot's own id to avoid loops.
    pub sender_id: Id,
    /// Message kind. Never treat a system notice as user input.
    pub kind: MessageKind,
    /// Text body; ciphertext in an encrypted conversation (`bots.md` §11).
    #[serde(deserialize_with = "de_string_or_null")]
    pub content: String,
    /// Red packet / lottery id, when attached.
    pub check_id: Option<Id>,
    /// Message this one replies to.
    pub reply_to_id: Option<Id>,
    /// Last edit timestamp.
    pub edited_at: Option<String>,
    /// Soft-delete timestamp; list endpoints already filter deleted messages.
    pub deleted_at: Option<String>,
    /// RFC 3339 creation timestamp.
    pub created_at: Option<String>,
    /// Display name of the sender.
    pub sender_name: Option<String>,
    /// Avatar URL of the sender.
    pub sender_avatar: Option<String>,
    /// Whether the sender is verified.
    pub sender_verified: bool,
    /// Whether the sender is a bot. Ignore other bots to avoid ping-pong.
    pub sender_is_bot: bool,
    /// Sender membership tier.
    pub sender_member_level: Option<i64>,
    /// Whether the sender's membership is active.
    pub sender_member_active: bool,
    /// Sender display-name colour.
    pub sender_name_color: Option<String>,
    /// Sender gradient end colour.
    pub sender_name_color_to: Option<String>,
    /// Sender animated-gradient flag.
    pub sender_name_dynamic: bool,
    /// Sender gradient colour list.
    pub sender_name_colors: Vec<String>,
    /// Sender gradient orientation.
    pub sender_name_gradient_direction: Option<String>,
    /// Sender avatar frame.
    pub sender_avatar_frame: Option<String>,
    /// Display name of the replied-to message's author.
    pub reply_to_name: Option<String>,
    /// Preview of the replied-to message's content.
    pub reply_to_content: Option<String>,
    /// Populated attachments.
    ///
    /// The output model has no `attachment_ids`; ids only ever travel in the
    /// request body. Use [`Message::attachment_ids`] to read them back.
    pub attachments: Vec<Attachment>,
    /// User ids mentioned; `-1` means `@everyone`.
    pub mentions: Vec<Id>,
    /// Burn-after-reading window in seconds; `0` means not armed.
    pub burn_seconds: i64,
    /// When the message expires, stamped on first read by a recipient.
    pub burn_at: Option<String>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Message {
    /// `true` when `-1` (`@everyone`) is among the mentions.
    pub fn mentions_everyone(&self) -> bool {
        self.mentions.contains(&MENTION_EVERYONE)
    }

    /// Whether this message pings `user_id` directly or through `@everyone`.
    pub fn mentions_user(&self, user_id: Id) -> bool {
        self.mentions.contains(&user_id) || self.mentions_everyone()
    }

    /// The anti-self-loop predicate from `bots.md` §9.2.
    ///
    /// `false` for the bot's own messages, for messages from other bots, and for
    /// anything that is not a plain text message.
    pub fn is_actionable_for(&self, bot_id: Id) -> bool {
        self.sender_id != bot_id && !self.sender_is_bot && self.kind == MessageKind::Text
    }

    /// Attachment ids carried by this message.
    pub fn attachment_ids(&self) -> Vec<Id> {
        self.attachments
            .iter()
            .map(|attachment| attachment.id)
            .collect()
    }
}

/// Mention id meaning `@everyone` (`bots.md` §7.1).
pub const MENTION_EVERYONE: Id = -1;

/// A conversation (direct chat or group) — `model.Conversation`.
///
/// The discriminator is `type` (`private` | `group`) and the label is `title`;
/// `bots.md` describes them as `kind`/`name`, which the source contradicts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Conversation {
    /// Conversation id.
    pub id: Id,
    /// `private` or `group`.
    #[serde(rename = "type", alias = "kind")]
    pub kind: Option<String>,
    /// Group title; empty for a direct chat.
    #[serde(alias = "name", deserialize_with = "de_string_or_null")]
    pub title: String,
    /// Group avatar URL.
    #[serde(deserialize_with = "de_string_or_null")]
    pub avatar_url: String,
    /// Owner user id for groups.
    pub owner_id: Id,
    /// RFC 3339 creation timestamp.
    pub created_at: Option<String>,
    /// RFC 3339 last-activity timestamp.
    pub updated_at: Option<String>,
    /// Whether the group is searchable and viewable by non-members.
    pub is_public: bool,
    /// Whether public groups may be joined without an invitation.
    pub allow_join: bool,
    /// While in the future, every non-staff member is muted.
    pub mute_all_until: Option<String>,
    /// `true` for end-to-end encrypted conversations (`bots.md` §11).
    pub encrypted: bool,
    /// Member count, populated on public group listings.
    pub member_count: Option<i64>,
    /// Whether the caller belongs to the group.
    pub is_member: bool,
    /// Latest message preview, for list endpoints.
    pub last_message: Option<Message>,
    /// Unread count for the caller.
    #[serde(alias = "unread_count")]
    pub unread: Option<i64>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Conversation {
    /// The group title (`""` for a direct chat). Alias kept for `bots.md` readers.
    pub fn name(&self) -> &str {
        &self.title
    }

    /// Whether this is an end-to-end encrypted conversation.
    ///
    /// `bots.md` §11: skip these unless the bot implements the group-key flow,
    /// otherwise `content` is ciphertext.
    pub fn is_encrypted(&self) -> bool {
        self.encrypted
    }

    /// Whether this is a group conversation.
    pub fn is_group(&self) -> bool {
        self.kind.as_deref() == Some("group")
    }
}

/// `GET /conversations/:id` — the handler always answers with a wrapper carrying
/// `conversation`, `members` and `my_membership`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationBody {
    /// The conversation.
    pub conversation: Conversation,
    /// Members, when returned.
    #[serde(default)]
    pub members: Vec<Value>,
    /// The caller's own membership row.
    #[serde(default)]
    pub my_membership: Option<Value>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

/// Either documented shape of `GET /conversations/:id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConversationDetail {
    /// The documented wrapper (`conversation` + `members` + `my_membership`).
    Wrapped(ConversationBody),
    /// A bare conversation object.
    Direct(Conversation),
}

impl ConversationDetail {
    /// The conversation, whichever shape arrived.
    pub fn conversation(&self) -> &Conversation {
        match self {
            Self::Wrapped(body) => &body.conversation,
            Self::Direct(conversation) => conversation,
        }
    }

    /// Whether the conversation is end-to-end encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.conversation().encrypted
    }

    /// The roster, when the wrapped shape was used.
    pub fn members(&self) -> &[Value] {
        match self {
            Self::Wrapped(body) => &body.members,
            Self::Direct(_) => &[],
        }
    }
}

/// A camp member entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CampMember {
    /// The membership row id.
    pub id: Option<Id>,
    /// The user's id.
    pub user_id: Option<Id>,
    /// Camp id.
    pub camp_id: Option<Id>,
    /// Nested user object, when the roster inlines one.
    pub user: Option<User>,
    /// Role within the camp: `owner` / `admin` / `member`.
    pub role: Option<String>,
    /// RFC 3339 join timestamp.
    pub joined_at: Option<String>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

impl CampMember {
    /// The member's user id, whichever field carried it.
    pub fn member_id(&self) -> Option<Id> {
        self.user_id
            .or_else(|| self.user.as_ref().map(|user| user.id))
            .or(self.id)
    }

    /// Whether this member is the camp creator.
    pub fn is_owner(&self) -> bool {
        self.role.as_deref() == Some("owner")
    }

    /// Whether this member is an admin or the owner.
    pub fn is_staff(&self) -> bool {
        matches!(self.role.as_deref(), Some("owner") | Some("admin"))
    }
}

/// A camp (community group) — `model.Camp`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Camp {
    /// Camp id.
    pub id: Id,
    /// Camp name; unique server-side, so a duplicate is a `409`.
    #[serde(deserialize_with = "de_string_or_null")]
    pub name: String,
    /// Description.
    #[serde(deserialize_with = "de_string_or_null")]
    pub description: String,
    /// Creator user id.
    pub creator_id: Id,
    /// Creator display name.
    pub creator_name: Option<String>,
    /// Visibility. Subgroups are forced to `false` by construction.
    pub is_visible: bool,
    /// Whether anyone may self-join. Subgroups force this to `false`.
    pub direct_join: bool,
    /// Parent camp for a subgroup; `0` or absent for a top-level camp.
    pub parent_camp_id: Option<Id>,
    /// Whether plain members may publish posts. Defaults to `true`.
    pub member_post: bool,
    /// Whether plain members may pin their own posts. Defaults to `false`.
    pub member_pin: bool,
    /// Short notice shown inside the camp.
    pub announcement: Option<String>,
    /// Member count.
    pub member_count: Option<i64>,
    /// Post count.
    pub post_count: Option<i64>,
    /// Whether the caller is a member.
    pub is_member: bool,
    /// The caller's role: `owner`, `admin`, `member`, or absent for non-members.
    pub my_role: Option<String>,
    /// RFC 3339 creation timestamp.
    pub created_at: Option<String>,
    /// RFC 3339 last-edit timestamp.
    pub updated_at: Option<String>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Camp {
    /// Whether the camp is hidden from the public directory.
    pub fn is_hidden(&self) -> bool {
        !self.is_visible
    }

    /// Whether plain members may post.
    pub fn members_can_post(&self) -> bool {
        self.member_post
    }

    /// Whether this camp is a subgroup of another camp.
    pub fn is_subgroup(&self) -> bool {
        self.parent_camp_id.is_some_and(|parent| parent != 0)
    }
}

/// A notification inbox entry (`repository.Notification`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notification {
    /// Notification id.
    pub id: Id,
    /// Notification type, emitted as `type`.
    #[serde(
        rename = "type",
        alias = "kind",
        deserialize_with = "de_string_or_null"
    )]
    pub kind: String,
    /// Title.
    pub title: Option<String>,
    /// Body text.
    pub body: Option<String>,
    /// Structured payload for the notification.
    pub data: Option<Value>,
    /// When the notification was read.
    pub read_at: Option<String>,
    /// RFC 3339 creation timestamp.
    pub created_at: Option<String>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Notification {
    /// Whether the notification has been read.
    pub fn is_read(&self) -> bool {
        self.read_at.is_some()
    }

    /// Body text, falling back to the title.
    pub fn text(&self) -> &str {
        self.body
            .as_deref()
            .filter(|body| !body.is_empty())
            .or(self.title.as_deref())
            .unwrap_or("")
    }
}

/// A submitted moderation report (`model.Report`, `reports.md`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    /// Report id.
    pub id: Id,
    /// Reporter user id.
    pub reporter_id: Id,
    /// Reporter display name resolved server-side.
    pub reporter_name: Option<String>,
    /// `post` | `message` | `user`.
    #[serde(deserialize_with = "de_string_or_null")]
    pub target_type: String,
    /// Reported resource id.
    pub target_id: Id,
    /// The reason text supplied when reporting.
    #[serde(deserialize_with = "de_string_or_null")]
    pub reason: String,
    /// `pending` | `reviewed` | `dismissed`.
    #[serde(deserialize_with = "de_string_or_null")]
    pub status: String,
    /// Panel account id that handled it, `0` while pending.
    pub reviewer_id: Option<Id>,
    /// Operator name, denormalised so history survives account changes.
    pub reviewer_username: Option<String>,
    /// Optional operator note.
    pub review_note: Option<String>,
    /// RFC 3339 creation timestamp.
    pub created_at: Option<String>,
    /// When the report was resolved.
    pub reviewed_at: Option<String>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Report {
    /// Whether the report is still open.
    pub fn is_pending(&self) -> bool {
        self.status == "pending"
    }
}

/// Target kinds accepted by `POST /api/v1/reports`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReportTarget {
    /// A post.
    Post,
    /// A chat message.
    Message,
    /// A user.
    User,
}

impl ReportTarget {
    /// The wire value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Post => "post",
            Self::Message => "message",
            Self::User => "user",
        }
    }
}

/// A reaction key accepted by `PUT /posts/:id/reactions`.
///
/// The server validates against `allowedReactions`, which holds exactly these seven
/// values. Validating locally turns a round trip and a `400 unknown reaction` into
/// an immediate [`crate::ErrorKind::InvalidArgument`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reaction {
    /// A like.
    Like,
    /// A dislike.
    Dislike,
    /// Love.
    Love,
    /// Laughter.
    Haha,
    /// Surprise.
    Wow,
    /// Sadness.
    Sad,
    /// Anger.
    Angry,
}

impl Reaction {
    /// The wire value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Like => "like",
            Self::Dislike => "dislike",
            Self::Love => "love",
            Self::Haha => "haha",
            Self::Wow => "wow",
            Self::Sad => "sad",
            Self::Angry => "angry",
        }
    }

    /// Every accepted reaction key, matching `allowedReactions`.
    pub const ALL: [Reaction; 7] = [
        Reaction::Like,
        Reaction::Dislike,
        Reaction::Love,
        Reaction::Haha,
        Reaction::Wow,
        Reaction::Sad,
        Reaction::Angry,
    ];

    /// Parse a reaction key.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|r| r.as_str() == raw)
    }
}

/// A permission key held by the caller (`bots.md` appendix B).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(transparent)]
pub struct Permission(pub String);

impl Permission {
    /// Dot-separated key, e.g. `chat.send`.
    pub fn key(&self) -> &str {
        &self.0
    }

    /// Whether the key is one of the seven administrative keys (`bots.md` §5).
    pub fn is_administrative(&self) -> bool {
        is_administrative_permission(&self.0)
    }
}

/// The seven management permission keys a new bot never gets (`bots.md` §5).
pub const ADMINISTRATIVE_PERMISSIONS: [&str; 7] = [
    "account.verify",
    "user.adjust_exp",
    "user.membership.grant",
    "user.punish",
    "wallet.manage",
    "plugin.manage",
    "app.announcements.manage",
];

/// Whether `key` is an administrative permission.
pub fn is_administrative_permission(key: &str) -> bool {
    ADMINISTRATIVE_PERMISSIONS.contains(&key)
}

/// The `/users/me/permissions` response.
///
/// The handler answers `{"permissions": [...], "groups": [...]}`; a bare list is
/// also accepted because `bots.md` §5 implies one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PermissionSet {
    /// A bare array of keys, as strings or as objects.
    List(Vec<Value>),
    /// The documented envelope.
    Envelope {
        /// The keys.
        permissions: Vec<Value>,
        /// The groups that granted them.
        #[serde(default)]
        groups: Vec<Value>,
    },
}

impl PermissionSet {
    /// Flatten into [`Permission`] values, accepting bare strings and objects with
    /// a `key` / `name` / `permission` field.
    pub fn permissions(&self) -> Vec<Permission> {
        self.items()
            .iter()
            .filter_map(|item| match item {
                Value::String(key) => Some(Permission(key.clone())),
                Value::Object(map) => map
                    .get("key")
                    .or_else(|| map.get("name"))
                    .or_else(|| map.get("permission"))
                    .and_then(Value::as_str)
                    .map(|key| Permission(key.to_string())),
                _ => None,
            })
            .collect()
    }

    /// Group names, when the server returned them.
    pub fn groups(&self) -> Vec<String> {
        match self {
            Self::List(_) => Vec::new(),
            Self::Envelope { groups, .. } => groups
                .iter()
                .filter_map(|group| match group {
                    Value::String(name) => Some(name.clone()),
                    Value::Object(map) => map
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|name| name.to_string()),
                    _ => None,
                })
                .collect(),
        }
    }

    /// Whether the caller holds `key`.
    pub fn contains(&self, key: &str) -> bool {
        self.permissions()
            .iter()
            .any(|permission| permission.key() == key)
    }

    fn items(&self) -> &[Value] {
        match self {
            Self::List(items) => items,
            Self::Envelope { permissions, .. } => permissions,
        }
    }
}

/// A page of posts — the handler answers `{posts, page, limit, query, tag}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostPage {
    /// Posts on this page.
    pub posts: Vec<Post>,
    /// The page number echoed back.
    pub page: i64,
    /// The page size echoed back.
    pub limit: Option<i64>,
    /// The `q` filter echoed back.
    pub query: Option<String>,
    /// The `tag` filter echoed back.
    pub tag: Option<String>,
    /// Additional fields some builds add (for example `total`).
    #[serde(flatten)]
    pub extra: Extra,
}

impl PostPage {
    /// Whether the page carried no posts, which `bots.md` §7.3 says means the end.
    pub fn is_empty(&self) -> bool {
        self.posts.is_empty()
    }
}

/// A page of replies — the handler answers `{replies, page, limit}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplyPage {
    /// Replies on this page.
    pub replies: Vec<Reply>,
    /// The page number echoed back.
    pub page: i64,
    /// The page size echoed back.
    pub limit: Option<i64>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

/// A page of messages.
///
/// **Ordering.** The repository query is `ORDER BY m.created_at DESC, m.id DESC`,
/// so messages arrive **newest first** — which is what `API.md` says and the
/// opposite of `bots.md` §7.3. The Go doc comment on `ListByConversation` still
/// claims "newest last", so the comment and the SQL disagree too.
/// [`MessagePage::newest`] therefore picks by **maximum id** rather than by
/// position, which is correct under either ordering.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessagePage {
    /// Messages.
    pub messages: Vec<Message>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

impl MessagePage {
    /// The newest message, chosen by greatest id so it is order-independent.
    pub fn newest(&self) -> Option<&Message> {
        self.messages.iter().max_by_key(|message| message.id)
    }

    /// The oldest message, chosen by smallest id.
    pub fn oldest(&self) -> Option<&Message> {
        self.messages.iter().min_by_key(|message| message.id)
    }

    /// Alias for [`MessagePage::newest`].
    ///
    /// Kept because `bots.md` §9.2 writes `messages[-1]`; prefer `newest`, which
    /// does not depend on the ordering.
    pub fn latest(&self) -> Option<&Message> {
        self.newest()
    }

    /// The largest message id on this page; `0` when empty. The dedupe cursor.
    pub fn max_id(&self) -> Id {
        self.messages
            .iter()
            .map(|message| message.id)
            .max()
            .unwrap_or(0)
    }

    /// Messages in ascending id order, which is the order a bot should process them.
    pub fn ascending(&self) -> Vec<&Message> {
        let mut messages: Vec<&Message> = self.messages.iter().collect();
        messages.sort_by_key(|message| message.id);
        messages
    }

    /// Whether the page carried no messages.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

/// The `GET /conversations` response — `{"conversations": [...]}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConversationList {
    /// The caller's conversations.
    pub conversations: Vec<Conversation>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

/// A consent request (`model.ConsentRequest`).
///
/// Chat is consent-based, so a bot's `POST /conversations/start` and
/// `POST /conversations/:id/invite` call is answered by the *target* accepting or
/// declining one of these.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsentRequest {
    /// Request id, used for accept/decline.
    pub id: Id,
    /// `private_chat` or `group_invite`, emitted as `type`.
    #[serde(
        rename = "type",
        alias = "kind",
        deserialize_with = "de_string_or_null"
    )]
    pub kind: String,
    /// Who sent it.
    pub requester_id: Id,
    /// Who must answer.
    pub target_user_id: Id,
    /// Conversation the invite concerns.
    pub conversation_id: Option<Id>,
    /// Group the invite concerns.
    pub group_id: Option<Id>,
    /// Opening message.
    #[serde(deserialize_with = "de_string_or_null")]
    pub message: String,
    /// `pending` | `accepted` | `declined`.
    #[serde(deserialize_with = "de_string_or_null")]
    pub status: String,
    /// RFC 3339 creation timestamp.
    pub created_at: Option<String>,
    /// When the target answered.
    pub responded_at: Option<String>,
    /// Requester display name.
    pub requester_name: Option<String>,
    /// Requester avatar URL.
    pub requester_avatar: Option<String>,
    /// Group title for a group invite.
    pub group_title: Option<String>,
    /// Whether the requester asked for an end-to-end encrypted private chat.
    pub encrypted: bool,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

impl ConsentRequest {
    /// Whether the request is still awaiting an answer.
    pub fn is_pending(&self) -> bool {
        self.status == "pending"
    }
}

/// The `GET /consent-requests` response — `{"requests": [...]}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsentRequestList {
    /// Pending requests.
    pub requests: Vec<ConsentRequest>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

/// The `GET /attachments` response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AttachmentList {
    /// Attachments.
    pub attachments: Vec<Attachment>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

/// The `GET /camps` response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CampList {
    /// Camps.
    pub camps: Vec<Camp>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

/// The `GET /notifications` response, tolerating a bare array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NotificationList {
    /// A bare array.
    List(Vec<Notification>),
    /// The envelope form.
    Envelope {
        /// Notifications.
        notifications: Vec<Notification>,
    },
}

impl NotificationList {
    /// The notifications, whichever shape arrived.
    pub fn notifications(&self) -> Vec<Notification> {
        match self {
            Self::List(items) => items.clone(),
            Self::Envelope { notifications } => notifications.clone(),
        }
    }
}

/// The `GET /reports/mine` response, tolerating a bare array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReportList {
    /// A bare array of reports.
    List(Vec<Report>),
    /// The envelope form.
    Envelope {
        /// Reports.
        reports: Vec<Report>,
    },
}

impl ReportList {
    /// The reports, whichever shape arrived.
    pub fn reports(&self) -> Vec<Report> {
        match self {
            Self::List(reports) => reports.clone(),
            Self::Envelope { reports } => reports.clone(),
        }
    }
}

/// `GET /storage/usage`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageUsage {
    /// Bytes stored.
    pub used: Option<i64>,
    /// Alternate spelling.
    pub used_bytes: Option<i64>,
    /// Alternate spelling.
    pub storage_used: Option<i64>,
    /// Quota in bytes, when reported here.
    pub quota: Option<i64>,
    /// Alternate spelling.
    pub storage_quota: Option<i64>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

impl StorageUsage {
    /// Bytes stored, whichever spelling the build used.
    pub fn used_bytes(&self) -> Option<i64> {
        self.used.or(self.used_bytes).or(self.storage_used)
    }

    /// Quota in bytes, whichever spelling the build used.
    pub fn quota_bytes(&self) -> Option<i64> {
        self.quota.or(self.storage_quota)
    }

    /// Remaining bytes, when both numbers are known.
    pub fn remaining_bytes(&self) -> Option<i64> {
        match (self.quota_bytes(), self.used_bytes()) {
            (Some(quota), Some(used)) => Some((quota - used).max(0)),
            _ => None,
        }
    }
}

/// One logical storage bucket (`storage-buckets.md`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageBucket {
    /// Logical bucket id, the only value clients send.
    pub name: String,
    /// Human label.
    pub label: Option<String>,
    /// Default quota in bytes for users on this bucket.
    pub default_quota: Option<i64>,
    /// Minimum membership level required; `0` means everyone.
    pub min_member_level: Option<i64>,
    /// Whether this is the default bucket.
    pub is_default: bool,
    /// Whether the requesting user may switch to it.
    pub locked: bool,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

/// The `GET /users/storage-buckets` response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageBucketList {
    /// Available buckets.
    pub buckets: Vec<StorageBucket>,
    /// The caller's current bucket.
    pub current: Option<String>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

/// One service entry inside a [`HealthReport`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceHealth {
    /// `up` | `down` | `degraded` | `unknown`.
    #[serde(deserialize_with = "de_string_or_null")]
    pub status: String,
    /// Measured latency of the probe.
    pub latency_ms: Option<i64>,
    /// Upstream detail map, e.g. `{"database":"up"}`.
    pub details: Extra,
    /// Failure text when the probe could not complete.
    pub error: Option<String>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

impl ServiceHealth {
    /// Whether the service reported `up`.
    pub fn is_up(&self) -> bool {
        self.status == "up"
    }
}

/// `GET /api/v1/health` — the gateway's aggregate probe (`health.md`).
///
/// The endpoint answers `200` when everything is healthy and `503` otherwise, but
/// the body is parseable either way, so the SDK accepts both statuses here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HealthReport {
    /// Whether every backend answered `up`.
    pub all_healthy: bool,
    /// When the gateway ran the probes.
    pub checked_at: Option<String>,
    /// Per-service results, keyed by service name.
    pub services: Map<String, Value>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

impl HealthReport {
    /// A service's report, decoded on demand.
    pub fn service(&self, name: &str) -> Option<ServiceHealth> {
        self.services
            .get(name)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    }

    /// Names of the services that are not `up`, sorted.
    pub fn unhealthy_services(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .services
            .iter()
            .filter(|(_, value)| {
                value
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status != "up")
            })
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }
}

/// A server announcement.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Announcement {
    /// Announcement id, used by clients to remember dismissals.
    pub id: Id,
    /// Title.
    pub title: Option<String>,
    /// Body.
    pub content: Option<String>,
    /// Whether the notice is currently active.
    pub active: bool,
    /// RFC 3339 creation timestamp.
    pub created_at: Option<String>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

/// The `GET /announcements` response, tolerating a bare array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnnouncementList {
    /// A bare array.
    List(Vec<Announcement>),
    /// The envelope form.
    Envelope {
        /// Announcements.
        announcements: Vec<Announcement>,
    },
}

impl AnnouncementList {
    /// The announcements, whichever shape arrived.
    pub fn announcements(&self) -> Vec<Announcement> {
        match self {
            Self::List(items) => items.clone(),
            Self::Envelope { announcements } => announcements.clone(),
        }
    }
}

/// A chunked upload session (`POST /attachments/chunk/init`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChunkedUpload {
    /// Upload session id, used by the per-chunk and complete calls.
    pub upload_id: Option<String>,
    /// Total chunks the server expects.
    pub total_chunks: Option<i64>,
    /// Bytes already received.
    pub received_bytes: Option<i64>,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Extra,
}

impl ChunkedUpload {
    /// The upload id under either documented spelling.
    pub fn id(&self) -> Option<&str> {
        self.upload_id
            .as_deref()
            .or_else(|| self.extra.get("id").and_then(Value::as_str))
    }
}

/// Server-side optional features, from the unauthenticated `GET /capabilities`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Capabilities(pub Value);

impl Capabilities {
    /// Whether the server advertises `feature`, checking the documented shapes:
    /// `{"features":["a"]}`, `{"features":{"a":true}}`, `["a"]`, `{"a":true}`.
    pub fn supports(&self, feature: &str) -> bool {
        match &self.0 {
            Value::Array(items) => items.iter().any(|value| value.as_str() == Some(feature)),
            Value::Object(map) => {
                if let Some(features) = map.get("features") {
                    if Self(features.clone()).supports(feature) {
                        return true;
                    }
                }
                match map.get(feature) {
                    Some(Value::Bool(flag)) => *flag,
                    Some(Value::String(text)) => !text.is_empty() && text != "disabled",
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// The raw document.
    pub fn raw(&self) -> &Value {
        &self.0
    }
}

/// The `POST /attachments` result — the handler answers with the attachment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttachmentResponse {
    /// An envelope wrapping the attachment.
    ///
    /// Listed before [`AttachmentResponse::Direct`] on purpose: `Attachment` defaults
    /// every field and keeps the rest in `extra`, so it would happily swallow an
    /// envelope object as a direct attachment with id `0`.
    Envelope {
        /// The attachment.
        attachment: Attachment,
    },
    /// The attachment object itself.
    Direct(Attachment),
}

impl AttachmentResponse {
    /// The attachment, whichever envelope was used.
    pub fn attachment(&self) -> &Attachment {
        match self {
            Self::Direct(attachment) => attachment,
            Self::Envelope { attachment } => attachment,
        }
    }

    /// The attachment id.
    pub fn id(&self) -> Id {
        self.attachment().id
    }
}

/// A generic JSON object, for endpoints whose body is `{"ok":true}` or an envelope
/// this SDK version does not model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct JsonObject(pub Map<String, Value>);

impl JsonObject {
    /// Whether this object carries `ok: true`.
    pub fn is_ok(&self) -> bool {
        matches!(self.0.get("ok"), Some(Value::Bool(true)))
    }

    /// Look up an arbitrary field.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// A borrowed string field.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    /// A borrowed integer field.
    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.0.get(key).and_then(Value::as_i64)
    }

    /// The underlying map.
    pub fn as_map(&self) -> &Map<String, Value> {
        &self.0
    }
}

/// Deserialize a field that may arrive as a number or a numeric string.
///
/// Used by the language bindings, which accept ids from loosely typed callers.
pub fn de_id_flexible<'de, D>(deserializer: D) -> Result<Option<Id>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_i64()
            .map(Some)
            .ok_or_else(|| de::Error::custom("id number is out of range")),
        Some(Value::String(text)) => text
            .trim()
            .parse::<Id>()
            .map(Some)
            .map_err(|_| de::Error::custom("id string is not an integer")),
        Some(other) => Err(de::Error::custom(format!("unexpected id value: {other}"))),
    }
}

/// Parse an id from a JSON value, accepting a number or a numeric string.
pub fn id_from_value(value: &Value) -> Option<Id> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// Deserialize a byte string from either a JSON array of octets or a base64 string.
///
/// The bindings need both: an array is the obvious JSON representation, but a base64
/// string is what keeps a 2 MB upload from becoming a two-million-element array on the
/// wire.
pub fn de_bytes_flexible<'de, D>(deserializer: D) -> std::result::Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_u64()
                    .filter(|octet| *octet <= u8::MAX as u64)
                    .map(|octet| octet as u8)
                    .ok_or_else(|| de::Error::custom("`bytes` must contain octets 0..=255"))
            })
            .collect(),
        Value::String(text) => {
            base64_decode(&text).ok_or_else(|| de::Error::custom("`bytes` is not valid base64"))
        }
        _ => Err(de::Error::custom(
            "`bytes` must be an array of octets or a base64 string",
        )),
    }
}

/// Standard base64 decoding, so a binding can pass a file without a giant JSON array.
///
/// Returns `None` for anything that is not valid base64, including a bad length or an
/// illegal character.
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn value_of(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let cleaned: Vec<u8> = input
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    for chunk in cleaned.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let mut buffer = [0u8; 4];
        let mut padding = 0usize;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte == b'=' {
                padding += 1;
                buffer[index] = 0;
            } else {
                buffer[index] = value_of(*byte)?;
            }
        }
        let triple = (u32::from(buffer[0]) << 18)
            | (u32::from(buffer[1]) << 12)
            | (u32::from(buffer[2]) << 6)
            | u32::from(buffer[3]);
        out.push((triple >> 16) as u8);
        if padding < 2 && chunk.len() > 2 {
            out.push((triple >> 8) as u8);
        }
        if padding < 1 && chunk.len() > 3 {
            out.push(triple as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_create_bot_response() {
        let raw = r#"{
            "user": {
                "id": 4242, "username": "weather_watcher", "nickname": "天气预报",
                "is_bot": true, "bot_owner_id": 7, "created_at": "2026-10-10T15:20:12Z"
            },
            "token": "ofb_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        }"#;
        let created: CreatedBot = serde_json::from_str(raw).unwrap();
        assert_eq!(created.user.id, 4242);
        assert_eq!(created.user.nickname, "天气预报");
        assert!(created.user.is_bot);
        assert_eq!(created.user.bot_owner_id, Some(7));
        assert!(created.bot_token().is_ok());
    }

    #[test]
    fn parses_a_bot_list_row() {
        let raw = r#"{"bots":[{"id":4242,"username":"w","nickname":"天气预报","avatar_url":"",
            "created_at":"2026-10-10T15:20:12Z","token_created_at":"2026-10-10T15:20:12Z"}],
            "limit":10}"#;
        let list: BotList = serde_json::from_str(raw).unwrap();
        assert_eq!(list.bots.len(), 1);
        assert_eq!(list.limit, 10);
        assert_eq!(list.bots[0].id, 4242);
    }

    #[test]
    fn bot_list_defaults_the_limit_when_absent() {
        let list: BotList = serde_json::from_str(r#"{"bots":[]}"#).unwrap();
        assert_eq!(list.limit, 10, "repository.MaxBotsPerOwner is 10");
    }

    #[test]
    fn parses_the_documented_message() {
        let raw = r#"{
            "id": 9001, "conversation_id": 33, "sender_id": 7, "kind": "text",
            "content": "你好", "created_at": "2026-10-10T15:22:41Z"
        }"#;
        let msg: Message = serde_json::from_str(raw).unwrap();
        assert_eq!(msg.id, 9001);
        assert_eq!(msg.conversation_id, 33);
        assert_eq!(msg.kind, MessageKind::Text);
        assert!(!msg.sender_is_bot);
        assert!(msg.mentions.is_empty());
        assert!(msg.extra.is_empty());
    }

    #[test]
    fn message_carries_populated_attachments_not_ids() {
        let raw = r#"{"id":1,"sender_id":2,"kind":"text","content":"x",
            "attachments":[{"id":9,"user_id":2,"original_name":"a.png","mime_type":"image/png",
            "size_bytes":10,"url":"http://x/a.png"}]}"#;
        let msg: Message = serde_json::from_str(raw).unwrap();
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachment_ids(), vec![9]);
        assert_eq!(msg.attachments[0].file_name(), "a.png");
    }

    #[test]
    fn unknown_message_fields_are_preserved_and_round_trip() {
        let raw = r#"{"id":1,"sender_id":2,"kind":"text","content":"x","weird_field":"alice"}"#;
        let msg: Message = serde_json::from_str(raw).unwrap();
        assert_eq!(msg.extra.get("weird_field").unwrap(), "alice");
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["weird_field"], "alice");
    }

    #[test]
    fn unknown_message_kinds_keep_their_raw_value() {
        let msg: Message =
            serde_json::from_str(r#"{"id":1,"sender_id":2,"kind":"system.mute.all","content":""}"#)
                .unwrap();
        assert_eq!(msg.kind, MessageKind::Other("system.mute.all".into()));
        assert_eq!(msg.kind.as_str(), "system.mute.all");
        assert!(msg.kind.is_system(), "any system.* kind is a system notice");
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["kind"], "system.mute.all");
    }

    #[test]
    fn every_documented_system_kind_is_recognised() {
        for kind in [
            "system.join",
            "system.leave",
            "system.mute",
            "system.unmute",
            "system.mute.all",
            "system.unmute.all",
        ] {
            let raw = format!(r#"{{"id":1,"sender_id":2,"kind":"{kind}","content":""}}"#);
            let msg: Message = serde_json::from_str(&raw).unwrap();
            assert!(msg.kind.is_system(), "{kind} must be a system kind");
            assert!(!msg.is_actionable_for(99));
        }
    }

    #[test]
    fn anti_self_loop_predicate_matches_the_docs() {
        let base = Message {
            id: 1,
            conversation_id: 1,
            sender_id: 42,
            sender_is_bot: true,
            ..Default::default()
        };
        assert!(!base.is_actionable_for(99), "another bot must be ignored");

        let mut mine = base.clone();
        mine.sender_is_bot = false;
        assert!(
            !mine.is_actionable_for(42),
            "the bot's own message must be skipped"
        );

        let mut human = mine.clone();
        human.sender_id = 7;
        assert!(human.is_actionable_for(42));

        let mut system = human.clone();
        system.kind = MessageKind::SystemJoin;
        assert!(
            !system.is_actionable_for(42),
            "system notices are not commands"
        );
    }

    #[test]
    fn mentions_helpers_cover_everyone() {
        let mut msg: Message = serde_json::from_str(
            r#"{"id":1,"sender_id":2,"kind":"text","content":"x","mentions":[42,7]}"#,
        )
        .unwrap();
        assert!(msg.mentions_user(42));
        assert!(!msg.mentions_user(8));
        assert!(!msg.mentions_everyone());
        msg.mentions.push(MENTION_EVERYONE);
        assert!(msg.mentions_everyone());
        assert!(msg.mentions_user(8), "@everyone pings every user");
    }

    #[test]
    fn message_page_is_order_independent() {
        // The repository returns DESC; bots.md claims ASC. Both must work.
        let ascending = r#"{"messages":[{"id":10,"sender_id":1,"kind":"text","content":"a"},
                                       {"id":12,"sender_id":1,"kind":"text","content":"b"}]}"#;
        let descending = r#"{"messages":[{"id":12,"sender_id":1,"kind":"text","content":"b"},
                                        {"id":10,"sender_id":1,"kind":"text","content":"a"}]}"#;
        for raw in [ascending, descending] {
            let page: MessagePage = serde_json::from_str(raw).unwrap();
            assert_eq!(page.newest().unwrap().id, 12, "failed for {raw}");
            assert_eq!(page.oldest().unwrap().id, 10, "failed for {raw}");
            assert_eq!(page.max_id(), 12, "failed for {raw}");
            let ids: Vec<Id> = page.ascending().iter().map(|message| message.id).collect();
            assert_eq!(ids, vec![10, 12], "ascending() normalises {raw}");
        }
    }

    #[test]
    fn empty_message_page_reports_a_zero_cursor() {
        let page: MessagePage = serde_json::from_str(r#"{"messages":[]}"#).unwrap();
        assert_eq!(page.max_id(), 0);
        assert!(page.newest().is_none());
        assert!(page.is_empty());
    }

    #[test]
    fn visibility_round_trips_and_tolerates_unknown_values() {
        for (text, variant) in [
            ("public", Visibility::Public),
            ("login", Visibility::Login),
            ("friends", Visibility::Friends),
            ("private", Visibility::Private),
        ] {
            let parsed: Visibility = serde_json::from_str(&format!("\"{text}\"")).unwrap();
            assert_eq!(parsed, variant);
            assert_eq!(variant.as_str(), text);
        }
        let unknown: Visibility = serde_json::from_str("\"followers\"").unwrap();
        assert_eq!(unknown, Visibility::Unknown("followers".into()));
        assert_eq!(
            serde_json::to_string(&unknown).unwrap(),
            "\"followers\"",
            "an unknown visibility must not be silently rewritten"
        );
    }

    #[test]
    fn every_allowed_reaction_parses() {
        // Mirrors allowedReactions in services/posts/internal/handler/post.go.
        let allowed = ["like", "dislike", "love", "haha", "wow", "sad", "angry"];
        for key in allowed {
            let reaction = Reaction::parse(key).expect("documented reaction must parse");
            assert_eq!(reaction.as_str(), key);
            let json = serde_json::to_string(&reaction).unwrap();
            assert_eq!(json, format!("\"{key}\""));
        }
        assert!(Reaction::parse("thumbsup").is_none());
        assert_eq!(Reaction::ALL.len(), allowed.len());
    }

    #[test]
    fn attachment_matches_the_server_json_tags() {
        let attachment: Attachment = serde_json::from_str(
            r#"{"id":1,"user_id":1,"original_name":"photo.jpg","mime_type":"image/jpeg",
                "size_bytes":12345,"url":"http://x/y.jpg","thumb_url":"http://x/t.jpg",
                "preview_url":"http://x/p.jpg","visibility":"public","bucket":"default",
                "created_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(attachment.original_name, "photo.jpg");
        assert_eq!(attachment.file_name(), "photo.jpg");
        assert_eq!(attachment.content_type(), "image/jpeg");
        assert_eq!(attachment.size(), 12345);
        assert_eq!(attachment.bucket.as_deref(), Some("default"));
        assert_eq!(attachment.visibility.as_deref(), Some("public"));
    }

    #[test]
    fn conversation_matches_the_server_json_tags() {
        let conversation: Conversation = serde_json::from_str(
            r#"{"id":5,"type":"group","title":"Family","avatar_url":"","owner_id":1,
                "is_public":true,"allow_join":false,"encrypted":false,"member_count":3,
                "is_member":true,"unread":7}"#,
        )
        .unwrap();
        assert_eq!(conversation.kind.as_deref(), Some("group"));
        assert_eq!(conversation.title, "Family");
        assert_eq!(conversation.name(), "Family");
        assert!(conversation.is_public);
        assert!(!conversation.allow_join);
        assert_eq!(conversation.unread, Some(7));
        assert!(conversation.is_member);
        assert!(conversation.is_group());

        // A private chat has a null title in the documented payload.
        let private: Conversation =
            serde_json::from_str(r#"{"id":6,"type":"private","title":null}"#).unwrap();
        assert_eq!(private.title, "");
        assert_eq!(private.name(), "");
        assert!(!private.is_group());

        // `type` is the emitted spelling.
        let json = serde_json::to_value(&private).unwrap();
        assert_eq!(json["type"], "private");
    }

    #[test]
    fn conversation_detail_matches_the_handler_envelope() {
        let wrapped: ConversationDetail = serde_json::from_str(
            r#"{"conversation":{"id":5,"type":"private","title":null,"encrypted":false},
                "members":[],"my_membership":{"user_id":1,"role":"member"}}"#,
        )
        .unwrap();
        assert_eq!(wrapped.conversation().id, 5);
        assert!(!wrapped.is_encrypted());
        assert!(wrapped.members().is_empty());

        let direct: ConversationDetail =
            serde_json::from_str(r#"{"id":9,"type":"group","title":"G","encrypted":true}"#)
                .unwrap();
        assert_eq!(direct.conversation().id, 9);
        assert!(
            direct.is_encrypted(),
            "the E2EE flag must survive either shape"
        );
    }

    #[test]
    fn camp_matches_the_server_json_tags() {
        let camp: Camp = serde_json::from_str(
            r#"{"id":66,"name":"营地","description":"d","creator_id":7,"creator_name":"alice",
                "is_visible":false,"direct_join":false,"parent_camp_id":12,"member_post":false,
                "member_pin":true,"announcement":"hello","member_count":5,"post_count":9,
                "is_member":true,"my_role":"owner","created_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(camp.creator_id, 7);
        assert!(camp.is_hidden(), "is_visible=false means hidden");
        assert!(!camp.members_can_post());
        assert!(camp.member_pin);
        assert!(camp.is_subgroup());
        assert_eq!(camp.announcement.as_deref(), Some("hello"));
        assert_eq!(camp.my_role.as_deref(), Some("owner"));

        // A top-level camp omits parent_camp_id entirely.
        let top: Camp =
            serde_json::from_str(r#"{"id":1,"name":"c","is_visible":true,"member_post":true}"#)
                .unwrap();
        assert!(!top.is_subgroup());
        assert!(!top.is_hidden());
    }

    #[test]
    fn camp_member_resolves_the_user_id_from_either_shape() {
        let flat: CampMember =
            serde_json::from_str(r#"{"id":3,"camp_id":1,"user_id":7,"role":"owner"}"#).unwrap();
        assert_eq!(flat.member_id(), Some(7));
        assert!(flat.is_owner());
        assert!(flat.is_staff());

        let nested: CampMember =
            serde_json::from_str(r#"{"user":{"id":9,"username":"a"},"role":"member"}"#).unwrap();
        assert_eq!(nested.member_id(), Some(9));
        assert!(!nested.is_staff());
    }

    #[test]
    fn post_matches_the_server_json_tags() {
        let post: Post = serde_json::from_str(
            r#"{"id":1,"user_id":2,"content":"hi","visibility":"public","reply_count":5,
                "view_count":10,"unique_views":8,"tip_total":0,"reactions":{"like":3},
                "my_reaction":"like","favorited":true,"favorite_count":2,"nickname":"alice",
                "is_verified":true,"is_bot":false,"pinned":true,"camp_id":66,"camp_pinned":false,
                "quoted_post_id":9,"attachments":[{"id":4,"original_name":"a.png","size_bytes":1}]}"#,
        )
        .unwrap();
        assert_eq!(post.reply_count, Some(5));
        assert_eq!(post.view_count, Some(10));
        assert_eq!(post.favorite_count, Some(2));
        assert_eq!(post.my_reaction.as_deref(), Some("like"));
        assert_eq!(post.reactions.get("like").unwrap(), 3);
        assert!(post.pinned);
        assert_eq!(post.camp_id, 66);
        assert_eq!(post.quoted_post_id, Some(9));
        assert_eq!(post.attachments.len(), 1);
        assert_eq!(post.visibility, Some(Visibility::Public));

        // `pinned` and `camp_pinned` are plain bools in the Go model.
        let bare: Post = serde_json::from_str(r#"{"id":2,"content":"x"}"#).unwrap();
        assert!(!bare.pinned);
        assert!(!bare.camp_pinned);
        assert_eq!(bare.camp_id, 0);
    }

    #[test]
    fn nested_quoted_post_parses() {
        let post: Post = serde_json::from_str(
            r#"{"id":1,"content":"quoting","quoted_post":{"id":2,"content":"original"}}"#,
        )
        .unwrap();
        assert_eq!(post.quoted_post.as_ref().unwrap().id, 2);
        assert_eq!(post.quoted_post.as_ref().unwrap().content, "original");
    }

    #[test]
    fn reply_matches_the_server_json_tags() {
        let reply: Reply = serde_json::from_str(
            r#"{"id":1,"post_id":2,"user_id":3,"content":"nice","parent_id":9,
                "username":"alice","nickname":"Alice","is_bot":true,"favorited":true,
                "favorite_count":4,"parent_content":"prev","parent_name":"bob",
                "attachments":[{"id":4,"original_name":"a.png","size_bytes":1}]}"#,
        )
        .unwrap();
        assert_eq!(reply.parent_id, Some(9));
        assert_eq!(reply.favorite_count, Some(4));
        assert!(reply.is_bot);
        assert_eq!(reply.parent_name.as_deref(), Some("bob"));
        assert_eq!(reply.attachments.len(), 1);
    }

    #[test]
    fn permission_set_accepts_both_documented_shapes() {
        let envelope: PermissionSet = serde_json::from_str(
            r#"{"permissions":["posts.view","chat.send","account.verify"],
                "groups":["admin","beta-users"]}"#,
        )
        .unwrap();
        assert!(envelope.contains("chat.send"));
        assert!(!envelope.contains("user.punish"));
        assert_eq!(
            envelope.groups(),
            vec!["admin".to_string(), "beta-users".to_string()]
        );

        let bare: PermissionSet =
            serde_json::from_str(r#"["chat.send",{"key":"posts.create"}]"#).unwrap();
        assert!(bare.contains("chat.send"));
        assert!(bare.contains("posts.create"));
        assert!(bare.groups().is_empty());

        assert!(envelope
            .permissions()
            .iter()
            .any(|key| key.is_administrative()));
    }

    #[test]
    fn administrative_permissions_match_the_documented_seven() {
        assert_eq!(ADMINISTRATIVE_PERMISSIONS.len(), 7);
        for key in ADMINISTRATIVE_PERMISSIONS {
            assert!(is_administrative_permission(key));
        }
        assert!(!is_administrative_permission("posts.create"));
        assert!(!is_administrative_permission("posts.react"));
    }

    #[test]
    fn capabilities_supports_every_documented_shape() {
        for raw in [
            r#"["chat.e2ee","camps.subgroups"]"#,
            r#"{"features":["chat.e2ee"]}"#,
            r#"{"features":{"chat.e2ee":true}}"#,
            r#"{"chat.e2ee":true}"#,
        ] {
            let caps: Capabilities = serde_json::from_str(raw).unwrap();
            assert!(caps.supports("chat.e2ee"), "failed for {raw}");
        }
        for raw in [
            r#"{"features":{"chat.e2ee":false}}"#,
            r#"{"features":{"chat.e2ee":"disabled"}}"#,
        ] {
            let caps: Capabilities = serde_json::from_str(raw).unwrap();
            assert!(!caps.supports("chat.e2ee"), "failed for {raw}");
        }
    }

    #[test]
    fn health_report_parses_the_documented_body() {
        let raw = r#"{
            "all_healthy": false,
            "checked_at": "2026-08-24T12:00:00Z",
            "services": {
                "gateway": {"status":"up","latency_ms":0,"details":{"database":"up"}},
                "posts": {"status":"down","latency_ms":3001,"error":"context deadline exceeded"}
            }
        }"#;
        let report: HealthReport = serde_json::from_str(raw).unwrap();
        assert!(!report.all_healthy);
        assert!(report.service("gateway").unwrap().is_up());
        assert!(!report.service("posts").unwrap().is_up());
        assert_eq!(
            report.service("posts").unwrap().error.as_deref(),
            Some("context deadline exceeded")
        );
        assert_eq!(report.unhealthy_services(), vec!["posts".to_string()]);
    }

    #[test]
    fn notification_matches_the_server_json_tags() {
        let notification: Notification = serde_json::from_str(
            r#"{"id":1,"type":"reply","title":"New reply","body":"alice replied",
                "data":{"post_id":9},"created_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(notification.kind, "reply");
        assert_eq!(notification.text(), "alice replied");
        assert!(!notification.is_read());
        assert_eq!(notification.data.as_ref().unwrap()["post_id"], 9);

        let read: Notification =
            serde_json::from_str(r#"{"id":2,"type":"x","read_at":"2026-01-02T00:00:00Z"}"#)
                .unwrap();
        assert!(read.is_read());
        assert_eq!(
            read.text(),
            "",
            "no body and no title yields an empty string"
        );
    }

    #[test]
    fn consent_request_matches_the_server_json_tags() {
        let request: ConsentRequest = serde_json::from_str(
            r#"{"id":3,"type":"private_chat","requester_id":7,"target_user_id":42,
                "message":"你好","status":"pending","requester_name":"bot",
                "encrypted":false}"#,
        )
        .unwrap();
        assert_eq!(request.kind, "private_chat");
        assert_eq!(request.requester_id, 7);
        assert!(request.is_pending());
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["type"], "private_chat");
    }

    #[test]
    fn report_matches_the_server_json_tags() {
        let report: Report = serde_json::from_str(
            r#"{"id":1,"reporter_id":7,"reporter_name":"alice","target_type":"post",
                "target_id":9,"reason":"spam","status":"pending"}"#,
        )
        .unwrap();
        assert_eq!(report.reporter_name.as_deref(), Some("alice"));
        assert!(report.is_pending());
    }

    #[test]
    fn storage_usage_accepts_the_documented_spellings() {
        let a: StorageUsage = serde_json::from_str(r#"{"used":10,"quota":100}"#).unwrap();
        assert_eq!(a.used_bytes(), Some(10));
        assert_eq!(a.quota_bytes(), Some(100));
        assert_eq!(a.remaining_bytes(), Some(90));

        let b: StorageUsage =
            serde_json::from_str(r#"{"used_bytes":5,"storage_quota":50}"#).unwrap();
        assert_eq!(b.used_bytes(), Some(5));
        assert_eq!(b.remaining_bytes(), Some(45));

        let c: StorageUsage = serde_json::from_str(r#"{"storage_used":200,"quota":100}"#).unwrap();
        assert_eq!(
            c.remaining_bytes(),
            Some(0),
            "over quota clamps at zero instead of going negative"
        );
    }

    #[test]
    fn storage_bucket_list_parses() {
        let list: StorageBucketList = serde_json::from_str(
            r#"{"buckets":[{"name":"default","label":"标准","default_quota":104857600,
                "min_member_level":0,"is_default":true,"locked":false}],"current":"default"}"#,
        )
        .unwrap();
        assert_eq!(list.buckets.len(), 1);
        assert_eq!(list.buckets[0].default_quota, Some(104_857_600));
        assert!(list.buckets[0].is_default);
        assert_eq!(list.current.as_deref(), Some("default"));
    }

    #[test]
    fn list_envelopes_accept_both_shapes() {
        let array: ReportList = serde_json::from_str(
            r#"[{"id":1,"target_type":"post","target_id":9,"status":"pending"}]"#,
        )
        .unwrap();
        assert!(array.reports()[0].is_pending());

        let envelope: ReportList =
            serde_json::from_str(r#"{"reports":[{"id":2,"status":"reviewed"}]}"#).unwrap();
        assert!(!envelope.reports()[0].is_pending());

        let announcements: AnnouncementList =
            serde_json::from_str(r#"[{"id":1,"title":"t"}]"#).unwrap();
        assert_eq!(announcements.announcements().len(), 1);

        let notifications: NotificationList =
            serde_json::from_str(r#"{"notifications":[{"id":1,"type":"reply"}]}"#).unwrap();
        assert_eq!(notifications.notifications().len(), 1);
    }

    #[test]
    fn attachment_response_accepts_both_shapes() {
        let direct: AttachmentResponse =
            serde_json::from_str(r#"{"id":9,"original_name":"chart.png"}"#).unwrap();
        assert_eq!(direct.id(), 9);
        let envelope: AttachmentResponse =
            serde_json::from_str(r#"{"attachment":{"id":11,"original_name":"a.png"}}"#).unwrap();
        assert_eq!(envelope.id(), 11);
        assert_eq!(envelope.attachment().file_name(), "a.png");
    }

    #[test]
    fn post_page_matches_the_handler_envelope() {
        let page: PostPage = serde_json::from_str(
            r#"{"posts":[{"id":1,"content":"x"}],"page":2,"limit":20,"query":"hi","tag":"news"}"#,
        )
        .unwrap();
        assert_eq!(page.page, 2);
        assert_eq!(page.limit, Some(20));
        assert_eq!(page.query.as_deref(), Some("hi"));
        assert_eq!(page.tag.as_deref(), Some("news"));
    }

    #[test]
    fn json_object_helpers() {
        let ok: JsonObject = serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert!(ok.is_ok());
        let other: JsonObject =
            serde_json::from_str(r#"{"ok":false,"reason":"nope","limit":10}"#).unwrap();
        assert!(!other.is_ok());
        assert_eq!(other.get_str("reason"), Some("nope"));
        assert_eq!(other.get_i64("limit"), Some(10));
    }

    #[test]
    fn user_profile_matches_the_server_json_tags() {
        let user: User = serde_json::from_str(
            r#"{"id":1,"username":"alice","nickname":"Alice","email":"a@b.c",
                "avatar_url":"https://x/a.png","banner_url":"https://x/b.png","role":"user",
                "needs_registration":false,"bio":"hi","is_verified":true,
                "storage_quota":104857600,"storage_used":2048000,"storage_bucket":"default",
                "exp":120,"level":2,"follower_count":3,"following_count":4,
                "hide_follow_lists":false,"is_following":true,"is_friend":false,
                "has_pin":true,"member_level":2,"auto_renew":false,"name_dynamic":false,
                "is_bot":true,"bot_owner_id":7,"online":true}"#,
        )
        .unwrap();
        assert_eq!(user.storage_quota, Some(104_857_600));
        assert_eq!(user.banner_url, "https://x/b.png");
        assert_eq!(user.member_level, Some(2));
        assert!(user.has_pin);
        assert!(user.is_following);
        assert!(user.is_bot);
        assert!(user.online);
    }

    #[test]
    fn display_strings_tolerate_null() {
        // API.md shows `"avatar_url": null` in a search result.
        let user: User =
            serde_json::from_str(r#"{"id":2,"username":"a","nickname":null,"avatar_url":null}"#)
                .unwrap();
        assert_eq!(user.id, 2);
        assert_eq!(user.avatar_url, "");
        assert_eq!(user.nickname, "");
    }

    #[test]
    fn flexible_id_deserializer_accepts_numbers_and_strings() {
        #[derive(Deserialize)]
        struct Holder {
            #[serde(default, deserialize_with = "de_id_flexible")]
            id: Option<Id>,
        }
        assert_eq!(
            serde_json::from_str::<Holder>(r#"{"id":42}"#).unwrap().id,
            Some(42)
        );
        assert_eq!(
            serde_json::from_str::<Holder>(r#"{"id":"42"}"#).unwrap().id,
            Some(42)
        );
        assert_eq!(serde_json::from_str::<Holder>("{}").unwrap().id, None);
        assert!(serde_json::from_str::<Holder>(r#"{"id":"abc"}"#).is_err());
    }

    #[test]
    fn id_from_value_accepts_both_encodings() {
        assert_eq!(id_from_value(&serde_json::json!(7)), Some(7));
        assert_eq!(id_from_value(&serde_json::json!("7")), Some(7));
        assert_eq!(id_from_value(&serde_json::json!(null)), None);
        assert_eq!(id_from_value(&serde_json::json!("x")), None);
    }
}
