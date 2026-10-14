//! Bot lifecycle endpoints (`bots.md` §4 and §12).
//!
//! These four calls are made with a **human** credential — a JWT from the app, or
//! another bot's token. A bot token can manage bots it owns, but no bot may create
//! another bot (`bots.md` §1, `bot.go:51`).

use crate::error::{Result, SdkError};
use crate::models::{Bot, BotList, CreatedBot, Id, TokenResponse};

use super::{AuthenticatedClient, API_PREFIX};

/// Ceiling on bots per owner, per `repository.MaxBotsPerOwner` (`bots.md` §12).
pub const MAX_BOTS_PER_OWNER: i64 = 10;

/// Validate a bot handle against the documented pattern `^[a-zA-Z0-9_]{3,32}$`.
///
/// Checking locally turns a round trip and a `400` into an immediate
/// [`crate::ErrorKind::InvalidArgument`].
pub fn validate_username(username: &str) -> Result<()> {
    let len = username.chars().count();
    if !(3..=32).contains(&len) {
        return Err(SdkError::InvalidArgument {
            message: format!("username must be 3..=32 characters, got {len}"),
        });
    }
    if !username
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(SdkError::InvalidArgument {
            message: "username may only contain ASCII letters, digits and `_`".into(),
        });
    }
    Ok(())
}

/// Validate a bot display name: 1..=100 characters after trimming (`bots.md` §4).
pub fn validate_nickname(nickname: &str) -> Result<()> {
    let trimmed = nickname.trim();
    if trimmed.is_empty() {
        return Err(SdkError::InvalidArgument {
            message: "nickname must not be empty".into(),
        });
    }
    let len = trimmed.chars().count();
    if len > 100 {
        return Err(SdkError::InvalidArgument {
            message: format!("nickname must be at most 100 characters, got {len}"),
        });
    }
    Ok(())
}

impl AuthenticatedClient {
    /// `POST /api/v1/bots` — create a bot and receive its token.
    ///
    /// The token in the response is shown **once** (`bots.md` §2.1): the server only
    /// stores its SHA-256 hash. Persist it before doing anything else.
    ///
    /// # Errors
    ///
    /// * `403 bot accounts cannot create bots` when called with a bot token;
    /// * `403 bot limit reached` past [`MAX_BOTS_PER_OWNER`];
    /// * `409 username already taken`.
    pub fn create_bot(&self, username: &str, nickname: &str) -> Result<CreatedBot> {
        validate_username(username)?;
        validate_nickname(nickname)?;
        let body = serde_json::json!({
            "username": username,
            "nickname": nickname,
        });
        self.post_json(&format!("{API_PREFIX}/bots"), &body)
    }

    /// `GET /api/v1/bots` — the caller's bots. Never other people's (`bots.md` §14).
    pub fn list_bots(&self) -> Result<BotList> {
        self.get_json(&format!("{API_PREFIX}/bots"))
    }

    /// `POST /api/v1/bots/:id/regenerate` — replace the token.
    ///
    /// The previous token stops working immediately, which is the only remedy once a
    /// token leaks (`bots.md` §12).
    pub fn regenerate_bot_token(&self, bot_id: Id) -> Result<TokenResponse> {
        let response = self.base.http.empty(
            "POST",
            &format!("{API_PREFIX}/bots/{bot_id}/regenerate"),
            &[],
        )?;
        response.decode()
    }

    /// `DELETE /api/v1/bots/:id` — delete a bot **and all of its content**.
    ///
    /// Cascades to posts, replies, messages, attachments, conversation memberships
    /// and camp memberships (`bots.md` §4). Irreversible.
    pub fn delete_bot(&self, bot_id: Id) -> Result<crate::models::JsonObject> {
        self.ok_object("DELETE", &format!("{API_PREFIX}/bots/{bot_id}"))
    }

    /// Look up one of the caller's bots by id.
    ///
    /// There is no `GET /bots/:id` endpoint (`bots.md` §14), so this filters the list.
    pub fn find_bot(&self, bot_id: Id) -> Result<Option<Bot>> {
        Ok(self.list_bots()?.bots.into_iter().find(|b| b.id == bot_id))
    }

    /// Whether another bot may still be created under the documented ceiling.
    pub fn can_create_bot(&self) -> Result<bool> {
        let list = self.list_bots()?;
        let limit = if list.limit > 0 {
            list.limit
        } else {
            MAX_BOTS_PER_OWNER
        };
        Ok((list.bots.len() as i64) < limit)
    }
}
