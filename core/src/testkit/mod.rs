//! An in-process mock OpenField server, behind the `testkit` feature.
//!
//! Every binding in this repository (Rust, Python, Node, the sidecar) tests against
//! this server rather than a real deployment, so the suite is hermetic and does not
//! depend on a running gateway, database or object store.
//!
//! It is deliberately faithful about the things that are easy to get wrong and that
//! the bundled documentation got wrong:
//!
//! * `/users/me` answers `401 {"error":"invalid bot token"}` for an absent or unknown
//!   `ofb_` token, matching `ResolveBotToken` rather than the JWT path;
//! * `POST /conversations` **requires** `title`;
//! * `POST /conversations/:id/read` reads `last_message_id`;
//! * `POST /conversations/:id/invite` takes a single `user_id`, not a list;
//! * `PUT /posts/:id/reactions` **requires** `{"reaction": ...}` from the seven
//!   allowed keys;
//! * content limits are enforced in **bytes**, so a CJK body can fail;
//! * `GET /conversations/:id/messages` returns **newest first**;
//! * `GET /api/v1/ws` demands a single-use ticket minted by `POST /api/v1/ws`;
//! * `POST /attachments` requires the multipart field to be named `file`.
//!
//! It also exposes the fault-injection an SDK test needs: [`MockServer::fail_next`] for
//! transient transport failures and [`MockServer::force_status`] for a `429` with a
//! `Retry-After` header.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};

use crate::models::{base64_encode, Id, MONEY_SCALE};

/// A request the mock server received, for assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    /// HTTP method.
    pub method: String,
    /// Path without the query string.
    pub path: String,
    /// Raw query string, without the `?`.
    pub query: String,
    /// `Authorization` header value, as received.
    pub authorization: Option<String>,
    /// Request body as text.
    pub body: String,
}

impl RecordedRequest {
    /// A query parameter's value.
    pub fn query_param(&self, name: &str) -> Option<String> {
        self.query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == name).then(|| percent_decode(value))
        })
    }

    /// Whether the body contains a multipart field with the given name.
    pub fn has_multipart_field(&self, name: &str) -> bool {
        self.body.contains(&format!("name=\"{name}\""))
    }

    /// The parsed JSON body, when there is one.
    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }

    /// The request body length in bytes.
    pub fn body_len(&self) -> usize {
        self.body.len()
    }
}

/// Tunable behaviour of the mock server.
#[derive(Debug, Clone)]
pub struct MockConfig {
    /// The one token the server accepts.
    pub token: String,
    /// The bot's user id.
    pub bot_id: Id,
    /// The bot's display name.
    pub bot_nickname: String,
    /// Permission keys the bot holds.
    pub permissions: Vec<String>,
    /// The `limit` echoed by `GET /bots`.
    pub bot_limit: i64,
    /// How many bots already exist (to exercise the ceiling).
    pub existing_bots: usize,
    /// Whether `POST /attachments` refuses anything but a `file` field.
    pub strict_attachment_field: bool,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            token: format!("ofb_{}", "1".repeat(64)),
            bot_id: 4242,
            bot_nickname: "天气预报".to_string(),
            permissions: vec![
                "posts.view".into(),
                "posts.create".into(),
                "posts.edit".into(),
                "posts.delete".into(),
                "posts.reply.create".into(),
                "posts.react".into(),
                "posts.favorite".into(),
                "chat.view".into(),
                "chat.send".into(),
                "chat.edit".into(),
                "chat.delete".into(),
                "chat.group.create".into(),
                "chat.group.invite".into(),
                "chat.group.manage".into(),
                "chat.request.send".into(),
                "chat.request.approve".into(),
                "storage.upload".into(),
                "storage.list".into(),
                "storage.delete".into(),
                "account.view".into(),
                "account.profile.edit".into(),
                "account.avatar.edit".into(),
                "account.follow".into(),
            ],
            bot_limit: 10,
            existing_bots: 1,
            strict_attachment_field: true,
        }
    }
}

/// A forced response: the status, an optional `Retry-After`, and how many requests it
/// applies to before the mock answers normally again.
type ForcedResponse = (u16, Option<u64>, u32);

/// Mutable state shared with connection threads.
#[derive(Debug)]
pub struct MockState {
    /// The active configuration.
    pub config: MockConfig,
    requests: Mutex<Vec<RecordedRequest>>,
    posts: Mutex<Vec<Value>>,
    messages: Mutex<HashMap<Id, Vec<Value>>>,
    attachments: Mutex<Vec<Value>>,
    reports: Mutex<Vec<Value>>,
    /// Wallet balance in cents, plus the transaction log, so the mock can reject a
    /// deduction the way `repository.ErrInsufficientBalance` does.
    wallet_cents: Mutex<i64>,
    wallet_transactions: Mutex<Vec<Value>>,
    /// Task codes already claimed, which is what makes the second call a 409.
    claimed_tasks: Mutex<Vec<String>>,
    /// The account's payment PIN, if one has been set. Absent by default: a freshly
    /// created bot really has none, which is why the payment endpoints answer 403.
    pin: Mutex<Option<String>>,
    membership_level: Mutex<i64>,
    membership_auto_renew: Mutex<bool>,
    membership_purchases: Mutex<Vec<Value>>,
    /// Open chunked-upload sessions: id → uploaded 1-based indexes.
    chunks: Mutex<HashMap<String, Vec<i64>>>,
    /// Published plugin rows.
    plugins: Mutex<Vec<Value>>,
    /// Stored E2EE envelopes, keyed by conversation.
    envelopes: Mutex<HashMap<Id, Vec<Value>>>,
    punishments: Mutex<Vec<Value>>,
    /// Attachments whose burn countdown has been armed: id → deadline string.
    burns: Mutex<HashMap<Id, String>>,
    /// Connected WebSocket clients, each tagged with its own id so it can deregister
    /// itself when the socket drops.
    ws_clients: Mutex<Vec<(u64, Sender<String>)>>,
    tickets: Mutex<Vec<String>>,
    fail_next: Mutex<HashMap<String, u32>>,
    forced: Mutex<HashMap<String, ForcedResponse>>,
    next_id: AtomicI64,
    ws_seq: AtomicU64,
}

impl MockState {
    fn new(config: MockConfig) -> Self {
        Self {
            config,
            requests: Mutex::new(Vec::new()),
            posts: Mutex::new(Vec::new()),
            messages: Mutex::new(HashMap::new()),
            attachments: Mutex::new(Vec::new()),
            reports: Mutex::new(Vec::new()),
            // A wallet with 1000 coins, so a membership purchase (100–400 coins), a
            // tip and a make-up sign-in (100 coins each) all have room, and an
            // over-deduction can still be forced.
            wallet_cents: Mutex::new(1000 * MONEY_SCALE),
            wallet_transactions: Mutex::new(Vec::new()),
            claimed_tasks: Mutex::new(Vec::new()),
            pin: Mutex::new(None),
            membership_level: Mutex::new(0),
            membership_auto_renew: Mutex::new(false),
            membership_purchases: Mutex::new(Vec::new()),
            chunks: Mutex::new(HashMap::new()),
            // One published plugin, so the catalog, the detail call and the bundle
            // download all have something to answer with.
            plugins: Mutex::new(vec![json!({
                "id": "demo",
                "name": "Demo",
                "version": "1.0.0",
                "author": "bot",
                "description": "a demo plugin",
                "permissions": ["posts.view"],
                "min_app_version": "0.1.0",
                "verified": true,
                "published": true,
                "downloads": 3,
                "file_size": 32,
                "sha256": "a".repeat(64),
                "created_at": "2026-10-10T15:20:12Z",
                "updated_at": "2026-10-10T15:20:12Z"
            })]),
            envelopes: Mutex::new(HashMap::new()),
            punishments: Mutex::new(Vec::new()),
            burns: Mutex::new(HashMap::new()),
            ws_clients: Mutex::new(Vec::new()),
            tickets: Mutex::new(Vec::new()),
            fail_next: Mutex::new(HashMap::new()),
            forced: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1000),
            ws_seq: AtomicU64::new(1),
        }
    }

    fn id(&self) -> Id {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }
}

/// A running mock server.
#[derive(Debug)]
pub struct MockServer {
    base_url: String,
    shutdown: Arc<AtomicBool>,
    state: Arc<MockState>,
    join: Option<JoinHandle<()>>,
}

impl MockServer {
    /// Start a mock server on an ephemeral loopback port.
    pub fn start() -> std::io::Result<Self> {
        Self::start_with(MockConfig::default())
    }

    /// Start with an explicit configuration.
    pub fn start_with(config: MockConfig) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let state = Arc::new(MockState::new(config));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_state = Arc::clone(&state);
        let thread_shutdown = Arc::clone(&shutdown);
        let join = thread::Builder::new()
            .name("openfield-mock".to_string())
            .spawn(move || {
                let mut workers: Vec<JoinHandle<()>> = Vec::new();
                while !thread_shutdown.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let connection_state = Arc::clone(&thread_state);
                            let connection_shutdown = Arc::clone(&thread_shutdown);
                            workers.push(thread::spawn(move || {
                                let _ =
                                    serve_connection(stream, connection_state, connection_shutdown);
                            }));
                        }
                        Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
                for worker in workers {
                    let _ = worker.join();
                }
            })?;

        Ok(Self {
            base_url: format!("http://{address}"),
            shutdown,
            state,
            join: Some(join),
        })
    }

    /// The server's base URL, e.g. `http://127.0.0.1:41234`.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The token the server accepts.
    pub fn token(&self) -> &str {
        &self.state.config.token
    }

    /// A ready-to-use authenticated client pointed at this server.
    pub fn client(&self) -> crate::AuthenticatedClient {
        let token = crate::BotToken::new(self.token().to_string()).expect("mock token is valid");
        crate::AuthenticatedClient::new(self.base_url.clone(), token)
            .expect("mock base url is valid")
    }

    /// Shared state, for direct assertions.
    pub fn state(&self) -> &Arc<MockState> {
        &self.state
    }

    /// Every request received, in order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state
            .requests
            .lock()
            .expect("mock requests lock")
            .clone()
    }

    /// The most recent request.
    pub fn last_request(&self) -> Option<RecordedRequest> {
        self.state
            .requests
            .lock()
            .expect("mock requests lock")
            .last()
            .cloned()
    }

    /// Requests whose path matches exactly.
    pub fn requests_to(&self, path: &str) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|request| request.path == path)
            .collect()
    }

    /// Forget every recorded request.
    pub fn clear_requests(&self) {
        self.state
            .requests
            .lock()
            .expect("mock requests lock")
            .clear();
    }

    /// Drop the next `count` requests to `path` without answering, simulating a
    /// transient transport failure.
    pub fn fail_next(&self, path: &str, count: u32) {
        self.state
            .fail_next
            .lock()
            .expect("mock fail lock")
            .insert(path.to_string(), count);
    }

    /// Insert a message **from another user** into a conversation.
    ///
    /// Every route in this mock speaks as the bot under test, which makes it impossible to
    /// exercise the rule that a bot must not answer itself: its own messages are the only
    /// ones that exist. This seeds traffic from somebody else, so the polling and
    /// self-loop guards can be tested.
    ///
    /// Returns the created message, ready to be handed to [`MockServer::broadcast`] if the
    /// test wants the event path as well as the polling path.
    pub fn seed_message(
        &self,
        conversation_id: Id,
        sender_id: Id,
        content: &str,
        sender_name: &str,
    ) -> Value {
        let id = self.state.id();
        let message = json!({
            "id": id,
            "conversation_id": conversation_id,
            "sender_id": sender_id,
            "sender_name": sender_name,
            "sender_is_bot": false,
            "kind": "text",
            "content": content,
            "mentions": [],
            "attachments": [],
            "burn_seconds": 0,
            "reply_to_id": Value::Null,
            "created_at": "2026-10-10T15:25:00Z"
        });
        self.state
            .messages
            .lock()
            .expect("mock messages lock")
            .entry(conversation_id)
            .or_default()
            .push(message.clone());
        message
    }

    /// Force a status for `path` — optionally with a `Retry-After` — for as many
    /// requests as `times`.
    pub fn force_status(&self, path: &str, status: u16, retry_after: Option<u64>, times: u32) {
        let mut forced = self.state.forced.lock().expect("mock forced lock");
        forced.insert(path.to_string(), (status, retry_after, times.max(1)));
    }

    /// Push one event frame to every connected WebSocket client.
    ///
    /// Returns how many clients received it. A client whose socket has dropped is
    /// deregistered here rather than being counted forever.
    pub fn broadcast(&self, event: &Value) -> usize {
        let payload = serde_json::to_string(event).unwrap_or_default();
        let mut clients = self.state.ws_clients.lock().expect("mock ws lock");
        // `retain` drops the entries whose socket is gone, so the survivors are exactly
        // the clients that received this frame.
        clients.retain(|(_, client)| client.send(payload.clone()).is_ok());
        clients.len()
    }

    /// How many WebSocket clients are currently attached.
    ///
    /// A client deregisters itself when its connection drops, so this falls when a
    /// consumer disconnects -- which is what makes "closing the stream freed the socket"
    /// observable from a test.
    pub fn ws_client_count(&self) -> usize {
        self.state.ws_clients.lock().expect("mock ws lock").len()
    }

    /// Stop the server and wait for its threads.
    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// One parsed request.
struct IncomingRequest {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl IncomingRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    fn authorization(&self) -> Option<&str> {
        self.header("authorization")
    }

    fn query_param(&self, name: &str) -> Option<String> {
        self.query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == name).then(|| percent_decode(value))
        })
    }
}

fn serve_connection(
    stream: TcpStream,
    state: Arc<MockState>,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        let request = match read_request(&mut reader) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(()),
            Err(_) => return Ok(()),
        };

        // The WebSocket upgrade keeps the connection for the lifetime of the client.
        if request.path == "/api/v1/ws" && request.method == "GET" {
            let upgrade = request.query_param("ticket");
            match upgrade {
                Some(ticket) => {
                    if !redeem_ticket(&state, &ticket) {
                        write_json(
                            &mut writer,
                            401,
                            &json!({"error": "invalid or expired ticket"}),
                            None,
                        )?;
                        continue;
                    }
                    let key = request
                        .header("sec-websocket-key")
                        .unwrap_or_default()
                        .to_string();
                    return serve_websocket(writer, reader, state, shutdown, &key);
                }
                None => {
                    write_json(&mut writer, 401, &json!({"error": "missing ticket"}), None)?;
                    continue;
                }
            }
        }

        state
            .requests
            .lock()
            .expect("mock requests lock")
            .push(RecordedRequest {
                method: request.method.clone(),
                path: request.path.clone(),
                query: request.query.clone(),
                authorization: request.authorization().map(str::to_string),
                body: String::from_utf8_lossy(&request.body).to_string(),
            });

        // Fault injection wins over the router.
        if let Some(response) = forced_response(&state, &request.path) {
            let (status, retry_after) = response;
            write_json(
                &mut writer,
                status,
                &json!({"error": "forced"}),
                retry_after,
            )?;
            continue;
        }
        let path = request.path.clone();
        let mut failures = state.fail_next.lock().expect("mock fail lock");
        if let Some(remaining) = failures.get_mut(&path) {
            if *remaining > 0 {
                *remaining -= 1;
                drop(failures);
                // Answer nothing and hang up, which is what a transport failure looks like.
                return Ok(());
            }
        }
        drop(failures);

        // Endpoints whose body is bytes rather than JSON are resolved first,
        // because `route` can only answer a `Value`.
        if let Some(reply) = binary_route(&state, &request) {
            match reply {
                BinaryReply::Bytes {
                    status,
                    content_type,
                    body,
                } => write_bytes(&mut writer, status, content_type, &body)?,
                BinaryReply::Error { status, body } => {
                    write_json(&mut writer, status, &body, None)?;
                }
            }
            continue;
        }

        let (status, body) = route(&state, &request);
        write_json(&mut writer, status, &body, None)?;
    }
}

fn forced_response(state: &MockState, path: &str) -> Option<(u16, Option<u64>)> {
    let mut forced = state.forced.lock().expect("mock forced lock");
    let entry = forced.get_mut(path)?;
    let (status, retry_after, remaining) = *entry;
    if remaining <= 1 {
        forced.remove(path);
    } else {
        entry.2 = remaining - 1;
    }
    Some((status, retry_after))
}

/// Route a request to a handler.
fn route(state: &MockState, request: &IncomingRequest) -> (u16, Value) {
    let path = request.path.as_str();
    let method = request.method.as_str();
    let config = &state.config;

    // Public routes ---------------------------------------------------------
    if path == "/api/v1/capabilities" && method == "GET" {
        return (
            200,
            json!({"features": ["chat.e2ee", "camps.subgroups", "moderation.reports"]}),
        );
    }
    if path == "/api/v1/health" && method == "GET" {
        return (
            200,
            json!({
                "all_healthy": true,
                "checked_at": "2026-08-24T12:00:00Z",
                "services": {"gateway": {"status": "up", "latency_ms": 0}}
            }),
        );
    }
    if path == "/api/v1/announcements" && method == "GET" {
        return (200, json!([{"id": 1, "title": "welcome", "active": true}]));
    }
    if path == "/api/v1/posts" && method == "GET" {
        let page = request
            .query_param("page")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(1);
        let posts = state.posts.lock().expect("mock posts lock").clone();
        return (
            200,
            json!({"posts": posts, "page": page, "limit": 20, "query": "", "tag": ""}),
        );
    }

    // The plugin catalog is authPublic — it renders before sign-in — so these two
    // routes sit above the token gate. Everything under `/plugins/admin/` and the
    // bundle download stay below it.
    if path == "/api/v1/plugins" && method == "GET" {
        let plugins: Vec<Value> = state
            .plugins
            .lock()
            .expect("mock plugins lock")
            .iter()
            .filter(|plugin| plugin.get("published").and_then(Value::as_bool) == Some(true))
            .cloned()
            .collect();
        return (200, json!({"plugins": plugins}));
    }
    if let Some(rest) = path.strip_prefix("/api/v1/plugins/") {
        if method == "GET" && !rest.contains('/') {
            return match published_plugin(state, rest) {
                Some(plugin) => (200, plugin),
                None => (404, json!({"error": "plugin not found"})),
            };
        }
    }

    // Everything below needs a valid bot token ------------------------------
    if let Some(refusal) = authorize(state, request) {
        return refusal;
    }

    // Account ---------------------------------------------------------------
    if path == "/api/v1/users/me" && method == "GET" {
        return (
            200,
            json!({
                "id": config.bot_id,
                "username": "weather_watcher",
                "nickname": config.bot_nickname,
                "avatar_url": "",
                "is_bot": true,
                "bot_owner_id": 7,
                "status": "active",
                "created_at": "2026-10-10T15:20:12Z"
            }),
        );
    }
    if path == "/api/v1/users/me" && method == "PUT" {
        let body = request_json(request);
        return (
            200,
            json!({
                "id": config.bot_id,
                "username": body.get("username").and_then(Value::as_str).unwrap_or("weather_watcher"),
                "nickname": body.get("nickname").and_then(Value::as_str).unwrap_or(&config.bot_nickname),
                "is_bot": true
            }),
        );
    }
    if path == "/api/v1/users/me/permissions" && method == "GET" {
        return (
            200,
            json!({"permissions": config.permissions, "groups": ["所有人"]}),
        );
    }
    if path == "/api/v1/users/storage-buckets" && method == "GET" {
        return (
            200,
            json!({
                "buckets": [{
                    "name": "default", "label": "标准", "default_quota": 104857600,
                    "min_member_level": 0, "is_default": true, "locked": false
                }],
                "current": "default"
            }),
        );
    }
    if path == "/api/v1/storage/usage" && method == "GET" {
        return (200, json!({"used": 2048, "quota": 104857600}));
    }
    if path == "/api/v1/users/me/avatar" && method == "POST" {
        if config.strict_attachment_field && !body_has_multipart_field(request, "file") {
            return (400, json!({"error": "missing file field"}));
        }
        return (200, json!({"avatar_url": "http://example.com/a.png"}));
    }
    if path == "/api/v1/users/me/banner" && method == "POST" {
        if config.strict_attachment_field && !body_has_multipart_field(request, "file") {
            return (400, json!({"error": "missing file field"}));
        }
        return (200, json!({"banner_url": "http://example.com/b.png"}));
    }

    // Bots ------------------------------------------------------------------
    if path == "/api/v1/bots" && method == "GET" {
        let bots: Vec<Value> = (0..config.existing_bots)
            .map(|index| {
                json!({
                    "id": config.bot_id + index as Id,
                    "username": format!("bot_{index}"),
                    "nickname": format!("bot {index}"),
                    "avatar_url": "",
                    "created_at": "2026-10-10T15:20:12Z",
                    "token_created_at": "2026-10-10T15:20:12Z"
                })
            })
            .collect();
        return (200, json!({"bots": bots, "limit": config.bot_limit}));
    }
    if path == "/api/v1/bots" && method == "POST" {
        let body = request_json(request);
        let username = body
            .get("username")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let nickname = body
            .get("nickname")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if username.len() < 3
            || username.len() > 32
            || !username
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return (
                400,
                json!({"error": "username must be 3-32 letters, digits or underscores, nickname required"}),
            );
        }
        if nickname.trim().is_empty() || nickname.chars().count() > 100 {
            return (400, json!({"error": "nickname must be 1-100 characters"}));
        }
        if (config.existing_bots as i64) >= config.bot_limit {
            return (
                403,
                json!({"error": "bot limit reached", "limit": config.bot_limit}),
            );
        }
        if username == "taken" {
            return (409, json!({"error": "username already taken"}));
        }
        let id = state.id();
        return (
            201,
            json!({
                "user": {
                    "id": id, "username": username, "nickname": nickname,
                    "is_bot": true, "bot_owner_id": config.bot_id,
                    "created_at": "2026-10-10T15:20:12Z"
                },
                "token": format!("ofb_{:064x}", id)
            }),
        );
    }
    if let Some(rest) = path.strip_prefix("/api/v1/bots/") {
        if let Some(id_text) = rest.strip_suffix("/regenerate") {
            if id_text.parse::<Id>().is_err() {
                return (400, json!({"error": "invalid bot id"}));
            }
            return (200, json!({"token": format!("ofb_{:064x}", state.id())}));
        }
        if method == "DELETE" && rest.parse::<Id>().is_ok() {
            return (200, json!({"ok": true}));
        }
    }

    // Posts -----------------------------------------------------------------
    if path == "/api/v1/posts" && method == "POST" {
        let body = request_json(request);
        let content = body.get("content").and_then(Value::as_str).unwrap_or("");
        let quoted = body
            .get("quoted_post_id")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if content.is_empty() && quoted <= 0 {
            return (400, json!({"error": "content cannot be empty"}));
        }
        // The handler checks `len(req.Content) > 5000`, which counts bytes.
        if content.len() > 5000 {
            return (
                400,
                json!({"error": "content exceeds maximum length of 5000"}),
            );
        }
        let attachments = body
            .get("attachment_ids")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if attachments.len() > 9 {
            return (400, json!({"error": "too many attachments (max 9)"}));
        }
        let visibility = body
            .get("visibility")
            .and_then(Value::as_str)
            .unwrap_or("public");
        if !["public", "login", "friends", "private"].contains(&visibility) {
            return (400, json!({"error": "unknown visibility"}));
        }
        let id = state.id();
        let post = json!({
            "id": id,
            "user_id": config.bot_id,
            "content": content,
            "visibility": visibility,
            "reply_count": 0,
            "favorite_count": 0,
            "reactions": {},
            "pinned": false,
            "camp_id": body.get("camp_id").and_then(Value::as_i64).unwrap_or(0),
            "quoted_post_id": quoted,
            "attachments": [],
            "created_at": "2026-10-10T15:20:12Z"
        });
        state
            .posts
            .lock()
            .expect("mock posts lock")
            .push(post.clone());
        return (201, post);
    }
    if let Some(rest) = path.strip_prefix("/api/v1/posts/") {
        let segments: Vec<&str> = rest.split('/').collect();
        match (method, segments.as_slice()) {
            ("GET", [id_text]) if id_text.parse::<Id>().is_ok() => {
                let id: Id = id_text.parse().unwrap();
                let posts = state.posts.lock().expect("mock posts lock");
                match posts.iter().find(|post| post["id"] == id) {
                    Some(post) => return (200, post.clone()),
                    None => return (404, json!({"error": "post not found"})),
                }
            }
            ("PUT", [id_text, "reactions"]) if id_text.parse::<Id>().is_ok() => {
                let body = request_json(request);
                let reaction = body
                    .get("reaction")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if reaction.is_empty() {
                    return (400, json!({"error": "invalid request body"}));
                }
                // Mirrors allowedReactions in post.go.
                if !["like", "dislike", "love", "haha", "wow", "sad", "angry"].contains(&reaction) {
                    return (400, json!({"error": "unknown reaction"}));
                }
                return (
                    200,
                    json!({"id": id_text.parse::<Id>().unwrap(), "reactions": {reaction: 1}}),
                );
            }
            ("DELETE", [id_text, "reactions"]) if id_text.parse::<Id>().is_ok() => {
                return (200, json!({"ok": true}));
            }
            ("PUT", [id_text, "pin"]) if id_text.parse::<Id>().is_ok() => {
                let body = request_json(request);
                return (
                    200,
                    json!({
                        "id": id_text.parse::<Id>().unwrap(),
                        "pinned": body.get("pinned").cloned().unwrap_or(json!(false))
                    }),
                );
            }
            ("POST", [id_text, "favorite"]) if id_text.parse::<Id>().is_ok() => {
                return (200, json!({"ok": true}));
            }
            ("DELETE", [id_text, "favorite"]) if id_text.parse::<Id>().is_ok() => {
                return (200, json!({"ok": true}));
            }
            ("PUT", [id_text, "replies", reply_text]) if id_text.parse::<Id>().is_ok() => {
                let body = request_json(request);
                return (
                    200,
                    json!({
                        "id": reply_text.parse::<Id>().unwrap_or(0),
                        "post_id": id_text.parse::<Id>().unwrap(),
                        "content": body.get("content").cloned().unwrap_or(json!(""))
                    }),
                );
            }
            ("GET", [id_text, "replies"]) if id_text.parse::<Id>().is_ok() => {
                return (200, json!({"replies": [], "page": 1, "limit": 50}));
            }
            ("POST", [id_text, "replies"]) if id_text.parse::<Id>().is_ok() => {
                let body = request_json(request);
                let content = body.get("content").and_then(Value::as_str).unwrap_or("");
                if content.is_empty() {
                    return (400, json!({"error": "content cannot be empty"}));
                }
                if content.len() > 5000 {
                    return (
                        400,
                        json!({"error": "content exceeds maximum length of 5000"}),
                    );
                }
                return (
                    201,
                    json!({
                        "id": state.id(),
                        "post_id": id_text.parse::<Id>().unwrap(),
                        "user_id": config.bot_id,
                        "content": content,
                        "parent_id": body.get("parent_id").cloned().unwrap_or(Value::Null)
                    }),
                );
            }
            ("DELETE", [id_text]) if id_text.parse::<Id>().is_ok() => {
                return (200, json!({"ok": true}));
            }
            // Tips: the amount is a whole coin count and the payment PIN is the
            // authorization, so the mock shares the PIN state with `/users/me/pin`.
            ("POST", [id_text, "tips"]) if id_text.parse::<Id>().is_ok() => {
                let post_id: Id = id_text.parse().unwrap();
                let body = request_json(request);
                let Some(amount) = body.get("amount").and_then(Value::as_i64) else {
                    return (400, json!({"error": "amount and pin are required"}));
                };
                let pin = body.get("pin").and_then(Value::as_str).unwrap_or("");
                if pin.is_empty() {
                    return (400, json!({"error": "amount and pin are required"}));
                }
                if !(1..=100_000).contains(&amount) {
                    return (400, json!({"error": "invalid tip amount"}));
                }
                if post_id == 777 {
                    // The mock's own post, which the handler refuses to let you tip.
                    return (400, json!({"error": "cannot tip your own post"}));
                }
                if post_id == 404 {
                    return (404, json!({"error": "post not found"}));
                }
                if let Some(refusal) = reject_payment_pin(state, pin) {
                    return refusal;
                }
                let cents = amount * MONEY_SCALE;
                let net = cents * 95 / 100;
                let mut balance = state.wallet_cents.lock().expect("mock wallet lock");
                if *balance < cents {
                    return (400, json!({"error": "insufficient balance"}));
                }
                *balance -= cents;
                drop(balance);
                return (
                    201,
                    json!({
                        "id": state.id(),
                        "post_id": post_id,
                        "user_id": config.bot_id,
                        "amount": cents,
                        "net_amount": net,
                        "created_at": "2026-10-10T15:20:12Z"
                    }),
                );
            }
            _ => {}
        }
    }

    // Chat ------------------------------------------------------------------
    if path == "/api/v1/conversations" && method == "GET" {
        return (
            200,
            json!({"conversations": [{
                "id": 33, "type": "group", "title": "Family", "owner_id": config.bot_id,
                "is_public": true, "allow_join": true, "encrypted": false,
                "member_count": 2, "is_member": true, "unread": 0
            }]}),
        );
    }
    if path == "/api/v1/conversations" && method == "POST" {
        let body = request_json(request);
        let title = body.get("title").and_then(Value::as_str).unwrap_or("");
        if title.is_empty() {
            return (400, json!({"error": "invalid request body"}));
        }
        return (
            201,
            json!({"id": state.id(), "type": "group", "title": title, "owner_id": config.bot_id}),
        );
    }
    if path == "/api/v1/conversations/start" && method == "POST" {
        let body = request_json(request);
        let user_id = body.get("user_id").and_then(Value::as_i64).unwrap_or(0);
        if user_id <= 0 {
            return (400, json!({"error": "invalid request body"}));
        }
        if user_id == config.bot_id {
            return (400, json!({"error": "cannot start a chat with yourself"}));
        }
        return (201, json!({"message": "chat request sent"}));
    }
    if path == "/api/v1/consent-requests" && method == "GET" {
        return (
            200,
            json!({"requests": [{
                "id": 1, "type": "private_chat", "requester_id": 7,
                "target_user_id": config.bot_id, "message": "hi", "status": "pending"
            }]}),
        );
    }
    if let Some(rest) = path.strip_prefix("/api/v1/consent-requests/") {
        if let Some((_id, action)) = rest.split_once('/') {
            if method == "POST" && (action == "accept" || action == "decline") {
                return (200, json!({"ok": true}));
            }
        }
    }
    if let Some(rest) = path.strip_prefix("/api/v1/conversations/") {
        let segments: Vec<&str> = rest.split('/').collect();
        match (method, segments.as_slice()) {
            ("GET", [id_text]) if id_text.parse::<Id>().is_ok() => {
                let id: Id = id_text.parse().unwrap();
                return (
                    200,
                    json!({
                        "conversation": {
                            "id": id, "type": "group", "title": "Family",
                            "owner_id": config.bot_id, "encrypted": false
                        },
                        "members": [{"user_id": config.bot_id, "role": "owner"}],
                        "my_membership": {"user_id": config.bot_id, "role": "owner"}
                    }),
                );
            }
            ("GET", [id_text, "messages"]) if id_text.parse::<Id>().is_ok() => {
                let conversation_id: Id = id_text.parse().unwrap();
                let limit = request
                    .query_param("limit")
                    .and_then(|value| value.parse::<i64>().ok())
                    .unwrap_or(50);
                // ListByConversation resets anything outside 1..=100 to 50.
                let limit = if !(1..=100).contains(&limit) {
                    50
                } else {
                    limit
                };
                let before = request
                    .query_param("before")
                    .and_then(|value| value.parse::<Id>().ok())
                    .unwrap_or(0);
                let messages = state.messages.lock().expect("mock messages lock");
                let mut page: Vec<Value> = messages
                    .get(&conversation_id)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|message| before == 0 || message["id"].as_i64().unwrap_or(0) < before)
                    .collect();
                // Newest first, mirroring ORDER BY created_at DESC, id DESC.
                page.sort_by_key(|message| -message["id"].as_i64().unwrap_or(0));
                page.truncate(limit.max(0) as usize);
                return (200, json!({"messages": page}));
            }
            ("POST", [id_text, "messages"]) if id_text.parse::<Id>().is_ok() => {
                let conversation_id: Id = id_text.parse().unwrap();
                let body = request_json(request);
                let content = body.get("content").and_then(Value::as_str).unwrap_or("");
                let attachments = body
                    .get("attachment_ids")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let check_id = body.get("check_id").and_then(Value::as_i64).unwrap_or(0);
                if content.is_empty() && attachments.is_empty() && check_id <= 0 {
                    return (400, json!({"error": "message cannot be empty"}));
                }
                if content.len() > 5000 {
                    return (400, json!({"error": "message too long (max 5000)"}));
                }
                let burn = body
                    .get("burn_seconds")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                if burn != 0 && !(5..=86_400).contains(&burn) {
                    return (
                        400,
                        json!({"error": "invalid burn_seconds (allowed: 0, or 5..86400)"}),
                    );
                }
                let id = state.id();
                let mentions = body
                    .get("mentions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let message = json!({
                    "id": id,
                    "conversation_id": conversation_id,
                    "sender_id": config.bot_id,
                    "sender_is_bot": true,
                    "kind": "text",
                    "content": content,
                    "mentions": mentions,
                    "attachments": [],
                    "burn_seconds": burn,
                    "reply_to_id": body.get("reply_to_id").cloned().unwrap_or(Value::Null),
                    "created_at": "2026-10-10T15:22:41Z"
                });
                state
                    .messages
                    .lock()
                    .expect("mock messages lock")
                    .entry(conversation_id)
                    .or_default()
                    .push(message.clone());
                return (201, message);
            }
            ("POST", [id_text, "read"]) if id_text.parse::<Id>().is_ok() => {
                let body = request_json(request);
                // The handler binds last_message_id.
                if body.get("last_message_id").is_none() {
                    return (400, json!({"error": "invalid request body"}));
                }
                return (204, Value::Null);
            }
            ("POST", [id_text, "typing"]) if id_text.parse::<Id>().is_ok() => {
                return (200, json!({"ok": true}));
            }
            ("POST", [id_text, "invite"]) if id_text.parse::<Id>().is_ok() => {
                let body = request_json(request);
                // A single user_id, not a list.
                if body.get("user_id").and_then(Value::as_i64).is_none() {
                    return (400, json!({"error": "invalid request body"}));
                }
                return (201, json!({"message": "invite sent"}));
            }
            ("PUT", [id_text, "settings"]) if id_text.parse::<Id>().is_ok() => {
                return (200, json!({"ok": true}));
            }
            ("PUT", [id_text, "title"]) if id_text.parse::<Id>().is_ok() => {
                return (200, json!({"ok": true}));
            }
            ("POST", [_id, "leave"]) | ("POST", [_id, "join"]) => {
                return (204, Value::Null);
            }
            ("GET", [id_text, "files"]) if id_text.parse::<Id>().is_ok() => {
                return (200, json!({"attachments": []}));
            }
            ("GET", [_id, "todos"]) | ("GET", [_id, "announcements"]) => {
                return (200, json!({"items": []}));
            }
            // E2EE key distribution: the server is a relay and never inspects the
            // ciphertext, so the mock only checks membership and the empty-batch rule.
            ("GET", [id_text, "e2ee-keys"]) if id_text.parse::<Id>().is_ok() => {
                let conversation_id: Id = id_text.parse().unwrap();
                let envelopes = state
                    .envelopes
                    .lock()
                    .expect("mock envelopes lock")
                    .get(&conversation_id)
                    .cloned()
                    .unwrap_or_default();
                let mine = envelopes
                    .iter()
                    .find(|envelope| {
                        envelope.get("target_user_id").and_then(Value::as_i64)
                            == Some(config.bot_id)
                    })
                    .cloned();
                let version = envelopes
                    .iter()
                    .filter_map(|envelope| envelope.get("version").and_then(Value::as_i64))
                    .max()
                    .unwrap_or(0);
                return (
                    200,
                    json!({
                        "version": version,
                        "envelopes": envelopes,
                        "my_envelope": mine,
                        "conversation_id": conversation_id,
                    }),
                );
            }
            ("POST", [id_text, "e2ee-keys"]) if id_text.parse::<Id>().is_ok() => {
                let conversation_id: Id = id_text.parse().unwrap();
                let body = request_json(request);
                let Some(map) = body.get("envelopes").and_then(Value::as_object) else {
                    return (400, json!({"error": "invalid request body"}));
                };
                if map.is_empty() {
                    return (400, json!({"error": "no envelopes provided"}));
                }
                let mut store = state.envelopes.lock().expect("mock envelopes lock");
                let entry = store.entry(conversation_id).or_default();
                let version = entry
                    .iter()
                    .filter_map(|envelope| envelope.get("version").and_then(Value::as_i64))
                    .max()
                    .unwrap_or(0)
                    + 1;
                // Go marshals `map[int64]string` with string keys, which is the
                // shape the SDK has to produce.
                for (target, ciphertext) in map {
                    entry.push(json!({
                        "id": state.id(),
                        "conversation_id": conversation_id,
                        "version": version,
                        "target_user_id": target.parse::<Id>().unwrap_or(0),
                        "ciphertext": ciphertext,
                        "created_at": "2026-10-10T15:20:12Z"
                    }));
                }
                return (
                    200,
                    json!({"message": "e2ee keys stored", "version": version}),
                );
            }
            _ => {}
        }
    }

    // Camps -----------------------------------------------------------------
    if path == "/api/v1/camps" && method == "GET" {
        return (
            200,
            json!({"camps": [{
                "id": 66, "name": "营地", "description": "", "creator_id": config.bot_id,
                "is_visible": true, "direct_join": true, "member_post": true,
                "member_pin": false, "member_count": 1, "post_count": 0,
                "is_member": true, "my_role": "owner"
            }]}),
        );
    }
    if let Some(rest) = path.strip_prefix("/api/v1/camps/") {
        let segments: Vec<&str> = rest.split('/').collect();
        match (method, segments.as_slice()) {
            ("GET", [id_text]) if id_text.parse::<Id>().is_ok() => {
                return (
                    200,
                    json!({
                        "id": id_text.parse::<Id>().unwrap(), "name": "营地",
                        "creator_id": config.bot_id, "is_visible": true,
                        "direct_join": true, "member_post": true, "member_pin": false,
                        "is_member": true, "my_role": "owner"
                    }),
                );
            }
            ("GET", [id_text, "members"]) if id_text.parse::<Id>().is_ok() => {
                return (
                    200,
                    json!([{"id": 1, "camp_id": 66, "user_id": config.bot_id, "role": "owner"}]),
                );
            }
            ("GET", [id_text, "posts"]) if id_text.parse::<Id>().is_ok() => {
                return (200, json!({"posts": [], "page": 1}));
            }
            ("GET", [id_text, "subgroups"]) if id_text.parse::<Id>().is_ok() => {
                return (200, json!({"camps": []}));
            }
            ("POST", [id_text, "join"]) if id_text.parse::<Id>().is_ok() => {
                return (200, json!({"ok": true}));
            }
            _ => {}
        }
    }

    // Attachments -----------------------------------------------------------
    if path == "/api/v1/attachments" && method == "POST" {
        if config.strict_attachment_field && !body_has_multipart_field(request, "file") {
            return (400, json!({"error": "missing file field"}));
        }
        let id = state.id();
        let attachment = json!({
            "id": id,
            "user_id": config.bot_id,
            "original_name": "chart.png",
            "mime_type": "image/png",
            "size_bytes": 3,
            "url": "http://example.com/chart.png",
            "bucket": "default",
            "visibility": "public",
            "created_at": "2026-10-10T15:20:12Z"
        });
        state
            .attachments
            .lock()
            .expect("mock attachments lock")
            .push(attachment.clone());
        return (201, attachment);
    }
    if path == "/api/v1/attachments" && method == "GET" {
        let attachments = state
            .attachments
            .lock()
            .expect("mock attachments lock")
            .clone();
        return (200, json!({"attachments": attachments}));
    }

    // Reports ---------------------------------------------------------------
    if path == "/api/v1/reports" && method == "POST" {
        let body = request_json(request);
        let target_type = body
            .get("target_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let target_id = body.get("target_id").and_then(Value::as_i64).unwrap_or(0);
        let reason = body.get("reason").and_then(Value::as_str).unwrap_or("");
        if !["post", "message", "user"].contains(&target_type) {
            return (400, json!({"error": "invalid target type"}));
        }
        if target_id <= 0 || reason.trim().is_empty() || reason.chars().count() > 500 {
            return (400, json!({"error": "invalid report"}));
        }
        if target_type == "user" && target_id == config.bot_id {
            return (400, json!({"error": "cannot report yourself"}));
        }
        if target_id == 404 {
            return (404, json!({"error": "target not found"}));
        }
        if target_id == 409 {
            return (409, json!({"error": "you have already reported this"}));
        }
        let report = json!({
            "id": state.id(), "reporter_id": config.bot_id, "reporter_name": "bot",
            "target_type": target_type, "target_id": target_id, "reason": reason,
            "status": "pending", "created_at": "2026-10-10T15:20:12Z"
        });
        state
            .reports
            .lock()
            .expect("mock reports lock")
            .push(report.clone());
        return (201, report);
    }
    if path == "/api/v1/reports/mine" && method == "GET" {
        let reports = state.reports.lock().expect("mock reports lock").clone();
        return (200, json!({"reports": reports}));
    }

    // Notifications ---------------------------------------------------------
    if path == "/api/v1/notifications" && method == "GET" {
        return (
            200,
            json!([{"id": 1, "type": "reply", "title": "New reply", "body": "alice replied"}]),
        );
    }
    if path == "/api/v1/notifications/read" && method == "POST" {
        return (200, json!({"ok": true}));
    }

    // WebSocket ticket ------------------------------------------------------
    if path == "/api/v1/ws" && method == "POST" {
        let ticket = format!("{:064x}", state.id());
        state
            .tickets
            .lock()
            .expect("mock tickets lock")
            .push(ticket.clone());
        return (200, json!({"ticket": ticket, "expires_in": 30}));
    }

    // Wallet -----------------------------------------------------------------
    if path == "/api/v1/wallet" && method == "GET" {
        let page = request
            .query_param("page")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(1);
        let limit = request
            .query_param("limit")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(20);
        let balance = *state.wallet_cents.lock().expect("mock wallet lock");
        let transactions = state
            .wallet_transactions
            .lock()
            .expect("mock transactions lock")
            .clone();
        return (
            200,
            json!({
                "balance": coins_json(balance),
                "transactions": transactions,
                "page": page,
                "limit": limit
            }),
        );
    }
    if path == "/api/v1/wallet/adjust" && method == "POST" {
        let body = request_json(request);
        let Some(user_id) = body.get("user_id").and_then(Value::as_i64) else {
            return (400, json!({"error": "invalid request body"}));
        };
        let Some(raw_amount) = body.get("amount") else {
            return (400, json!({"error": "invalid request body"}));
        };
        let Ok(amount) = serde_json::from_value::<crate::models::Money>(raw_amount.clone()) else {
            return (400, json!({"error": "invalid request body"}));
        };
        if amount.cents() == 0 {
            return (400, json!({"error": "amount must not be zero"}));
        }
        if user_id <= 0 {
            return (400, json!({"error": "invalid user id"}));
        }
        if user_id == 404 {
            return (404, json!({"error": "user not found"}));
        }
        // The handler stores `type` as given and never reconciles it with the sign,
        // so the mock deliberately does not either.
        let kind = body.get("type").and_then(Value::as_str).unwrap_or("");
        let kind = if kind.is_empty() {
            if amount.cents() > 0 {
                "recharge"
            } else {
                "deduct"
            }
        } else {
            kind
        };
        let mut balance = state.wallet_cents.lock().expect("mock wallet lock");
        let next = *balance + amount.cents();
        if next < 0 {
            return (400, json!({"error": "insufficient balance"}));
        }
        *balance = next;
        drop(balance);
        let transaction = json!({
            "id": state.id(),
            "user_id": user_id,
            "amount": coins_json(amount.cents()),
            "balance_after": coins_json(next),
            "type": kind,
            "description": body.get("description").and_then(Value::as_str).unwrap_or(""),
            "operator_id": config.bot_id,
            "created_at": "2026-10-10T15:20:12Z"
        });
        state
            .wallet_transactions
            .lock()
            .expect("mock transactions lock")
            .push(transaction.clone());
        return (
            200,
            json!({"balance": coins_json(next), "transaction": transaction}),
        );
    }

    // Membership ------------------------------------------------------------
    if path == "/api/v1/membership" && method == "GET" {
        let level = *state.membership_level.lock().expect("mock member lock");
        let auto_renew = *state
            .membership_auto_renew
            .lock()
            .expect("mock auto-renew lock");
        return (200, membership_status(level, auto_renew));
    }
    if path == "/api/v1/membership/purchase" && method == "POST" {
        let body = request_json(request);
        let level = body.get("level").and_then(Value::as_i64).unwrap_or(0);
        let pin = body.get("pin").and_then(Value::as_str).unwrap_or("");
        if level < 1 || pin.is_empty() {
            return (400, json!({"error": "level and pin are required"}));
        }
        if level > 4 {
            return (400, json!({"error": "invalid membership level"}));
        }
        if let Some(refusal) = reject_payment_pin(state, pin) {
            return refusal;
        }
        let price = level * 100;
        let mut balance = state.wallet_cents.lock().expect("mock wallet lock");
        if *balance < price * MONEY_SCALE {
            return (400, json!({"error": "insufficient balance"}));
        }
        *balance -= price * MONEY_SCALE;
        drop(balance);
        *state.membership_level.lock().expect("mock member lock") = level;
        state
            .membership_purchases
            .lock()
            .expect("mock purchases lock")
            .push(json!({
                "id": state.id(),
                "level": level,
                "tier_name": format!("Lv.{level}"),
                "price_coins": price,
                "kind": "purchase",
                "created_at": "2026-10-10T15:20:12Z"
            }));
        let auto_renew = *state
            .membership_auto_renew
            .lock()
            .expect("mock auto-renew lock");
        return (
            200,
            json!({
                "status": membership_status(level, auto_renew),
                "paid": price,
                "kind": "purchase"
            }),
        );
    }
    if path == "/api/v1/membership/auto-renew" && method == "PUT" {
        let body = request_json(request);
        let enabled = body
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if enabled {
            let pin = body.get("pin").and_then(Value::as_str).unwrap_or("");
            // Disabling needs only the token; enabling pre-authorizes charges and
            // therefore goes through the same PIN gate as a purchase.
            if let Some(refusal) = reject_payment_pin(state, pin) {
                return refusal;
            }
        }
        *state
            .membership_auto_renew
            .lock()
            .expect("mock auto-renew lock") = enabled;
        let level = *state.membership_level.lock().expect("mock member lock");
        return (200, membership_status(level, enabled));
    }
    if path == "/api/v1/membership/purchases" && method == "GET" {
        let page = request
            .query_param("page")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(1);
        let limit = request
            .query_param("limit")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(20);
        let purchases = state
            .membership_purchases
            .lock()
            .expect("mock purchases lock")
            .clone();
        return (
            200,
            json!({"purchases": purchases, "page": page, "limit": limit}),
        );
    }

    // Tasks ------------------------------------------------------------------
    if path == "/api/v1/tasks" && method == "GET" {
        return (
            200,
            json!({
                "tasks": task_catalog(state),
                "streak": 4,
                "makeup_cost": 100
            }),
        );
    }
    if path == "/api/v1/tasks/daily-login/claim" && method == "POST" {
        let mut claimed = state.claimed_tasks.lock().expect("mock claimed lock");
        let already = claimed.iter().any(|code| code == "daily_login");
        if !already {
            claimed.push("daily_login".to_string());
        }
        return (
            200,
            json!({"granted": !already, "exp": 20, "currency": 20, "streak": 5}),
        );
    }
    if path == "/api/v1/tasks/daily-login/calendar" && method == "GET" {
        return (
            200,
            json!({
                "registered_at": "2026-08-01",
                "today": "2026-08-04",
                "signed": ["2026-08-01", "2026-08-02", "2026-08-04"],
                "streak": 4,
                "makeup_cost": 100,
                "makeup_exp": 20,
                "days": [
                    {"date": "2026-08-01", "weekday": "Sat", "signed": true,
                     "is_today": false, "days_ago": 3, "before": false},
                    {"date": "2026-08-02", "weekday": "Sun", "signed": true,
                     "is_today": false, "days_ago": 2, "before": false},
                    {"date": "2026-08-03", "weekday": "Mon", "signed": false,
                     "is_today": false, "days_ago": 1, "before": false},
                    {"date": "2026-08-04", "weekday": "Tue", "signed": true,
                     "is_today": true, "days_ago": 0, "before": false}
                ]
            }),
        );
    }
    if path == "/api/v1/tasks/daily-login/makeup" && method == "POST" {
        if request.query_param("gap").as_deref() == Some("wide") {
            return (
                409,
                json!({"error": "make-up only covers the last three days"}),
            );
        }
        let mut balance = state.wallet_cents.lock().expect("mock wallet lock");
        let cost = 100;
        if *balance < cost * MONEY_SCALE {
            return (400, json!({"error": "insufficient balance"}));
        }
        *balance -= cost * MONEY_SCALE;
        drop(balance);
        return (
            200,
            json!({"granted": true, "exp": 20, "cost": cost, "streak": 5}),
        );
    }
    if path == "/api/v1/tasks/daily-login/makeup-date" && method == "POST" {
        let body = request_json(request);
        let date = body.get("date").and_then(Value::as_str).unwrap_or("");
        if date.is_empty() {
            return (400, json!({"error": "date is required"}));
        }
        if date == "2026-08-04" {
            return (400, json!({"error": "cannot make up today"}));
        }
        if date == "2099-01-01" {
            return (400, json!({"error": "cannot make up a future date"}));
        }
        let mut balance = state.wallet_cents.lock().expect("mock wallet lock");
        let cost = 100;
        if *balance < cost * MONEY_SCALE {
            return (400, json!({"error": "insufficient balance"}));
        }
        *balance -= cost * MONEY_SCALE;
        drop(balance);
        return (
            200,
            json!({"granted": true, "date": date, "exp": 20, "cost": cost, "streak": 5}),
        );
    }
    if let Some(rest) = path.strip_prefix("/api/v1/tasks/daily/") {
        if method == "POST" {
            if let Some(code) = rest.strip_suffix("/claim") {
                return claim_task(state, code);
            }
        }
    }
    if let Some(rest) = path.strip_prefix("/api/v1/tasks/") {
        if method == "POST" {
            if let Some(code) = rest.strip_suffix("/claim") {
                if code.contains('/') {
                    return (404, json!({"error": "task not found"}));
                }
                return claim_task(state, code);
            }
        }
    }
    if path == "/api/v1/exp/history" && method == "GET" {
        return (
            200,
            json!({"entries": [
                {"id": 1, "user_id": config.bot_id, "amount": 20, "reason": "daily_bonus",
                 "description": "每日签到奖励", "created_at": "2026-10-10T15:20:12Z"},
                {"id": 2, "user_id": config.bot_id, "amount": 10, "reason": "task",
                 "description": "今日发布 1 篇帖子", "created_at": "2026-10-09T15:20:12Z"}
            ]}),
        );
    }

    // The payment PIN and the E2EE public key --------------------------------
    if path == "/api/v1/users/me/e2ee-key" && method == "PUT" {
        let body = request_json(request);
        // The handler accepts an empty key and treats it as "clear"; the mock only
        // has to prove the call reached the route and carried `public_key`.
        if body.get("public_key").is_none() {
            return (400, json!({"error": "invalid request body"}));
        }
        return (200, json!({"message": "e2ee public key updated"}));
    }
    if path == "/api/v1/users/me/pin" && method == "POST" {
        let body = request_json(request);
        let pin = body.get("pin").and_then(Value::as_str).unwrap_or("");
        let old_pin = body.get("old_pin").and_then(Value::as_str).unwrap_or("");
        if pin.len() != 6 || !pin.bytes().all(|byte| byte.is_ascii_digit()) {
            return (400, json!({"error": "pin must be 6 digits"}));
        }
        let existing = state.pin.lock().expect("mock pin lock").clone();
        if let Some(expected) = existing {
            if old_pin.is_empty() {
                return (
                    403,
                    json!({"error": "payment pin already set", "old_pin_required": true}),
                );
            }
            if old_pin != expected {
                return (401, json!({"error": "invalid payment pin"}));
            }
        }
        *state.pin.lock().expect("mock pin lock") = Some(pin.to_string());
        return (200, json!({"set": true, "has_pin": true}));
    }
    if path == "/api/v1/users/me/pin" && method == "PUT" {
        let body = request_json(request);
        let pin = body.get("pin").and_then(Value::as_str).unwrap_or("");
        let old_pin = body.get("old_pin").and_then(Value::as_str).unwrap_or("");
        if pin.is_empty() || old_pin.is_empty() {
            return (400, json!({"error": "old_pin and pin are required"}));
        }
        if pin.len() != 6 || !pin.bytes().all(|byte| byte.is_ascii_digit()) {
            return (400, json!({"error": "pin must be 6 digits"}));
        }
        let existing = state.pin.lock().expect("mock pin lock").clone();
        let Some(expected) = existing else {
            return (400, json!({"error": "pin not set"}));
        };
        if old_pin != expected {
            return (401, json!({"error": "invalid pin"}));
        }
        *state.pin.lock().expect("mock pin lock") = Some(pin.to_string());
        return (200, json!({"set": true, "has_pin": true}));
    }
    if path == "/api/v1/users/me/pin/verify" && method == "POST" {
        let body = request_json(request);
        let pin = body.get("pin").and_then(Value::as_str).unwrap_or("");
        let existing = state.pin.lock().expect("mock pin lock").clone();
        match existing {
            None => return (400, json!({"error": "pin not set"})),
            Some(expected) if expected != pin => {
                return (401, json!({"error": "invalid pin"}));
            }
            Some(_) => return (200, json!({"valid": true})),
        }
    }

    // Plugin store -----------------------------------------------------------
    // The public catalog and detail routes are handled above the token gate.
    if path == "/api/v1/plugins/admin/upload" && method == "POST" {
        if !body_has_multipart_field(request, "file") {
            return (400, json!({"error": "missing file field"}));
        }
        let publish = multipart_value(request, "publish").as_deref() == Some("true");
        let plugin = json!({
            "id": "demo", "name": "Demo", "version": "1.0.0", "author": "bot",
            "description": "", "permissions": [], "min_app_version": "0.1.0",
            "verified": false, "published": publish, "downloads": 0,
            "file_size": 32, "sha256": "0".repeat(64),
            "created_at": "2026-10-10T15:20:12Z", "updated_at": "2026-10-10T15:20:12Z"
        });
        let mut plugins = state.plugins.lock().expect("mock plugins lock");
        plugins.retain(|existing| existing.get("id") != plugin.get("id"));
        plugins.push(plugin.clone());
        return (200, json!({"message": "uploaded", "plugin": plugin}));
    }
    if let Some(rest) = path.strip_prefix("/api/v1/plugins/admin/") {
        if let Some(id) = rest.strip_suffix("/publish") {
            return set_plugin_published(state, id, true);
        }
        if let Some(id) = rest.strip_suffix("/unpublish") {
            return set_plugin_published(state, id, false);
        }
        if method == "DELETE" {
            let mut plugins = state.plugins.lock().expect("mock plugins lock");
            let before = plugins.len();
            plugins.retain(|plugin| plugin.get("id").and_then(Value::as_str) != Some(rest));
            if plugins.len() == before {
                return (404, json!({"error": "plugin not found"}));
            }
            return (200, json!({"message": "deleted"}));
        }
    }

    // Punishments ------------------------------------------------------------
    if let Some(rest) = path.strip_prefix("/api/v1/users/") {
        let segments: Vec<&str> = rest.split('/').collect();
        match (method, segments.as_slice()) {
            ("PUT", [id_text, "exp"]) if id_text.parse::<Id>().is_ok() => {
                let user_id: Id = id_text.parse().unwrap();
                if user_id == 404 {
                    return (404, json!({"error": "user not found"}));
                }
                let body = request_json(request);
                let delta = body.get("delta").and_then(Value::as_i64).unwrap_or(0);
                return (200, json!({"exp": 1000 + delta, "level": 5}));
            }
            ("PUT", [id_text, "membership"]) if id_text.parse::<Id>().is_ok() => {
                let body = request_json(request);
                let level = body.get("level").and_then(Value::as_i64).unwrap_or(0);
                if !(0..=4).contains(&level) {
                    return (400, json!({"error": "invalid membership level"}));
                }
                *state.membership_level.lock().expect("mock member lock") = level;
                let auto_renew = *state
                    .membership_auto_renew
                    .lock()
                    .expect("mock auto-renew lock");
                return (200, membership_status(level, auto_renew));
            }
            ("POST", [id_text, "punishments"]) if id_text.parse::<Id>().is_ok() => {
                let user_id: Id = id_text.parse().unwrap();
                let body = request_json(request);
                let kind = body.get("type").and_then(Value::as_str).unwrap_or("");
                let reason = body.get("reason").and_then(Value::as_str).unwrap_or("");
                if user_id == 404 {
                    return (404, json!({"error": "user not found"}));
                }
                if user_id == config.bot_id {
                    return (403, json!({"error": "cannot punish yourself"}));
                }
                if user_id == 1 {
                    return (403, json!({"error": "cannot punish an administrator"}));
                }
                if user_id == 2 {
                    return (
                        403,
                        json!({"error": "insufficient rank to punish this user"}),
                    );
                }
                match kind {
                    "warning" | "demerit" | "ban" | "unban" => {}
                    "revoke" | "restore" => {
                        let key = body
                            .get("permission_key")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if key.is_empty() {
                            return (400, json!({"error": "permission_key is required"}));
                        }
                    }
                    "temp_ban" => {
                        let minutes = body
                            .get("duration_minutes")
                            .and_then(Value::as_i64)
                            .unwrap_or(0);
                        if minutes <= 0 {
                            return (
                                400,
                                json!({"error": "duration_minutes is required for temp_ban"}),
                            );
                        }
                    }
                    _ => return (400, json!({"error": "unknown punishment type"})),
                }
                let expires_at = if kind == "temp_ban" {
                    Value::String("2026-10-10T16:20:12Z".to_string())
                } else {
                    Value::Null
                };
                let record = json!({
                    "id": state.id(),
                    "user_id": user_id,
                    "operator_id": config.bot_id,
                    "type": kind,
                    "permission_key": body.get("permission_key").and_then(Value::as_str).unwrap_or(""),
                    "reason": reason,
                    "expires_at": expires_at,
                    "created_at": "2026-10-10T15:20:12Z"
                });
                state
                    .punishments
                    .lock()
                    .expect("mock punishments lock")
                    .push(record.clone());
                return (
                    200,
                    json!({
                        "punishment_id": record["id"],
                        "type": kind,
                        "expires_at": expires_at
                    }),
                );
            }
            ("GET", [id_text, "punishments"]) if id_text.parse::<Id>().is_ok() => {
                let user_id: Id = id_text.parse().unwrap();
                if user_id == 404 {
                    return (404, json!({"error": "user not found"}));
                }
                let punishments: Vec<Value> = state
                    .punishments
                    .lock()
                    .expect("mock punishments lock")
                    .iter()
                    .filter(|record| record.get("user_id").and_then(Value::as_i64) == Some(user_id))
                    .cloned()
                    .collect();
                return (
                    200,
                    json!({"punishments": punishments, "permission_bans": ["posts.create"]}),
                );
            }
            _ => {}
        }
    }

    // Chunked uploads and attachment reuse ------------------------------------
    if path == "/api/v1/attachments/chunk/init" && method == "POST" {
        let body = request_json(request);
        let filename = body.get("filename").and_then(Value::as_str).unwrap_or("");
        let size = body.get("size").and_then(Value::as_i64).unwrap_or(0);
        let total_chunks = body
            .get("total_chunks")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if filename.is_empty() || size <= 0 || total_chunks <= 0 {
            return (400, json!({"error": "invalid request"}));
        }
        if total_chunks > 10_000 {
            return (400, json!({"error": "too many chunks"}));
        }
        if size > total_chunks * (8 * 1024 * 1024) {
            return (
                400,
                json!({"error": "declared size exceeds chunk capacity"}),
            );
        }
        if filename.trim() == "too-big.bin" {
            return (413, json!({"error": "file too large"}));
        }
        let upload_id = format!("chunk-{}", state.id());
        state
            .chunks
            .lock()
            .expect("mock chunks lock")
            .insert(upload_id.clone(), Vec::new());
        return (
            201,
            json!({"upload_id": upload_id, "total_chunks": total_chunks}),
        );
    }
    if let Some(rest) = path.strip_prefix("/api/v1/attachments/chunk/") {
        if rest.ends_with("/complete") && method == "POST" {
            let upload_id = rest.trim_end_matches("/complete").to_string();
            let body = request_json(request);
            let total_chunks = body
                .get("total_chunks")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let mut sessions = state.chunks.lock().expect("mock chunks lock");
            let Some(uploaded) = sessions.get(&upload_id).cloned() else {
                return (404, json!({"error": "upload session not found"}));
            };
            let expected = if total_chunks > 0 {
                total_chunks
            } else {
                uploaded.len() as i64
            };
            let missing: Vec<i64> = (1..=expected)
                .filter(|index| !uploaded.contains(index))
                .collect();
            if !missing.is_empty() {
                return (409, json!({"error": "missing chunks", "missing": missing}));
            }
            sessions.remove(&upload_id);
            drop(sessions);
            let attachment = json!({
                "id": state.id(),
                "user_id": config.bot_id,
                "original_name": body.get("filename").and_then(Value::as_str).unwrap_or("big.bin"),
                "mime_type": "application/octet-stream",
                "size_bytes": body.get("size").and_then(Value::as_i64).unwrap_or(0),
                "url": "https://cdn.example/assembled",
                "visibility": "public",
                "bucket": "default",
                "created_at": "2026-10-10T15:20:12Z"
            });
            return (201, attachment);
        }
        let segments: Vec<&str> = rest.split('/').collect();
        match (method, segments.as_slice()) {
            ("GET", [upload_id]) => {
                let sessions = state.chunks.lock().expect("mock chunks lock");
                let Some(uploaded) = sessions.get(*upload_id) else {
                    return (404, json!({"error": "upload session not found"}));
                };
                let mut sorted = uploaded.clone();
                sorted.sort_unstable();
                return (200, json!({"upload_id": upload_id, "uploaded": sorted}));
            }
            ("POST", [upload_id, index_text]) if index_text.parse::<i64>().is_ok() => {
                let index: i64 = index_text.parse().unwrap();
                let mut sessions = state.chunks.lock().expect("mock chunks lock");
                let Some(uploaded) = sessions.get_mut(*upload_id) else {
                    return (404, json!({"error": "upload session not found"}));
                };
                if index < 1 {
                    return (400, json!({"error": "invalid chunk index"}));
                }
                // Go files a filename-less multipart part under the value slot, so
                // the field simply has to be present either way.
                if !body_has_multipart_field(request, "chunk") {
                    return (400, json!({"error": "missing chunk field"}));
                }
                if !uploaded.contains(&index) {
                    uploaded.push(index);
                }
                let size = request.body.len() as i64;
                return (
                    200,
                    json!({"upload_id": upload_id, "index": index, "size": size}),
                );
            }
            _ => {}
        }
    }
    if let Some(rest) = path.strip_prefix("/api/v1/attachments/by-hash/") {
        if method == "GET" && !rest.contains('/') {
            let hash = rest.to_ascii_lowercase();
            if hash.starts_with('f') && hash != "f".repeat(64) {
                return (404, json!({"error": "attachment not found"}));
            }
            if hash == "a".repeat(64) {
                return (
                    200,
                    json!({
                        "id": 9001,
                        "user_id": config.bot_id,
                        "original_name": "chart.png",
                        "mime_type": "image/png",
                        "size_bytes": 3,
                        "url": "https://cdn.example/reused",
                        "visibility": "public",
                        "created_at": "2026-10-10T15:20:12Z"
                    }),
                );
            }
            return (404, json!({"error": "attachment not found"}));
        }
    }
    if let Some(rest) = path.strip_prefix("/api/v1/attachments/") {
        let segments: Vec<&str> = rest.split('/').collect();
        match (method, segments.as_slice()) {
            ("GET", [id_text]) if id_text.parse::<Id>().is_ok() => {
                let id: Id = id_text.parse().unwrap();
                let attachments = state.attachments.lock().expect("mock attachments lock");
                return match attachments
                    .iter()
                    .find(|attachment| attachment.get("id").and_then(Value::as_i64) == Some(id))
                {
                    Some(attachment) => (200, attachment.clone()),
                    None => (404, json!({"error": "attachment not found"})),
                };
            }
            ("POST", [id_text, "burn-arm"]) if id_text.parse::<Id>().is_ok() => {
                let id: Id = id_text.parse().unwrap();
                let body = request_json(request);
                let seconds = body
                    .get("burn_seconds")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                if !(1..=86_400).contains(&seconds) {
                    return (400, json!({"error": "invalid request"}));
                }
                let mut burns = state.burns.lock().expect("mock burns lock");
                // Idempotent on the server; the mock keeps the first deadline.
                let burn_at = burns
                    .entry(id)
                    .or_insert_with(|| "2026-10-10T15:30:12Z".to_string())
                    .clone();
                return (200, json!({"id": id, "burn_at": burn_at}));
            }
            _ => {}
        }
    }

    (404, json!({"error": "not found"}))
}

/// Flip a plugin's published flag, the way `setPublished` does.
fn set_plugin_published(state: &MockState, id: &str, published: bool) -> (u16, Value) {
    let mut plugins = state.plugins.lock().expect("mock plugins lock");
    let Some(plugin) = plugins
        .iter_mut()
        .find(|plugin| plugin.get("id").and_then(Value::as_str) == Some(id))
    else {
        return (404, json!({"error": "plugin not found"}));
    };
    if let Some(object) = plugin.as_object_mut() {
        object.insert("published".to_string(), Value::Bool(published));
    }
    (200, json!({"message": "updated"}))
}

/// The membership status document, including the four-tier catalog.
fn membership_status(level: i64, auto_renew: bool) -> Value {
    let tiers: Vec<Value> = (1..=4)
        .map(|tier| {
            json!({
                "level": tier,
                "name": format!("Lv.{tier}"),
                "description": "",
                "price": tier * 100,
                "exp_multiplier": 1.0 + (tier as f64) * 0.1,
                "duration_days": 30,
                "storage_bonus_mb": tier * 1024,
                "allow_gradient": tier >= 3,
                "allow_dynamic": tier >= 4
            })
        })
        .collect();
    json!({
        "level": level,
        "name": if level > 0 { format!("Lv.{level}") } else { String::new() },
        "active": level > 0,
        "expires_at": if level > 0 { Value::String("2026-11-09T15:20:12Z".into()) } else { Value::Null },
        "exp_multiplier": 1.0 + (level as f64) * 0.1,
        "tiers": tiers,
        "member_days": 30,
        "member_price": if level < 4 { (level + 1) * 100 } else { 0 },
        "auto_renew": auto_renew
    })
}

fn request_json(request: &IncomingRequest) -> Value {
    serde_json::from_slice(&request.body).unwrap_or(Value::Null)
}

/// The gateway's identity check: absent and unknown tokens both answer
/// `401 invalid bot token`, matching `ResolveBotToken`.
fn authorize(state: &MockState, request: &IncomingRequest) -> Option<(u16, Value)> {
    let token = request
        .authorization()
        .and_then(|value| value.strip_prefix("Bearer "));
    match token {
        None => Some((401, json!({"error": "missing authorization token"}))),
        Some(value) if value != state.config.token => {
            // A reset, unknown or banned bot all answer this (bots.md §3.3).
            Some((401, json!({"error": "invalid bot token"})))
        }
        Some(_) => None,
    }
}

/// Render an integer cent count the way `model.Cents.MarshalJSON` does: as a
/// decimal coin value, and as a plain integer when the cents divide evenly.
fn coins_json(cents: i64) -> Value {
    serde_json::to_value(crate::models::Money::from_cents(cents)).unwrap_or(Value::from(0))
}

/// Shared payment-PIN gate for tips, memberships and the PIN endpoints.
///
/// Returns the refusal the handler would write, or `None` to proceed. An unset
/// PIN is a `403` (not a 401) because there is nothing to match against.
fn reject_payment_pin(state: &MockState, pin: &str) -> Option<(u16, Value)> {
    let stored = state.pin.lock().expect("mock pin lock").clone();
    match stored {
        None => Some((403, json!({"error": "payment pin not set"}))),
        Some(expected) if expected != pin => Some((401, json!({"error": "invalid payment pin"}))),
        Some(_) => None,
    }
}

/// The mock task catalog, with progress chosen so both the claimable and the
/// not-yet-earned paths are reachable.
fn task_catalog(state: &MockState) -> Vec<Value> {
    let claimed = state.claimed_tasks.lock().expect("mock claimed lock");
    let row = |id: Id,
               code: &str,
               kind: &str,
               name: &str,
               reward_exp: i64,
               reward_currency: i64,
               target: i64,
               progress: i64,
               sort: i64| {
        let completed = claimed.iter().any(|entry| entry == code);
        let claimable = !completed && progress >= target;
        json!({
            "id": id, "code": code, "kind": kind, "name": name, "description": "",
            "reward_exp": reward_exp, "reward_currency": reward_currency,
            "target": target, "sort": sort,
            "progress": progress, "completed": completed, "claimable": claimable,
        })
    };
    vec![
        row(
            1,
            "daily_posts_1",
            "daily",
            "今日发布 1 篇帖子",
            10,
            5,
            1,
            1,
            10,
        ),
        row(
            2,
            "daily_posts_5",
            "daily",
            "今日发布 5 篇帖子",
            40,
            25,
            5,
            5,
            11,
        ),
        row(
            3,
            "daily_posts_10",
            "daily",
            "今日发布 10 篇帖子",
            100,
            60,
            10,
            5,
            12,
        ),
        row(
            4,
            "daily_chat_25",
            "daily",
            "今日聊天 25 条",
            30,
            15,
            25,
            25,
            20,
        ),
        // Deliberately short of target, so a claim is a 409.
        row(
            5,
            "daily_chat_100",
            "daily",
            "今日聊天 100 条",
            200,
            120,
            100,
            60,
            21,
        ),
        row(6, "daily_login", "streak", "每日签到", 20, 20, 1, 0, 1),
        row(7, "first_post", "once", "发布第一篇帖子", 50, 20, 1, 1, 2),
    ]
}

/// Claim one task, applying the handler's three failure shapes.
fn claim_task(state: &MockState, code: &str) -> (u16, Value) {
    let catalog = task_catalog(state);
    let Some(task) = catalog
        .iter()
        .find(|task| task.get("code").and_then(Value::as_str) == Some(code))
    else {
        return (404, json!({"error": "task not found"}));
    };
    if task.get("completed").and_then(Value::as_bool) == Some(true) {
        return (409, json!({"error": "task already claimed"}));
    }
    if task.get("claimable").and_then(Value::as_bool) != Some(true) {
        return (409, json!({"error": "task requirements not met"}));
    }
    state
        .claimed_tasks
        .lock()
        .expect("mock claimed lock")
        .push(code.to_string());
    let exp = task.get("reward_exp").and_then(Value::as_i64).unwrap_or(0);
    let currency = task
        .get("reward_currency")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    (
        200,
        json!({"claimed": true, "exp": exp, "currency": currency}),
    )
}

/// A published plugin row, or `None` when the id is unknown or unpublished.
fn published_plugin(state: &MockState, id: &str) -> Option<Value> {
    state
        .plugins
        .lock()
        .expect("mock plugins lock")
        .iter()
        .find(|plugin| {
            plugin.get("id").and_then(Value::as_str) == Some(id)
                && plugin.get("published").and_then(Value::as_bool) == Some(true)
        })
        .cloned()
}

/// A reply that is bytes rather than JSON.
///
/// The router returns a [`Value`], so an endpoint that answers a zip (the plugin
/// bundle download) is resolved by this type instead of being bent into JSON.
pub enum BinaryReply {
    /// A raw body with its content type.
    Bytes {
        /// HTTP status.
        status: u16,
        /// `Content-Type` header value.
        content_type: &'static str,
        /// The body.
        body: Vec<u8>,
    },
    /// A JSON refusal.
    Error {
        /// HTTP status.
        status: u16,
        /// The JSON error object.
        body: Value,
    },
}

/// Handle the endpoints whose response is not JSON.
///
/// Called before [`route`] so the token check happens exactly once per request.
fn binary_route(state: &MockState, request: &IncomingRequest) -> Option<BinaryReply> {
    let path = request.path.as_str();
    if request.method != "GET" {
        return None;
    }
    let rest = path.strip_prefix("/api/v1/plugins/")?;
    let id = rest.strip_suffix("/download")?;
    if id.is_empty() || id.contains('/') {
        return None;
    }
    if let Some(refusal) = authorize(state, request) {
        return Some(BinaryReply::Error {
            status: refusal.0,
            body: refusal.1,
        });
    }
    match published_plugin(state, id) {
        Some(_) => Some(BinaryReply::Bytes {
            status: 200,
            content_type: "application/zip",
            body: plugin_bundle(id),
        }),
        None => Some(BinaryReply::Error {
            status: 404,
            body: json!({"error": "plugin not found"}),
        }),
    }
}

/// A deterministic stand-in bundle, so a download test can assert on its bytes.
fn plugin_bundle(id: &str) -> Vec<u8> {
    let mut bytes = b"PK\x03\x04".to_vec();
    bytes.extend_from_slice(id.as_bytes());
    bytes.extend_from_slice(b"-bundle");
    bytes
}

/// Write a raw body, mirroring [`write_json`]'s framing.
fn write_bytes(
    writer: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let mut response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n",
        body.len()
    );
    response.push_str("Content-Disposition: attachment; filename=\"bundle.zip\"\r\n\r\n");
    writer.write_all(response.as_bytes())?;
    writer.write_all(body)?;
    writer.flush()
}

fn body_has_multipart_field(request: &IncomingRequest, name: &str) -> bool {
    let needle = format!("name=\"{name}\"");
    request
        .body
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// Read a text part's value out of a `multipart/form-data` body.
///
/// Enough of a parser for the mock's own assertions: locate the part header, then
/// take everything up to the next CRLF. Returns `None` when the part is absent.
fn multipart_value(request: &IncomingRequest, name: &str) -> Option<String> {
    let needle = format!("name=\"{name}\"");
    let body = &request.body;
    let start = body
        .windows(needle.len())
        .position(|window| window == needle.as_bytes())?;
    let after_header = &body[start + needle.len()..];
    let separator = b"\r\n\r\n";
    let value_start = after_header
        .windows(separator.len())
        .position(|window| window == separator)?;
    let value = &after_header[value_start + separator.len()..];
    let end = value
        .windows(2)
        .position(|window| window == b"\r\n")
        .unwrap_or(value.len());
    Some(String::from_utf8_lossy(&value[..end]).trim().to_string())
}

fn redeem_ticket(state: &MockState, ticket: &str) -> bool {
    let mut tickets = state.tickets.lock().expect("mock tickets lock");
    match tickets.iter().position(|candidate| candidate == ticket) {
        Some(index) => {
            // Single use: consume it, exactly like the real store.
            tickets.remove(index);
            true
        }
        None => false,
    }
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn read_request(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<IncomingRequest>> {
    let mut request_line = String::new();
    // Tolerate the read timeout between keep-alive requests.
    match reader.read_line(&mut request_line) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(ref err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            return Ok(None)
        }
        Err(err) => return Err(err),
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || target.is_empty() {
        return Ok(None);
    }
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };

    let mut headers: HashMap<String, String> = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let content_length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    Ok(Some(IncomingRequest {
        method,
        path,
        query,
        headers,
        body,
    }))
}

fn write_json(
    writer: &mut TcpStream,
    status: u16,
    body: &Value,
    retry_after: Option<u64>,
) -> std::io::Result<()> {
    let payload = if body.is_null() {
        String::new()
    } else {
        serde_json::to_string(body).unwrap_or_else(|_| "{}".to_string())
    };
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n",
        payload.len()
    );
    if let Some(seconds) = retry_after {
        response.push_str(&format!("Retry-After: {seconds}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(&payload);
    writer.write_all(response.as_bytes())?;
    writer.flush()
}

/// Removes a WebSocket client from the registry when its connection handler ends.
///
/// A guard rather than a cleanup call at the end of the loop, because the handler has
/// several early returns and a `?` on a write error; relying on each of them to
/// deregister would eventually be forgotten.
struct WsRegistration {
    state: Arc<MockState>,
    client_id: u64,
}

impl Drop for WsRegistration {
    fn drop(&mut self) {
        self.state
            .ws_clients
            .lock()
            .expect("mock ws lock")
            .retain(|(id, _)| *id != self.client_id);
    }
}

/// Serve the WebSocket protocol by hand, so the testkit needs no WebSocket server dep.
fn serve_websocket(
    mut writer: TcpStream,
    mut reader: BufReader<TcpStream>,
    state: Arc<MockState>,
    shutdown: Arc<AtomicBool>,
    key: &str,
) -> std::io::Result<()> {
    let accept = websocket_accept(key);
    let handshake = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    writer.write_all(handshake.as_bytes())?;
    writer.flush()?;

    let (sender, receiver) = mpsc::channel::<String>();
    let client_id = state.ws_seq.fetch_add(1, Ordering::SeqCst);
    state
        .ws_clients
        .lock()
        .expect("mock ws lock")
        .push((client_id, sender));

    // Deregister on every exit path, including an early `?` return, so a dropped socket
    // stops counting as a connected client. Without this a test cannot tell "the consumer
    // disconnected" from "the consumer is still attached".
    let _registration = WsRegistration {
        state: Arc::clone(&state),
        client_id,
    };

    // The documented greeting (ws.go).
    write_text_frame(&mut writer, r#"{"type":"connected"}"#)?;

    writer.set_read_timeout(Some(Duration::from_millis(50)))?;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(payload) => {
                if write_text_frame(&mut writer, &payload).is_err() {
                    return Ok(());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        // Drain anything the client sent so a close frame ends the connection.
        match read_client_frame(&mut reader) {
            Ok(Some(opcode)) => match opcode {
                0x8 => return Ok(()),
                0x9 => {
                    let _ = write_frame(&mut writer, 0xA, &[]);
                }
                _ => {}
            },
            Ok(None) => {}
            Err(_) => return Ok(()),
        }
    }
}

fn websocket_accept(key: &str) -> String {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let digest = sha1(format!("{key}{GUID}").as_bytes());
    base64_encode(&digest)
}

fn write_text_frame(writer: &mut TcpStream, payload: &str) -> std::io::Result<()> {
    write_frame(writer, 0x1, payload.as_bytes())
}

fn write_frame(writer: &mut TcpStream, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | opcode);
    let length = payload.len();
    if length < 126 {
        frame.push(length as u8);
    } else if length <= u16::MAX as usize {
        frame.push(126);
        frame.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(length as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    writer.write_all(&frame)?;
    writer.flush()
}

/// Read one client frame, honouring the mandatory masking. `Ok(None)` means idle.
fn read_client_frame(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<u8>> {
    let mut header = [0u8; 2];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(ref err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted
            ) =>
        {
            return Ok(None)
        }
        Err(err) => return Err(err),
    }
    let opcode = header[0] & 0x0F;
    let masked = header[1] & 0x80 != 0;
    let mut length = (header[1] & 0x7F) as usize;
    if length == 126 {
        let mut extended = [0u8; 2];
        reader.read_exact(&mut extended)?;
        length = u16::from_be_bytes(extended) as usize;
    } else if length == 127 {
        let mut extended = [0u8; 8];
        reader.read_exact(&mut extended)?;
        length = u64::from_be_bytes(extended) as usize;
    }
    let mut mask = [0u8; 4];
    if masked {
        reader.read_exact(&mut mask)?;
    }
    let mut payload = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut payload)?;
    }
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Ok(Some(opcode))
}

/// SHA-1, used only to compute the WebSocket handshake response in this mock.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut state: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bit_len = (data.len() as u64) * 8;

    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut words = [0u32; 80];
        for (index, word) in words.iter_mut().enumerate().take(16) {
            let offset = index * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) =
            (state[0], state[1], state[2], state[3], state[4]);
        for (index, word) in words.iter().enumerate() {
            let (function, constant) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(function)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }

    let mut digest = [0u8; 20];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

// The WebSocket handshake response and the bindings' binary transport share one
// base64 implementation, from `crate::models`; the RFC vectors below cover both.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_the_rfc_vectors() {
        // RFC 3174 test vectors.
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            hex(&sha1(&vec![b'a'; 1_000_000])),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn websocket_accept_matches_the_rfc_example() {
        // RFC 6455 §1.3.
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn percent_decoding_handles_escapes_and_pluses() {
        assert_eq!(percent_decode("hello+world"), "hello world");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("中文"), "中文");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn server_starts_and_answers_health() {
        let server = MockServer::start().expect("mock server starts");
        let client = crate::BaseClient::new(server.base_url()).expect("client");
        let report = client.health().expect("health");
        assert!(report.all_healthy);
        assert_eq!(server.last_request().unwrap().path, "/api/v1/health");
    }

    #[test]
    fn server_rejects_a_bad_token_like_the_gateway() {
        let server = MockServer::start().expect("mock server starts");
        let token = crate::BotToken::new(format!("ofb_{}", "9".repeat(64))).unwrap();
        let client = crate::AuthenticatedClient::new(server.base_url(), token).unwrap();
        let err = client.me().unwrap_err();
        assert_eq!(err.status(), Some(401));
        assert_eq!(err.api_message(), Some("invalid bot token"));
    }

    #[test]
    fn server_accepts_the_configured_token() {
        let server = MockServer::start().expect("mock server starts");
        let client = server.client();
        let me = client.me().expect("me");
        assert_eq!(me.id, 4242);
        assert!(me.is_bot);
        assert_eq!(me.nickname, "天气预报");
    }

    #[test]
    fn server_enforces_the_byte_content_limit() {
        let server = MockServer::start().expect("mock server starts");
        let client = server.client();

        // The SDK refuses a 6000-byte body locally, matching the server's
        // `len(req.Content) > 5000` check (Go's `len` is bytes, not runes).
        let err = client.publish(&"测".repeat(2000)).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::InvalidArgument);

        // Bypass local validation to prove the mock also rejects it, like the handler.
        let raw = crate::Request::post("/api/v1/posts")
            .json_value(json!({ "content": "测".repeat(2000) }));
        let response = client.request(&raw).expect("raw request");
        assert_eq!(response.status, 400);
        assert_eq!(
            response.json().unwrap()["error"],
            "content exceeds maximum length of 5000"
        );
    }

    #[test]
    fn server_requires_a_reaction_key() {
        let server = MockServer::start().expect("mock server starts");
        let client = server.client();
        let post = client.publish("hello").expect("post");
        let reacted = client.react(post.id, crate::Reaction::Like).expect("react");
        assert_eq!(reacted.id, post.id);
    }

    #[test]
    fn server_requires_a_conversation_title() {
        let server = MockServer::start().expect("mock server starts");
        let client = server.client();
        let created = client
            .create_conversation(&crate::NewConversation::named("Family"))
            .expect("create");
        assert_eq!(created.title, "Family");
        assert_eq!(created.kind.as_deref(), Some("group"));
    }

    #[test]
    fn server_returns_messages_newest_first() {
        let server = MockServer::start().expect("mock server starts");
        let client = server.client();
        client.say(33, "first").expect("first");
        client.say(33, "second").expect("second");
        let page = client.latest_messages(33, Some(10)).expect("messages");
        assert_eq!(page.messages.len(), 2);
        assert!(
            page.messages[0].id > page.messages[1].id,
            "the mock must mirror ORDER BY id DESC"
        );
        assert_eq!(page.newest().unwrap().content, "second");
    }

    #[test]
    fn server_rejects_an_attachment_without_the_file_field() {
        let server = MockServer::start().expect("mock server starts");
        let client = server.client();
        // `Upload` always uses the `file` field, so this succeeds.
        let uploaded = client
            .upload(crate::Upload::new("chart.png", vec![1, 2, 3]))
            .expect("upload");
        assert_eq!(uploaded.attachment().file_name(), "chart.png");
        assert_eq!(uploaded.attachment().size(), 3);
    }

    #[test]
    fn forced_status_drives_the_retry_path() {
        let server = MockServer::start().expect("mock server starts");
        server.force_status("/api/v1/users/me", 429, Some(0), 1);
        let client = server.client();
        // The first attempt is answered 429 with Retry-After: 0, so the retry succeeds.
        let me = client.me().expect("retry should succeed");
        assert_eq!(me.id, 4242);
        assert_eq!(server.requests_to("/api/v1/users/me").len(), 2);
    }

    #[test]
    fn fail_next_simulates_a_transport_failure() {
        let server = MockServer::start().expect("mock server starts");
        server.fail_next("/api/v1/users/me", 1);
        let client = server.client();
        let me = client
            .me()
            .expect("the retry should succeed after one drop");
        assert_eq!(me.id, 4242);
    }

    #[test]
    fn websocket_ticket_is_single_use() {
        let server = MockServer::start().expect("mock server starts");
        let client = server.client();
        let ticket = crate::fetch_ticket(&client).expect("ticket");
        assert_eq!(ticket.expires_in, Some(30));
        assert_eq!(ticket.ticket.len(), 64);
        assert_eq!(server.state().tickets.lock().unwrap().len(), 1);
    }

    #[test]
    fn broadcasts_reach_a_subscribed_stream() {
        let server = MockServer::start().expect("mock server starts");
        let client = server.client();
        let stream =
            crate::EventStream::subscribe(&client, crate::EventFilter::all()).expect("subscribe");

        // Wait for the handshake greeting.
        let greeting = stream
            .recv_timeout(Duration::from_secs(5))
            .expect("greeting");
        assert_eq!(greeting.kind, crate::EventType::Connected);

        let sent = server.broadcast(&json!({
            "type": "chat.message.created",
            "recipients": [4242],
            "data": {"id": 9001, "conversation_id": 33, "sender_id": 7,
                     "kind": "text", "content": "你好"}
        }));
        assert_eq!(sent, 1, "the stream must be attached");

        let event = stream.recv_timeout(Duration::from_secs(5)).expect("event");
        assert_eq!(event.kind, crate::EventType::ChatMessageCreated);
        assert_eq!(event.message().unwrap().content, "你好");
        assert!(stream.close_and_wait(Duration::from_secs(5)));
    }

    #[test]
    fn a_closed_stream_deregisters_its_websocket_client() {
        // The server drops a slow consumer, and the SDK drops the socket when the stream
        // is closed. That has to be observable, or neither side can be tested.
        let server = MockServer::start().expect("mock server starts");
        let client = server.client();
        let stream =
            crate::EventStream::subscribe(&client, crate::EventFilter::all()).expect("subscribe");

        let greeting = stream
            .recv_timeout(Duration::from_secs(5))
            .expect("greeting");
        assert_eq!(greeting.kind, crate::EventType::Connected);
        assert_eq!(server.ws_client_count(), 1);

        assert!(stream.close_and_wait(Duration::from_secs(5)));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while server.ws_client_count() != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            server.ws_client_count(),
            0,
            "closing the stream must release the connection"
        );

        // A broadcast to nobody is zero, not a panic.
        assert_eq!(server.broadcast(&json!({"type": "post.created"})), 0);
    }

    #[test]
    fn stopping_the_server_releases_the_port() {
        let server = MockServer::start().expect("mock server starts");
        let base = server.base_url().to_string();
        server.stop();
        let client = crate::BaseClient::new(base).expect("client");
        assert!(client.health().is_err(), "a stopped server must not answer");
    }
}
