//! Moderation: recording punishments against a user.
//!
//! Requires `user.punish`, which the default group does **not** hold — a bot
//! with the stock permissions can file reports but cannot sanction anyone
//! (`bots.md` §14). When the permission is granted, the server adds three
//! guards that a caller should expect rather than discover:
//!
//! * **no self-moderation** — `403 cannot punish yourself`, so an operator
//!   cannot lift their own ban or clear a permission ban imposed on them;
//! * **rank** — `403 cannot punish an administrator` when a non-admin targets
//!   an admin, and `403 insufficient rank to punish this user` when a `ban`,
//!   `temp_ban` or `revoke` targets a peer-level non-`user` role;
//! * **per-type parameters** — `revoke`/`restore` need `permission_key`,
//!   `temp_ban` needs a positive `duration_minutes`.
//!
//! Records are permanent history. `GET /users/:id/punishments` returns that
//! history plus `permission_bans`, which is a list of permission **keys**
//! currently withheld (not a list of records) — the effective bans are what the
//! permission resolver excludes until a `restore` or `unban`.

use serde::{Deserialize, Serialize};

use crate::error::{Result, SdkError};
use crate::models::{Id, PunishmentList, PunishmentReceipt};

use super::AuthenticatedClient;
use crate::client::API_PREFIX;

/// The seven punishment types accepted by `POST /users/:id/punishments`.
///
/// The wire values are exactly these strings; anything else answers
/// `400 unknown punishment type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PunishmentType {
    /// 警告 — recorded, no side effect.
    Warning,
    /// 记过 — recorded, no side effect.
    Demerit,
    /// 剥夺权限 — also writes `user_permission_bans`.
    Revoke,
    /// 暂时封禁 — bans until `expires_at`.
    TempBan,
    /// 永久封禁 — bans with no deadline.
    Ban,
    /// 解除封禁 — reactivates and clears every permission ban.
    Unban,
    /// 恢复权限 — removes one `user_permission_bans` row.
    Restore,
}

impl PunishmentType {
    /// The wire string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Demerit => "demerit",
            Self::Revoke => "revoke",
            Self::TempBan => "temp_ban",
            Self::Ban => "ban",
            Self::Unban => "unban",
            Self::Restore => "restore",
        }
    }

    /// Parse a wire string, rejecting anything the server would refuse.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim() {
            "warning" => Ok(Self::Warning),
            "demerit" => Ok(Self::Demerit),
            "revoke" => Ok(Self::Revoke),
            "temp_ban" => Ok(Self::TempBan),
            "ban" => Ok(Self::Ban),
            "unban" => Ok(Self::Unban),
            "restore" => Ok(Self::Restore),
            other => Err(SdkError::InvalidArgument {
                message: format!(
                    "unknown punishment type `{other}`; expected one of warning, demerit, \
                     revoke, temp_ban, ban, unban, restore"
                ),
            }),
        }
    }

    /// Whether this type needs `permission_key`.
    pub fn needs_permission_key(&self) -> bool {
        matches!(self, Self::Revoke | Self::Restore)
    }

    /// Whether this type needs a positive `duration_minutes`.
    pub fn needs_duration(&self) -> bool {
        matches!(self, Self::TempBan)
    }

    /// Whether this type removes someone's standing, and is therefore subject to
    /// the peer-level rank guard.
    pub fn is_restrictive(&self) -> bool {
        matches!(self, Self::Ban | Self::TempBan | Self::Revoke)
    }
}

impl std::fmt::Display for PunishmentType {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One punishment to apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PunishmentRequest {
    /// The action to record.
    ///
    /// The wire field is `type`; `kind` is the Rust-side name because `type` is a
    /// reserved word.
    #[serde(rename = "type")]
    pub kind: PunishmentType,
    /// Optional free-text reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Required for `revoke` / `restore`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_key: Option<String>,
    /// Required for `temp_ban`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_minutes: Option<i64>,
}

impl PunishmentRequest {
    /// A `warning` with `reason`.
    pub fn warning(reason: impl Into<String>) -> Self {
        Self::record(PunishmentType::Warning, reason)
    }

    /// A `demerit` with `reason`.
    pub fn demerit(reason: impl Into<String>) -> Self {
        Self::record(PunishmentType::Demerit, reason)
    }

    /// A permanent ban.
    pub fn ban(reason: impl Into<String>) -> Self {
        Self::record(PunishmentType::Ban, reason)
    }

    /// An unban.
    pub fn unban(reason: impl Into<String>) -> Self {
        Self::record(PunishmentType::Unban, reason)
    }

    /// A temporary ban of `minutes`.
    pub fn temp_ban(minutes: i64, reason: impl Into<String>) -> Self {
        Self {
            kind: PunishmentType::TempBan,
            reason: Some(reason.into()),
            permission_key: None,
            duration_minutes: Some(minutes),
        }
    }

    /// Withdraw one permission.
    pub fn revoke(permission_key: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            kind: PunishmentType::Revoke,
            reason: Some(reason.into()),
            permission_key: Some(permission_key.into()),
            duration_minutes: None,
        }
    }

    /// Give one permission back.
    pub fn restore(permission_key: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            kind: PunishmentType::Restore,
            reason: Some(reason.into()),
            permission_key: Some(permission_key.into()),
            duration_minutes: None,
        }
    }

    fn record(kind: PunishmentType, reason: impl Into<String>) -> Self {
        Self {
            kind,
            reason: Some(reason.into()),
            permission_key: None,
            duration_minutes: None,
        }
    }

    /// Check what the server would check, before the round trip.
    ///
    /// # Errors
    ///
    /// `revoke`/`restore` without a `permission_key`, `temp_ban` without a
    /// positive `duration_minutes`.
    pub fn validate(&self) -> Result<()> {
        if self.kind.needs_permission_key() {
            match self.permission_key.as_deref().map(str::trim) {
                Some(key) if !key.is_empty() => {}
                _ => {
                    return Err(SdkError::InvalidArgument {
                        message: format!("`{}` requires permission_key", self.kind.as_str()),
                    })
                }
            }
        }
        if self.kind.needs_duration() && self.duration_minutes.is_none_or(|minutes| minutes <= 0) {
            return Err(SdkError::InvalidArgument {
                message: "temp_ban requires a positive duration_minutes".into(),
            });
        }
        Ok(())
    }
}

impl AuthenticatedClient {
    /// `POST /api/v1/users/:user_id/punishments` — record a moderation action.
    ///
    /// # Errors
    ///
    /// * `400 invalid user id` / `400 invalid request`;
    /// * `400 permission_key is required` — `revoke`/`restore` without a key;
    /// * `400 duration_minutes is required for temp_ban`;
    /// * `400 unknown punishment type`;
    /// * `403 cannot punish yourself`;
    /// * `403 cannot punish an administrator`;
    /// * `403 insufficient rank to punish this user`;
    /// * `404 user not found`.
    pub fn punish(&self, user_id: Id, request: &PunishmentRequest) -> Result<PunishmentReceipt> {
        if user_id <= 0 {
            return Err(SdkError::InvalidArgument {
                message: "user_id must be a positive id".into(),
            });
        }
        request.validate()?;
        let body = serde_json::to_value(request).map_err(super::json_error)?;
        self.post_json(&format!("{API_PREFIX}/users/{user_id}/punishments"), &body)
    }

    /// `GET /api/v1/users/:user_id/punishments` — history plus live bans.
    ///
    /// The server clamps `limit` into `1..=100` and defaults it to 50.
    pub fn punishments(&self, user_id: Id, limit: Option<i64>) -> Result<PunishmentList> {
        if user_id <= 0 {
            return Err(SdkError::InvalidArgument {
                message: "user_id must be a positive id".into(),
            });
        }
        let query = match limit {
            Some(limit) => vec![("limit".to_string(), limit.to_string())],
            None => Vec::new(),
        };
        self.get_json_query(&format!("{API_PREFIX}/users/{user_id}/punishments"), &query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_documented_type_round_trips() {
        for (wire, expected) in [
            ("warning", PunishmentType::Warning),
            ("demerit", PunishmentType::Demerit),
            ("revoke", PunishmentType::Revoke),
            ("temp_ban", PunishmentType::TempBan),
            ("ban", PunishmentType::Ban),
            ("unban", PunishmentType::Unban),
            ("restore", PunishmentType::Restore),
        ] {
            let parsed = PunishmentType::parse(wire).unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.as_str(), wire);
            // Serde must use the same spelling as the wire.
            let json = serde_json::to_value(parsed).unwrap();
            assert_eq!(json, wire, "serde spelling drifted from as_str()");
        }
    }

    #[test]
    fn an_unknown_type_is_refused_locally() {
        let error = PunishmentType::parse("exile").unwrap_err();
        assert!(error.to_string().contains("unknown punishment type"));
    }

    #[test]
    fn revoke_and_restore_need_a_permission_key() {
        let mut request = PunishmentRequest::revoke("posts.create", "spam");
        assert!(request.validate().is_ok());
        request.permission_key = Some("   ".into());
        assert!(request.validate().is_err());
        assert!(PunishmentRequest {
            kind: PunishmentType::Restore,
            reason: None,
            permission_key: None,
            duration_minutes: None,
        }
        .validate()
        .is_err());
    }

    #[test]
    fn temp_ban_needs_a_positive_duration() {
        assert!(PunishmentRequest::temp_ban(60, "cooldown")
            .validate()
            .is_ok());
        assert!(PunishmentRequest::temp_ban(0, "cooldown")
            .validate()
            .is_err());
        assert!(PunishmentRequest::temp_ban(-5, "cooldown")
            .validate()
            .is_err());
    }

    #[test]
    fn the_history_only_types_need_nothing_extra() {
        assert!(PunishmentRequest::warning("be nice").validate().is_ok());
        assert!(PunishmentRequest::demerit("again").validate().is_ok());
        assert!(PunishmentRequest::ban("spam").validate().is_ok());
        assert!(PunishmentRequest::unban("appeal").validate().is_ok());
    }

    #[test]
    fn requests_serialise_only_the_fields_that_were_set() {
        let json = serde_json::to_value(PunishmentRequest::warning("be nice")).unwrap();
        assert_eq!(json["type"], "warning");
        assert_eq!(json["reason"], "be nice");
        assert!(json.get("permission_key").is_none());
        assert!(json.get("duration_minutes").is_none());

        let json = serde_json::to_value(PunishmentRequest::temp_ban(90, "cooldown")).unwrap();
        assert_eq!(json["type"], "temp_ban");
        assert_eq!(json["duration_minutes"], 90);
    }

    #[test]
    fn the_restrictive_types_are_the_ones_with_a_rank_guard() {
        // `post.go`'s guard covers exactly ban / temp_ban / revoke.
        assert!(PunishmentType::Ban.is_restrictive());
        assert!(PunishmentType::TempBan.is_restrictive());
        assert!(PunishmentType::Revoke.is_restrictive());
        assert!(!PunishmentType::Warning.is_restrictive());
        assert!(!PunishmentType::Demerit.is_restrictive());
        assert!(!PunishmentType::Unban.is_restrictive());
        assert!(!PunishmentType::Restore.is_restrictive());
    }

    #[test]
    fn the_type_is_displayed_as_the_wire_spelling() {
        assert_eq!(PunishmentType::TempBan.to_string(), "temp_ban");
    }
}
