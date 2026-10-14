//! End-to-end encryption key distribution.
//!
//! The server is a dumb relay for E2EE material: it stores the caller's public
//! key and a per-member map of opaque group-key envelopes, and never inspects
//! either. Two details are load-bearing:
//!
//! * **the envelope map is keyed by user id as a JSON string.** The handler binds
//!   `map[int64]string`, and Go marshals an integer-keyed map with string keys, so
//!   the body really is `{"7":"<base64>","9":"<base64>"}`. This SDK models it as
//!   [`E2eeEnvelopeMap`] so a binding can hand one across unchanged.
//! * **an empty `envelopes` object is rejected** (`400 no envelopes provided`),
//!   and `PUT /users/me/e2ee-key` accepts an empty `public_key`, which is how a
//!   client *clears* its published key.
//!
//! Rotating the group key is the caller's problem: the server keeps a
//! monotonically increasing `version` and answers it back, and it rejects a
//! non-member with `403` on both conversation routes. Every successful write
//! publishes a `chat.e2ee.keys.updated` event to the conversation's active
//! members, which [`EventType::ChatE2eeKeysUpdated`](crate::EventType) surfaces.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Result, SdkError};
use crate::models::{E2eeKeys, E2eePutResult, Id, JsonObject};

use super::AuthenticatedClient;
use crate::client::API_PREFIX;

/// A batch of group-key envelopes, keyed by the member they are wrapped for.
///
/// Serialises to the object shape the handler binds, with the ids written as
/// JSON strings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct E2eeEnvelopeMap(pub BTreeMap<Id, String>);

impl E2eeEnvelopeMap {
    /// An empty batch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from `(target_user_id, ciphertext)` pairs.
    pub fn from_pairs<I, S>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (Id, S)>,
        S: Into<String>,
    {
        Self(
            pairs
                .into_iter()
                .map(|(user_id, ciphertext)| (user_id, ciphertext.into()))
                .collect(),
        )
    }

    /// Add or replace one member's envelope.
    pub fn insert(&mut self, target_user_id: Id, ciphertext: impl Into<String>) -> &mut Self {
        self.0.insert(target_user_id, ciphertext.into());
        self
    }

    /// How many members the batch covers.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing is wrapped.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// One member's ciphertext.
    pub fn get(&self, target_user_id: Id) -> Option<&str> {
        self.0.get(&target_user_id).map(String::as_str)
    }

    /// The pairs, in ascending user-id order.
    pub fn iter(&self) -> impl Iterator<Item = (Id, &str)> + '_ {
        self.0.iter().map(|(id, text)| (*id, text.as_str()))
    }

    /// Refuse a batch the server would reject with `400 no envelopes provided`.
    ///
    /// # Errors
    ///
    /// Rejects an empty batch or a non-positive member id.
    pub fn validate(&self) -> Result<()> {
        if self.0.is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "at least one envelope is required".into(),
            });
        }
        if let Some(bad) = self.0.keys().find(|id| **id <= 0) {
            return Err(SdkError::InvalidArgument {
                message: format!("envelope target_user_id must be positive, got {bad}"),
            });
        }
        Ok(())
    }
}

impl AuthenticatedClient {
    /// `PUT /api/v1/users/me/e2ee-key` — publish (or clear) the caller's public key.
    ///
    /// Passing an empty string clears it; the handler accepts that and answers
    /// `200`.
    ///
    /// # Errors
    ///
    /// * `400 invalid request body`.
    pub fn set_e2ee_public_key(&self, public_key: &str) -> Result<JsonObject> {
        let body = serde_json::json!({ "public_key": public_key });
        self.put_json(&format!("{API_PREFIX}/users/me/e2ee-key"), &body)
    }

    /// `GET /api/v1/conversations/:id/e2ee-keys` — every envelope plus the caller's.
    ///
    /// # Errors
    ///
    /// * `400 invalid conversation ID`;
    /// * `403 you are not a member of this conversation`.
    pub fn e2ee_keys(&self, conversation_id: Id) -> Result<E2eeKeys> {
        if conversation_id <= 0 {
            return Err(SdkError::InvalidArgument {
                message: "conversation_id must be a positive id".into(),
            });
        }
        self.get_json(&format!(
            "{API_PREFIX}/conversations/{conversation_id}/e2ee-keys"
        ))
    }

    /// `POST /api/v1/conversations/:id/e2ee-keys` — store a rotated key batch.
    ///
    /// The caller must be an active member (the server checks `IsMember`, so a
    /// pending invitee is refused). `envelopes` must not be empty.
    ///
    /// # Errors
    ///
    /// * `400 no envelopes provided`;
    /// * `400 invalid request body`;
    /// * `403 you are not a member of this conversation`.
    pub fn put_e2ee_keys(
        &self,
        conversation_id: Id,
        envelopes: &E2eeEnvelopeMap,
    ) -> Result<E2eePutResult> {
        if conversation_id <= 0 {
            return Err(SdkError::InvalidArgument {
                message: "conversation_id must be a positive id".into(),
            });
        }
        envelopes.validate()?;
        let body = serde_json::json!({ "envelopes": envelopes });
        self.post_json(
            &format!("{API_PREFIX}/conversations/{conversation_id}/e2ee-keys"),
            &body,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_envelope_map_serialises_ids_as_json_strings() {
        // `map[int64]string` in Go, so the keys are strings on the wire.
        let envelopes = E2eeEnvelopeMap::from_pairs([(7, "AAA"), (9, "BBB")]);
        let json = serde_json::to_value(&envelopes).unwrap();
        assert_eq!(json["7"], "AAA");
        assert_eq!(json["9"], "BBB");
        assert!(json.as_object().unwrap().keys().all(|key| key.len() == 1));
    }

    #[test]
    fn the_envelope_map_decodes_the_same_shape() {
        let parsed: E2eeEnvelopeMap = serde_json::from_str(r#"{"7":"AAA","12":"CCC"}"#).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed.get(7), Some("AAA"));
        assert_eq!(parsed.get(12), Some("CCC"));
        assert_eq!(parsed.get(8), None);
    }

    #[test]
    fn an_empty_batch_is_refused_before_the_round_trip() {
        let error = E2eeEnvelopeMap::new().validate().unwrap_err();
        assert!(error.to_string().contains("at least one envelope"));
    }

    #[test]
    fn a_nonpositive_target_is_refused() {
        let envelopes = E2eeEnvelopeMap::from_pairs([(0, "AAA")]);
        assert!(envelopes.validate().is_err());
    }

    #[test]
    fn pairs_come_back_in_ascending_order() {
        let envelopes = E2eeEnvelopeMap::from_pairs([(9, "B"), (3, "A")]);
        let ids: Vec<Id> = envelopes.iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec![3, 9]);
    }

    #[test]
    fn the_request_body_wraps_the_map() {
        let envelopes = E2eeEnvelopeMap::from_pairs([(5, "KEY")]);
        let body = serde_json::json!({ "envelopes": envelopes });
        assert_eq!(body["envelopes"]["5"], "KEY");
    }
}
