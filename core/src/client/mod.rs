//! The two client types: anonymous and token-authenticated.
//!
//! [`BaseClient`] owns the transport and covers the endpoints that work without a
//! token (or that become personalised when one is present, per `bots.md` §3.2).
//! [`AuthenticatedClient`] adds the token and exposes the rest of the surface.

use std::sync::Arc;

use crate::config::{BotToken, Config};
use crate::error::{Result, SdkError};
use crate::http::{HttpClient, HttpResponse};
use crate::models::{
    Camp, CampList, Capabilities, HealthReport, Id, JsonObject, Post, PostPage, ReplyPage, User,
};

pub mod account;
pub mod any;
pub mod bots;
pub mod camps;
pub mod chat;
pub mod misc;
pub mod posts;
pub mod request;
pub mod storage;

pub use any::AnyClient;
pub use request::{Request, RequestBody};

/// Everything a bot may call lives under this prefix (`bots.md` §2).
pub(crate) const API_PREFIX: &str = "/api/v1";

/// Reject absolute URLs and traversal before a request is built: a bot must not be
/// trickable into sending its token to another host.
pub(crate) fn validate_api_path(path: &str) -> Result<()> {
    if path.starts_with("http://") || path.starts_with("https://") || path.starts_with("//") {
        return Err(SdkError::InvalidArgument {
            message: format!("paths must be server-relative (`/api/v1/...`), got `{path}`"),
        });
    }
    if !path.starts_with('/') {
        return Err(SdkError::InvalidArgument {
            message: format!("paths must start with `/`, got `{path}`"),
        });
    }
    if path.split('/').any(|segment| segment == "..") {
        return Err(SdkError::InvalidArgument {
            message: format!("paths must not contain `..`, got `{path}`"),
        });
    }
    Ok(())
}

/// Convert string pairs into owned query parameters.
pub(crate) fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

/// `page`/`limit` query pair, omitting absent values.
pub(crate) fn pagination(page: Option<i64>, limit: Option<i64>) -> Vec<(String, String)> {
    let mut query = Vec::new();
    if let Some(page) = page {
        query.push(("page".to_string(), page.to_string()));
    }
    if let Some(limit) = limit {
        query.push(("limit".to_string(), limit.to_string()));
    }
    query
}

/// `limit`/`before` query pair for the cursor-paginated message endpoint.
///
/// Reminder from `bots.md` §7.3 and `message.go`: there is no `after` parameter, and
/// `ListByConversation` resets `limit` to 50 unless it is within `1..=100`.
pub(crate) fn message_query(limit: Option<i64>, before: Option<Id>) -> Vec<(String, String)> {
    let mut query = Vec::new();
    if let Some(limit) = limit {
        query.push(("limit".to_string(), limit.to_string()));
    }
    if let Some(before) = before {
        if before > 0 {
            query.push(("before".to_string(), before.to_string()));
        }
    }
    query
}

/// Fields accepted by `PUT /users/me` (`bots.md` §12, `user.go`).
///
/// Only these three are ever sent; the bot marker and ownership are immutable.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProfileUpdate {
    /// New handle, `^[a-zA-Z0-9_]{3,32}$`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// New display name, 1..=100 characters after trimming (counted in runes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    /// New bio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
}

impl ProfileUpdate {
    /// Empty update; set fields individually.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: set the handle.
    pub fn username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    /// Builder: set the display name.
    pub fn nickname(mut self, nickname: impl Into<String>) -> Self {
        self.nickname = Some(nickname.into());
        self
    }

    /// Builder: set the bio.
    pub fn bio(mut self, bio: impl Into<String>) -> Self {
        self.bio = Some(bio.into());
        self
    }

    /// Reject a locally invalid update before spending a request.
    ///
    /// `nickname` is measured in runes because both `bot.go` and `user.go` use
    /// `len([]rune(...))` there, unlike the content limits, which count bytes.
    pub fn validate(&self) -> Result<()> {
        if self.username.is_none() && self.nickname.is_none() && self.bio.is_none() {
            return Err(SdkError::InvalidArgument {
                message: "profile update must set at least one of username, nickname, bio".into(),
            });
        }
        if let Some(username) = &self.username {
            bots::validate_username(username)?;
        }
        if let Some(nickname) = &self.nickname {
            let trimmed = nickname.trim();
            if trimmed.is_empty() || trimmed.chars().count() > 100 {
                return Err(SdkError::InvalidArgument {
                    message: "nickname must be 1..=100 characters after trimming".into(),
                });
            }
        }
        Ok(())
    }
}

/// A client without a token.
///
/// Use it for the unauthenticated endpoints (`GET /capabilities`, public timelines)
/// and as the building block for [`AuthenticatedClient`].
#[derive(Debug, Clone)]
pub struct BaseClient {
    pub(crate) http: Arc<HttpClient>,
}

impl BaseClient {
    /// Build a client from a full [`Config`], which must not carry a token.
    pub fn with_config(config: Config) -> Result<Self> {
        if config.token.is_some() {
            return Err(SdkError::InvalidArgument {
                message: "BaseClient does not accept a token; use AuthenticatedClient".into(),
            });
        }
        Ok(Self {
            http: Arc::new(HttpClient::new(config)?),
        })
    }

    /// Build an anonymous client for `base_url`.
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        Self::with_config(Config::new(base_url, None))
    }

    /// The configuration in force.
    pub fn config(&self) -> &Config {
        self.http.config()
    }

    /// Issue a raw request and return the response **without** raising on 4xx/5xx.
    ///
    /// This is the escape hatch for endpoints this SDK version does not wrap, and for
    /// reading an error body deliberately. The path must be server-relative.
    pub fn request(&self, request: &Request) -> Result<HttpResponse> {
        validate_api_path(&request.path)?;
        self.http.send_raw(request)
    }

    /// `GET /api/v1/capabilities` — unauthenticated feature probe (`bots.md` §5).
    pub fn capabilities(&self) -> Result<Capabilities> {
        self.http
            .json("GET", &format!("{API_PREFIX}/capabilities"), None)
    }

    /// `GET /api/v1/health` — the gateway's aggregate probe (`health.md`).
    ///
    /// The endpoint answers `200` when healthy and `503` when a backend is down, but
    /// the body is parseable either way, so both are decoded rather than raised.
    /// `health.md` suggests polling no faster than every 30 seconds: the route sits
    /// behind the gateway's per-IP bucket.
    pub fn health(&self) -> Result<HealthReport> {
        let request = Request::get(format!("{API_PREFIX}/health"));
        let response = self.http.send_raw(&request)?;
        match response.status {
            200 | 503 => response.decode(),
            _ => Err(response.to_error()),
        }
    }

    /// `GET /api/v1/announcements` — active server notices (`features.md`).
    ///
    /// Public. `include_retired` is the `?all=1` management view.
    pub fn announcements(&self, include_retired: bool) -> Result<crate::models::AnnouncementList> {
        let query: Vec<(String, String)> = if include_retired {
            vec![("all".to_string(), "1".to_string())]
        } else {
            Vec::new()
        };
        let response =
            self.http
                .json_raw("GET", &format!("{API_PREFIX}/announcements"), None, &query)?;
        response.decode()
    }

    /// `GET /api/v1/posts` — public timeline (`bots.md` §6.2).
    ///
    /// Page-numbered: an empty `posts` array means the end (`bots.md` §7.3).
    /// Recognised filters are `page`, `limit`, `q`, `author`, `tag`, `author_id`,
    /// `from` and `to`.
    pub fn posts(&self, query: &[(&str, &str)]) -> Result<PostPage> {
        let response =
            self.http
                .json_raw("GET", &format!("{API_PREFIX}/posts"), None, &owned(query))?;
        response.decode()
    }

    /// `GET /api/v1/posts/:id` — one post. Records a view server-side.
    pub fn post(&self, post_id: Id) -> Result<Post> {
        let response =
            self.http
                .json_raw("GET", &format!("{API_PREFIX}/posts/{post_id}"), None, &[])?;
        response.decode()
    }

    /// `GET /api/v1/posts/:id/replies` — replies to a post.
    pub fn post_replies(
        &self,
        post_id: Id,
        page: Option<i64>,
        limit: Option<i64>,
    ) -> Result<ReplyPage> {
        let response = self.http.json_raw(
            "GET",
            &format!("{API_PREFIX}/posts/{post_id}/replies"),
            None,
            &pagination(page, limit),
        )?;
        response.decode()
    }

    /// `GET /api/v1/camps` — public camp directory (`bots.md` §6.4).
    pub fn camps(&self) -> Result<CampList> {
        let response = self
            .http
            .json_raw("GET", &format!("{API_PREFIX}/camps"), None, &[])?;
        response.decode()
    }

    /// `GET /api/v1/camps/:id` — camp detail; a hidden camp answers `404` for
    /// non-members (`bots.md` §6.4).
    pub fn camp(&self, camp_id: Id) -> Result<Camp> {
        let response =
            self.http
                .json_raw("GET", &format!("{API_PREFIX}/camps/{camp_id}"), None, &[])?;
        response.decode()
    }

    /// `GET /api/v1/users/:user_id` — a public profile (`authPublic`).
    pub fn user(&self, user_id: Id) -> Result<User> {
        let response =
            self.http
                .json_raw("GET", &format!("{API_PREFIX}/users/{user_id}"), None, &[])?;
        response.decode()
    }
}

/// A client authenticated as one bot.
///
/// The token is sent as `Authorization: Bearer ofb_…` on every request (`bots.md` §3.1).
#[derive(Debug, Clone)]
pub struct AuthenticatedClient {
    pub(crate) base: BaseClient,
    pub(crate) token: BotToken,
}

impl AuthenticatedClient {
    /// Build a client from a [`Config`] that carries a token.
    pub fn with_config(config: Config) -> Result<Self> {
        let token = config
            .token
            .clone()
            .ok_or_else(|| SdkError::InvalidArgument {
                message: "AuthenticatedClient requires a token in the config".into(),
            })?;
        let base = BaseClient {
            http: Arc::new(HttpClient::new(config)?),
        };
        Ok(Self { base, token })
    }

    /// Build a client for `base_url`, authenticated with `token`.
    pub fn new(base_url: impl Into<String>, token: impl Into<BotToken>) -> Result<Self> {
        Self::with_config(Config::authenticated(base_url, token))
    }

    /// The configuration in force.
    pub fn config(&self) -> &Config {
        self.base.config()
    }

    /// The redacted token (`ofb_…abcd`). Use [`AuthenticatedClient::raw_token`] only
    /// when the credential itself is needed.
    pub fn token(&self) -> &BotToken {
        &self.token
    }

    /// The raw credential. Never log this.
    pub fn raw_token(&self) -> &str {
        self.token.expose()
    }

    /// Issue a raw authenticated request, without raising on 4xx/5xx.
    pub fn request(&self, request: &Request) -> Result<HttpResponse> {
        self.base.request(request)
    }

    /// The underlying anonymous client, for the endpoints that do not need a token.
    pub fn base(&self) -> &BaseClient {
        &self.base
    }

    /// Build the `ws://` / `wss://` base URL for the event channel (`bots.md` §8.1).
    pub fn websocket_base_url(&self) -> String {
        let base = self.config().normalized_base_url();
        if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            base.to_string()
        }
    }

    /// `GET /api/v1/capabilities` on the authenticated client.
    pub fn capabilities(&self) -> Result<Capabilities> {
        self.base.capabilities()
    }

    /// `GET /api/v1/health` on the authenticated client.
    pub fn health(&self) -> Result<HealthReport> {
        self.base.health()
    }

    /// A client for the same server without credentials.
    pub fn anonymous(&self) -> Result<BaseClient> {
        BaseClient::new(self.config().base_url.clone())
    }

    /// `GET /api/v1/announcements` on the authenticated client.
    pub fn announcements(&self, include_retired: bool) -> Result<crate::models::AnnouncementList> {
        self.base.announcements(include_retired)
    }

    // --- shared helpers ------------------------------------------------------

    pub(crate) fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.get_json_query(path, &[])
    }

    pub(crate) fn get_json_query<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(String, String)],
    ) -> Result<T> {
        let response = self.base.http.json_raw("GET", path, None, query)?;
        response.decode()
    }

    pub(crate) fn send_json<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        let response = self.base.http.json_raw(method, path, Some(body), &[])?;
        response.decode()
    }

    pub(crate) fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        self.send_json("POST", path, body)
    }

    pub(crate) fn put_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        self.send_json("PUT", path, body)
    }

    /// `POST`/`DELETE`/`PUT` endpoints whose body is either absent or uninteresting.
    ///
    /// A `204 No Content` is success, so an empty body becomes an empty object rather
    /// than a decode failure.
    pub(crate) fn ok_object(&self, method: &str, path: &str) -> Result<JsonObject> {
        validate_api_path(path)?;
        let response = self.base.http.empty(method, path, &[])?;
        match response.json()? {
            serde_json::Value::Object(map) => Ok(JsonObject(map)),
            serde_json::Value::Null => Ok(JsonObject(serde_json::Map::new())),
            other => {
                let mut map = serde_json::Map::new();
                map.insert("result".to_string(), other);
                Ok(JsonObject(map))
            }
        }
    }
}

/// A JSON serialisation failure, surfaced through the SDK error type.
pub(crate) fn json_error(err: serde_json::Error) -> SdkError {
    SdkError::InvalidArgument {
        message: format!("could not serialise request body: {err}"),
    }
}
