//! Rust core of the OpenField community bot SDK.
//!
//! This crate implements the protocol surface described in `bots.md` — bot lifecycle,
//! the `ofb_` token flow, the REST endpoints a bot may call, and the WebSocket event
//! channel — reconciled against the server source
//! (`github.com/midSanCode/openField-server`). Where the bundled documents disagreed
//! with the code, the code won; [`docs::KNOWN_DIVERGENCES`] lists every conflict and
//! its resolution.
//!
//! It is deliberately synchronous and narrowly dependent so the same code backs three
//! consumers:
//!
//! * Rust programs, through this crate directly;
//! * any language, through the `openfield-bot-sidecar` JSON-RPC process;
//! * C-ABI embedders, through the `openfield-bot-ffi` crate.
//!
//! ```
//! use openfield_bot::{AuthenticatedClient, BotToken};
//!
//! # fn main() -> openfield_bot::Result<()> {
//! let token = BotToken::new(format!("ofb_{}", "0".repeat(64)))?;
//! let bot = AuthenticatedClient::new("http://localhost:8080", token)?;
//! assert_eq!(bot.config().base_url, "http://localhost:8080");
//! # Ok(())
//! # }
//! ```
//!
//! ## Auth model
//!
//! A bot authenticates with a static token, not a password and not a JWT
//! (`bots.md` §3). One bot has exactly one token, which is why [`BotToken`] redacts
//! itself in `Debug`/`Display`/`Serialize`: the credential cannot reach a log line or
//! a serialised payload by accident.
//!
//! ## Deliberate choices
//!
//! * **Blocking, not `async`.** The REST surface is small and the event stream is one
//!   socket, so threads bind across languages more simply than a runtime would.
//! * **Typed models plus an `extra` map.** Unknown server fields survive a
//!   deserialize/serialize round trip instead of being dropped.
//! * **Errors are values, not panics.** `401`/`403`/`409`/`429` all reach the caller
//!   with the server's own `error` string intact.
//! * **Mutations are not retried blindly.** Only `GET`/`HEAD` are retried on transport
//!   failures and `5xx`; a mutating verb retries on `429` only, because replaying a
//!   `POST /posts` would duplicate content.
//! * **No `unsafe`.** The event loop stays interruptible through a socket read
//!   timeout rather than a raw-`poll` integration, so the crate can forbid `unsafe`.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod client;
pub mod config;
pub mod docs;
pub mod error;
pub mod events;
pub mod http;
pub mod models;
pub mod retry;
pub mod rpc;

#[cfg(feature = "testkit")]
pub mod testkit;

pub use client::account::{NameStyle, StartupCheck, IMAGE_FIELD};
pub use client::bots::{validate_nickname, validate_username, MAX_BOTS_PER_OWNER};
pub use client::camps::{CampUpdate, NewCamp};
pub use client::chat::{
    burn_seconds_allowed, ChatRequest, ConversationSettings, MessageCursor, NewConversation,
    NewMessage, DEFAULT_MESSAGE_LIMIT, MAX_MESSAGE_LIMIT,
};
pub use client::misc::{NewReport, MAX_REASON_CHARS, MAX_REPORTS_PER_HOUR};
pub use client::posts::{NewPost, NewReply, MAX_ATTACHMENTS, MAX_CONTENT_BYTES};
pub use client::request::{Request, RequestBody};
pub use client::storage::{Upload, ATTACHMENT_FIELD};
pub use client::{AnyClient, AuthenticatedClient, BaseClient, ProfileUpdate};
pub use config::{BotToken, Config, BOT_TOKEN_LEN, BOT_TOKEN_PREFIX, DEFAULT_BASE_URL};
pub use error::{ApiErrorBody, ErrorKind, Result, SdkError};
pub use events::{
    fetch_ticket, upgrade_url, Event, EventFilter, EventStream, EventStreamStats, EventType,
    WsTicket, TICKET_TTL_SECONDS,
};
pub use events::{EventControl, EventReceiver};
pub use http::{HttpResponse, MultipartFile};
pub use models::{
    Announcement, AnnouncementList, Attachment, AttachmentList, Bot, BotList, Camp, CampMember,
    Capabilities, ChunkedUpload, ConsentRequest, ConsentRequestList, Conversation,
    ConversationDetail, CreatedBot, HealthReport, Id, JsonObject, Message, MessageKind,
    MessagePage, Notification, NotificationList, Permission, PermissionSet, Post, PostPage,
    Reaction, Reply, ReplyPage, Report, ReportList, ReportTarget, ServiceHealth, StorageBucket,
    StorageBucketList, StorageUsage, TokenResponse, User, Visibility, ADMINISTRATIVE_PERMISSIONS,
    MENTION_EVERYONE,
};
pub use retry::{ReconnectPolicy, RetryPolicy};
pub use rpc::{error_object, event_message, EventSink, RpcState, METHODS, PROTOCOL_VERSION};

/// Crate version, matching the sidecar's `version` reply.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `User-Agent` sent by default.
pub fn user_agent() -> String {
    format!("openfield-bot-sdk/{VERSION}")
}
