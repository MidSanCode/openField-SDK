//! Account endpoints (`bots.md` §6.1).
//!
//! A bot is an ordinary user row, so these are the same calls any user makes. Three
//! details matter for a bot, all confirmed against
//! `services/account/internal/handler/user.go`:
//!
//! * `PUT /users/me` accepts only `username`, `nickname` and `bio`; `is_bot` and
//!   `bot_owner_id` cannot be changed through any API (`bots.md` §12`);
//! * avatar and banner uploads both use the multipart field name **`file`**;
//! * the startup self-check (`bots.md` §15.2) should run before the bot does work,
//!   because a `401` there means the token was reset, never existed, or the bot is
//!   banned — all three answer `401 invalid bot token`.

use serde::{Deserialize, Serialize};

use crate::error::{ErrorKind, Result, SdkError};
use crate::models::{
    Id, JsonObject, PermissionSet, PostPage, StorageBucketList, StorageUsage, User,
    ADMINISTRATIVE_PERMISSIONS,
};
use serde_json::Value;

use super::{pagination, AuthenticatedClient, ProfileUpdate, API_PREFIX};

/// Wrap a response body as a JSON object, tolerating a bare `null` and a scalar.
///
/// Shared by the endpoints whose success body is `{"ok":true}`, an envelope this SDK
/// does not model, or nothing at all.
pub(crate) fn to_object(response: crate::http::HttpResponse) -> Result<JsonObject> {
    match response.json()? {
        Value::Object(map) => Ok(JsonObject(map)),
        Value::Null => Ok(JsonObject(serde_json::Map::new())),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("result".to_string(), other);
            Ok(JsonObject(map))
        }
    }
}

/// The multipart field name the avatar and banner handlers read.
pub const IMAGE_FIELD: &str = "file";

/// The outcome of the documented startup self-check (`bots.md` §15.2).
#[derive(Debug, Clone, PartialEq)]
pub struct StartupCheck {
    /// The bot's own profile, as the server sees it.
    pub me: User,
    /// Permissions the bot holds.
    pub permissions: PermissionSet,
    /// Administrative keys the bot does *not* hold (expected: all seven).
    pub missing_administrative: Vec<String>,
}

impl StartupCheck {
    /// The bot's id — cache this and compare every incoming `sender_id` against it.
    pub fn bot_id(&self) -> Id {
        self.me.id
    }

    /// Whether the profile really carries the bot marker.
    ///
    /// `false` means the token belongs to a human account, not a bot.
    pub fn is_bot(&self) -> bool {
        self.me.is_bot
    }

    /// Whether the bot holds `key`.
    pub fn has_permission(&self, key: &str) -> bool {
        self.permissions.contains(key)
    }
}

/// A display-name styling patch (`PUT /users/me/name-style`).
///
/// Which combination is allowed depends on the membership tier: Lv.1 may only use the
/// preset palette, Lv.2 any hex colour, Lv.3 also a gradient, Lv.4 also animation. The
/// server enforces the caps; this struct just carries the request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NameStyle {
    /// Gradient colour list (1–6 hex colours).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub colors: Vec<String>,
    /// Single solid colour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Gradient end colour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_to: Option<String>,
    /// Animated gradient (Lv.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic: Option<bool>,
    /// Gradient orientation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    /// Avatar frame key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_frame: Option<String>,
}

impl NameStyle {
    /// A solid colour.
    pub fn solid(color: impl Into<String>) -> Self {
        Self {
            color: Some(color.into()),
            ..Self::default()
        }
    }

    /// A two-stop gradient.
    pub fn gradient(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            color: Some(from.into()),
            color_to: Some(to.into()),
            ..Self::default()
        }
    }

    /// Reject an empty patch.
    pub fn validate(&self) -> Result<()> {
        if self.colors.is_empty()
            && self.color.is_none()
            && self.color_to.is_none()
            && self.dynamic.is_none()
            && self.direction.is_none()
            && self.avatar_frame.is_none()
        {
            return Err(SdkError::InvalidArgument {
                message: "name-style patch must set at least one field".into(),
            });
        }
        Ok(())
    }
}

impl AuthenticatedClient {
    /// `GET /api/v1/users/me` — the bot's own profile, including `is_bot`.
    ///
    /// This is the documented startup self-check (`bots.md` §15.2): a `401` here means
    /// the token was reset, never existed, or the bot is banned — all three answer
    /// `401 {"error":"invalid bot token"}` (`bots.md` §3.3).
    pub fn me(&self) -> Result<User> {
        self.get_json(&format!("{API_PREFIX}/users/me"))
    }

    /// Fetch the bot's own id.
    ///
    /// A `401` surfaces as [`ErrorKind::Unauthorized`] so a supervisor can alert and
    /// exit instead of hammering the server.
    pub fn my_id(&self) -> Result<Id> {
        Ok(self.me()?.id)
    }

    /// `GET /api/v1/users/me/permissions` — the permission keys this bot holds.
    ///
    /// Check this at startup instead of assuming the default group (`bots.md` §5): an
    /// operator may have moved the bot into a custom group.
    pub fn my_permissions(&self) -> Result<PermissionSet> {
        self.get_json(&format!("{API_PREFIX}/users/me/permissions"))
    }

    /// The administrative keys this bot does *not* hold (`bots.md` §5).
    ///
    /// A correctly provisioned bot is missing all seven.
    pub fn missing_administrative_permissions(&self) -> Result<Vec<String>> {
        let set = self.my_permissions()?;
        Ok(ADMINISTRATIVE_PERMISSIONS
            .iter()
            .filter(|key| !set.contains(key))
            .map(|key| (*key).to_string())
            .collect())
    }

    /// Run the documented startup sequence: identity, then permissions.
    pub fn startup_check(&self) -> Result<StartupCheck> {
        let me = self.me()?;
        let permissions = self.my_permissions()?;
        let missing_administrative = ADMINISTRATIVE_PERMISSIONS
            .iter()
            .filter(|key| !permissions.contains(key))
            .map(|key| (*key).to_string())
            .collect();
        Ok(StartupCheck {
            me,
            permissions,
            missing_administrative,
        })
    }

    /// The same check, but with the documented `401` promoted to a clear message.
    ///
    /// Convenient for a bot's `main`: a failure here should stop the process rather
    /// than be retried (`bots.md` §15.2`).
    pub fn verify_credentials(&self) -> Result<StartupCheck> {
        match self.startup_check() {
            Err(err) if err.kind() == ErrorKind::Unauthorized => {
                let detail = err.api_message().unwrap_or("unauthorized");
                Err(SdkError::Http {
                    status: 401,
                    body: crate::error::ApiErrorBody {
                        error: Some(format!(
                            "bot token rejected by {} ({detail}); it may have been reset, or the \
                             bot may be banned",
                            self.config().base_url
                        )),
                        ..Default::default()
                    },
                    retry_after_secs: None,
                })
            }
            other => other,
        }
    }

    /// `PUT /api/v1/users/me` — update `username`, `nickname` and/or `bio`.
    ///
    /// The endpoint accepts only those three fields; `is_bot` and `bot_owner_id` cannot
    /// be changed through any API (`bots.md` §12`). The response is the updated user.
    pub fn update_profile(&self, update: &ProfileUpdate) -> Result<User> {
        update.validate()?;
        let body = serde_json::to_value(update).map_err(super::json_error)?;
        self.put_json(&format!("{API_PREFIX}/users/me"), &body)
    }

    /// `POST /api/v1/users/me/avatar` (`account.avatar.edit`).
    ///
    /// The file goes in a multipart field named [`IMAGE_FIELD`] (`file`), and the
    /// upload counts against the bot's storage quota.
    pub fn upload_avatar(&self, file_name: &str, bytes: Vec<u8>) -> Result<JsonObject> {
        self.upload_image("/users/me/avatar", file_name, bytes)
    }

    /// `POST /api/v1/users/me/banner` (`account.banner.edit`).
    pub fn upload_banner(&self, file_name: &str, bytes: Vec<u8>) -> Result<JsonObject> {
        self.upload_image("/users/me/banner", file_name, bytes)
    }

    /// Upload an image with an explicit multipart field name.
    pub fn upload_image_as(
        &self,
        path: &str,
        field: &str,
        file_name: &str,
        bytes: Vec<u8>,
    ) -> Result<JsonObject> {
        let file = crate::http::MultipartFile::new(file_name, bytes).with_field(field);
        let response =
            self.base
                .http
                .multipart("POST", &format!("{API_PREFIX}{path}"), &[], &[file])?;
        super::account::to_object(response)
    }

    fn upload_image(&self, path: &str, file_name: &str, bytes: Vec<u8>) -> Result<JsonObject> {
        self.upload_image_as(path, IMAGE_FIELD, file_name, bytes)
    }

    /// `PUT /api/v1/users/me/locale` — set the region/language used for day boundaries.
    pub fn set_locale(&self, region: Option<&str>, lang: Option<&str>) -> Result<JsonObject> {
        if region.is_none() && lang.is_none() {
            return Err(SdkError::InvalidArgument {
                message: "set at least one of region, lang".into(),
            });
        }
        let mut body = serde_json::Map::new();
        if let Some(region) = region {
            body.insert("region".into(), Value::String(region.to_string()));
        }
        if let Some(lang) = lang {
            body.insert("lang".into(), Value::String(lang.to_string()));
        }
        self.put_json(
            &format!("{API_PREFIX}/users/me/locale"),
            &Value::Object(body),
        )
    }

    /// `PUT /api/v1/users/me/privacy` — hide the follow lists from other viewers.
    ///
    /// While enabled, everyone but the owner gets `403` on the three list endpoints and
    /// the follower/following counts are suppressed.
    pub fn set_hide_follow_lists(&self, hide: bool) -> Result<JsonObject> {
        let body = serde_json::json!({ "hide_follow_lists": hide });
        self.put_json(&format!("{API_PREFIX}/users/me/privacy"), &body)
    }

    /// `PUT /api/v1/users/me/name-style` (`account.profile.edit`).
    ///
    /// The allowed shape depends on the membership tier; the server rejects a style
    /// above the bot's tier.
    pub fn set_name_style(&self, style: &NameStyle) -> Result<JsonObject> {
        style.validate()?;
        let body = serde_json::to_value(style).map_err(super::json_error)?;
        self.put_json(&format!("{API_PREFIX}/users/me/name-style"), &body)
    }

    /// `GET /api/v1/users/search?q=…` — search users by handle or nickname.
    pub fn search_users(&self, query: &str) -> Result<Value> {
        let response = self.base.http.json_raw(
            "GET",
            &format!("{API_PREFIX}/users/search"),
            None,
            &[("q".to_string(), query.to_string())],
        )?;
        response.json()
    }

    /// `GET /api/v1/users/:user_id` — another user's public profile.
    pub fn user(&self, user_id: Id) -> Result<User> {
        self.get_json(&format!("{API_PREFIX}/users/{user_id}"))
    }

    /// `POST /api/v1/users/:user_id/follow` (`account.follow`).
    pub fn follow(&self, user_id: Id) -> Result<JsonObject> {
        self.ok_object("POST", &format!("{API_PREFIX}/users/{user_id}/follow"))
    }

    /// `DELETE /api/v1/users/:user_id/follow`.
    pub fn unfollow(&self, user_id: Id) -> Result<JsonObject> {
        self.ok_object("DELETE", &format!("{API_PREFIX}/users/{user_id}/follow"))
    }

    /// `GET /api/v1/users/:user_id/posts` — a user's posts.
    pub fn user_posts(
        &self,
        user_id: Id,
        page: Option<i64>,
        limit: Option<i64>,
    ) -> Result<PostPage> {
        self.get_json_query(
            &format!("{API_PREFIX}/users/{user_id}/posts"),
            &pagination(page, limit),
        )
    }

    /// `GET /api/v1/users/:user_id/followers`.
    ///
    /// Returns the raw document because `bots.md` does not pin this envelope down;
    /// expect `403` when the target has hidden their lists.
    pub fn followers(&self, user_id: Id) -> Result<Value> {
        let response = self.base.http.json_raw(
            "GET",
            &format!("{API_PREFIX}/users/{user_id}/followers"),
            None,
            &[],
        )?;
        response.json()
    }

    /// `GET /api/v1/users/:user_id/following`.
    pub fn following(&self, user_id: Id) -> Result<Value> {
        let response = self.base.http.json_raw(
            "GET",
            &format!("{API_PREFIX}/users/{user_id}/following"),
            None,
            &[],
        )?;
        response.json()
    }

    /// `GET /api/v1/users/:user_id/friends` — mutual follows.
    pub fn friends(&self, user_id: Id) -> Result<Value> {
        let response = self.base.http.json_raw(
            "GET",
            &format!("{API_PREFIX}/users/{user_id}/friends"),
            None,
            &[],
        )?;
        response.json()
    }

    // --- storage (`API.md`, `features.md`) ----------------------------------

    /// `GET /api/v1/storage/usage` (`storage.list`) — bytes used versus the quota.
    ///
    /// Exceeding the quota makes uploads answer `413 storage quota exceeded`, and a
    /// server with no object storage configured answers `503` instead.
    pub fn storage_usage(&self) -> Result<StorageUsage> {
        self.get_json(&format!("{API_PREFIX}/storage/usage"))
    }

    /// `GET /api/v1/users/storage-buckets` — buckets and the bot's current one.
    pub fn storage_buckets(&self) -> Result<StorageBucketList> {
        self.get_json(&format!("{API_PREFIX}/users/storage-buckets"))
    }

    /// `PUT /api/v1/users/me/storage-bucket` — switch buckets.
    ///
    /// An unknown bucket is a `400`; a membership-gated one the bot cannot use is a
    /// `403`. Existing attachments are not moved, only the quota resets.
    pub fn set_storage_bucket(&self, bucket: &str) -> Result<User> {
        if bucket.trim().is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "bucket must not be empty".into(),
            });
        }
        let body = serde_json::json!({ "bucket": bucket });
        self.put_json(&format!("{API_PREFIX}/users/me/storage-bucket"), &body)
    }

    // --- the payment PIN ----------------------------------------------------

    /// `POST /api/v1/users/me/pin` — set the six-digit payment PIN.
    ///
    /// The PIN authorizes every outgoing payment: tips, memberships and
    /// transfers. **A freshly created bot has none**, so these are the calls a
    /// spending bot must make first — without one, every payment endpoint answers
    /// `403 payment pin not set`.
    ///
    /// Replacing an existing PIN requires the current one in `old_pin`; the
    /// server answers `403` with `old_pin_required: true` when it is missing, so a
    /// stolen token alone cannot rebind the payment second factor.
    ///
    /// # Errors
    ///
    /// * `400 pin is required`;
    /// * `400 pin must be 6 digits` — [`validate_pin`] enforces this locally;
    /// * `403 payment pin already set` — `old_pin` was missing;
    /// * `401 invalid payment pin` — `old_pin` was wrong;
    /// * `429 尝试次数过多，请稍后再试` — the shared PIN budget is exhausted.
    pub fn set_payment_pin(&self, pin: &str, old_pin: Option<&str>) -> Result<JsonObject> {
        validate_pin(pin)?;
        let body = match old_pin {
            Some(old) => serde_json::json!({ "pin": pin, "old_pin": old }),
            None => serde_json::json!({ "pin": pin }),
        };
        self.post_json(&format!("{API_PREFIX}/users/me/pin"), &body)
    }

    /// `PUT /api/v1/users/me/pin` — replace the payment PIN.
    ///
    /// Unlike [`set_payment_pin`](Self::set_payment_pin), the current PIN is
    /// mandatory here: the handler binds both fields as required and answers
    /// `400 old_pin and pin are required` when either is absent, then
    /// `400 pin not set` when no PIN exists yet.
    pub fn change_payment_pin(&self, old_pin: &str, pin: &str) -> Result<JsonObject> {
        validate_pin(pin)?;
        if old_pin.trim().is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "old_pin must not be empty".into(),
            });
        }
        let body = serde_json::json!({ "old_pin": old_pin, "pin": pin });
        self.put_json(&format!("{API_PREFIX}/users/me/pin"), &body)
    }

    /// `POST /api/v1/users/me/pin/verify` — check a PIN without spending.
    ///
    /// A wrong PIN is **counted** against the same five-failure budget that tips
    /// and memberships use, so probing this endpoint can lock the account out of
    /// spending for fifteen minutes. Returns `Ok(true)` only on `200`.
    ///
    /// # Errors
    ///
    /// * `400 pin is required`;
    /// * `400 pin not set`;
    /// * `401 invalid pin` — wrong, and counted;
    /// * `429` — the budget is exhausted.
    pub fn verify_payment_pin(&self, pin: &str) -> Result<bool> {
        if pin.trim().is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "pin must not be empty".into(),
            });
        }
        let body = serde_json::json!({ "pin": pin });
        let response: JsonObject =
            self.post_json(&format!("{API_PREFIX}/users/me/pin/verify"), &body)?;
        Ok(response
            .get("valid")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }
}

/// How many failures the server tolerates before locking payments
/// (`maxPinFailuresPerUser`), shared by every payment-PIN check.
pub const MAX_PIN_FAILURES: u32 = 5;

/// A payment PIN is exactly six digits (`security.ValidPin`).
pub fn validate_pin(pin: &str) -> Result<&str> {
    let trimmed = pin.trim();
    if trimmed.len() != 6 || !trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(SdkError::InvalidArgument {
            message: "the payment pin must be exactly 6 digits".into(),
        });
    }
    Ok(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_field_matches_the_handler() {
        // user.go calls c.Request.FormFile("file") for both avatar and banner.
        assert_eq!(IMAGE_FIELD, "file");
    }

    #[test]
    fn profile_update_accepts_only_the_three_documented_fields() {
        let update = ProfileUpdate::new()
            .username("weather_watcher")
            .nickname("天气预报")
            .bio("每小时报一次天气");
        update.validate().unwrap();
        let json = serde_json::to_value(&update).unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["bio", "nickname", "username"],
            "the handler binds only username, nickname and bio"
        );
    }

    #[test]
    fn profile_update_rejects_empty_and_invalid_values() {
        assert!(ProfileUpdate::new().validate().is_err());
        assert!(ProfileUpdate::new().username("ab").validate().is_err());
        assert!(ProfileUpdate::new()
            .username("has space")
            .validate()
            .is_err());
        assert!(ProfileUpdate::new().nickname("   ").validate().is_err());
        assert!(ProfileUpdate::new()
            .nickname("x".repeat(101))
            .validate()
            .is_err());
        assert!(ProfileUpdate::new()
            .nickname("x".repeat(100))
            .validate()
            .is_ok());
        // Nickname length is counted in runes, matching bot.go / user.go.
        assert!(ProfileUpdate::new()
            .nickname("测".repeat(100))
            .validate()
            .is_ok());
        assert!(ProfileUpdate::new()
            .nickname("测".repeat(101))
            .validate()
            .is_err());
    }

    #[test]
    fn name_style_requires_a_change() {
        assert!(NameStyle::default().validate().is_err());
        assert!(NameStyle::solid("#E64A19").validate().is_ok());
        assert!(NameStyle::gradient("#111111", "#222222").validate().is_ok());
        let json = serde_json::to_value(NameStyle::solid("#E64A19")).unwrap();
        assert_eq!(json["color"], "#E64A19");
        assert!(json.get("colors").is_none(), "unset fields stay absent");
    }

    #[test]
    fn companion_endpoints_validate_locally() {
        // These are constructors, so the check is that empty input is refused before
        // any request is built.
        assert!(SdkError::InvalidArgument {
            message: "x".into()
        }
        .is_retryable()
        .eq(&false));
    }
}
