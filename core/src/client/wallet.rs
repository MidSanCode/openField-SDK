//! Wallet endpoints (the `wallet.view` / `wallet.manage` permissions).
//!
//! Money only moves through an operator: `POST /wallet/adjust` requires
//! `wallet.manage`, and the gateway enforces it. Two traps make this surface
//! worth a typed wrapper rather than the escape hatch:
//!
//! * **Units.** Every amount here is the server's `model.Cents`, which puts a
//!   **decimal coin** value on the wire — `10` means ten coins (1000 cents) and
//!   `12.5` means twelve and a half. Tips, memberships and task rewards, by
//!   contrast, are whole coins. [`Money`] keeps the exact cent count so a value
//!   such as `10.01` survives a round trip.
//! * **Sign.** The handler stores `amount` exactly as sent and never checks it
//!   against `type`: a positive amount with `type: "deduct"` is a *credit*
//!   (`wallet.go`, and its own comment admits the consistency check is missing).
//!   [`NewWalletAdjustment::deduct`] therefore negates the amount itself.
//!
//! `wallet.md` predates both `post_tips` and `POST /transfers` and claims money
//! "only changes hands through an admin"; the source disagreed. See
//! [`crate::docs::KNOWN_DIVERGENCES`].

use serde::{Deserialize, Serialize};

use crate::error::{Result, SdkError};
use crate::models::{Id, Money, WalletAdjustment, WalletPage};

use super::AuthenticatedClient;
use crate::client::{pagination, API_PREFIX};

/// The `recharge` transaction type.
pub const KIND_RECHARGE: &str = "recharge";
/// The `deduct` transaction type.
pub const KIND_DEDUCT: &str = "deduct";

/// A wallet adjustment to submit.
///
/// Build one with [`NewWalletAdjustment::recharge`] or
/// [`NewWalletAdjustment::deduct`], which fix the sign of `amount` for you.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewWalletAdjustment {
    /// Target user.
    pub user_id: Id,
    /// Signed amount; positive credits, negative debits.
    pub amount: Money,
    /// `recharge` | `deduct`. Defaulted from the sign when omitted.
    ///
    /// The wire field is `type`; `kind` is the Rust-side name because `type` is a
    /// reserved word.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Optional operator note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl NewWalletAdjustment {
    /// Credit `amount` (which must be positive).
    ///
    /// # Errors
    ///
    /// Rejects a non-positive amount; use [`Self::deduct`] for the other side.
    pub fn recharge(user_id: Id, amount: Money) -> Result<Self> {
        Self::build(user_id, amount, KIND_RECHARGE, 1)
    }

    /// Debit `amount` (which must be positive), negating it for the wire.
    ///
    /// # Errors
    ///
    /// Rejects a non-positive amount.
    pub fn deduct(user_id: Id, amount: Money) -> Result<Self> {
        Self::build(user_id, amount, KIND_DEDUCT, -1)
    }

    fn build(user_id: Id, amount: Money, kind: &str, sign: i64) -> Result<Self> {
        if user_id <= 0 {
            return Err(SdkError::InvalidArgument {
                message: "user_id must be a positive id".into(),
            });
        }
        if amount.is_zero() {
            // The handler answers 400 `amount must not be zero`, and `binding:"required"`
            // rejects it even earlier.
            return Err(SdkError::InvalidArgument {
                message: "amount must not be zero".into(),
            });
        }
        if amount.is_negative() {
            return Err(SdkError::InvalidArgument {
                message: "pass a positive amount; choose recharge or deduct instead".into(),
            });
        }
        let cents = amount
            .cents()
            .checked_mul(sign)
            .ok_or_else(|| SdkError::InvalidArgument {
                message: "amount out of range".into(),
            })?;
        Ok(Self {
            user_id,
            amount: Money::from_cents(cents),
            kind: Some(kind.to_string()),
            description: None,
        })
    }

    /// Builder: attach the operator note.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Builder: override the transaction type.
    ///
    /// The server never enforces that `kind` agrees with the sign of `amount`;
    /// overriding it changes only the recorded label.
    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }
}

impl AuthenticatedClient {
    /// `GET /api/v1/wallet` — the caller's balance and a page of transactions.
    ///
    /// Requires `wallet.view`, which the default `everyone` group holds.
    ///
    /// # Errors
    ///
    /// * `403` — the gateway rejected `wallet.view`.
    pub fn wallet(&self, page: Option<i64>, limit: Option<i64>) -> Result<WalletPage> {
        self.get_json_query(&format!("{API_PREFIX}/wallet"), &pagination(page, limit))
    }

    /// `POST /api/v1/wallet/adjust` — apply a recharge or deduction.
    ///
    /// Requires `wallet.manage`. The default group holds it, so **any
    /// authenticated bot can currently move any user's balance** until group
    /// permissions are tightened; treat this as an operator tool.
    ///
    /// # Errors
    ///
    /// * `400 insufficient balance` — a deduction would drive the balance below
    ///   zero. Note the handler answers **400**, not the `409` `wallet.md`
    ///   documents;
    /// * `400 invalid request body` — a zero amount or a non-positive `user_id`;
    /// * `404 user not found` — the target user does not exist;
    /// * `403` — the gateway rejected `wallet.manage`.
    pub fn adjust_wallet(&self, adjustment: &NewWalletAdjustment) -> Result<WalletAdjustment> {
        let body = serde_json::to_value(adjustment).map_err(super::json_error)?;
        self.post_json(&format!("{API_PREFIX}/wallet/adjust"), &body)
    }

    /// Convenience: credit `amount` to `user_id`, returning the new balance.
    pub fn recharge_wallet(&self, user_id: Id, amount: Money) -> Result<WalletAdjustment> {
        self.adjust_wallet(&NewWalletAdjustment::recharge(user_id, amount)?)
    }

    /// Convenience: debit `amount` from `user_id`, returning the new balance.
    pub fn deduct_wallet(&self, user_id: Id, amount: Money) -> Result<WalletAdjustment> {
        self.adjust_wallet(&NewWalletAdjustment::deduct(user_id, amount)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coins(value: f64) -> Money {
        Money::from_coins(value).unwrap()
    }

    #[test]
    fn recharge_keeps_the_amount_positive() {
        let adjustment = NewWalletAdjustment::recharge(42, coins(12.5)).unwrap();
        assert_eq!(adjustment.amount.cents(), 1250);
        assert_eq!(adjustment.kind.as_deref(), Some(KIND_RECHARGE));
        let json = serde_json::to_value(&adjustment).unwrap();
        assert_eq!(json["user_id"], 42);
        assert_eq!(json["amount"], 12.5);
        assert_eq!(json["type"], "recharge");
        assert!(json.get("description").is_none(), "absent fields stay out");
    }

    #[test]
    fn deduct_negates_what_the_server_stores() {
        // The handler stores `amount` verbatim and never reconciles it with `type`,
        // so the sign has to be carried by the amount itself.
        let adjustment = NewWalletAdjustment::deduct(42, coins(5.0)).unwrap();
        let json = serde_json::to_value(&adjustment).unwrap();
        assert_eq!(json["amount"], -5);
        assert_eq!(json["type"], "deduct");
    }

    #[test]
    fn whole_coin_amounts_serialise_as_integers() {
        // `10` rather than `10.0`, matching the server's own MarshalJSON output.
        let adjustment = NewWalletAdjustment::recharge(1, coins(10.0)).unwrap();
        let text = serde_json::to_string(&adjustment).unwrap();
        assert!(text.contains("\"amount\":10"), "got: {text}");
        assert!(!text.contains("10.0"), "got: {text}");
    }

    #[test]
    fn a_zero_amount_is_refused_before_the_round_trip() {
        let error = NewWalletAdjustment::recharge(1, Money::ZERO).unwrap_err();
        assert!(
            error.to_string().contains("must not be zero"),
            "got: {error}"
        );
    }

    #[test]
    fn a_negative_amount_must_use_deduct() {
        let error = NewWalletAdjustment::recharge(1, coins(-1.0)).unwrap_err();
        assert!(
            error.to_string().contains("positive amount"),
            "got: {error}"
        );
        assert!(NewWalletAdjustment::deduct(1, coins(-1.0)).is_err());
    }

    #[test]
    fn a_nonpositive_user_id_is_refused() {
        assert!(NewWalletAdjustment::recharge(0, coins(1.0)).is_err());
        assert!(NewWalletAdjustment::deduct(-3, coins(1.0)).is_err());
    }

    #[test]
    fn the_description_is_optional_but_round_trips() {
        let adjustment = NewWalletAdjustment::recharge(7, coins(1.0))
            .unwrap()
            .with_description("补偿");
        let json = serde_json::to_value(&adjustment).unwrap();
        assert_eq!(json["description"], "补偿");
    }

    #[test]
    fn money_round_trips_the_server_shapes() {
        // A number.
        let parsed: Money = serde_json::from_str("12.5").unwrap();
        assert_eq!(parsed.cents(), 1250);
        // A whole number.
        let parsed: Money = serde_json::from_str("100").unwrap();
        assert_eq!(parsed.cents(), 10_000);
        // A numeric string, which `Cents.UnmarshalJSON` also accepts.
        let parsed: Money = serde_json::from_str("\"0.01\"").unwrap();
        assert_eq!(parsed.cents(), 1);
        // null, which several handlers emit for an absent amount.
        let parsed: Money = serde_json::from_str("null").unwrap();
        assert_eq!(parsed, Money::ZERO);
    }

    #[test]
    fn money_rounds_to_the_nearest_representable_cent() {
        // The server rounds too (`FromCoins` calls math.Round), so the behaviour
        // matches — but the third decimal is subject to binary floating point on
        // both sides: 1.005 is really 1.00499999…, so it lands on 100 cents.
        let parsed: Money = serde_json::from_str("1.005").unwrap();
        assert_eq!(parsed.cents(), 100);
        // A value that does round up.
        let parsed: Money = serde_json::from_str("1.006").unwrap();
        assert_eq!(parsed.cents(), 101);
        // And one with no ambiguity at all.
        let parsed: Money = serde_json::from_str("0.01").unwrap();
        assert_eq!(parsed.cents(), 1);
    }

    #[test]
    fn money_carries_no_float_error() {
        // The reason money is not a bare f64: 10.01 has no exact binary form.
        let parsed: Money = serde_json::from_str("10.01").unwrap();
        assert_eq!(parsed.cents(), 1001);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), "10.01");
    }

    #[test]
    fn money_rejects_nonsense() {
        assert!(serde_json::from_str::<Money>("\"abc\"").is_err());
        assert!(Money::from_coins(f64::NAN).is_err());
        assert!(Money::from_coins(f64::INFINITY).is_err());
    }
}
