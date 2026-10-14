//! The JSON method dispatcher shared by the sidecar and the C ABI.
//!
//! Both cross-language transports need the same thing: take a method name and a JSON
//! parameter object, call the right endpoint, and answer with a JSON value. Keeping that
//! table in the core means a language binding gets the same surface whether it talks to
//! the sidecar process or loads the shared library, and it means an endpoint is added in
//! exactly one place.
//!
//! The transports differ only in the two things they own:
//!
//! * **input**, which stays outside this module — the sidecar reads newline-delimited
//!   JSON from stdin, the C ABI receives a C string;
//! * **event delivery**, which is injected as an [`EventSink`] so the sidecar can write
//!   frames to its parent while the C ABI uses its own `ofbot_events_*` queue.
//!
//! Call [`RpcState::dispatch`] with a method name and a `serde_json::Value` of
//! parameters; see [`METHODS`] for the full list.

use std::sync::Arc;
use std::thread::JoinHandle;

use serde_json::{json, Map, Value};

use crate::client::any::AnyClient;
use crate::client::Request;
use crate::config::{BotToken, Config};
use crate::error::{Result, SdkError};
use crate::events::{Event, EventControl, EventFilter};
use crate::models::AnnouncementList;
use crate::retry::{ReconnectPolicy, RetryPolicy};
use crate::{
    ChatRequest, ConversationSettings, NewCamp, NewConversation, NewMessage, NewPost, NewReply,
    ProfileUpdate, Upload,
};

/// Protocol revision. Bumped when a change would break an existing client.
pub const PROTOCOL_VERSION: u32 = 1;

/// Called for every event while a subscription is active.
///
/// The sink runs on the subscription's reader thread, so it must be cheap and must not
/// call back into the state it came from.
pub type EventSink = Arc<dyn Fn(&Event) + Send + Sync>;

/// Every method name this dispatcher answers, in a stable order.
pub const METHODS: &[&str] = &[
    // meta
    "version",
    "ping",
    "capabilities",
    // client
    "client.init",
    "client.set_token",
    "client.config",
    "client.has_token",
    // escape hatch
    "request",
    // account
    "me",
    "permissions",
    "startup_check",
    "update_profile",
    "user",
    "search_users",
    "follow",
    "unfollow",
    "user_posts",
    "storage_usage",
    "storage_buckets",
    "set_storage_bucket",
    "upload_avatar",
    "health",
    "announcements",
    // bots
    "bots.create",
    "bots.list",
    "bots.regenerate",
    "bots.delete",
    // posts
    "posts.create",
    "posts.publish",
    "posts.list",
    "posts.search",
    "posts.get",
    "posts.update",
    "posts.delete",
    "posts.pin",
    "posts.react",
    "posts.unreact",
    "posts.favorite",
    "posts.unfavorite",
    "posts.reply",
    "posts.replies",
    "posts.update_reply",
    "posts.delete_reply",
    // chat
    "chat.conversations",
    "chat.create",
    "chat.settings",
    "chat.get",
    "chat.messages",
    "chat.latest",
    "chat.search",
    "chat.send",
    "chat.edit",
    "chat.delete",
    "chat.read",
    "chat.message_read",
    "chat.typing",
    "chat.join",
    "chat.invite",
    "chat.invite_many",
    "chat.leave",
    "chat.remove_member",
    "chat.start",
    "chat.consent_requests",
    "chat.accept_consent",
    "chat.decline_consent",
    "chat.files",
    "chat.todos",
    // camps
    "camps.list",
    "camps.get",
    "camps.create",
    "camps.update",
    "camps.delete",
    "camps.posts",
    "camps.join",
    "camps.leave",
    "camps.members",
    "camps.subgroups",
    "camps.create_subgroup",
    "camps.set_announcement",
    "camps.post",
    // storage
    "storage.upload",
    "storage.upload_many",
    "storage.list",
    "storage.delete",
    // moderation
    "reports.create",
    "reports.mine",
    // notifications
    "notifications.list",
    "notifications.read",
    // events (needs an event sink)
    "events.subscribe",
    "events.unsubscribe",
    "events.stats",
    // test support
    "testkit.start",
    "testkit.stop",
    "testkit.force_status",
    "testkit.fail_next",
    "testkit.broadcast",
    "testkit.requests",
    "testkit.clear_requests",
    "testkit.ws_client_count",
    "testkit.seed_message",
];

/// The state a transport keeps between calls.
pub struct RpcState {
    client: Option<AnyClient>,
    events: Option<EventControl>,
    event_thread: Option<JoinHandle<()>>,
    sink: Option<EventSink>,
    #[cfg(feature = "testkit")]
    mock: Option<crate::testkit::MockServer>,
}

impl Default for RpcState {
    fn default() -> Self {
        Self::new()
    }
}

impl RpcState {
    /// A state with no client and no event sink.
    pub fn new() -> Self {
        Self {
            client: None,
            events: None,
            event_thread: None,
            sink: None,
            #[cfg(feature = "testkit")]
            mock: None,
        }
    }

    /// A state whose subscriptions deliver events through `sink`.
    pub fn with_event_sink(sink: EventSink) -> Self {
        let mut state = Self::new();
        state.sink = Some(sink);
        state
    }

    /// The client, once [`RpcState::dispatch`] has run `client.init`.
    pub fn client(&self) -> Result<&AnyClient> {
        self.client
            .as_ref()
            .ok_or_else(|| SdkError::InvalidArgument {
                message: "call client.init before any other method".into(),
            })
    }

    /// The client, mutably.
    pub fn client_mut(&mut self) -> Result<&mut AnyClient> {
        self.client
            .as_mut()
            .ok_or_else(|| SdkError::InvalidArgument {
                message: "call client.init before any other method".into(),
            })
    }

    /// Replace the client wholesale, which `client.init` does.
    pub fn set_client(&mut self, client: AnyClient) {
        self.client = Some(client);
    }

    /// The authenticated client, for the calls that only make sense for a bot.
    fn bot(&mut self) -> Result<&crate::AuthenticatedClient> {
        self.client_mut()?.authenticated()
    }

    /// The running mock server, when the `testkit` feature is on and it was started.
    #[cfg(feature = "testkit")]
    fn mock(&self) -> Result<&crate::testkit::MockServer> {
        self.mock.as_ref().ok_or_else(|| SdkError::InvalidArgument {
            message: "call testkit.start first".into(),
        })
    }

    /// Whether a subscription is running.
    pub fn is_subscribed(&self) -> bool {
        self.events.is_some()
    }

    /// Stop any active subscription.
    pub fn unsubscribe(&mut self) {
        if let Some(control) = self.events.take() {
            control.close();
        }
    }

    /// Dispatch one method.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError::InvalidArgument`] for an unknown method, a missing parameter,
    /// or a call made before `client.init`. Endpoint failures keep their status and the
    /// server's own error string.
    pub fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value> {
        let params = Params::new(params);
        self.dispatch_params(method, &params)
    }

    fn dispatch_params(&mut self, method: &str, params: &Params) -> Result<Value> {
        match method {
            // --- meta --------------------------------------------------------
            "version" => Ok(json!({
                "name": "openfield-bot-sdk",
                "version": crate::VERSION,
                "protocol": PROTOCOL_VERSION,
            })),
            "ping" => Ok(json!({ "pong": true })),
            "capabilities" => Ok(json!({
                "protocol": PROTOCOL_VERSION,
                "features": {
                    "ffi": true,
                    "sidecar": true,
                    "events": true,
                    "multipart": true,
                    "retry": true,
                    "reconnect": true,
                    "testkit": cfg!(feature = "testkit"),
                },
                "methods": METHODS,
            })),

            // --- client ------------------------------------------------------
            "client.init" => {
                let base_url = params
                    .opt_string("base_url")?
                    .unwrap_or_else(|| crate::config::DEFAULT_BASE_URL.to_string());
                let token = match params.opt_string("token")? {
                    Some(raw) if !raw.trim().is_empty() => Some(BotToken::new(raw)?),
                    _ => None,
                };

                let mut config = Config::new(base_url, token);
                if let Some(timeout) = params.opt_i64("timeout_ms")? {
                    config.timeout = std::time::Duration::from_millis(timeout.max(1) as u64);
                }
                if let Some(connect) = params.opt_i64("connect_timeout_ms")? {
                    config.connect_timeout =
                        std::time::Duration::from_millis(connect.max(1) as u64);
                }
                if let Some(agent) = params.opt_string("user_agent")? {
                    config.user_agent = agent;
                }
                if let Some(limit) = params.opt_i64("max_response_bytes")? {
                    config.max_response_bytes = limit.max(1024) as u64;
                }
                if let Some(retry) = params.opt_deserialize::<RetryPolicy>("retry")? {
                    config.retry = retry;
                }
                if let Some(reconnect) = params.opt_deserialize::<ReconnectPolicy>("reconnect")? {
                    config.reconnect = reconnect;
                }
                if let Some(Value::Object(headers)) = params.get("headers") {
                    for (name, value) in headers {
                        let rendered = value
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| strip_quotes(value));
                        config.headers.push((name.clone(), rendered));
                    }
                }

                config.validate()?;
                let holder = AnyClient::new(config)?;
                let has_token = holder.has_token();
                self.set_client(holder);
                Ok(json!({ "ok": true, "has_token": has_token }))
            }
            "client.set_token" => {
                let replacement = match params.opt_string("token")? {
                    Some(raw) if !raw.trim().is_empty() => Some(BotToken::new(raw)?),
                    _ => None,
                };
                self.client_mut()?.set_token(replacement)?;
                Ok(json!({ "ok": true, "has_token": self.client()?.has_token() }))
            }
            "client.config" => {
                let config = self.client()?.config();
                Ok(json!({
                    "base_url": config.base_url,
                    // The token serialises as a redacted string; the raw value never
                    // leaves the process (bots.md §15.1).
                    "token": config.token.as_ref().map(|token| token.redacted()),
                    "has_token": config.token.is_some(),
                    "timeout_ms": config.timeout.as_millis() as u64,
                    "connect_timeout_ms": config.connect_timeout.as_millis() as u64,
                    "user_agent": config.user_agent,
                    "max_response_bytes": config.max_response_bytes,
                    "retry": config.retry,
                    "reconnect": config.reconnect,
                }))
            }
            "client.has_token" => Ok(json!({ "has_token": self.client()?.has_token() })),

            // --- escape hatch ------------------------------------------------
            "request" => {
                let request = build_request(params)?;
                let response = self.client_mut()?.request(&request)?;
                let body = response.json().unwrap_or(Value::Null);
                let mut object = Map::new();
                object.insert("ok".into(), Value::Bool(response.is_success()));
                object.insert("status".into(), Value::from(response.status));
                object.insert("body".into(), body);
                if let Some(seconds) = response.retry_after_secs() {
                    object.insert("retry_after_secs".into(), Value::from(seconds));
                }
                Ok(Value::Object(object))
            }

            // --- account -----------------------------------------------------
            "me" => to_value(self.bot()?.me()?),
            "permissions" => {
                let set = self.bot()?.my_permissions()?;
                let keys: Vec<String> = set
                    .permissions()
                    .into_iter()
                    .map(|permission| permission.key().to_string())
                    .collect();
                Ok(json!({ "permissions": keys, "groups": set.groups() }))
            }
            "startup_check" => {
                let check = self.bot()?.startup_check()?;
                Ok(json!({
                    "user": check.me,
                    "permissions": check.permissions.permissions(),
                    "groups": check.permissions.groups(),
                    "missing_administrative": check.missing_administrative,
                    "is_bot": check.is_bot(),
                    "bot_id": check.bot_id(),
                }))
            }
            "update_profile" => {
                let update: ProfileUpdate = params.deserialize("profile")?;
                to_value(self.bot()?.update_profile(&update)?)
            }
            "user" => to_value(self.bot()?.user(params.id("user_id")?)?),
            "search_users" => self.bot()?.search_users(&params.string("query")?),
            "follow" => to_value(self.bot()?.follow(params.id("user_id")?)?),
            "unfollow" => to_value(self.bot()?.unfollow(params.id("user_id")?)?),
            "user_posts" => {
                let (page, limit) = params.pagination();
                to_value(self.bot()?.user_posts(params.id("user_id")?, page, limit)?)
            }
            "storage_usage" => to_value(self.bot()?.storage_usage()?),
            "storage_buckets" => to_value(self.bot()?.storage_buckets()?),
            "set_storage_bucket" => {
                to_value(self.bot()?.set_storage_bucket(&params.string("bucket")?)?)
            }
            "upload_avatar" => {
                let upload: Upload = params.deserialize("upload")?;
                to_value(self.bot()?.upload_avatar(&upload.file_name, upload.bytes)?)
            }
            "health" => to_value(self.client_mut()?.anonymous()?.health()?),
            "announcements" => {
                let all = params.flag("all")?;
                let list: AnnouncementList = self.client_mut()?.anonymous()?.announcements(all)?;
                to_value(list.announcements())
            }

            // --- bots --------------------------------------------------------
            "bots.create" => {
                let created = self
                    .bot()?
                    .create_bot(&params.string("username")?, &params.string("nickname")?)?;
                // The token is echoed deliberately: the server shows it exactly once
                // and a binding has to surface it to its caller.
                to_value(created)
            }
            "bots.list" => to_value(self.bot()?.list_bots()?),
            "bots.regenerate" => {
                let token = self.bot()?.regenerate_bot_token(params.id("bot_id")?)?;
                // Keep the running client usable with the token it was just issued.
                if params.flag("adopt")? {
                    self.client_mut()?
                        .set_token(Some(BotToken::new(token.token.clone())?))?;
                }
                to_value(token)
            }
            "bots.delete" => to_value(self.bot()?.delete_bot(params.id("bot_id")?)?),

            // --- posts -------------------------------------------------------
            "posts.create" | "posts.publish" => {
                let post: NewPost = match params.opt_deserialize::<NewPost>("post")? {
                    Some(post) => post,
                    None => NewPost::text(params.string("content")?),
                };
                to_value(self.bot()?.create_post(&post)?)
            }
            "posts.list" => {
                let (page, limit) = params.pagination();
                to_value(self.bot()?.posts(page, limit)?)
            }
            "posts.search" => {
                with_query_pairs(params, |pairs| to_value(self.bot()?.search_posts(pairs)?))
            }
            "posts.get" => to_value(self.bot()?.post(params.id("post_id")?)?),
            "posts.update" => {
                let post: NewPost = params.deserialize("post")?;
                to_value(self.bot()?.update_post(params.id("post_id")?, &post)?)
            }
            "posts.delete" => to_value(self.bot()?.delete_post(params.id("post_id")?)?),
            "posts.pin" => to_value(
                self.bot()?
                    .pin_post(params.id("post_id")?, params.flag("pinned")?)?,
            ),
            "posts.react" => {
                let raw = params.string("reaction")?;
                let reaction =
                    crate::Reaction::parse(&raw).ok_or_else(|| SdkError::InvalidArgument {
                        message: format!(
                            "unknown reaction `{raw}`; expected one of like, dislike, love, \
                             haha, wow, sad, angry"
                        ),
                    })?;
                to_value(self.bot()?.react(params.id("post_id")?, reaction)?)
            }
            "posts.unreact" => to_value(self.bot()?.unreact(params.id("post_id")?)?),
            "posts.favorite" => to_value(self.bot()?.favorite(params.id("post_id")?)?),
            "posts.unfavorite" => to_value(self.bot()?.unfavorite(params.id("post_id")?)?),
            "posts.reply" => {
                let reply: NewReply = match params.opt_deserialize::<NewReply>("reply")? {
                    Some(reply) => reply,
                    None => NewReply::text(params.string("content")?),
                };
                to_value(self.bot()?.create_reply(params.id("post_id")?, &reply)?)
            }
            "posts.replies" => {
                let (page, limit) = params.pagination();
                to_value(self.bot()?.replies(params.id("post_id")?, page, limit)?)
            }
            "posts.update_reply" => to_value(self.bot()?.update_reply(
                params.id("post_id")?,
                params.id("reply_id")?,
                &params.string("content")?,
            )?),
            "posts.delete_reply" => to_value(
                self.bot()?
                    .delete_reply(params.id("post_id")?, params.id("reply_id")?)?,
            ),

            // --- chat --------------------------------------------------------
            "chat.conversations" => to_value(self.bot()?.conversations()?),
            "chat.create" => {
                let title = params.string("title")?;
                to_value(
                    self.bot()?
                        .create_conversation(&NewConversation::named(title))?,
                )
            }
            "chat.settings" => {
                let settings: ConversationSettings = params.deserialize("settings")?;
                to_value(
                    self.bot()?
                        .update_conversation_settings(params.id("conversation_id")?, &settings)?,
                )
            }
            "chat.get" => to_value(self.bot()?.conversation(params.id("conversation_id")?)?),
            "chat.messages" => {
                let limit = params.opt_i64("limit")?;
                let before = params.opt_id("before")?;
                to_value(
                    self.bot()?
                        .messages(params.id("conversation_id")?, limit, before)?,
                )
            }
            "chat.latest" => {
                let limit = params.opt_i64("limit")?;
                to_value(
                    self.bot()?
                        .latest_messages(params.id("conversation_id")?, limit)?,
                )
            }
            "chat.search" => with_query_pairs(params, |pairs| {
                to_value(
                    self.bot()?
                        .search_messages(params.id("conversation_id")?, pairs)?,
                )
            }),
            "chat.send" => {
                let message: NewMessage = match params.opt_deserialize::<NewMessage>("message")? {
                    Some(message) => message,
                    None => {
                        let mut message = NewMessage::text(params.string("content")?);
                        if let Some(reply_to) = params.opt_id("reply_to_id")? {
                            message = message.replying_to(reply_to);
                        }
                        for mention in params.id_list("mentions")? {
                            message = message.mention(mention);
                        }
                        if let Some(seconds) = params.opt_i64("burn_seconds")? {
                            message = message.burn_after(seconds)?;
                        }
                        message
                    }
                };
                to_value(
                    self.bot()?
                        .send_message(params.id("conversation_id")?, &message)?,
                )
            }
            "chat.edit" => to_value(self.bot()?.edit_message(
                params.id("conversation_id")?,
                params.id("message_id")?,
                &params.string("content")?,
            )?),
            "chat.delete" => to_value(
                self.bot()?
                    .delete_message(params.id("conversation_id")?, params.id("message_id")?)?,
            ),
            "chat.read" => to_value(self.bot()?.mark_read(
                params.id("conversation_id")?,
                params.opt_id("last_message_id")?.unwrap_or(0),
            )?),
            "chat.message_read" => to_value(
                self.bot()?
                    .mark_message_read(params.id("conversation_id")?, params.id("message_id")?)?,
            ),
            "chat.typing" => to_value(self.bot()?.send_typing(params.id("conversation_id")?)?),
            "chat.join" => to_value(
                self.bot()?
                    .join_conversation(params.id("conversation_id")?)?,
            ),
            "chat.invite" => to_value(self.bot()?.invite(
                params.id("conversation_id")?,
                params.id("user_id")?,
                params.opt_string("message")?.as_deref(),
            )?),
            "chat.invite_many" => to_value(self.bot()?.invite_many(
                params.id("conversation_id")?,
                &params.id_list("user_ids")?,
                params.opt_string("message")?.as_deref(),
            )?),
            "chat.leave" => to_value(
                self.bot()?
                    .leave_conversation(params.id("conversation_id")?)?,
            ),
            "chat.remove_member" => to_value(
                self.bot()?
                    .remove_member(params.id("conversation_id")?, params.id("user_id")?)?,
            ),
            "chat.start" => {
                let mut request = ChatRequest::new(params.id("user_id")?);
                if let Some(message) = params.opt_string("message")? {
                    request = request.with_message(message);
                }
                if params.flag("encrypted")? {
                    request = request.encrypted();
                }
                to_value(self.bot()?.start_conversation(&request)?)
            }
            "chat.consent_requests" => to_value(self.bot()?.consent_requests()?),
            "chat.accept_consent" => to_value(
                self.bot()?
                    .accept_consent_request(params.id("request_id")?)?,
            ),
            "chat.decline_consent" => to_value(
                self.bot()?
                    .decline_consent_request(params.id("request_id")?)?,
            ),
            "chat.files" => to_value(
                self.bot()?
                    .conversation_files(params.id("conversation_id")?)?,
            ),
            "chat.todos" => self
                .bot()?
                .conversation_todos(params.id("conversation_id")?),

            // --- camps -------------------------------------------------------
            "camps.list" => {
                with_query_pairs(params, |pairs| to_value(self.bot()?.camps_with(pairs)?))
            }
            "camps.get" => to_value(self.bot()?.camp(params.id("camp_id")?)?),
            "camps.create" => {
                let camp: NewCamp = params.deserialize("camp")?;
                to_value(self.bot()?.create_camp(&camp)?)
            }
            "camps.update" => {
                let update: crate::CampUpdate = params.deserialize("update")?;
                to_value(self.bot()?.update_camp(params.id("camp_id")?, &update)?)
            }
            "camps.delete" => to_value(self.bot()?.delete_camp(params.id("camp_id")?)?),
            "camps.posts" => {
                let (page, limit) = params.pagination();
                to_value(self.bot()?.camp_posts(params.id("camp_id")?, page, limit)?)
            }
            "camps.join" => to_value(self.bot()?.join_camp(params.id("camp_id")?)?),
            "camps.leave" => to_value(self.bot()?.leave_camp(params.id("camp_id")?)?),
            "camps.members" => to_value(self.bot()?.camp_members(params.id("camp_id")?)?),
            "camps.subgroups" => to_value(self.bot()?.camp_subgroups(params.id("camp_id")?)?),
            "camps.create_subgroup" => {
                let camp: NewCamp = params.deserialize("camp")?;
                to_value(
                    self.bot()?
                        .create_subgroup(params.id("parent_id")?, &camp)?,
                )
            }
            "camps.set_announcement" => {
                to_value(self.bot()?.set_camp_announcement(
                    params.id("camp_id")?,
                    &params.string("announcement")?,
                )?)
            }
            "camps.post" => {
                let post: NewPost = match params.opt_deserialize::<NewPost>("post")? {
                    Some(post) => post,
                    None => NewPost::text(params.string("content")?),
                };
                to_value(self.bot()?.post_to_camp(params.id("camp_id")?, &post)?)
            }

            // --- storage -----------------------------------------------------
            "storage.upload" => {
                let upload: Upload = params.deserialize("upload")?;
                to_value(self.bot()?.upload(upload)?)
            }
            "storage.upload_many" => {
                let uploads: Vec<Upload> = params.deserialize("uploads")?;
                to_value(self.bot()?.upload_many(uploads)?)
            }
            "storage.list" => to_value(self.bot()?.attachments()?),
            "storage.delete" => {
                to_value(self.bot()?.delete_attachment(params.id("attachment_id")?)?)
            }

            // --- moderation --------------------------------------------------
            "reports.create" => {
                let report: crate::NewReport = params.deserialize("report")?;
                to_value(self.bot()?.report(&report)?)
            }
            "reports.mine" => self.bot()?.my_reports(),

            // --- notifications -----------------------------------------------
            "notifications.list" => to_value(self.bot()?.notifications()?),
            "notifications.read" => to_value(self.bot()?.mark_notifications_read()?),

            // --- events ------------------------------------------------------
            "events.subscribe" => self.subscribe(params),
            "events.unsubscribe" => {
                self.unsubscribe();
                Ok(json!({ "ok": true, "subscribed": false }))
            }
            "events.stats" => {
                let stats = match &self.events {
                    Some(control) => control.stats(),
                    None => crate::EventStreamStats::default(),
                };
                to_value(stats)
            }

            // --- test support ------------------------------------------------
            #[cfg(feature = "testkit")]
            "testkit.start" => {
                let server =
                    crate::testkit::MockServer::start_with(crate::testkit::MockConfig::default())
                        .map_err(|err| SdkError::Transport {
                        message: format!("could not start the mock server: {err}"),
                    })?;
                let base_url = server.base_url().to_string();
                let token = server.token().to_string();
                self.mock = Some(server);
                Ok(json!({ "base_url": base_url, "token": token, "bot_id": 4242 }))
            }
            #[cfg(feature = "testkit")]
            "testkit.stop" => {
                self.mock = None;
                Ok(json!({ "ok": true }))
            }
            // These exist so the Python and Node suites can exercise error mapping and
            // event delivery against the real mock rather than a second, drifting one.
            // Compile-time gated with the rest of `testkit`, so a production build has no
            // way to reach them.
            #[cfg(feature = "testkit")]
            "testkit.force_status" => {
                let mock = self.mock()?;
                let path = params.string("path")?;
                let status =
                    params
                        .opt_i64("status")?
                        .ok_or_else(|| SdkError::InvalidArgument {
                            message: "`status` is required".into(),
                        })?;
                let retry_after = params.opt_i64("retry_after_secs")?.map(|s| s.max(0) as u64);
                let times = params.opt_i64("times")?.unwrap_or(1).max(1) as u32;
                mock.force_status(&path, status.clamp(100, 599) as u16, retry_after, times);
                Ok(json!({ "ok": true }))
            }
            #[cfg(feature = "testkit")]
            "testkit.fail_next" => {
                let mock = self.mock()?;
                let path = params.string("path")?;
                let times = params.opt_i64("times")?.unwrap_or(1).max(1) as u32;
                mock.fail_next(&path, times);
                Ok(json!({ "ok": true }))
            }
            #[cfg(feature = "testkit")]
            "testkit.broadcast" => {
                let event = params.required("event")?.clone();
                let delivered = self.mock()?.broadcast(&event);
                Ok(json!({ "ok": true, "delivered": delivered }))
            }
            #[cfg(feature = "testkit")]
            "testkit.requests" => {
                let recorded = self.mock()?.requests();
                let items: Vec<Value> = recorded
                    .iter()
                    .map(|request| {
                        json!({
                            "method": request.method,
                            "path": request.path,
                            "query": request.query,
                            "authorization": request.authorization,
                            "body": request.body,
                            // The body is lossily decoded for display, so the octet count
                            // is the only trustworthy measure of a binary upload.
                            "body_len": request.body_len(),
                        })
                    })
                    .collect();
                Ok(json!({ "requests": items }))
            }
            #[cfg(feature = "testkit")]
            "testkit.clear_requests" => {
                self.mock()?.clear_requests();
                Ok(json!({ "ok": true }))
            }
            #[cfg(feature = "testkit")]
            "testkit.ws_client_count" => Ok(json!({ "clients": self.mock()?.ws_client_count() })),
            #[cfg(feature = "testkit")]
            "testkit.seed_message" => {
                let conversation_id = params.id("conversation_id")?;
                let sender_id = params.opt_id("sender_id")?.unwrap_or(7);
                let content = params.string("content")?;
                let sender_name = params
                    .opt_string("sender_name")?
                    .unwrap_or_else(|| "alice".to_string());
                let broadcast = params.flag("broadcast")?;
                let message =
                    self.mock()?
                        .seed_message(conversation_id, sender_id, &content, &sender_name);
                if broadcast {
                    // Mirror what the server does: store the message, then notify. Both
                    // paths must agree, which is exactly what `Bot.watch` merges.
                    self.mock()?.broadcast(&json!({
                        "type": "chat.message.created",
                        "recipients": [sender_id],
                        "data": message,
                    }));
                }
                Ok(message)
            }
            #[cfg(not(feature = "testkit"))]
            "testkit.start" | "testkit.stop" => Err(SdkError::InvalidArgument {
                message: "this build does not include the `testkit` feature".into(),
            }),

            other => Err(SdkError::InvalidArgument {
                message: format!("unknown method `{other}`"),
            }),
        }
    }

    /// Start a subscription and hand events to the installed sink.
    fn subscribe(&mut self, params: &Params) -> Result<Value> {
        let sink = self.sink.clone().ok_or_else(|| SdkError::InvalidArgument {
            message: "this transport has no event sink; use its own event API".into(),
        })?;

        let filter = build_filter(params)?;
        let reconnect = params.opt_deserialize::<ReconnectPolicy>("reconnect")?;
        let capacity = params
            .opt_i64("queue_capacity")?
            .map(|value| value.max(1) as usize)
            .unwrap_or(crate::events::DEFAULT_QUEUE_CAPACITY);

        // Replace any previous subscription rather than stacking reader threads.
        self.unsubscribe();

        let client = self.client_mut()?;
        let stream = match reconnect {
            Some(reconnect) => client.subscribe_full(filter, reconnect, capacity),
            None => client.subscribe(filter),
        }?;

        let (receiver, control) = stream.split();
        let handle = std::thread::Builder::new()
            .name("openfield-rpc-events".to_string())
            .spawn(move || {
                while let Some(event) = receiver.recv() {
                    sink(&event);
                }
            })
            .map_err(|err| SdkError::Transport {
                message: format!("could not start the event forwarder: {err}"),
            })?;

        self.events = Some(control);
        self.event_thread = Some(handle);
        Ok(json!({ "ok": true, "subscribed": true }))
    }
}

impl Drop for RpcState {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

/// Render an [`Event`] as the transport-neutral event object.
pub fn event_message(event: &Event) -> Value {
    let mut object = Map::new();
    object.insert("event".into(), Value::String(event.kind.as_str().into()));
    object.insert("data".into(), event.data.clone());
    if !event.recipients.is_empty() {
        object.insert(
            "recipients".into(),
            serde_json::to_value(&event.recipients).unwrap_or(Value::Null),
        );
    }
    Value::Object(object)
}

/// Strip surrounding quotes from a JSON scalar, for header values.
fn strip_quotes(value: &Value) -> String {
    value.to_string().trim_matches('"').to_string()
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(|err| SdkError::InvalidArgument {
        message: format!("could not serialise the result: {err}"),
    })
}

/// Run `body` with the `query` object as borrowed pairs, which the search helpers take.
fn with_query_pairs<T>(params: &Params, body: impl FnOnce(&[(&str, &str)]) -> T) -> T {
    let owned = params.query_pairs();
    let borrowed: Vec<(&str, &str)> = owned
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    body(&borrowed)
}

/// Read a parameter object without repeating the downcast everywhere.
pub struct Params<'a> {
    map: Option<&'a Map<String, Value>>,
}

impl<'a> Params<'a> {
    /// Wrap a params value. A non-object is treated as empty.
    pub fn new(value: &'a Value) -> Self {
        Self {
            map: value.as_object(),
        }
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        self.map
            .and_then(|map| map.get(key))
            .filter(|value| !value.is_null())
    }

    fn required(&self, key: &str) -> Result<&'a Value> {
        self.get(key).ok_or_else(|| SdkError::InvalidArgument {
            message: format!("`{key}` is required"),
        })
    }

    fn id(&self, key: &str) -> Result<crate::Id> {
        self.opt_id(key)?.ok_or_else(|| SdkError::InvalidArgument {
            message: format!("`{key}` is required and must be an integer id"),
        })
    }

    fn opt_id(&self, key: &str) -> Result<Option<crate::Id>> {
        match self.get(key) {
            None => Ok(None),
            Some(value) => crate::models::id_from_value(value)
                .map(Some)
                .ok_or_else(|| SdkError::InvalidArgument {
                    message: format!("`{key}` must be an integer id"),
                }),
        }
    }

    fn string(&self, key: &str) -> Result<String> {
        self.opt_string(key)?
            .ok_or_else(|| SdkError::InvalidArgument {
                message: format!("`{key}` is required and must be a string"),
            })
    }

    fn opt_string(&self, key: &str) -> Result<Option<String>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(SdkError::InvalidArgument {
                message: format!("`{key}` must be a string"),
            }),
        }
    }

    fn flag(&self, key: &str) -> Result<bool> {
        Ok(self.opt_flag(key)?.unwrap_or(false))
    }

    fn opt_flag(&self, key: &str) -> Result<Option<bool>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Bool(flag)) => Ok(Some(*flag)),
            Some(_) => Err(SdkError::InvalidArgument {
                message: format!("`{key}` must be a boolean"),
            }),
        }
    }

    fn opt_i64(&self, key: &str) -> Result<Option<i64>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Number(number)) => Ok(number.as_i64()),
            Some(Value::String(text)) => Ok(text.trim().parse().ok()),
            Some(_) => Err(SdkError::InvalidArgument {
                message: format!("`{key}` must be an integer"),
            }),
        }
    }

    fn deserialize<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<T> {
        let value = self.required(key)?.clone();
        serde_json::from_value(value).map_err(|err| SdkError::InvalidArgument {
            message: format!("`{key}` is not a valid object: {err}"),
        })
    }

    fn opt_deserialize<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.get(key) {
            None => Ok(None),
            Some(value) => serde_json::from_value(value.clone())
                .map(Some)
                .map_err(|err| SdkError::InvalidArgument {
                    message: format!("`{key}` is not a valid object: {err}"),
                }),
        }
    }

    /// A string list, accepting a single string too.
    fn string_list(&self, key: &str) -> Result<Vec<String>> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::String(text)) => Ok(vec![text.clone()]),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| SdkError::InvalidArgument {
                            message: format!("`{key}` must contain only strings"),
                        })
                })
                .collect(),
            Some(_) => Err(SdkError::InvalidArgument {
                message: format!("`{key}` must be a string or an array of strings"),
            }),
        }
    }

    /// An id list, accepting a single id too.
    fn id_list(&self, key: &str) -> Result<Vec<crate::Id>> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    crate::models::id_from_value(item).ok_or_else(|| SdkError::InvalidArgument {
                        message: format!("`{key}` must contain only integer ids"),
                    })
                })
                .collect(),
            Some(value) => crate::models::id_from_value(value)
                .map(|id| vec![id])
                .ok_or_else(|| SdkError::InvalidArgument {
                    message: format!("`{key}` must be an id or an array of ids"),
                }),
        }
    }

    /// `page`/`limit`, for the page-numbered endpoints.
    fn pagination(&self) -> (Option<i64>, Option<i64>) {
        (
            self.opt_i64("page").ok().flatten(),
            self.opt_i64("limit").ok().flatten(),
        )
    }

    /// The raw `query` object as pairs, for the pass-through endpoints.
    fn query_pairs(&self) -> Vec<(String, String)> {
        match self.map.and_then(|map| map.get("query")) {
            Some(Value::Object(map)) => map
                .iter()
                .map(|(key, value)| {
                    let rendered = match value {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    (key.clone(), rendered)
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Build a [`Request`] from the transport-neutral payload.
///
/// Shared by the sidecar's `request` method and the C ABI's `ofbot_client_request`, so
/// both accept the same shapes: `body`, `body_json`, `bytes`, `empty_body`, `multipart`.
pub fn build_request(params: &Params) -> Result<Request> {
    let method = params.string("method")?;
    let path = params.string("path")?;
    let mut request = Request::new(method, path);
    for (key, value) in params.query_pairs() {
        request = request.query(key, value);
    }

    if let Some(Value::Object(multipart)) = params.get("multipart") {
        let nested = Params {
            map: Some(multipart),
        };
        let mut files = Vec::new();
        if let Some(Value::Array(items)) = nested.get("files") {
            for item in items {
                let entry = Params {
                    map: item.as_object(),
                };
                let file_name = entry.string("file_name")?;
                let bytes = match (entry.opt_string("path")?, entry.get("bytes")) {
                    (Some(path), _) => {
                        std::fs::read(&path).map_err(|err| SdkError::InvalidArgument {
                            message: format!("cannot read upload file `{path}`: {err}"),
                        })?
                    }
                    (None, Some(value)) => decode_bytes(value)?,
                    (None, None) => {
                        return Err(SdkError::InvalidArgument {
                            message: "each multipart file needs `path` or `bytes`".into(),
                        })
                    }
                };
                let mut part = crate::MultipartFile::new(file_name, bytes);
                if let Some(field) = entry.opt_string("field")? {
                    part = part.with_field(field);
                }
                if let Some(mime) = entry.opt_string("mime")? {
                    part = part.with_mime(mime);
                }
                files.push(part);
            }
        }
        let fields: Vec<(String, String)> = match nested.get("fields") {
            Some(Value::Object(map)) => map
                .iter()
                .map(|(key, value)| {
                    let rendered = match value {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    (key.clone(), rendered)
                })
                .collect(),
            _ => Vec::new(),
        };
        return Ok(request.multipart(fields, files));
    }

    if let Some(body) = params.get("body") {
        return Ok(request.json_value(body.clone()));
    }
    if let Some(raw) = params.opt_string("body_json")? {
        let value: Value = serde_json::from_str(&raw).map_err(|err| SdkError::InvalidArgument {
            message: format!("`body_json` is not valid JSON: {err}"),
        })?;
        return Ok(request.json_value(value));
    }
    if let Some(value) = params.get("bytes") {
        return Ok(request.bytes(decode_bytes(value)?));
    }
    if params.flag("empty_body")? {
        return Ok(request.empty_json());
    }
    Ok(request)
}

/// Decode a byte payload, accepting an array of numbers or a base64 string.
fn decode_bytes(value: &Value) -> Result<Vec<u8>> {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|byte| {
                byte.as_u64()
                    .filter(|value| *value <= 255)
                    .map(|value| value as u8)
                    .ok_or_else(|| SdkError::InvalidArgument {
                        message: "`bytes` must be an array of byte values".into(),
                    })
            })
            .collect(),
        Value::String(text) => {
            crate::models::base64_decode(text).ok_or_else(|| SdkError::InvalidArgument {
                message: "`bytes` is not valid base64".into(),
            })
        }
        _ => Err(SdkError::InvalidArgument {
            message: "`bytes` must be an array of byte values or a base64 string".into(),
        }),
    }
}

/// Test alias for the shared base64 decoder, which lives in [`crate::models`] because the
/// models need it too.
#[cfg(test)]
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    crate::models::base64_decode(input)
}
fn build_filter(params: &Params) -> Result<EventFilter> {
    let types = params.string_list("types")?;
    let mut filter = if types.is_empty() {
        EventFilter::all()
    } else {
        EventFilter::types(types.iter().map(|raw| crate::EventType::parse(raw)))
    };
    for conversation_id in params.id_list("conversation_ids")? {
        filter = filter.conversation(conversation_id);
    }
    Ok(filter)
}

/// Convert an error into the JSON object every transport reports.
pub fn error_object(error: &SdkError) -> Value {
    let mut object = Map::new();
    object.insert("kind".into(), Value::String(error.kind().as_str().into()));
    object.insert("message".into(), Value::String(error.to_string()));
    if let Some(status) = error.status() {
        object.insert("status".into(), Value::from(status));
    }
    if let Some(permission) = error.permission() {
        object.insert("permission".into(), Value::String(permission.into()));
    }
    if let Some(api) = error.api_message() {
        object.insert("api_error".into(), Value::String(api.into()));
    }
    if let Some(seconds) = error.retry_after_secs() {
        object.insert("retry_after_secs".into(), Value::from(seconds));
    }
    object.insert("retryable".into(), Value::Bool(error.is_retryable()));
    if let SdkError::Http { body, .. } = error {
        object.insert(
            "body".into(),
            serde_json::to_value(body).unwrap_or(Value::Null),
        );
    }
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_index_is_unique_and_complete() {
        let mut seen: Vec<&str> = Vec::new();
        for method in METHODS {
            assert!(!seen.contains(method), "duplicate method {method}");
            seen.push(method);
        }
        for required in [
            "version",
            "client.init",
            "request",
            "posts.create",
            "chat.send",
        ] {
            assert!(METHODS.contains(&required), "{required} must be listed");
        }
        assert!(METHODS.len() > 80, "the surface should be broad");
    }

    #[test]
    fn version_and_capabilities_work_without_a_client() {
        let mut state = RpcState::new();
        let version = state.dispatch("version", &Value::Null).unwrap();
        assert_eq!(version["protocol"], PROTOCOL_VERSION);
        assert_eq!(version["name"], "openfield-bot-sdk");

        let caps = state.dispatch("capabilities", &Value::Null).unwrap();
        assert_eq!(caps["protocol"], PROTOCOL_VERSION);
        assert_eq!(caps["features"]["ffi"], true);
        assert!(caps["methods"].as_array().unwrap().len() > 80);
    }

    #[test]
    fn an_unknown_method_is_reported() {
        let mut state = RpcState::new();
        let err = state.dispatch("nope", &Value::Null).unwrap_err();
        assert!(err.to_string().contains("unknown method"), "got: {err}");
    }

    #[test]
    fn calls_before_client_init_explain_themselves() {
        let mut state = RpcState::new();
        let err = state.dispatch("me", &Value::Null).unwrap_err();
        assert!(err.to_string().contains("client.init"), "got: {err}");
    }

    #[test]
    fn client_init_accepts_options_and_redacts_the_token() {
        let mut state = RpcState::new();
        let token = format!("ofb_{}", "1".repeat(64));
        let params = json!({
            "base_url": "http://127.0.0.1:9",
            "token": token,
            "timeout_ms": 1500,
            "retry": { "max_attempts": 5 },
        });
        let result = state.dispatch("client.init", &params).unwrap();
        assert_eq!(result["has_token"], true);

        let config = state.dispatch("client.config", &Value::Null).unwrap();
        assert_eq!(config["timeout_ms"], 1500);
        assert_eq!(config["retry"]["max_attempts"], 5);
        let rendered = config.to_string();
        assert!(
            !rendered.contains(&"1".repeat(64)),
            "the token body must never be serialised: {rendered}"
        );
        assert!(rendered.contains("ofb_…"), "a redacted form is expected");
    }

    #[test]
    fn client_init_defaults_the_base_url() {
        let mut state = RpcState::new();
        let result = state.dispatch("client.init", &json!({})).unwrap();
        assert_eq!(result["has_token"], false);
        let config = state.dispatch("client.config", &Value::Null).unwrap();
        assert_eq!(config["base_url"], crate::config::DEFAULT_BASE_URL);
    }

    #[test]
    fn a_bad_token_is_rejected_at_init() {
        let mut state = RpcState::new();
        let err = state
            .dispatch("client.init", &json!({ "token": "nope" }))
            .unwrap_err();
        assert!(err.to_string().contains("ofb_"), "got: {err}");
    }

    #[test]
    fn set_token_switches_between_authenticated_and_anonymous() {
        let mut state = RpcState::new();
        state.dispatch("client.init", &json!({})).unwrap();
        // Anonymous: a bot-only call explains the problem.
        let err = state.dispatch("me", &Value::Null).unwrap_err();
        assert!(err.to_string().contains("bot token"), "got: {err}");

        let token = format!("ofb_{}", "2".repeat(64));
        state
            .dispatch("client.set_token", &json!({ "token": token }))
            .unwrap();
        assert_eq!(
            state.dispatch("client.has_token", &Value::Null).unwrap()["has_token"],
            true
        );

        // An empty string clears it again.
        state
            .dispatch("client.set_token", &json!({ "token": "" }))
            .unwrap();
        assert_eq!(
            state.dispatch("client.has_token", &Value::Null).unwrap()["has_token"],
            false
        );
    }

    #[test]
    fn a_request_shape_error_is_reported() {
        let mut state = RpcState::new();
        state
            .dispatch("client.init", &json!({ "base_url": "http://127.0.0.1:9" }))
            .unwrap();
        let err = state
            .dispatch("request", &json!({ "path": "/x" }))
            .unwrap_err();
        assert!(
            err.to_string().contains("`method` is required"),
            "got: {err}"
        );
    }

    #[test]
    fn an_absolute_path_is_refused() {
        let mut state = RpcState::new();
        state
            .dispatch("client.init", &json!({ "base_url": "http://127.0.0.1:9" }))
            .unwrap();
        let err = state
            .dispatch(
                "request",
                &json!({ "method": "GET", "path": "https://evil.example/x" }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("server-relative"), "got: {err}");
    }

    #[test]
    fn a_transport_failure_surfaces_as_a_network_error() {
        let mut state = RpcState::new();
        state
            .dispatch("client.init", &json!({ "base_url": "http://127.0.0.1:9" }))
            .unwrap();
        let err = state
            .dispatch(
                "request",
                &json!({ "method": "GET", "path": "/api/v1/capabilities" }),
            )
            .unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::Network);
        assert!(err.is_retryable());
    }

    #[test]
    fn events_need_a_sink() {
        let mut state = RpcState::new();
        state
            .dispatch("client.init", &json!({ "base_url": "http://127.0.0.1:9" }))
            .unwrap();
        let err = state
            .dispatch("events.subscribe", &Value::Null)
            .unwrap_err();
        assert!(
            err.to_string().contains("no event sink"),
            "a transport without a sink must say so: {err}"
        );
    }

    #[test]
    fn event_message_matches_the_documented_shape() {
        let event = Event::parse(
            r#"{"type":"chat.message.created","recipients":[7,4242],"data":{"id":1}}"#,
        )
        .unwrap();
        let message = event_message(&event);
        assert_eq!(message["event"], "chat.message.created");
        assert_eq!(message["data"]["id"], 1);
        assert_eq!(message["recipients"][0], 7);

        // A broadcast event omits `recipients` rather than sending an empty array.
        let broadcast = Event::parse(r#"{"type":"post.created","data":{"id":5}}"#).unwrap();
        let message = event_message(&broadcast);
        assert!(message.get("recipients").is_none());
    }

    #[test]
    fn error_object_carries_the_status_and_permission() {
        let error = SdkError::Http {
            status: 403,
            body: crate::ApiErrorBody::parse(
                br#"{"error":"permission denied","permission":"posts.create"}"#,
            ),
            retry_after_secs: None,
        };
        let object = error_object(&error);
        assert_eq!(object["kind"], "forbidden");
        assert_eq!(object["status"], 403);
        assert_eq!(object["permission"], "posts.create");
        assert_eq!(object["retryable"], false);
    }

    #[test]
    fn base64_round_trips_against_the_rfc_vectors() {
        // "foobar" encodes to Zm9vYmFy.
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(base64_decode("Zg==").unwrap(), b"f");
        assert_eq!(base64_decode("Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode("").unwrap(), Vec::<u8>::new());
        assert_eq!(base64_decode(" Zm9v\nYmFy ").unwrap(), b"foobar");
        assert!(base64_decode("!!").is_none());
    }

    #[test]
    fn bytes_accepts_an_array_or_base64() {
        assert_eq!(decode_bytes(&json!([1, 2, 3])).unwrap(), vec![1u8, 2, 3]);
        assert_eq!(decode_bytes(&json!("Zm9vYmFy")).unwrap(), b"foobar");
        assert!(decode_bytes(&json!([300])).is_err());
        assert!(decode_bytes(&json!(5)).is_err());
    }

    #[test]
    fn params_tolerate_a_non_object() {
        let mut state = RpcState::new();
        // A bare string where an object is expected must not panic.
        let err = state.dispatch("me", &json!("nope")).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::InvalidArgument);
    }
}
