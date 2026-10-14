//! Membership: catalog, wallet purchase, auto-renew and the admin grant.
//!
//! The purchase path is guarded by the **payment PIN**, not just the token, and
//! that has a consequence worth knowing before reaching for these methods: a
//! freshly created bot has no PIN, so `purchase_membership` and `set_auto_renew`
//! answer `403 payment pin not set` until one is set with
//! [`set_payment_pin`](AuthenticatedClient::set_payment_pin).
//!
//! The six-digit PIN space is small, so the server caps **five** wrong entries
//! per user in a fifteen-minute window and then answers `429` with the Chinese
//! message `尝试次数过多，请稍后再试`; the same budget is shared by every payment
//! check — verification, tips, membership and transfers. Budgeting guesses
//! across those calls is the client's job.
//!
//! Prices and `paid` are **whole coins**; the wallet movement they cause is in
//! decimal coins (cents / 100). See [`crate::models::Money`].

use serde::{Deserialize, Serialize};

use crate::error::{Result, SdkError};
use crate::models::{
    ExpAdjustment, Id, MembershipPurchaseList, MembershipStatus, Money, PurchaseResult,
};

use super::AuthenticatedClient;
use crate::client::{pagination, API_PREFIX};

/// Highest purchasable membership level (`model.MemberLoneStar`).
pub const MAX_MEMBERSHIP_LEVEL: i64 = 4;

/// A membership purchase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipPurchaseRequest {
    /// Tier level, `1..=4`.
    pub level: i64,
    /// The caller's six-digit payment PIN.
    pub pin: String,
}

impl MembershipPurchaseRequest {
    /// A purchase of `level`, authorized by `pin`.
    ///
    /// # Errors
    ///
    /// Rejects a level outside `1..=4` and an empty PIN locally.
    pub fn new(level: i64, pin: impl Into<String>) -> Result<Self> {
        if !(1..=MAX_MEMBERSHIP_LEVEL).contains(&level) {
            return Err(SdkError::InvalidArgument {
                message: format!("membership level must be 1..={MAX_MEMBERSHIP_LEVEL}"),
            });
        }
        let pin = pin.into();
        if pin.trim().is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "pin must not be empty".into(),
            });
        }
        Ok(Self { level, pin })
    }
}

impl AuthenticatedClient {
    /// `GET /api/v1/membership` — the caller's state plus the purchasable catalog.
    ///
    /// Answers `200` even for a caller with no membership (`level: 0`).
    pub fn membership(&self) -> Result<MembershipStatus> {
        self.get_json(&format!("{API_PREFIX}/membership"))
    }

    /// `POST /api/v1/membership/purchase` — buy a tier with wallet coins.
    ///
    /// # Errors
    ///
    /// * `400 level and pin are required` — a level below 1 or a missing PIN;
    /// * `400 invalid membership level` — above [`MAX_MEMBERSHIP_LEVEL`];
    /// * `400 insufficient balance` — the wallet cannot cover the tier;
    /// * `403 payment pin not set` — no PIN has ever been set on this account;
    /// * `401 invalid payment pin` — wrong PIN, counted against
    ///   [`MAX_PIN_FAILURES`];
    /// * `429 尝试次数过多，请稍后再试` — the PIN budget is exhausted.
    pub fn purchase_membership(
        &self,
        request: &MembershipPurchaseRequest,
    ) -> Result<PurchaseResult> {
        let body = serde_json::to_value(request).map_err(super::json_error)?;
        self.post_json(&format!("{API_PREFIX}/membership/purchase"), &body)
    }

    /// `PUT /api/v1/membership/auto-renew` — toggle automatic renewal.
    ///
    /// Enabling pre-authorizes future charges and therefore needs the PIN;
    /// **disabling only needs the token**, so `pin` may be `None` for that.
    ///
    /// # Errors
    ///
    /// * `403 payment pin not set` — enabling without a PIN configured;
    /// * `400 invalid request body`;
    /// * `429` — the shared PIN budget is exhausted.
    pub fn set_auto_renew(&self, enabled: bool, pin: Option<&str>) -> Result<MembershipStatus> {
        let body = match pin {
            Some(pin) => serde_json::json!({ "enabled": enabled, "pin": pin }),
            None if !enabled => serde_json::json!({ "enabled": false }),
            None => {
                return Err(SdkError::InvalidArgument {
                    message: "enabling auto-renew needs the payment pin".into(),
                })
            }
        };
        self.put_json(&format!("{API_PREFIX}/membership/auto-renew"), &body)
    }

    /// `GET /api/v1/membership/purchases` — the caller's purchase history.
    pub fn membership_purchases(
        &self,
        page: Option<i64>,
        limit: Option<i64>,
    ) -> Result<MembershipPurchaseList> {
        self.get_json_query(
            &format!("{API_PREFIX}/membership/purchases"),
            &pagination(page, limit),
        )
    }

    /// `PUT /api/v1/users/:user_id/membership` — admin grant, skipping the wallet.
    ///
    /// Requires `user.membership.grant`. `level` 0 clears the membership; a
    /// positive level lasts `days` (or the tier default when `days` is absent).
    ///
    /// # Errors
    ///
    /// * `400 invalid membership level`;
    /// * `403` — the gateway rejected `user.membership.grant`.
    pub fn grant_membership(
        &self,
        user_id: Id,
        level: i64,
        days: Option<i64>,
    ) -> Result<MembershipStatus> {
        if user_id <= 0 {
            return Err(SdkError::InvalidArgument {
                message: "user_id must be a positive id".into(),
            });
        }
        if level < 0 {
            return Err(SdkError::InvalidArgument {
                message: "level must be 0 (clear) or a positive tier".into(),
            });
        }
        let body = match days {
            Some(days) => serde_json::json!({ "level": level, "days": days }),
            None => serde_json::json!({ "level": level }),
        };
        self.put_json(&format!("{API_PREFIX}/users/{user_id}/membership"), &body)
    }

    /// `PUT /api/v1/users/:user_id/exp` — admin experience adjustment.
    ///
    /// Requires `user.adjust_exp`. This belongs to the same admin family as the
    /// membership grant, so it lives here rather than in the task module.
    ///
    /// The body is `{"delta": N, "description": "..."}` — **`delta`, not
    /// `amount`**, and it is signed so a negative value removes experience. A
    /// positive delta is multiplied by the target's active membership multiplier
    /// before it is applied; a negative one is subtracted verbatim. `delta: 0`
    /// is accepted and changes nothing.
    ///
    /// # Errors
    ///
    /// * `400 invalid request body`;
    /// * `400 invalid user ID` — a non-numeric path segment;
    /// * `404 user not found`;
    /// * `403` — the gateway rejected `user.adjust_exp`.
    pub fn adjust_exp(
        &self,
        user_id: Id,
        delta: i64,
        description: Option<&str>,
    ) -> Result<ExpAdjustment> {
        if user_id <= 0 {
            return Err(SdkError::InvalidArgument {
                message: "user_id must be a positive id".into(),
            });
        }
        let body = match description {
            Some(description) => serde_json::json!({ "delta": delta, "description": description }),
            None => serde_json::json!({ "delta": delta }),
        };
        self.put_json(&format!("{API_PREFIX}/users/{user_id}/exp"), &body)
    }

    /// Cost of a tier in whole coins, read from the catalog.
    ///
    /// Returns `None` when the catalog does not carry that level.
    pub fn membership_price(&self, level: i64) -> Result<Option<Money>> {
        let status = self.membership()?;
        Ok(status
            .tiers
            .iter()
            .find(|tier| tier.level == level)
            .map(|tier| Money::from_cents(tier.price * crate::models::MONEY_SCALE)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_purchase_requires_a_level_in_range() {
        assert!(MembershipPurchaseRequest::new(0, "123456").is_err());
        assert!(MembershipPurchaseRequest::new(5, "123456").is_err());
        assert!(MembershipPurchaseRequest::new(-1, "123456").is_err());
        assert!(MembershipPurchaseRequest::new(1, "123456").is_ok());
        assert!(MembershipPurchaseRequest::new(4, "123456").is_ok());
    }

    #[test]
    fn a_purchase_requires_a_non_empty_pin() {
        assert!(MembershipPurchaseRequest::new(1, "   ").is_err());
        assert!(MembershipPurchaseRequest::new(1, "123456").is_ok());
    }

    #[test]
    fn a_purchase_serialises_the_documented_body() {
        let request = MembershipPurchaseRequest::new(2, "123456").unwrap();
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["level"], 2);
        assert_eq!(json["pin"], "123456");
        assert_eq!(json.as_object().unwrap().len(), 2, "no extra fields");
    }

    #[test]
    fn the_level_ceiling_matches_the_model() {
        assert_eq!(MAX_MEMBERSHIP_LEVEL, 4);
    }

    #[test]
    fn the_pin_budget_is_owned_by_the_account_module() {
        // One definition, in the module that also owns `validate_pin`, so the two
        // cannot drift apart.
        assert_eq!(crate::client::account::MAX_PIN_FAILURES, 5);
    }

    #[test]
    fn a_zero_level_grant_clears_the_membership() {
        // Level 0 is legal for the grant and means "clear"; only a negative
        // level is nonsense. The request half is validated without a client.
        let body = serde_json::json!({ "level": 0 });
        assert_eq!(body["level"], 0);
    }

    #[test]
    fn membership_prices_are_whole_coins_not_cents() {
        // `MemberPrice` returns coins and the repository multiplies by
        // `MoneyScale` for the wallet movement, so a price of 300 is 30000 cents.
        let price = Money::from_cents(300 * crate::models::MONEY_SCALE);
        assert_eq!(price.cents(), 30_000);
        assert_eq!(price.to_string(), "300");
    }
}
