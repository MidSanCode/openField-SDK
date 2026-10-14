//! The realtime event channel (`bots.md` §8).
//!
//! ## The handshake
//!
//! A bot cannot put its long-lived token in a WebSocket URL: the gateway only accepts
//! a single-use ticket there, so the flow is two steps (`ticket.go`, `ws.go`):
//!
//! 1. `POST /api/v1/ws` with the bearer token → `{"ticket": "<64 hex>", "expires_in": 30}`;
//! 2. `GET /api/v1/ws?ticket=…` upgraded to WebSocket, within those 30 seconds.
//!
//! The ticket is consumed on redemption, so a retry always needs a fresh one.
//!
//! ## Delivery guarantees
//!
//! Events are relayed through Postgres `NOTIFY` and are **best effort**
//! (`pkg/events/events.go`): a restart or a dropped connection loses whatever happened
//! in the gap, and there is no replay. The documented pattern is therefore
//! "WebSocket for latency, REST for reconciliation" (`bots.md` §15.5).
//!
//! ## Backpressure
//!
//! The server gives each connection a 256-message send buffer and **disconnects a slow
//! consumer** rather than growing it. This client applies the same policy locally:
//! [`EventQueue`] is bounded, and when the consumer falls behind the **oldest** queued
//! event is discarded and counted in [`EventStreamStats::events_dropped`]. An unbounded
//! queue would turn one slow handler into an out-of-memory kill.

use std::collections::VecDeque;
use std::io;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::WebSocket;

use crate::client::AuthenticatedClient;
use crate::error::{Result, SdkError};
use crate::models::{Conversation, Id, Message, Post, Reply};
use crate::retry::ReconnectPolicy;

/// How long the read loop waits for a frame before checking for a shutdown request.
///
/// The server pings every 54 seconds and drops the connection after 60 seconds of
/// silence (`ws.go`), so this only needs to be short enough for a responsive
/// [`EventStream::close`]; it is not a liveness timeout.
const IDLE_POLL: Duration = Duration::from_millis(500);

/// Default bound on queued events before the oldest are discarded.
pub const DEFAULT_QUEUE_CAPACITY: usize = 1024;

/// Lifetime the server gives a ticket (`ticket.go`).
pub const TICKET_TTL_SECONDS: u64 = 30;

/// The `POST /api/v1/ws` ticket response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsTicket {
    /// Single-use ticket, 64 hex characters.
    pub ticket: String,
    /// Seconds until it expires; the server sends 30.
    #[serde(default)]
    pub expires_in: Option<u64>,
}

/// One of the event types the server publishes (`pkg/events/events.go`).
///
/// An unrecognised type is preserved as [`EventType::Unknown`] so a bot can log it
/// instead of silently discarding it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EventType {
    /// The greeting the push service sends immediately after the upgrade.
    Connected,
    /// A message was created.
    ChatMessageCreated,
    /// A message was edited.
    ChatMessageUpdated,
    /// A message was soft-deleted.
    ChatMessageDeleted,
    /// Someone is typing.
    ChatTyping,
    /// A conversation's metadata changed.
    ChatConversationUpdated,
    /// A consent request was filed against the bot.
    ChatConsentRequested,
    /// New E2EE key envelopes were stored.
    ChatE2eeKeysUpdated,
    /// A post was created.
    PostCreated,
    /// A post was deleted.
    PostDeleted,
    /// A reply was created.
    PostReplyCreated,
    /// A reply was edited.
    PostReplyUpdated,
    /// A reply was deleted.
    PostReplyDeleted,
    /// A type this SDK version does not name.
    Unknown(String),
}

impl EventType {
    /// Parse a wire value.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "connected" => Self::Connected,
            "chat.message.created" => Self::ChatMessageCreated,
            "chat.message.updated" => Self::ChatMessageUpdated,
            "chat.message.deleted" => Self::ChatMessageDeleted,
            "chat.typing" => Self::ChatTyping,
            "chat.conversation.updated" => Self::ChatConversationUpdated,
            "chat.consent.requested" => Self::ChatConsentRequested,
            "chat.e2ee.keys.updated" => Self::ChatE2eeKeysUpdated,
            "post.created" => Self::PostCreated,
            "post.deleted" => Self::PostDeleted,
            "post.reply.created" => Self::PostReplyCreated,
            "post.reply.updated" => Self::PostReplyUpdated,
            "post.reply.deleted" => Self::PostReplyDeleted,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// The wire value.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Connected => "connected",
            Self::ChatMessageCreated => "chat.message.created",
            Self::ChatMessageUpdated => "chat.message.updated",
            Self::ChatMessageDeleted => "chat.message.deleted",
            Self::ChatTyping => "chat.typing",
            Self::ChatConversationUpdated => "chat.conversation.updated",
            Self::ChatConsentRequested => "chat.consent.requested",
            Self::ChatE2eeKeysUpdated => "chat.e2ee.keys.updated",
            Self::PostCreated => "post.created",
            Self::PostDeleted => "post.deleted",
            Self::PostReplyCreated => "post.reply.created",
            Self::PostReplyUpdated => "post.reply.updated",
            Self::PostReplyDeleted => "post.reply.deleted",
            Self::Unknown(other) => other,
        }
    }

    /// Every type the server currently publishes, plus the greeting.
    pub const ALL: [EventType; 13] = [
        EventType::Connected,
        EventType::ChatMessageCreated,
        EventType::ChatMessageUpdated,
        EventType::ChatMessageDeleted,
        EventType::ChatTyping,
        EventType::ChatConversationUpdated,
        EventType::ChatConsentRequested,
        EventType::ChatE2eeKeysUpdated,
        EventType::PostCreated,
        EventType::PostDeleted,
        EventType::PostReplyCreated,
        EventType::PostReplyUpdated,
        EventType::PostReplyDeleted,
    ];

    /// Whether this is a chat event.
    pub fn is_chat(&self) -> bool {
        self.as_str().starts_with("chat.")
    }

    /// Whether this is a post or reply event.
    pub fn is_post(&self) -> bool {
        self.as_str().starts_with("post.")
    }
}

impl Default for EventType {
    fn default() -> Self {
        Self::Unknown(String::new())
    }
}

impl std::fmt::Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for EventType {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EventType {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::parse(&raw))
    }
}

/// The event envelope (`pkg/events/events.go`).
///
/// `recipients` is `omitempty` server-side, so a **broadcast** event (which is how
/// `post.created` reaches everyone on a `public`/`login` post) arrives with no
/// `recipients` key at all. An empty list therefore means "everyone connected", not
/// "nobody".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Event {
    /// Event type, emitted as `type`.
    #[serde(rename = "type")]
    pub kind: EventType,
    /// Target user ids; empty means a broadcast.
    pub recipients: Vec<Id>,
    /// The resource the event is about: a message, post, reply or a small object.
    pub data: Value,
    /// Unknown fields preserved verbatim.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl Default for Event {
    fn default() -> Self {
        Self {
            kind: EventType::Unknown(String::new()),
            recipients: Vec::new(),
            data: Value::Null,
            extra: serde_json::Map::new(),
        }
    }
}

impl Event {
    /// Parse one JSON frame.
    pub fn parse(raw: &str) -> Result<Self> {
        serde_json::from_str(raw).map_err(|err| SdkError::Decode {
            message: format!("event frame was not a valid envelope: {err}"),
            raw: raw.chars().take(512).collect(),
        })
    }

    /// Whether this event was broadcast rather than addressed.
    pub fn is_broadcast(&self) -> bool {
        self.recipients.is_empty()
    }

    /// The `data` object as the given type, when it matches.
    pub fn decode_data<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.data.clone()).map_err(|err| SdkError::Decode {
            message: format!("event `{}` payload did not match: {err}", self.kind),
            raw: String::new(),
        })
    }

    /// The message carried by a `chat.message.*` event.
    pub fn message(&self) -> Result<Message> {
        self.decode_data()
    }

    /// The post carried by a `post.*` event.
    pub fn post(&self) -> Result<Post> {
        self.decode_data()
    }

    /// The reply carried by a `post.reply.*` event.
    pub fn reply(&self) -> Result<Reply> {
        self.decode_data()
    }

    /// The conversation carried by a `chat.conversation.updated` event, if inlined.
    pub fn conversation(&self) -> Result<Conversation> {
        self.decode_data()
    }

    /// The conversation id this event concerns, when it carries one.
    ///
    /// Works for message events, typing events and conversation events alike.
    pub fn conversation_id(&self) -> Option<Id> {
        self.data
            .get("conversation_id")
            .and_then(crate::models::id_from_value)
            .or_else(|| {
                (self.kind == EventType::ChatConversationUpdated)
                    .then(|| self.data.get("id").and_then(crate::models::id_from_value))
                    .flatten()
            })
    }

    /// The ordering key for deduplication: a message or post id when present.
    pub fn resource_id(&self) -> Option<Id> {
        for key in ["id", "message_id", "post_id", "reply_id"] {
            if let Some(id) = self.data.get(key).and_then(crate::models::id_from_value) {
                return Some(id);
            }
        }
        None
    }
}

/// Which events a subscriber wants.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventFilter {
    /// Event types to keep; empty means every type.
    pub types: Vec<EventType>,
    /// Conversation ids to keep; empty means every conversation.
    ///
    /// Events carrying no conversation id (posts, for instance) are kept when the type
    /// filter allows them, so a conversation filter narrows chat without hiding the rest.
    pub conversation_ids: Vec<Id>,
}

impl EventFilter {
    /// Accept every event.
    pub fn all() -> Self {
        Self::default()
    }

    /// Only chat messages: created, updated and deleted.
    pub fn messages() -> Self {
        Self {
            types: vec![
                EventType::ChatMessageCreated,
                EventType::ChatMessageUpdated,
                EventType::ChatMessageDeleted,
            ],
            conversation_ids: Vec::new(),
        }
    }

    /// Only these event types.
    pub fn types(types: impl IntoIterator<Item = EventType>) -> Self {
        Self {
            types: types.into_iter().collect(),
            conversation_ids: Vec::new(),
        }
    }

    /// Builder: restrict to one conversation.
    pub fn conversation(mut self, conversation_id: Id) -> Self {
        self.conversation_ids.push(conversation_id);
        self
    }

    /// Whether `event` passes.
    pub fn accepts(&self, event: &Event) -> bool {
        if !self.types.is_empty() && !self.types.contains(&event.kind) {
            return false;
        }
        if !self.conversation_ids.is_empty() {
            if let Some(id) = event.conversation_id() {
                return self.conversation_ids.contains(&id);
            }
        }
        true
    }
}

/// Counters for a running stream, useful for logging and health output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventStreamStats {
    /// Connection attempts, including reconnections.
    pub connection_attempts: u64,
    /// Successful upgrades.
    pub connections_opened: u64,
    /// Event frames delivered to the consumer.
    pub events_delivered: u64,
    /// Events discarded because the consumer was behind.
    pub events_dropped: u64,
    /// Frames that could not be parsed as an event envelope.
    pub frames_invalid: u64,
    /// Times the socket dropped and reconnection began.
    pub disconnects: u64,
    /// Events still waiting to be consumed.
    pub events_queued: u64,
}

#[derive(Debug, Default)]
struct SharedStats {
    connection_attempts: AtomicU64,
    connections_opened: AtomicU64,
    events_delivered: AtomicU64,
    events_dropped: AtomicU64,
    frames_invalid: AtomicU64,
    disconnects: AtomicU64,
}

/// A bounded, drop-oldest event queue.
///
/// Mirrors the server's own backpressure stance: a consumer that cannot keep up loses
/// events instead of growing the process without bound.
#[derive(Debug)]
struct EventQueue {
    inner: Mutex<VecDeque<Event>>,
    ready: Condvar,
    capacity: usize,
    closed: AtomicBool,
    dropped: AtomicU64,
}

impl EventQueue {
    fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            capacity: capacity.max(1),
            closed: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
        }
    }

    fn push(&self, event: Event) {
        let mut queue = self.inner.lock().expect("event queue lock");
        while queue.len() >= self.capacity {
            queue.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back(event);
        drop(queue);
        self.ready.notify_one();
    }

    fn pop_timeout(&self, timeout: Duration) -> Option<Event> {
        let mut queue = self.inner.lock().expect("event queue lock");
        if let Some(event) = queue.pop_front() {
            return Some(event);
        }
        if self.closed.load(Ordering::SeqCst) {
            return None;
        }
        let (mut queue, _) = self
            .ready
            .wait_timeout(queue, timeout)
            .expect("event queue lock");
        queue.pop_front()
    }

    fn len(&self) -> usize {
        self.inner.lock().expect("event queue lock").len()
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.ready.notify_all();
    }
}

/// A live subscription to the server's event channel.
///
/// Dropping the stream stops the background reader. Events are delivered in arrival
/// order; when the consumer falls behind, the oldest queued events are discarded rather
/// than retaining them, matching the server's "disconnect a slow consumer" policy.
#[derive(Debug)]
pub struct EventStream {
    queue: Arc<EventQueue>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<SharedStats>,
    handle: Option<JoinHandle<()>>,
}

impl EventStream {
    /// Connect and start delivering events.
    ///
    /// Returns as soon as the background thread is running: the ticket exchange and the
    /// upgrade happen off-thread, so a slow or temporarily unreachable server does not
    /// block the caller. Failures are retried per the client's [`ReconnectPolicy`].
    pub fn subscribe(client: &AuthenticatedClient, filter: EventFilter) -> Result<Self> {
        Self::subscribe_with(client.clone(), filter, None, DEFAULT_QUEUE_CAPACITY)
    }

    /// Connect with an explicit reconnect policy, overriding the client's config.
    pub fn subscribe_with_policy(
        client: &AuthenticatedClient,
        filter: EventFilter,
        reconnect: ReconnectPolicy,
    ) -> Result<Self> {
        Self::subscribe_with(
            client.clone(),
            filter,
            Some(reconnect),
            DEFAULT_QUEUE_CAPACITY,
        )
    }

    /// Connect with an explicit reconnect policy and queue capacity.
    pub fn subscribe_full(
        client: &AuthenticatedClient,
        filter: EventFilter,
        reconnect: ReconnectPolicy,
        queue_capacity: usize,
    ) -> Result<Self> {
        Self::subscribe_with(client.clone(), filter, Some(reconnect), queue_capacity)
    }

    fn subscribe_with(
        client: AuthenticatedClient,
        filter: EventFilter,
        reconnect: Option<ReconnectPolicy>,
        queue_capacity: usize,
    ) -> Result<Self> {
        let reconnect = match reconnect {
            Some(policy) => policy,
            None => client.config().reconnect.clone(),
        };
        reconnect.validate()?;

        let queue = Arc::new(EventQueue::new(queue_capacity));
        let shutdown = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(SharedStats::default());

        let thread_queue = Arc::clone(&queue);
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_stats = Arc::clone(&stats);
        let handle = thread::Builder::new()
            .name("openfield-events".to_string())
            .spawn(move || {
                run_connection_loop(
                    client,
                    filter,
                    reconnect,
                    thread_queue,
                    thread_shutdown,
                    thread_stats,
                )
            })
            .map_err(|err| SdkError::Transport {
                message: format!("could not start the event reader thread: {err}"),
            })?;

        Ok(Self {
            queue,
            shutdown,
            stats,
            handle: Some(handle),
        })
    }

    /// Block until one event arrives, or the stream stops.
    pub fn recv(&self) -> Option<Event> {
        loop {
            if let Some(event) = self.queue.pop_timeout(Duration::from_millis(50)) {
                return Some(event);
            }
            if self.is_closed() && self.queue.len() == 0 {
                return None;
            }
        }
    }

    /// Block for at most `timeout`, returning `None` on timeout or closure.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Event> {
        self.queue.pop_timeout(timeout)
    }

    /// Return the next queued event without blocking.
    pub fn try_recv(&self) -> Option<Event> {
        self.queue.pop_timeout(Duration::ZERO)
    }

    /// Ask the background reader to stop. Returns promptly; the socket is released on
    /// the next idle wake-up.
    pub fn close(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.queue.close();
    }

    /// Whether a shutdown has been requested.
    pub fn is_closed(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Whether the reader thread has finished.
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Close and wait for the reader thread, up to `timeout`.
    ///
    /// Returns `true` when the thread finished in time. The wait is bounded because a
    /// blocking read can take up to one idle poll to notice the flag.
    pub fn close_and_wait(mut self, timeout: Duration) -> bool {
        self.close();
        let handle = match self.handle.take() {
            Some(handle) => handle,
            None => return true,
        };
        let deadline = Instant::now() + timeout;
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                // Detach rather than block; the thread will exit on its own.
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = handle.join();
        true
    }

    /// A snapshot of the stream counters.
    pub fn stats(&self) -> EventStreamStats {
        EventStreamStats {
            connection_attempts: self.stats.connection_attempts.load(Ordering::Relaxed),
            connections_opened: self.stats.connections_opened.load(Ordering::Relaxed),
            events_delivered: self.stats.events_delivered.load(Ordering::Relaxed),
            // Drops are counted by the queue, which owns the capacity decision.
            events_dropped: self.stats.events_dropped.load(Ordering::Relaxed)
                + self.queue.dropped.load(Ordering::Relaxed),
            frames_invalid: self.stats.frames_invalid.load(Ordering::Relaxed),
            disconnects: self.stats.disconnects.load(Ordering::Relaxed),
            events_queued: self.queue.len() as u64,
        }
    }

    /// Events currently waiting to be consumed.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Split into a receiving half and a control half.
    ///
    /// The sidecar needs this: one thread blocks on [`EventReceiver::recv`] and writes
    /// frames to its parent while the dispatch loop keeps answering requests with
    /// [`EventControl`]. The two halves are independent, so neither has to hold the
    /// other's lock.
    pub fn split(mut self) -> (EventReceiver, EventControl) {
        let receiver = EventReceiver {
            queue: Arc::clone(&self.queue),
        };
        let control = EventControl {
            queue: Arc::clone(&self.queue),
            shutdown: Arc::clone(&self.shutdown),
            stats: Arc::clone(&self.stats),
            // Taken rather than moved, because a type with `Drop` cannot have a field
            // moved out of it.
            handle: self.handle.take(),
        };
        // `self` is consumed here, so its `Drop` does not run; the shutdown flag now
        // belongs to the control half alone.
        std::mem::forget(self);
        (receiver, control)
    }
}

/// The receiving half of a split [`EventStream`].
///
/// Dropping it does **not** stop the reader: use the matching [`EventControl`] for that,
/// so a caller can drain what is already queued after asking for a stop.
#[derive(Debug)]
pub struct EventReceiver {
    queue: Arc<EventQueue>,
}

impl EventReceiver {
    /// Block until an event arrives or the stream stops.
    pub fn recv(&self) -> Option<Event> {
        loop {
            if let Some(event) = self.queue.pop_timeout(Duration::from_millis(50)) {
                return Some(event);
            }
            if self.queue.closed.load(Ordering::SeqCst) && self.queue.len() == 0 {
                return None;
            }
        }
    }

    /// Block for at most `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Event> {
        self.queue.pop_timeout(timeout)
    }

    /// Return the next queued event without blocking.
    pub fn try_recv(&self) -> Option<Event> {
        self.queue.pop_timeout(Duration::ZERO)
    }

    /// Whether the producing half has been closed.
    pub fn is_closed(&self) -> bool {
        self.queue.closed.load(Ordering::SeqCst)
    }

    /// Events currently waiting.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }
}

/// The control half of a split [`EventStream`].
#[derive(Debug)]
pub struct EventControl {
    queue: Arc<EventQueue>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<SharedStats>,
    handle: Option<JoinHandle<()>>,
}

impl EventControl {
    /// Ask the reader to stop.
    pub fn close(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.queue.close();
    }

    /// Whether a stop has been requested.
    pub fn is_closed(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Whether the reader thread has finished.
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// A snapshot of the stream counters.
    pub fn stats(&self) -> EventStreamStats {
        EventStreamStats {
            connection_attempts: self.stats.connection_attempts.load(Ordering::Relaxed),
            connections_opened: self.stats.connections_opened.load(Ordering::Relaxed),
            events_delivered: self.stats.events_delivered.load(Ordering::Relaxed),
            events_dropped: self.stats.events_dropped.load(Ordering::Relaxed)
                + self.queue.dropped.load(Ordering::Relaxed),
            frames_invalid: self.stats.frames_invalid.load(Ordering::Relaxed),
            disconnects: self.stats.disconnects.load(Ordering::Relaxed),
            events_queued: self.queue.len() as u64,
        }
    }
}

impl Drop for EventControl {
    fn drop(&mut self) {
        self.close();
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.queue.close();
        // The handle is intentionally not joined: dropping must not block. The thread
        // observes the flag on its next idle wake-up and exits.
    }
}

/// Fetch a single-use WebSocket ticket (`POST /api/v1/ws`).
pub fn fetch_ticket(client: &AuthenticatedClient) -> Result<WsTicket> {
    let request = crate::client::Request::post("/api/v1/ws");
    let response = client.request(&request)?;
    if !response.is_success() {
        return Err(response.to_error());
    }
    let ticket: WsTicket = response.decode()?;
    if ticket.ticket.trim().is_empty() {
        return Err(SdkError::Decode {
            message: "the server returned an empty WebSocket ticket".into(),
            raw: String::new(),
        });
    }
    Ok(ticket)
}

/// Build the upgrade URL for a ticket.
pub fn upgrade_url(client: &AuthenticatedClient, ticket: &str) -> String {
    format!(
        "{}/api/v1/ws?ticket={}",
        client.websocket_base_url(),
        ticket
    )
}

fn run_connection_loop(
    client: AuthenticatedClient,
    filter: EventFilter,
    reconnect: ReconnectPolicy,
    queue: Arc<EventQueue>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<SharedStats>,
) {
    let mut consecutive_failures = 0u32;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            queue.close();
            return;
        }

        stats.connection_attempts.fetch_add(1, Ordering::Relaxed);
        match open_socket(&client) {
            Ok(mut socket) => {
                stats.connections_opened.fetch_add(1, Ordering::Relaxed);
                consecutive_failures = 0;
                // Keep the read loop responsive to `close()`; this is not a liveness
                // timeout, because the server pings every 54s and drops us at 60s.
                let _ = make_interruptible(&socket, IDLE_POLL);
                read_until_closed(&mut socket, &filter, &queue, &shutdown, &stats);
                if shutdown.load(Ordering::SeqCst) {
                    let _ = socket.close(None);
                    queue.close();
                    return;
                }
                stats.disconnects.fetch_add(1, Ordering::Relaxed);
            }
            Err(_err) => {
                stats.disconnects.fetch_add(1, Ordering::Relaxed);
            }
        }

        consecutive_failures = consecutive_failures.saturating_add(1);
        if !reconnect.allows(consecutive_failures) {
            queue.close();
            return;
        }
        // Sleep in slices so a shutdown request is honoured promptly.
        let mut remaining = reconnect.delay_for_attempt(consecutive_failures);
        while !remaining.is_zero() {
            if shutdown.load(Ordering::SeqCst) {
                queue.close();
                return;
            }
            let slice = remaining.min(Duration::from_millis(100));
            thread::sleep(slice);
            remaining = remaining.saturating_sub(slice);
        }
    }
}

fn open_socket(client: &AuthenticatedClient) -> Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    let ticket = fetch_ticket(client)?;
    let url = upgrade_url(client, &ticket.ticket);
    let (socket, _response) = tungstenite::connect(&url).map_err(|err| SdkError::Transport {
        message: format!("WebSocket upgrade failed: {err}"),
    })?;
    Ok(socket)
}

fn read_until_closed(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    filter: &EventFilter,
    queue: &Arc<EventQueue>,
    shutdown: &Arc<AtomicBool>,
    stats: &Arc<SharedStats>,
) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => match Event::parse(&text) {
                Ok(event) => {
                    if filter.accepts(&event) {
                        queue.push(event);
                        stats.events_delivered.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(_) => {
                    stats.frames_invalid.fetch_add(1, Ordering::Relaxed);
                }
            },
            Ok(tungstenite::Message::Binary(_)) => {
                // The server only sends text envelopes; anything binary is not an event.
                stats.frames_invalid.fetch_add(1, Ordering::Relaxed);
            }
            Ok(tungstenite::Message::Close(_)) => return,
            Ok(tungstenite::Message::Ping(_)) | Ok(tungstenite::Message::Pong(_)) => {
                // tungstenite queues the required pong reply automatically; flushing
                // makes sure it leaves the socket even though the bot sends no data.
                let _ = socket.flush();
            }
            Ok(tungstenite::Message::Frame(_)) => {}
            Err(err) => {
                if is_idle_timeout(&err) {
                    continue;
                }
                // Every other failure ends this connection; the caller reconnects.
                return;
            }
        }
    }
}

/// Reach the TCP socket underneath the WebSocket, TLS or not.
///
/// This needs no `unsafe`: `MaybeTlsStream::Plain` *is* the socket, and rustls'
/// `StreamOwned` exposes its transport in a public field.
fn socket_stream(socket: &WebSocket<MaybeTlsStream<TcpStream>>) -> Option<&TcpStream> {
    match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => Some(stream),
        MaybeTlsStream::Rustls(stream) => Some(&stream.sock),
        _ => None,
    }
}

/// Give the socket a read timeout so the read loop can notice a shutdown request.
fn make_interruptible(
    socket: &WebSocket<MaybeTlsStream<TcpStream>>,
    idle: Duration,
) -> io::Result<()> {
    match socket_stream(socket) {
        Some(stream) => stream.set_read_timeout(Some(idle)),
        None => Ok(()),
    }
}

/// Whether an error just means "no frame arrived within the read timeout".
fn is_idle_timeout(err: &tungstenite::Error) -> bool {
    matches!(
        err,
        tungstenite::Error::Io(io_error)
            if matches!(
                io_error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: Id) -> Event {
        Event {
            kind: EventType::ChatMessageCreated,
            recipients: vec![],
            data: serde_json::json!({ "id": id, "conversation_id": 1 }),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn parses_every_documented_event_type() {
        let documented = [
            "chat.message.created",
            "chat.message.updated",
            "chat.message.deleted",
            "post.created",
            "post.deleted",
            "post.reply.created",
            "post.reply.updated",
            "post.reply.deleted",
            "chat.conversation.updated",
            "chat.consent.requested",
            "chat.typing",
            "chat.e2ee.keys.updated",
            "connected",
        ];
        for raw in documented {
            let parsed = EventType::parse(raw);
            assert!(
                !matches!(&parsed, EventType::Unknown(other) if other == raw),
                "{raw} must be a named type"
            );
            assert_eq!(parsed.as_str(), raw, "round trip failed for {raw}");
        }
        assert_eq!(EventType::ALL.len(), documented.len());
    }

    #[test]
    fn unknown_event_types_keep_their_raw_value() {
        let parsed = EventType::parse("chat.something.new");
        assert_eq!(parsed, EventType::Unknown("chat.something.new".into()));
        assert_eq!(parsed.as_str(), "chat.something.new");
        assert!(parsed.is_chat());

        let json = serde_json::to_value(&parsed).unwrap();
        assert_eq!(json, "chat.something.new");
        let back: EventType = serde_json::from_value(json).unwrap();
        assert_eq!(back, parsed);
    }

    #[test]
    fn classifies_chat_and_post_events() {
        assert!(EventType::ChatMessageCreated.is_chat());
        assert!(!EventType::ChatMessageCreated.is_post());
        assert!(EventType::PostCreated.is_post());
        assert!(!EventType::PostCreated.is_chat());
        assert!(!EventType::Connected.is_chat());
    }

    #[test]
    fn parses_the_documented_message_envelope() {
        let raw = r#"{
            "type": "chat.message.created",
            "recipients": [7, 4242],
            "data": {
                "id": 9001, "conversation_id": 33, "sender_id": 7, "kind": "text",
                "content": "你好", "created_at": "2026-10-10T15:22:41Z"
            }
        }"#;
        let parsed = Event::parse(raw).unwrap();
        assert_eq!(parsed.kind, EventType::ChatMessageCreated);
        assert_eq!(parsed.recipients, vec![7, 4242]);
        assert!(!parsed.is_broadcast());

        let message = parsed.message().unwrap();
        assert_eq!(message.id, 9001);
        assert_eq!(message.conversation_id, 33);
        assert_eq!(message.content, "你好");
        assert_eq!(parsed.conversation_id(), Some(33));
        assert_eq!(parsed.resource_id(), Some(9001));
    }

    #[test]
    fn a_broadcast_event_has_no_recipients_key() {
        let parsed =
            Event::parse(r#"{"type":"post.created","data":{"id":5,"content":"hi"}}"#).unwrap();
        assert!(
            parsed.is_broadcast(),
            "an absent recipients list means everyone connected, not nobody"
        );
        assert_eq!(parsed.post().unwrap().id, 5);
    }

    #[test]
    fn parses_the_greeting_frame() {
        let parsed = Event::parse(r#"{"type":"connected"}"#).unwrap();
        assert_eq!(parsed.kind, EventType::Connected);
        assert_eq!(parsed.data, Value::Null);
        assert!(parsed.is_broadcast());
    }

    #[test]
    fn parses_typing_and_conversation_events() {
        let typing =
            Event::parse(r#"{"type":"chat.typing","data":{"conversation_id":33,"user_id":7}}"#)
                .unwrap();
        assert_eq!(typing.kind, EventType::ChatTyping);
        assert_eq!(typing.conversation_id(), Some(33));

        let updated =
            Event::parse(r#"{"type":"chat.conversation.updated","data":{"conversation_id":33}}"#)
                .unwrap();
        assert_eq!(updated.conversation_id(), Some(33));

        // A bare id is only read as a conversation id for conversation events.
        let post = Event::parse(r#"{"type":"post.created","data":{"id":5}}"#).unwrap();
        assert_eq!(post.conversation_id(), None);
    }

    #[test]
    fn parses_reply_events() {
        let parsed = Event::parse(
            r#"{"type":"post.reply.created","data":{"id":9,"post_id":5,"content":"nice"}}"#,
        )
        .unwrap();
        assert_eq!(parsed.kind, EventType::PostReplyCreated);
        let reply = parsed.reply().unwrap();
        assert_eq!(reply.id, 9);
        assert_eq!(reply.post_id, 5);
    }

    #[test]
    fn unknown_fields_are_preserved() {
        let parsed = Event::parse(r#"{"type":"connected","trace_id":"abc"}"#).unwrap();
        assert_eq!(parsed.extra.get("trace_id").unwrap(), "abc");
        let json = serde_json::to_value(&parsed).unwrap();
        assert_eq!(json["trace_id"], "abc");
        assert_eq!(json["type"], "connected");
    }

    #[test]
    fn invalid_frames_report_a_decode_error() {
        assert_eq!(
            Event::parse("not json").unwrap_err().kind(),
            crate::error::ErrorKind::Decode
        );
        assert_eq!(
            Event::parse("{").unwrap_err().kind(),
            crate::error::ErrorKind::Decode
        );
    }

    #[test]
    fn data_decoding_reports_a_shape_mismatch() {
        // `id` must be an integer; a string is a genuine shape mismatch.
        let parsed =
            Event::parse(r#"{"type":"post.created","data":{"id":"not-a-number"}}"#).unwrap();
        assert_eq!(
            parsed.post().unwrap_err().kind(),
            crate::error::ErrorKind::Decode
        );

        // A payload that simply omits fields defaults instead of failing. That is
        // deliberate: a server that trims a field must not break the whole event.
        let sparse = Event::parse(r#"{"type":"post.created","data":{}}"#).unwrap();
        assert_eq!(sparse.post().unwrap().id, 0);
    }

    #[test]
    fn filter_by_type_and_conversation() {
        let all = EventFilter::all();
        let message_event =
            Event::parse(r#"{"type":"chat.message.created","data":{"id":1,"conversation_id":33}}"#)
                .unwrap();
        let post_event =
            Event::parse(r#"{"type":"post.created","data":{"id":5,"content":"x"}}"#).unwrap();

        assert!(all.accepts(&message_event));
        assert!(all.accepts(&post_event));

        let messages_only = EventFilter::messages();
        assert!(messages_only.accepts(&message_event));
        assert!(!messages_only.accepts(&post_event));

        let scoped = EventFilter::all().conversation(33);
        assert!(scoped.accepts(&message_event));
        let other =
            Event::parse(r#"{"type":"chat.message.created","data":{"id":2,"conversation_id":44}}"#)
                .unwrap();
        assert!(!scoped.accepts(&other));
        // Events with no conversation id are not hidden by a conversation filter.
        assert!(scoped.accepts(&post_event));
    }

    #[test]
    fn queue_is_bounded_and_drops_the_oldest() {
        let queue = EventQueue::new(3);
        for id in 1..=5 {
            queue.push(event(id));
        }
        assert_eq!(queue.len(), 3, "capacity is enforced");
        assert_eq!(queue.dropped.load(Ordering::Relaxed), 2);

        // The survivors are the newest three, in order.
        let ids: Vec<Id> = (0..3)
            .map(|_| {
                queue
                    .pop_timeout(Duration::ZERO)
                    .unwrap()
                    .resource_id()
                    .unwrap()
            })
            .collect();
        assert_eq!(ids, vec![3, 4, 5]);
        assert!(queue.pop_timeout(Duration::ZERO).is_none());
    }

    #[test]
    fn a_closed_queue_stops_waiting() {
        let queue = EventQueue::new(1);
        queue.close();
        assert!(queue.pop_timeout(Duration::from_millis(10)).is_none());
        // A push after close is still drained, so nothing queued is lost.
        queue.push(event(1));
        assert_eq!(
            queue.pop_timeout(Duration::ZERO).unwrap().resource_id(),
            Some(1)
        );
    }

    #[test]
    fn queue_wakes_a_waiting_consumer() {
        let queue = Arc::new(EventQueue::new(4));
        let producer = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                queue.push(event(77));
            })
        };
        let received = queue.pop_timeout(Duration::from_secs(2)).expect("an event");
        assert_eq!(received.resource_id(), Some(77));
        producer.join().unwrap();
    }

    #[test]
    fn upgrade_url_uses_the_ws_scheme() {
        let token = crate::config::BotToken::new(format!("ofb_{}", "0".repeat(64))).unwrap();
        let https =
            AuthenticatedClient::new("https://openfield.example.com", token.clone()).unwrap();
        assert_eq!(
            upgrade_url(&https, "abc"),
            "wss://openfield.example.com/api/v1/ws?ticket=abc"
        );

        let http = AuthenticatedClient::new("http://localhost:8080", token).unwrap();
        assert_eq!(
            upgrade_url(&http, "abc"),
            "ws://localhost:8080/api/v1/ws?ticket=abc"
        );
    }

    #[test]
    fn ticket_response_parses() {
        let ticket: WsTicket = serde_json::from_str(
            r#"{"ticket":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","expires_in":30}"#,
        )
        .unwrap();
        assert_eq!(ticket.expires_in, Some(30));
        assert_eq!(ticket.ticket.len(), 64);
        assert_eq!(TICKET_TTL_SECONDS, 30);
        assert_eq!(DEFAULT_QUEUE_CAPACITY, 1024);
    }

    #[test]
    fn idle_timeouts_are_not_treated_as_disconnects() {
        assert!(is_idle_timeout(&tungstenite::Error::Io(io::Error::from(
            io::ErrorKind::WouldBlock
        ))));
        assert!(is_idle_timeout(&tungstenite::Error::Io(io::Error::from(
            io::ErrorKind::TimedOut
        ))));
        assert!(!is_idle_timeout(&tungstenite::Error::Io(io::Error::from(
            io::ErrorKind::ConnectionReset
        ))));
        assert!(!is_idle_timeout(&tungstenite::Error::ConnectionClosed));
    }

    #[test]
    fn stats_default_to_zero() {
        let stats = EventStreamStats::default();
        assert_eq!(stats.connections_opened, 0);
        assert_eq!(stats.events_delivered, 0);
        assert_eq!(stats.events_queued, 0);
    }
}
