//! Reporting and notification endpoints (`bots.md` §6.6).
//!
//! Reporting is the only moderation lever a bot has: the default group holds no
//! `user.punish` (`bots.md` §14), so a bot can flag content but never act on it.
//! The server applies two rules worth enforcing locally: one pending report per
//! reporter per target (`409` otherwise), and 20 reports per hour (`429`).

use serde::{Deserialize, Serialize};

use crate::error::{Result, SdkError};
use crate::models::{Id, JsonObject, NotificationList, Report, ReportTarget};

use super::AuthenticatedClient;
use crate::client::API_PREFIX;

/// Maximum report reason length (`bots.md` §6.6).
pub const MAX_REASON_CHARS: usize = 500;
/// Reports allowed per reporter per hour (`repository.MaxReportsPerHour`).
pub const MAX_REPORTS_PER_HOUR: u32 = 20;

/// A report to submit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewReport {
    /// `post` | `message` | `user`.
    pub target_type: ReportTarget,
    /// The reported resource id.
    pub target_id: Id,
    /// Why it is being reported.
    pub reason: String,
}

impl NewReport {
    /// A report of `target_type`/`target_id` with a reason.
    pub fn new(target_type: ReportTarget, target_id: Id, reason: impl Into<String>) -> Self {
        Self {
            target_type,
            target_id,
            reason: reason.into(),
        }
    }

    /// Report a post.
    pub fn post(post_id: Id, reason: impl Into<String>) -> Self {
        Self::new(ReportTarget::Post, post_id, reason)
    }

    /// Report a message.
    pub fn message(message_id: Id, reason: impl Into<String>) -> Self {
        Self::new(ReportTarget::Message, message_id, reason)
    }

    /// Report a user.
    pub fn user(user_id: Id, reason: impl Into<String>) -> Self {
        Self::new(ReportTarget::User, user_id, reason)
    }

    /// Reject locally what the server would reject with `400`.
    ///
    /// `self_id` lets the self-report rule be applied: reporting yourself as a user
    /// is a `400` (`bots.md` §6.6).
    pub fn validate(&self, self_id: Option<Id>) -> Result<()> {
        let trimmed = self.reason.trim();
        if trimmed.is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "reason is required".into(),
            });
        }
        if trimmed.chars().count() > MAX_REASON_CHARS {
            return Err(SdkError::InvalidArgument {
                message: format!("reason must be at most {MAX_REASON_CHARS} characters"),
            });
        }
        if self.target_id <= 0 {
            return Err(SdkError::InvalidArgument {
                message: "target_id must be a positive id".into(),
            });
        }
        if self.target_type == ReportTarget::User {
            if let Some(self_id) = self_id {
                if self_id == self.target_id && self_id > 0 {
                    return Err(SdkError::InvalidArgument {
                        message: "cannot report yourself".into(),
                    });
                }
            }
        }
        Ok(())
    }
}

impl AuthenticatedClient {
    /// `POST /api/v1/reports` — submit a report.
    ///
    /// # Errors
    ///
    /// * `409 you have already reported this` — one pending report per target;
    /// * `429 too many reports, please try again later` — above
    ///   [`MAX_REPORTS_PER_HOUR`];
    /// * `404` — the target does not exist.
    pub fn report(&self, report: &NewReport) -> Result<Report> {
        report.validate(None)?;
        let body = serde_json::to_value(report).map_err(super::json_error)?;
        let response =
            self.base
                .http
                .json_raw("POST", &format!("{API_PREFIX}/reports"), Some(&body), &[])?;
        // Some builds answer with the report, others with `{"ok":true}`.
        match response.decode::<Report>() {
            Ok(report) => Ok(report),
            Err(_) => {
                let value = response.json()?;
                Ok(Report {
                    id: 0,
                    reporter_id: 0,
                    target_type: report.target_type.as_str().to_string(),
                    target_id: report.target_id,
                    reason: report.reason.clone(),
                    status: "pending".to_string(),
                    extra: match value {
                        serde_json::Value::Object(map) => map,
                        _ => serde_json::Map::new(),
                    },
                    ..Report::default()
                })
            }
        }
    }

    /// `GET /api/v1/reports/mine` — reports the bot filed, with their outcomes.
    pub fn my_reports(&self) -> Result<serde_json::Value> {
        let response =
            self.base
                .http
                .json_raw("GET", &format!("{API_PREFIX}/reports/mine"), None, &[])?;
        response.json()
    }

    /// `GET /api/v1/notifications` — the notification inbox.
    pub fn notifications(&self) -> Result<NotificationList> {
        self.get_json(&format!("{API_PREFIX}/notifications"))
    }

    /// `POST /api/v1/notifications/read` — mark notifications read.
    pub fn mark_notifications_read(&self) -> Result<JsonObject> {
        self.ok_object("POST", &format!("{API_PREFIX}/notifications/read"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_requires_a_reason() {
        assert!(NewReport::post(1, "   ").validate(None).is_err());
        assert!(NewReport::post(1, "垃圾广告").validate(None).is_ok());
        assert!(NewReport::message(1, "spam").validate(None).is_ok());
        assert!(NewReport::user(1, "spam").validate(None).is_ok());
    }

    #[test]
    fn report_reason_length_is_capped() {
        let long = "a".repeat(MAX_REASON_CHARS + 1);
        assert!(NewReport::post(1, long).validate(None).is_err());
        let at_limit = "a".repeat(MAX_REASON_CHARS);
        assert!(NewReport::post(1, at_limit).validate(None).is_ok());
    }

    #[test]
    fn report_rejects_self_and_nonpositive_ids() {
        assert!(NewReport::user(42, "spam").validate(Some(42)).is_err());
        assert!(NewReport::user(43, "spam").validate(Some(42)).is_ok());
        assert!(NewReport::post(0, "spam").validate(None).is_err());
        // The self-report rule only applies to user targets.
        assert!(NewReport::post(42, "spam").validate(Some(42)).is_ok());
    }

    #[test]
    fn report_serialises_the_documented_shape() {
        let json = serde_json::to_value(NewReport::post(123, "垃圾广告")).unwrap();
        assert_eq!(json["target_type"], "post");
        assert_eq!(json["target_id"], 123);
        assert_eq!(json["reason"], "垃圾广告");
    }

    #[test]
    fn documented_quota_constant() {
        assert_eq!(MAX_REPORTS_PER_HOUR, 20);
        assert_eq!(MAX_REASON_CHARS, 500);
    }
}
