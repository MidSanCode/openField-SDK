//! Camp (community group) endpoints (`bots.md` §6.4).
//!
//! Verified against `services/posts/internal/handler/camp.go` and `model.Camp`:
//!
//! * creation binds `name` (required), `description`, `is_visible`, `direct_join`,
//!   `member_post`, `member_pin` — the last four as optional pointers, so omitting
//!   one means "use the server default" rather than "set false";
//! * `camps.name` is `UNIQUE`, so a duplicate is a **`409`**, and the same holds for a
//!   rename through `PUT /camps/:id`;
//! * subgroups are private by construction — creation forces `is_visible=false` and
//!   `direct_join=false`, and `name` is the only meaningful input;
//! * `GET /camps/:id/posts` is members-only, and posting needs membership.

use serde::{Deserialize, Serialize};

use crate::error::{Result, SdkError};
use crate::models::{Camp, CampList, CampMember, Id, JsonObject, PostPage, User};
use serde_json::Value;

use super::{owned, AuthenticatedClient, API_PREFIX};

/// A camp to create.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewCamp {
    /// Camp name. Required, and unique server-side.
    pub name: String,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the camp is listed publicly. Subgroups are forced to `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_visible: Option<bool>,
    /// Whether anyone may self-join. Subgroups are forced to `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_join: Option<bool>,
    /// Whether plain members may post. Server default: `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_post: Option<bool>,
    /// Whether plain members may pin. Server default: `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_pin: Option<bool>,
}

impl NewCamp {
    /// A camp with a name.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Builder: set the description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Builder: hide the camp from the public directory.
    pub fn hidden(mut self) -> Self {
        self.is_visible = Some(false);
        self
    }

    /// Builder: make the camp publicly visible.
    pub fn visible(mut self) -> Self {
        self.is_visible = Some(true);
        self
    }

    /// Builder: allow anyone to self-join.
    pub fn direct_join(mut self, allowed: bool) -> Self {
        self.direct_join = Some(allowed);
        self
    }

    /// Builder: allow plain members to post.
    pub fn members_can_post(mut self, allowed: bool) -> Self {
        self.member_post = Some(allowed);
        self
    }

    /// Builder: allow plain members to pin their own posts.
    pub fn members_can_pin(mut self, allowed: bool) -> Self {
        self.member_pin = Some(allowed);
        self
    }

    /// Reject locally invalid input.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "camp name is required".into(),
            });
        }
        Ok(())
    }
}

/// A partial camp update for `PUT /camps/:id`.
///
/// Every field is optional: the handler binds pointers and only touches what was
/// sent. `member_post` and `member_pin` are owner-only there, while the rest is
/// open to admins.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CampUpdate {
    /// New name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// New description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// New visibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_visible: Option<bool>,
    /// New self-join policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_join: Option<bool>,
    /// New member-posting policy (owner only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_post: Option<bool>,
    /// New member-pinning policy (owner only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_pin: Option<bool>,
}

impl CampUpdate {
    /// Empty patch; set fields individually.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: rename.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Builder: set the description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Builder: set visibility.
    pub fn visible(mut self, visible: bool) -> Self {
        self.is_visible = Some(visible);
        self
    }

    /// Builder: set the self-join policy.
    pub fn direct_join(mut self, allowed: bool) -> Self {
        self.direct_join = Some(allowed);
        self
    }

    /// Builder: set the member-posting policy.
    pub fn members_can_post(mut self, allowed: bool) -> Self {
        self.member_post = Some(allowed);
        self
    }

    /// Builder: set the member-pinning policy.
    pub fn members_can_pin(mut self, allowed: bool) -> Self {
        self.member_pin = Some(allowed);
        self
    }

    /// Reject an empty patch.
    pub fn validate(&self) -> Result<()> {
        if self.name.is_none()
            && self.description.is_none()
            && self.is_visible.is_none()
            && self.direct_join.is_none()
            && self.member_post.is_none()
            && self.member_pin.is_none()
        {
            return Err(SdkError::InvalidArgument {
                message: "camp update must set at least one field".into(),
            });
        }
        if let Some(name) = &self.name {
            if name.trim().is_empty() {
                return Err(SdkError::InvalidArgument {
                    message: "camp name must not be empty".into(),
                });
            }
        }
        Ok(())
    }
}

impl AuthenticatedClient {
    /// `GET /api/v1/camps` — the public camp directory, hidden camps excluded.
    ///
    /// Supports `?q=` for a name search and `?mine=1` to include the bot's own hidden
    /// camps. `?mine=1` answers `401` for an anonymous caller, which is one reason the
    /// SDK always sends the token it holds.
    pub fn camps(&self) -> Result<CampList> {
        self.camps_with(&[])
    }

    /// `GET /api/v1/camps` with filters (`q`, `mine`).
    pub fn camps_with(&self, query: &[(&str, &str)]) -> Result<CampList> {
        self.get_json_query(&format!("{API_PREFIX}/camps"), &owned(query))
    }

    /// `GET /api/v1/camps?mine=1` — every camp the bot belongs to, including hidden ones.
    pub fn my_camps(&self) -> Result<CampList> {
        self.camps_with(&[("mine", "1")])
    }

    /// `GET /api/v1/camps?q=…` — search camps by name.
    pub fn search_camps(&self, query: &str) -> Result<CampList> {
        self.camps_with(&[("q", query)])
    }

    /// `POST /api/v1/camps` — create a top-level camp.
    ///
    /// Subject to a per-user creation quota (base 100, scaled by membership tier), so
    /// exhausting it answers `403`. A duplicate name answers `409 camp name already
    /// taken`.
    pub fn create_camp(&self, camp: &NewCamp) -> Result<Camp> {
        camp.validate()?;
        let body = serde_json::to_value(camp).map_err(super::json_error)?;
        self.post_json(&format!("{API_PREFIX}/camps"), &body)
    }

    /// `GET /api/v1/camps/:id` — camp detail.
    ///
    /// A hidden camp is `404` for non-members, which is a normal answer rather than an
    /// error to retry (`bots.md` §6.4). Camp payloads carry `my_role` for the caller.
    pub fn camp(&self, camp_id: Id) -> Result<Camp> {
        self.get_json(&format!("{API_PREFIX}/camps/{camp_id}"))
    }

    /// `PUT /api/v1/camps/:id` — edit the basics (admins) and the permission switches
    /// (owner only).
    pub fn update_camp(&self, camp_id: Id, update: &CampUpdate) -> Result<Camp> {
        update.validate()?;
        let body = serde_json::to_value(update).map_err(super::json_error)?;
        self.put_json(&format!("{API_PREFIX}/camps/{camp_id}"), &body)
    }

    /// `PUT /api/v1/camps/:id` with a full replacement payload.
    pub fn replace_camp(&self, camp_id: Id, camp: &NewCamp) -> Result<Camp> {
        camp.validate()?;
        let body = serde_json::to_value(camp).map_err(super::json_error)?;
        self.put_json(&format!("{API_PREFIX}/camps/{camp_id}"), &body)
    }

    /// `DELETE /api/v1/camps/:id` — creator only.
    ///
    /// Deleting a private camp or a subgroup also removes its posts in the same
    /// transaction; a **public** camp's posts survive and become global feed posts.
    pub fn delete_camp(&self, camp_id: Id) -> Result<JsonObject> {
        self.ok_object("DELETE", &format!("{API_PREFIX}/camps/{camp_id}"))
    }

    /// `GET /api/v1/camps/:id/posts` — camp posts, members only.
    ///
    /// Hidden camps answer `404` to outsiders and visible ones `403` with a join hint.
    /// The owner and admins see every post regardless of its own `visibility`, because
    /// they moderate the camp.
    pub fn camp_posts(
        &self,
        camp_id: Id,
        page: Option<i64>,
        limit: Option<i64>,
    ) -> Result<PostPage> {
        self.get_json_query(
            &format!("{API_PREFIX}/camps/{camp_id}/posts"),
            &super::pagination(page, limit),
        )
    }

    /// `POST /api/v1/camps/:id/join` — join a camp.
    ///
    /// Requires `direct_join`; a hidden camp refuses self-service joining outright.
    /// Call this before [`AuthenticatedClient::post_to_camp`].
    pub fn join_camp(&self, camp_id: Id) -> Result<JsonObject> {
        self.ok_object("POST", &format!("{API_PREFIX}/camps/{camp_id}/join"))
    }

    /// `DELETE /api/v1/camps/:id/members/me` — leave a camp.
    ///
    /// The creator cannot leave.
    pub fn leave_camp(&self, camp_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "DELETE",
            &format!("{API_PREFIX}/camps/{camp_id}/members/me"),
        )
    }

    /// `GET /api/v1/camps/:id/members` — the roster, members only, highest roles first.
    pub fn camp_members(&self, camp_id: Id) -> Result<Vec<CampMember>> {
        let value: Value = self.get_json(&format!("{API_PREFIX}/camps/{camp_id}/members"))?;
        decode_camp_members(value)
    }

    /// The roster decoded as full user profiles, when the server inlines them.
    pub fn camp_members_as_users(&self, camp_id: Id) -> Result<Vec<User>> {
        let value: Value = self.get_json(&format!("{API_PREFIX}/camps/{camp_id}/members"))?;
        let entries = decode_camp_value(value);
        Ok(entries
            .into_iter()
            .filter_map(|entry| serde_json::from_value::<CampMember>(entry).ok())
            .filter_map(|member| member.user)
            .collect())
    }

    /// `POST /api/v1/camps/:id/members/:user_id` — enrol a user directly (admin+).
    ///
    /// Bypasses `direct_join`. On a **subgroup**, the target must already belong to the
    /// parent camp, otherwise the server answers `403`.
    pub fn add_camp_member(&self, camp_id: Id, user_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "POST",
            &format!("{API_PREFIX}/camps/{camp_id}/members/{user_id}"),
        )
    }

    /// `PUT /api/v1/camps/:id/members/:user_id/role` — promote or demote (owner only).
    ///
    /// `role` is required by the handler. The owner's own role is immutable.
    pub fn set_camp_member_role(&self, camp_id: Id, user_id: Id, role: &str) -> Result<JsonObject> {
        if !matches!(role, "owner" | "admin" | "member") {
            return Err(SdkError::InvalidArgument {
                message: format!("role must be owner, admin or member, got `{role}`"),
            });
        }
        let body = serde_json::json!({ "role": role });
        let response = self.base.http.json_raw(
            "PUT",
            &format!("{API_PREFIX}/camps/{camp_id}/members/{user_id}/role"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// `DELETE /api/v1/camps/:id/members/:user_id` — kick a member (admin+).
    ///
    /// Only the owner may remove an admin, and nobody may remove the owner.
    pub fn kick_camp_member(&self, camp_id: Id, user_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "DELETE",
            &format!("{API_PREFIX}/camps/{camp_id}/members/{user_id}"),
        )
    }

    /// `PUT /api/v1/camps/:id/announcement` — set the camp notice (owner/admin).
    pub fn set_camp_announcement(&self, camp_id: Id, announcement: &str) -> Result<JsonObject> {
        let body = serde_json::json!({ "announcement": announcement });
        let response = self.base.http.json_raw(
            "PUT",
            &format!("{API_PREFIX}/camps/{camp_id}/announcement"),
            Some(&body),
            &[],
        )?;
        super::account::to_object(response)
    }

    /// `GET /api/v1/camps/:id/subgroups` — subgroups of a camp.
    ///
    /// Only members of the *parent* camp may read this; everyone else gets the same
    /// `404` a hidden camp answers (`bots.md` §6.4).
    pub fn camp_subgroups(&self, camp_id: Id) -> Result<CampList> {
        self.get_json(&format!("{API_PREFIX}/camps/{camp_id}/subgroups"))
    }

    /// `POST /api/v1/camps/:id/subgroups` — create a subgroup (parent owner/admin).
    ///
    /// Nesting is exactly one level deep, and the parent must be top-level. Only `name`
    /// and `description` matter: visibility and self-join are forced to `false`.
    pub fn create_subgroup(&self, parent_id: Id, camp: &NewCamp) -> Result<Camp> {
        camp.validate()?;
        let body = serde_json::to_value(camp).map_err(super::json_error)?;
        self.post_json(&format!("{API_PREFIX}/camps/{parent_id}/subgroups"), &body)
    }

    /// `POST /api/v1/posts` with a `camp_id`, after joining.
    ///
    /// Joining first is not politeness: without membership the server answers
    /// `403 join the camp before posting into it`, and with `member_post` off it
    /// answers `403 only camp admins may post in this camp` (`bots.md` §6.4).
    pub fn post_to_camp(
        &self,
        camp_id: Id,
        post: &super::posts::NewPost,
    ) -> Result<crate::models::Post> {
        let mut post = post.clone();
        post.camp_id = Some(camp_id);
        self.create_post(&post)
    }

    /// Join a camp if the bot is not already a member.
    ///
    /// Returns `true` when a join was issued. Membership is probed through the roster;
    /// a `404`/`403` from that read is treated as "not a member" and the join is
    /// attempted, letting the server give the authoritative answer.
    ///
    /// A camp with `direct_join=false` will refuse, which surfaces as a `403`.
    pub fn ensure_camp_membership(&self, bot_id: Id, camp_id: Id) -> Result<bool> {
        match self.camp_members(camp_id) {
            Ok(members) => {
                if members
                    .iter()
                    .any(|member| member.member_id() == Some(bot_id))
                {
                    return Ok(false);
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    crate::ErrorKind::NotFound | crate::ErrorKind::Forbidden
                ) => {}
            Err(err) => return Err(err),
        }
        self.join_camp(camp_id)?;
        Ok(true)
    }
}

/// Decode a member payload that may be a bare array or an envelope.
fn decode_camp_value(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items,
        Value::Object(mut map) => map
            .remove("members")
            .or_else(|| map.remove("users"))
            .and_then(|inner| match inner {
                Value::Array(items) => Some(items),
                _ => None,
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn decode_camp_members(value: Value) -> Result<Vec<CampMember>> {
    decode_camp_value(value)
        .into_iter()
        .map(|entry| {
            serde_json::from_value(entry).map_err(|err| SdkError::Decode {
                message: format!("camp member entry did not match the expected shape: {err}"),
                raw: String::new(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_camp_requires_a_name() {
        assert!(NewCamp::default().validate().is_err());
        assert!(NewCamp::named("  ").validate().is_err());
        assert!(NewCamp::named("营地").validate().is_ok());
    }

    #[test]
    fn new_camp_uses_the_handler_field_names() {
        let json = serde_json::to_value(NewCamp::named("camp")).unwrap();
        assert_eq!(json["name"], "camp");
        assert!(
            json.get("is_visible").is_none(),
            "an omitted policy means \"use the server default\", so it must not be sent"
        );
        assert!(json.get("member_post").is_none());

        let json = serde_json::to_value(
            NewCamp::named("camp")
                .hidden()
                .direct_join(false)
                .members_can_post(false)
                .members_can_pin(true),
        )
        .unwrap();
        assert_eq!(json["is_visible"], false, "hidden() sets is_visible=false");
        assert_eq!(json["direct_join"], false);
        assert_eq!(json["member_post"], false);
        assert_eq!(json["member_pin"], true);
    }

    #[test]
    fn visible_overrides_hidden() {
        let json = serde_json::to_value(NewCamp::named("c").hidden().visible()).unwrap();
        assert_eq!(json["is_visible"], true);
    }

    #[test]
    fn camp_update_requires_a_change_and_validates_the_name() {
        assert!(CampUpdate::new().validate().is_err());
        assert!(CampUpdate::new().name("new").validate().is_ok());
        assert!(CampUpdate::new().name("  ").validate().is_err());
        assert!(CampUpdate::new().description("d").validate().is_ok());

        let json = serde_json::to_value(CampUpdate::new().members_can_post(false)).unwrap();
        assert_eq!(json["member_post"], false);
        assert!(json.get("name").is_none(), "untouched fields stay absent");
    }

    #[test]
    fn camp_member_payloads_decode_from_both_shapes() {
        let array = serde_json::json!([
            {"user_id": 7, "role": "owner"},
            {"user": {"id": 9}, "role": "member"}
        ]);
        let members = decode_camp_members(array).unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].member_id(), Some(7));
        assert!(members[0].is_owner());
        assert_eq!(members[1].member_id(), Some(9));

        let envelope = serde_json::json!({"members": [{"user_id": 1, "role": "admin"}]});
        let members = decode_camp_members(envelope).unwrap();
        assert_eq!(members.len(), 1);
        assert!(members[0].is_staff());
    }

    #[test]
    fn decode_camp_members_reports_a_bad_entry() {
        let broken = serde_json::json!([{"user_id": "not-a-number"}]);
        assert!(decode_camp_members(broken).is_err());
    }
}
