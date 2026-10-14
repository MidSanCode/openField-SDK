//! Tasks, daily sign-in and experience history.
//!
//! Three shapes share this module and they are easy to confuse:
//!
//! * the **catalog** (`GET /tasks`) carries every task with the caller's
//!   `progress` / `completed` / `claimable`, plus the sign-in `streak` and the
//!   make-up price;
//! * **claims** award experience and coins. One-time/streak tasks go through
//!   `POST /tasks/:code/claim`, per-day activity tiers through
//!   `POST /tasks/daily/:code/claim` — the daily tiers are cumulative, so a
//!   twenty-post day can claim all four posting tiers, once each per local day;
//! * **make-up sign-in** (`/tasks/daily-login/makeup`, `.../makeup-date`) charges
//!   coins to backfill a missed day: it renews the streak and grants experience
//!   but never the daily currency.
//!
//! Watch the spelling: the **route** is `daily-login` (a hyphen) while the task
//! **code** is `daily_login` (an underscore). Sending the code to the route, or
//! the route to `POST /tasks/:code/claim`, silently answers the wrong endpoint —
//! `/tasks/daily_login/claim` matches the generic `:code` route and is reported
//! as `404 task not found`.
//!
//! Every currency figure here is a plain integer count of **whole coins**
//! (`reward_currency`, `makeup_cost`, `currency`), unlike the wallet's decimal
//! coin amounts under [`crate::client::wallet`].

use crate::error::{Result, SdkError};
use crate::models::{
    CheckinCalendar, ClaimResult, DailyLoginClaim, ExpHistory, MakeupResult, TaskList,
};

use super::AuthenticatedClient;
use crate::client::API_PREFIX;

/// Days back a make-up may reach (`repository.ErrMakeupGapTooLarge`).
pub const MAX_MAKEUP_DAYS: i64 = 3;

/// The daily sign-in **route** segment, `POST /tasks/daily-login/claim`.
pub const DAILY_LOGIN_ROUTE: &str = "daily-login";

/// The daily sign-in **task code** as seeded in `tasks.code`.
pub const DAILY_LOGIN_CODE: &str = "daily_login";

impl AuthenticatedClient {
    /// `GET /api/v1/tasks` — the task catalog for the caller.
    ///
    /// Each entry carries `kind` / `progress` / `target` / `completed` /
    /// `claimable`; group by `task.is_daily()` to render the daily tiers.
    pub fn tasks(&self) -> Result<TaskList> {
        self.get_json(&format!("{API_PREFIX}/tasks"))
    }

    /// `POST /api/v1/tasks/:code/claim` — claim a one-time or streak task.
    ///
    /// # Errors
    ///
    /// * `404 task not found`;
    /// * `409 task requirements not met`;
    /// * `409 task already claimed`.
    pub fn claim_task(&self, code: &str) -> Result<ClaimResult> {
        let code = require_code(code)?;
        self.post_json(
            &format!("{API_PREFIX}/tasks/{code}/claim"),
            &serde_json::json!({}),
        )
    }

    /// `POST /api/v1/tasks/daily/:code/claim` — claim today's activity tier.
    ///
    /// # Errors
    ///
    /// * `404 task not found`;
    /// * `409 task requirements not met` — today's progress is below the target;
    /// * `409 task already claimed today` — the tier is once per local day, and
    ///   the check is a unique index on `(user_id, task_id, cycle_key)`.
    pub fn claim_daily_task(&self, code: &str) -> Result<ClaimResult> {
        let code = require_code(code)?;
        self.post_json(
            &format!("{API_PREFIX}/tasks/daily/{code}/claim"),
            &serde_json::json!({}),
        )
    }

    /// `POST /api/v1/tasks/daily-login/claim` — sign in for today.
    ///
    /// Answers `granted: false` rather than an error when today is already
    /// signed in.
    pub fn claim_daily_login(&self) -> Result<DailyLoginClaim> {
        self.post_json(
            &format!("{API_PREFIX}/tasks/{DAILY_LOGIN_ROUTE}/claim"),
            &serde_json::json!({}),
        )
    }

    /// `GET /api/v1/tasks/daily-login/calendar` — the sign-in grid.
    pub fn checkin_calendar(&self) -> Result<CheckinCalendar> {
        self.get_json(&format!("{API_PREFIX}/tasks/{DAILY_LOGIN_ROUTE}/calendar"))
    }

    /// `POST /api/v1/tasks/daily-login/makeup` — paid make-up of the missed day.
    ///
    /// # Errors
    ///
    /// * `400 insufficient balance` — the caller cannot pay the make-up price;
    /// * `409 make-up only covers the last three days` — the gap is wider than
    ///   [`MAX_MAKEUP_DAYS`].
    pub fn makeup_checkin(&self) -> Result<MakeupResult> {
        self.post_json(
            &format!("{API_PREFIX}/tasks/{DAILY_LOGIN_ROUTE}/makeup"),
            &serde_json::json!({}),
        )
    }

    /// `POST /api/v1/tasks/daily-login/makeup-date` — make up one specific day.
    ///
    /// `date` is `YYYY-MM-DD` in the configured game timezone.
    ///
    /// # Errors
    ///
    /// * `400 date is required` — an empty or missing date;
    /// * `400` — the date is today, in the future, or before registration;
    /// * `400 insufficient balance`.
    pub fn makeup_checkin_on(&self, date: &str) -> Result<MakeupResult> {
        let date = date.trim();
        validate_date(date)?;
        self.post_json(
            &format!("{API_PREFIX}/tasks/{DAILY_LOGIN_ROUTE}/makeup-date"),
            &serde_json::json!({ "date": date }),
        )
    }

    /// `GET /api/v1/exp/history` — newest-first experience awards.
    pub fn exp_history(&self, limit: Option<i64>) -> Result<ExpHistory> {
        let query = match limit {
            Some(limit) => vec![("limit".to_string(), limit.to_string())],
            None => Vec::new(),
        };
        self.get_json_query(&format!("{API_PREFIX}/exp/history"), &query)
    }
}

/// A task code is a path segment, so it must not introduce one.
fn require_code(code: &str) -> Result<String> {
    let trimmed = code.trim();
    if trimmed.is_empty() {
        return Err(SdkError::InvalidArgument {
            message: "task code must not be empty".into(),
        });
    }
    if trimmed.contains('/') || trimmed.contains("..") || trimmed.contains('?') {
        return Err(SdkError::InvalidArgument {
            message: format!("task code `{code}` must be a single path segment"),
        });
    }
    Ok(trimmed.to_string())
}

/// Reject a date the server would refuse anyway, so the mistake surfaces locally.
fn validate_date(date: &str) -> Result<()> {
    let bytes = date.as_bytes();
    let shaped = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit());
    if !shaped {
        return Err(SdkError::InvalidArgument {
            message: match date {
                "" => "date must not be empty; expected `YYYY-MM-DD`".to_string(),
                other => format!("date must be `YYYY-MM-DD`, got `{other}`"),
            },
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_code_is_refused() {
        assert!(require_code("   ").is_err());
        assert!(require_code("").is_err());
    }

    #[test]
    fn a_code_that_would_add_a_path_segment_is_refused() {
        assert!(require_code("../wallet").is_err());
        assert!(require_code("a/b").is_err());
        assert!(require_code("daily?x=1").is_err());
        assert_eq!(require_code(" daily_posts_5 ").unwrap(), "daily_posts_5");
    }

    #[test]
    fn dates_are_shape_checked() {
        assert!(validate_date("2026-08-03").is_ok());
        assert!(validate_date("2026-8-3").is_err());
        assert!(validate_date("20260803").is_err());
        assert!(validate_date("2026-08-3x").is_err());
        assert!(validate_date("").is_err());
    }

    #[test]
    fn the_route_and_the_code_are_spelled_differently() {
        // The hyphen/underscore split is the whole reason both constants exist.
        assert_eq!(DAILY_LOGIN_ROUTE, "daily-login");
        assert_eq!(DAILY_LOGIN_CODE, "daily_login");
        assert_ne!(DAILY_LOGIN_ROUTE, DAILY_LOGIN_CODE);
    }

    #[test]
    fn the_makeup_window_matches_the_repository_error() {
        assert_eq!(MAX_MAKEUP_DAYS, 3);
    }
}
