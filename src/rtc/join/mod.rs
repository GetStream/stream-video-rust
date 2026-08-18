//! The participant join flow, event stream, leave, and reconnect driver.
//!
//! Ported from JS `Call.ts` (recovery is canonical) and stream-py / videosdk
//! (transport). The public entry points are [`Call::join`](crate::Call::join)
//! and [`Call::leave`](crate::Call::leave); this module holds the machinery:
//!
//! - the join for-loop ([`RtcCore::join`]) with `max_join_retries`, JS full-jitter
//!   backoff, unrecoverable abort, and SFU switching via `migrating_from`;
//! - the SFU WebSocket handshake (`JoinRequest` → `JoinResponse`), subscriber
//!   answer negotiation, and ICE trickle;
//! - a typed [`CallEvent`] broadcast stream (participant joined/left, tracks, …);
//! - the reconnect state machine (`RtcCore::run_reconnect`) driven by the pure
//!   decision logic in [`super::reconnect`], with dedup, the rejoin rate limiter,
//!   the ICE / negotiation caps, the disconnection timeout, and the
//!   `restore_published_tracks` / `restore_subscribed_tracks` hooks.
//!
//! This root file holds [`RtcCore`] itself — its fields, lifecycle/generation
//! bookkeeping, and the shared types — while each submodule owns one part of
//! the flow as `impl RtcCore` blocks:
//!
//! - `lifecycle` — join, leave, token refresh, coordinator events;
//! - `connection` — the SFU WebSocket handshake, callbacks, event dispatch;
//! - `publish` — the publish path; `publication` — its per-track state;
//! - `subscriptions_runtime` — subscription negotiation and inbound tracks;
//! - `roster` — the participant roster and cached call state;
//! - `reconnect_runtime` — reconnect execution and media restoration.
//!
//! `reconnect_runtime` and `subscriptions_runtime` carry the suffix to avoid
//! shadowing the sibling [`super::reconnect`] / [`super::subscriptions`]
//! modules, which every submodule sees through its `use super::*`.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex as TokioMutex, Notify, broadcast};
use tokio::task::JoinHandle;
use url::Url;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::track::track_remote::TrackRemote;

use crate::client::Client;
use crate::models::CallRequest;

use super::client::UserTokenSource;
use super::coordinator::{self, Credentials, JoinCallRequest, StatsOptions};
use super::coordinator_ws::{self, ConnectUserDetails, CoordinatorEvent, WsAuthMessage};
use super::error::{Result, RtcError, SfuJoinError, SfuTimeoutError};
use super::identity;
use super::local_track::LocalTrack;
use super::peer;
use super::proto::event::{self, JoinRequest, JoinResponse, ReconnectDetails, SfuEvent, sfu_event};
use super::proto::models::{self, PeerType, TrackType};
use super::proto::signal;
use super::publish_options::ClientPublishOptions;
use super::publisher;
use super::reconnect::{
    self, FailureCaps, ReconnectStrategy, SlidingWindowRateLimiter, escalate_strategy,
    strategy_after_signal_close,
};
use super::remote_track::{RemoteParticipant, RemoteTrack};
use super::sfu_ws::{self, SfuReceiver, SfuSender};
use super::signal::SignalClient;
use super::stats::{self, StatsReporter, StatsReporterParts};
use super::subscriptions::{SubscriptionConfig, SubscriptionTarget, TrackKey};
use super::tracer::Tracer;

use serde_json::json;

mod connection;
mod lifecycle;
mod publication;
mod publish;
mod reconnect_runtime;
mod roster;
mod subscriptions_runtime;

use connection::{
    await_join_response, build_sfu_ws_url, event_loop, flush_candidates, ping_loop,
    register_connection_state, register_ice_trickle, register_on_track,
};
use publication::{MediaState, PublicationStatus};
use roster::{CallStateCache, RosterEntry};

const MIGRATION_COMPLETE_TIMEOUT: Duration = Duration::from_secs(7);

/// A callback invoked with each [`RemoteTrack`] delivered by the subscriber PC.
pub type OnTrackCallback = Arc<dyn Fn(RemoteTrack) + Send + Sync>;

/// Options for [`Call::join`](crate::Call::join).
///
/// JS omits `location` (the SDK discovers it) and `e2ee`. Extra knobs beyond the
/// JS `JoinCallData` are `max_join_retries`, `join_response_timeout`, and
/// `rpc_request_timeout`.
#[derive(Debug, Clone)]
pub struct JoinCallData {
    /// The user id to join as. A user token is minted internally.
    pub user_id: String,
    /// Create the call if it doesn't exist.
    pub create: bool,
    /// Max initial-join attempts (default 3, clamped ≥ 1).
    pub max_join_retries: u32,
    /// Deadline waiting for the SFU `JoinResponse` (default 5s).
    pub join_response_timeout: Duration,
    /// Per-RPC Twirp timeout (default 5s).
    pub rpc_request_timeout: Duration,
    /// Explicit edge location; `None` runs CloudFront hint discovery.
    pub location: Option<String>,
    /// Call creation data applied when `create` is set.
    pub data: Option<CallRequest>,
    /// Ring members.
    pub ring: bool,
    /// Notify members.
    pub notify: bool,
    /// Request video call semantics.
    pub video: bool,
}

impl Default for JoinCallData {
    fn default() -> Self {
        Self {
            user_id: String::new(),
            create: false,
            max_join_retries: reconnect::DEFAULT_MAX_JOIN_RETRIES,
            join_response_timeout: Duration::from_secs(5),
            rpc_request_timeout: Duration::from_secs(5),
            location: None,
            data: None,
            ring: false,
            notify: false,
            video: false,
        }
    }
}

impl JoinCallData {
    /// Join as `user_id` with defaults.
    pub fn new(user_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
            ..Default::default()
        }
    }

    /// Join as `user_id`, creating the call if needed.
    pub fn create(user_id: impl Into<String>) -> Self {
        Self {
            create: true,
            ..Self::new(user_id)
        }
    }
}

/// The lifecycle state of a call (JS `CallingState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallingState {
    /// Not joined.
    Idle,
    /// Join in progress.
    Joining,
    /// Joined and healthy.
    Joined,
    /// Recovering the connection.
    Reconnecting,
    /// Migrating to a new SFU.
    Migrating,
    /// Reconnect gave up within the disconnection timeout.
    ReconnectingFailed,
    /// Left (terminal).
    Left,
    /// Network is offline; waiting to resume.
    Offline,
}

/// A typed SFU event delivered on the [`Call`](crate::Call) event stream.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum CallEvent {
    /// A participant joined the call.
    ParticipantJoined(models::Participant),
    /// A participant left the call.
    ParticipantLeft(models::Participant),
    /// A participant's user data changed.
    ParticipantUpdated(models::Participant),
    /// A call-scoped coordinator WebSocket event.
    Coordinator(CoordinatorEvent),
    /// A track was published (audio/video/screenshare).
    TrackPublished {
        /// The publisher's user id.
        user_id: String,
        /// The publisher's session id.
        session_id: String,
        /// The `TrackType` value.
        track_type: i32,
    },
    /// A track was unpublished.
    TrackUnpublished {
        /// The publisher's user id.
        user_id: String,
        /// The publisher's session id.
        session_id: String,
        /// The `TrackType` value.
        track_type: i32,
    },
    /// The dominant speaker changed.
    DominantSpeakerChanged {
        /// The dominant speaker's user id.
        user_id: String,
        /// The dominant speaker's session id.
        session_id: String,
    },
    /// Audio levels changed for one or more participants.
    ///
    /// The SFU derives these from the RFC 6464 header extension and reports
    /// only participants it currently considers speaking, so a publisher that
    /// sends no level never appears here.
    AudioLevelChanged(Vec<event::AudioLevel>),
    /// Connection quality changed for one or more participants.
    ConnectionQualityChanged(Vec<event::ConnectionQualityInfo>),
    /// The SFU's participant totals changed.
    ParticipantCountChanged(models::ParticipantCount),
    /// The server-side pin ordering changed.
    PinsUpdated(Vec<models::Pin>),
    /// The SFU paused or resumed inbound tracks for this subscriber.
    InboundStateChanged(Vec<event::InboundVideoState>),
    /// The SFU replaced the publish-option table for this connection.
    PublishOptionsChanged {
        /// The new authoritative publish options.
        publish_options: Vec<models::PublishOption>,
        /// Human-readable reason supplied by the SFU.
        reason: String,
    },
    /// The SFU requested changes to active publisher encodings.
    PublishQualityChanged(event::ChangePublishQuality),
    /// The current participant's publishing grants changed.
    CallGrantsUpdated(event::CallGrantsUpdated),
    /// An SFU-directed ICE restart completed for a peer connection.
    IceRestarted(PeerType),
    /// The SFU reported an error for this participant.
    Error(SfuJoinError),
    /// The call ended.
    CallEnded,
    /// The connection state changed.
    CallingStateChanged(CallingState),
}

/// A point-in-time view of SFU call state maintained by [`RtcCore`].
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct CallStateSnapshot {
    /// Known non-anonymous participants. Large calls may provide a truncated list.
    pub participants: Vec<RemoteParticipant>,
    /// SFU participant totals, including anonymous participants.
    pub participant_count: models::ParticipantCount,
    /// Server-side pins in descending priority order.
    pub pins: Vec<models::Pin>,
    /// Time the current call session started.
    pub started_at: Option<prost_types::Timestamp>,
    /// Whether the call requires encoded-frame end-to-end encryption.
    pub e2ee_enabled: bool,
    /// Capabilities currently granted to this participant.
    pub own_capabilities: Vec<String>,
    /// Latest idempotent publishing grants reported by the SFU.
    pub current_grants: Option<models::CallGrants>,
}

/// Buffers remote ICE candidates that arrive before a PeerConnection's remote
/// description is set, then releases them once it is.
///
/// webrtc-rs rejects `add_ice_candidate` before the remote description exists,
/// and the SFU trickles its candidates as soon as it receives our offer — often
/// before our `set_remote_description` runs. Dropping those candidates leaves
/// the agent with no pairs and ICE fails. The queue serializes "buffer vs add"
/// under one lock so no candidate is lost to the race (JS `SfuClient` pending
/// candidate handling).
#[derive(Default)]
struct CandidateQueue {
    inner: StdMutex<CandidateQueueInner>,
}

#[derive(Default)]
struct CandidateQueueInner {
    remote_set: bool,
    pending: Vec<RTCIceCandidateInit>,
}

#[derive(Debug)]
struct Lifecycle {
    state: CallingState,
    generation: u64,
    publish_options: ClientPublishOptions,
    generation_publish_options: ClientPublishOptions,
}

impl CandidateQueue {
    /// Offer a freshly-trickled candidate: returns the candidates to add now
    /// (the new one if the remote description is set, else none — it is buffered).
    fn offer(&self, init: RTCIceCandidateInit) -> Vec<RTCIceCandidateInit> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.remote_set {
            vec![init]
        } else {
            g.pending.push(init);
            Vec::new()
        }
    }

    /// Mark the remote description as set and return every buffered candidate to
    /// be added now. Idempotent across renegotiations.
    fn mark_ready(&self) -> Vec<RTCIceCandidateInit> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.remote_set = true;
        std::mem::take(&mut g.pending)
    }
}

/// Per-connection ICE candidate buffers for both PeerConnections.
#[derive(Default)]
struct PendingIce {
    publisher: CandidateQueue,
    subscriber: CandidateQueue,
}

/// A live SFU connection bundle. Swapped out wholesale on REJOIN/MIGRATE.
struct Connection {
    generation: u64,
    epoch: u64,
    session_id: String,
    subscriber: Arc<RTCPeerConnection>,
    publisher: Arc<RTCPeerConnection>,
    signal: SignalClient,
    sfu_sender: Arc<TokioMutex<SfuSender>>,
    credentials: Credentials,
    fast_reconnect_deadline: Duration,
    pending_ice: Arc<PendingIce>,
    /// Codec + option-id table the SFU validates each published track against.
    publish_options: Vec<models::PublishOption>,
    /// Background telemetry reporter (periodic `SendStats`).
    stats: Arc<StatsReporter>,
    ws_healthy: Arc<AtomicBool>,
    reconnect_enabled: Arc<AtomicBool>,
    signal_tasks: Vec<JoinHandle<()>>,
    /// RTCP readers belong to the publisher PC and survive FAST reconnect.
    publisher_tasks: Vec<JoinHandle<()>>,
    stats_task: JoinHandle<()>,
}

#[derive(Default)]
struct RuntimeTaskCensus {
    active: AtomicUsize,
    spawned: AtomicUsize,
    completed: AtomicUsize,
}

struct RuntimeTaskGuard {
    census: Arc<RuntimeTaskCensus>,
}

impl RuntimeTaskCensus {
    fn start(self: &Arc<Self>) -> RuntimeTaskGuard {
        self.spawned.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_add(1, Ordering::SeqCst);
        RuntimeTaskGuard {
            census: self.clone(),
        }
    }
}

impl Drop for RuntimeTaskGuard {
    fn drop(&mut self) {
        self.census.active.fetch_sub(1, Ordering::SeqCst);
        self.census.completed.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReconnectFaultPoint {
    BeforeAttempt,
    AfterPublishedRestore,
    AfterSubscribedRestore,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReconnectFault {
    strategy: ReconnectStrategy,
    point: ReconnectFaultPoint,
}

#[cfg(test)]
#[derive(Default)]
struct ReconnectProbe {
    faults: StdMutex<std::collections::VecDeque<ReconnectFault>>,
    attempts: StdMutex<Vec<ReconnectStrategy>>,
    restores: StdMutex<Vec<(ReconnectStrategy, ReconnectFaultPoint)>>,
}

#[cfg(test)]
impl ReconnectProbe {
    fn fail_once(&self, strategy: ReconnectStrategy, point: ReconnectFaultPoint) {
        self.faults
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(ReconnectFault { strategy, point });
    }

    fn observe(&self, strategy: ReconnectStrategy, point: ReconnectFaultPoint) -> Result<()> {
        match point {
            ReconnectFaultPoint::BeforeAttempt => self
                .attempts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(strategy),
            ReconnectFaultPoint::AfterPublishedRestore
            | ReconnectFaultPoint::AfterSubscribedRestore => self
                .restores
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((strategy, point)),
        }
        let mut faults = self
            .faults
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if faults
            .front()
            .is_some_and(|fault| fault.strategy == strategy && fault.point == point)
        {
            faults.pop_front();
            return Err(RtcError::Media(format!(
                "injected {strategy:?} reconnect failure at {point:?}"
            )));
        }
        Ok(())
    }
}

struct JoinSuccess {
    edge_name: String,
    retained_connection: Option<Connection>,
}

struct JoinOnceOptions<'a> {
    user_token: &'a str,
    request: &'a JoinCallRequest,
    attempt: u32,
    strategy: ReconnectStrategy,
    reconnect_details: Option<ReconnectDetails>,
    generation: u64,
    session_id: Option<String>,
    retain_old: bool,
}

struct EventLoopContext {
    core: Arc<RtcCore>,
    subscriber: Arc<RTCPeerConnection>,
    publisher: Arc<RTCPeerConnection>,
    signal: SignalClient,
    session_id: String,
    pending_ice: Arc<PendingIce>,
    generation: u64,
    ws_healthy: Arc<AtomicBool>,
    reconnect_enabled: Arc<AtomicBool>,
}

impl Connection {
    /// Drain telemetry one final time (end-of-segment events), then abort the
    /// background tasks and close the PeerConnections. The flush runs while the
    /// PeerConnections are still live so `get_stats()` can sample them.
    async fn teardown(mut self) {
        self.reconnect_enabled.store(false, Ordering::SeqCst);
        self.ws_healthy.store(false, Ordering::SeqCst);
        self.stats.stop();
        self.stats.flush().await;
        let mut tasks = std::mem::take(&mut self.signal_tasks);
        tasks.append(&mut self.publisher_tasks);
        tasks.push(self.stats_task);
        abort_tasks(tasks).await;
        let _ = self.subscriber.close().await;
        let _ = self.publisher.close().await;
    }
}

async fn abort_tasks(tasks: Vec<JoinHandle<()>>) {
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        if let Err(error) = task.await
            && !error.is_cancelled()
        {
            tracing::warn!(%error, "stream.rtc.background_task_failed");
        }
    }
}

/// Shared inner state for a call's participant path. Held by `Call` (as an
/// `Arc`) and by the background WS/ping tasks so a `leave()` on any clone tears
/// the same session down.
pub struct RtcCore {
    client: Arc<Client>,
    user_token: StdMutex<String>,
    token_source: StdMutex<Option<UserTokenSource>>,
    token_refresh: TokioMutex<()>,
    api_key: String,
    call_type: String,
    call_id: String,
    events_tx: broadcast::Sender<CallEvent>,
    lifecycle: StdMutex<Lifecycle>,
    lifecycle_changed: Notify,
    connection: TokioMutex<Option<Connection>>,
    stats_options: StdMutex<StatsOptions>,
    own_capabilities: StdMutex<HashSet<String>>,
    disconnection_timeout: StdMutex<Duration>,
    caps: StdMutex<FailureCaps>,
    rate_limiter: StdMutex<SlidingWindowRateLimiter>,
    confirmed_bad_sfus: StdMutex<Vec<String>>,
    reconnect_edge_failures: StdMutex<HashMap<String, u32>>,
    reconnect_generation: StdMutex<Option<u64>>,
    reconnect_attempts: AtomicU32,
    next_connection_epoch: AtomicU64,
    migration_waiter: StdMutex<Option<(u64, tokio::sync::oneshot::Sender<()>)>>,
    join_data: StdMutex<JoinCallData>,
    started: StdMutex<Option<Instant>>,
    /// Stable session id spanning reconnects within one join→leave lifecycle,
    /// reported as `SendStats.unified_session_id` so the dashboard correlates a
    /// participant across FAST/REJOIN/MIGRATE (JS `unifiedSessionId`).
    unified_session_id: StdMutex<String>,
    // media
    /// Fired for each inbound [`RemoteTrack`] the subscriber PC delivers.
    on_track_cb: StdMutex<Option<OnTrackCallback>>,
    /// The active subscription policy (applied once `subs_active` is set).
    sub_config: StdMutex<SubscriptionConfig>,
    /// Set once the caller opts into subscriptions via `update_subscriptions`.
    subs_active: AtomicBool,
    /// Tracks the caller explicitly dropped (unsubscribed); never re-subscribed
    /// until the publisher republishes them.
    manual_unsub: StdMutex<HashSet<TrackKey>>,
    /// Exact per-session subscriptions, or `None` while using the coarse policy.
    manual_subscriptions: StdMutex<Option<Vec<SubscriptionTarget>>>,
    /// Known participants keyed by session id (correlation + subscription build).
    roster: StdMutex<HashMap<String, RosterEntry>>,
    /// Call-level state supplied by join and incremental SFU events.
    call_state: StdMutex<CallStateCache>,
    /// Serialized publisher negotiation and retryable local publication state.
    media: TokioMutex<MediaState>,
    /// The last subscription list sent on the current connection (dedup guard).
    active_subs: StdMutex<Vec<signal::TrackSubscriptionDetails>>,
    coordinator_connection_id: StdMutex<Option<(u64, String)>>,
    coordinator_tasks: StdMutex<Vec<JoinHandle<()>>>,
    runtime_tasks: Arc<RuntimeTaskCensus>,
    #[cfg(test)]
    reconnect_probe: StdMutex<Option<Arc<ReconnectProbe>>>,
}

struct ReconnectClaim {
    core: Arc<RtcCore>,
    generation: u64,
}

impl Drop for ReconnectClaim {
    fn drop(&mut self) {
        self.core.release_reconnect(self.generation);
    }
}

impl RtcCore {
    /// Build a fresh (idle) core for a call handle.
    pub(crate) fn new(client: Arc<Client>, call_type: String, call_id: String) -> Arc<Self> {
        let (events_tx, _rx) = broadcast::channel(256);
        Arc::new(Self {
            api_key: client.api_key().to_owned(),
            client,
            user_token: StdMutex::new(String::new()),
            token_source: StdMutex::new(None),
            token_refresh: TokioMutex::new(()),
            call_type,
            call_id,
            events_tx,
            lifecycle: StdMutex::new(Lifecycle {
                state: CallingState::Idle,
                generation: 0,
                publish_options: ClientPublishOptions::default(),
                generation_publish_options: ClientPublishOptions::default(),
            }),
            lifecycle_changed: Notify::new(),
            connection: TokioMutex::new(None),
            stats_options: StdMutex::new(StatsOptions::default()),
            own_capabilities: StdMutex::new(HashSet::new()),
            disconnection_timeout: StdMutex::new(Duration::ZERO),
            caps: StdMutex::new(FailureCaps::default()),
            rate_limiter: StdMutex::new(SlidingWindowRateLimiter::rejoin_default()),
            confirmed_bad_sfus: StdMutex::new(Vec::new()),
            reconnect_edge_failures: StdMutex::new(HashMap::new()),
            reconnect_generation: StdMutex::new(None),
            reconnect_attempts: AtomicU32::new(0),
            next_connection_epoch: AtomicU64::new(0),
            migration_waiter: StdMutex::new(None),
            join_data: StdMutex::new(JoinCallData::new("")),
            started: StdMutex::new(None),
            unified_session_id: StdMutex::new(String::new()),
            on_track_cb: StdMutex::new(None),
            sub_config: StdMutex::new(SubscriptionConfig::default()),
            subs_active: AtomicBool::new(false),
            manual_unsub: StdMutex::new(HashSet::new()),
            manual_subscriptions: StdMutex::new(None),
            roster: StdMutex::new(HashMap::new()),
            call_state: StdMutex::new(CallStateCache::default()),
            media: TokioMutex::new(MediaState::default()),
            active_subs: StdMutex::new(Vec::new()),
            coordinator_connection_id: StdMutex::new(None),
            coordinator_tasks: StdMutex::new(Vec::new()),
            runtime_tasks: Arc::new(RuntimeTaskCensus::default()),
            #[cfg(test)]
            reconnect_probe: StdMutex::new(None),
        })
    }

    fn spawn_runtime_task<F>(self: &Arc<Self>, future: F) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let guard = self.runtime_tasks.start();
        tokio::spawn(async move {
            let _guard = guard;
            future.await;
        })
    }

    fn cid(&self) -> String {
        format!("{}:{}", self.call_type, self.call_id)
    }

    /// Current calling state.
    pub fn state(&self) -> CallingState {
        self.lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state
    }

    pub(crate) fn update_publish_options(&self, options: ClientPublishOptions) {
        tracing::warn!(
            cid = %self.cid(),
            "stream.rtc.publish_options.manual_override"
        );
        let mut lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(lifecycle.state, CallingState::Idle | CallingState::Left) {
            tracing::warn!(
                cid = %self.cid(),
                state = ?lifecycle.state,
                "stream.rtc.publish_options.update_after_join_has_no_effect"
            );
        }
        lifecycle.publish_options = options;
    }

    fn preferred_publish_options(&self, generation: u64) -> Result<Vec<models::PublishOption>> {
        let lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if lifecycle.generation != generation {
            return Err(join_cancelled());
        }
        Ok(lifecycle
            .generation_publish_options
            .preferred_publish_options())
    }

    pub(crate) fn set_disconnection_timeout(&self, timeout: Duration) {
        *self
            .disconnection_timeout
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = timeout;
    }

    fn set_state_if_current(&self, generation: u64, next: CallingState) -> bool {
        {
            let mut guard = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            if guard.generation != generation {
                return false;
            }
            guard.state = next;
        }
        let _ = self.events_tx.send(CallEvent::CallingStateChanged(next));
        true
    }

    fn generation(&self) -> u64 {
        self.lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .generation
    }

    fn is_generation_current(&self, generation: u64) -> bool {
        self.generation() == generation
    }

    async fn is_connection_current(&self, generation: u64, epoch: u64) -> bool {
        if !self.is_generation_current(generation) {
            return false;
        }
        self.connection
            .lock()
            .await
            .as_ref()
            .is_some_and(|connection| {
                connection.generation == generation && connection.epoch == epoch
            })
    }

    async fn while_generation<F, T>(&self, generation: u64, future: F) -> Result<T>
    where
        F: Future<Output = T>,
    {
        let changed = self.lifecycle_changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if !self.is_generation_current(generation) {
            return Err(join_cancelled());
        }
        tokio::select! {
            biased;
            () = &mut changed => Err(join_cancelled()),
            output = future => {
                if self.is_generation_current(generation) {
                    Ok(output)
                } else {
                    Err(join_cancelled())
                }
            }
        }
    }

    /// Guard against a double join (JS "call.join() shall be called only once").
    /// Transitions `Idle`/`Left` → `Joining` and starts a new generation.
    fn begin_join(&self) -> Result<u64> {
        let generation = {
            let mut guard = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            match guard.state {
                CallingState::Idle | CallingState::Left => {
                    guard.generation = guard.generation.wrapping_add(1);
                    guard.state = CallingState::Joining;
                    guard.generation_publish_options = guard.publish_options;
                    guard.generation
                }
                _ => {
                    return Err(RtcError::IllegalState(
                        "call.join() shall be called only once".to_owned(),
                    ));
                }
            }
        };
        self.lifecycle_changed.notify_waiters();
        *self
            .reconnect_generation
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        self.reconnect_attempts.store(0, Ordering::SeqCst);
        *self.caps.lock().unwrap_or_else(|e| e.into_inner()) = FailureCaps::default();
        *self.rate_limiter.lock().unwrap_or_else(|e| e.into_inner()) =
            SlidingWindowRateLimiter::rejoin_default();
        self.confirmed_bad_sfus
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.reconnect_edge_failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        Ok(generation)
    }

    fn cancel_generation(&self) -> u64 {
        let generation = {
            let mut guard = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            guard.generation = guard.generation.wrapping_add(1);
            if guard.state == CallingState::Joining {
                guard.state = CallingState::Reconnecting;
            }
            guard.generation
        };
        self.lifecycle_changed.notify_waiters();
        generation
    }

    fn claim_reconnect(&self, generation: u64) -> bool {
        let lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if lifecycle.generation != generation {
            return false;
        }
        let mut active = self
            .reconnect_generation
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if active.is_some() {
            return false;
        }
        *active = Some(generation);
        true
    }

    fn release_reconnect(&self, generation: u64) {
        let mut active = self
            .reconnect_generation
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *active == Some(generation) {
            *active = None;
        }
    }

    fn install_migration_waiter(
        &self,
        generation: u64,
        sender: tokio::sync::oneshot::Sender<()>,
    ) -> Result<()> {
        let lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if lifecycle.generation != generation {
            return Err(join_cancelled());
        }
        let mut waiter = self
            .migration_waiter
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if waiter
            .as_ref()
            .is_some_and(|(waiter_generation, _)| *waiter_generation == generation)
        {
            return Err(RtcError::IllegalState(
                "migration already pending".to_owned(),
            ));
        }
        *waiter = Some((generation, sender));
        Ok(())
    }

    fn take_migration_waiter(&self, generation: u64) -> Option<tokio::sync::oneshot::Sender<()>> {
        let mut waiter = self
            .migration_waiter
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if waiter
            .as_ref()
            .is_some_and(|(waiter_generation, _)| *waiter_generation == generation)
        {
            waiter.take().map(|(_, sender)| sender)
        } else {
            None
        }
    }

    #[cfg(test)]
    fn lifecycle_snapshot(&self) -> (CallingState, u64) {
        let guard = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        (guard.state, guard.generation)
    }

    #[cfg(test)]
    fn active_reconnect_generation(&self) -> Option<u64> {
        *self
            .reconnect_generation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    fn runtime_task_snapshot(&self) -> (usize, usize, usize) {
        (
            self.runtime_tasks.active.load(Ordering::SeqCst),
            self.runtime_tasks.spawned.load(Ordering::SeqCst),
            self.runtime_tasks.completed.load(Ordering::SeqCst),
        )
    }

    #[cfg(test)]
    fn install_reconnect_probe(&self, probe: Arc<ReconnectProbe>) {
        *self
            .reconnect_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(probe);
    }

    fn observe_reconnect(
        &self,
        strategy: ReconnectStrategy,
        point: ReconnectFaultPoint,
    ) -> Result<()> {
        #[cfg(test)]
        {
            let probe = self
                .reconnect_probe
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            if let Some(probe) = probe {
                return probe.observe(strategy, point);
            }
        }
        let _ = (strategy, point);
        Ok(())
    }

    /// Subscribe to the typed event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<CallEvent> {
        self.events_tx.subscribe()
    }

    fn user_request_query(&self) -> Option<Vec<(String, String)>> {
        let generation = self.generation();
        let (connection_generation, connection_id) = self
            .coordinator_connection_id
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()?;
        if connection_generation != generation {
            return None;
        }
        let user_id = self
            .join_data
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .user_id
            .clone();
        if connection_id.is_empty() || user_id.is_empty() {
            return None;
        }
        Some(vec![
            ("user_id".to_owned(), user_id),
            ("connection_id".to_owned(), connection_id),
        ])
    }

    pub(crate) fn user_auth(&self) -> Option<(String, Vec<(String, String)>)> {
        let query = self.user_request_query()?;
        let token = self
            .user_token
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        (!token.is_empty()).then(|| (token.clone(), query))
    }

    /// Register a callback for typed call events.
    ///
    /// Rust callers receive the full [`CallEvent`] enum and can pattern-match
    /// the variants they need. Pass the returned handle to [`Self::off`].
    pub fn on<F>(&self, callback: F) -> tokio::task::AbortHandle
    where
        F: Fn(CallEvent) + Send + 'static,
    {
        let mut events = self.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => callback(event),
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "stream.rtc.event_handler_lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .abort_handle()
    }

    /// Remove an event callback registered with [`Self::on`].
    pub fn off(&self, handler: &tokio::task::AbortHandle) {
        handler.abort();
    }

    /// The cached stats options from the last coordinator join.
    pub fn stats_options(&self) -> StatsOptions {
        self.stats_options
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The stable unified session id for the current join lifecycle.
    fn unified_session_id(&self) -> String {
        self.unified_session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The session id of the live connection, if joined.
    pub async fn session_id(&self) -> Option<String> {
        self.connection
            .lock()
            .await
            .as_ref()
            .map(|c| c.session_id.clone())
    }
}

fn required_publish_capability(track_type: TrackType) -> &'static str {
    match track_type {
        TrackType::Audio => "send-audio",
        TrackType::Video => "send-video",
        TrackType::ScreenShare | TrackType::ScreenShareAudio => "screenshare",
        TrackType::Unspecified => "send-video",
    }
}

fn grants_allow(grants: &models::CallGrants, track_type: TrackType) -> bool {
    match track_type {
        TrackType::Audio => grants.can_publish_audio,
        TrackType::Video => grants.can_publish_video,
        TrackType::ScreenShare | TrackType::ScreenShareAudio => grants.can_screenshare,
        TrackType::Unspecified => false,
    }
}

fn own_capabilities_from_event(
    event: &CoordinatorEvent,
    local_user_id: &str,
) -> Option<HashSet<String>> {
    if event.event_type != "call.permissions_updated"
        || event
            .raw
            .pointer("/user/id")
            .and_then(|value| value.as_str())
            != Some(local_user_id)
    {
        return None;
    }
    event
        .raw
        .get("own_capabilities")?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

/// A send-only transceiver init for publishing a local track.
pub(crate) fn send_only() -> RTCRtpTransceiverInit {
    RTCRtpTransceiverInit {
        direction: RTCRtpTransceiverDirection::Sendonly,
        send_encodings: vec![],
    }
}

async fn remove_sender_for_track(publisher: &Arc<RTCPeerConnection>, track_id: &str) -> Result<()> {
    for transceiver in publisher.get_transceivers().await {
        let sender = transceiver.sender().await;
        if sender
            .track()
            .await
            .is_some_and(|bound| bound.id() == track_id)
        {
            publisher.remove_track(&sender).await?;
        }
    }
    Ok(())
}

fn media_rollback_error(
    operation: &str,
    error: impl std::fmt::Display,
    rollback_error: impl std::fmt::Display,
) -> RtcError {
    RtcError::Media(format!(
        "{operation} failed: {error}; rollback failed: {rollback_error}"
    ))
}

fn is_video_type(track_type: TrackType) -> bool {
    matches!(track_type, TrackType::Video | TrackType::ScreenShare)
}

#[allow(deprecated)]
fn mark_fast_reconnect(request: &mut JoinRequest) {
    request.fast_reconnect = true;
}

fn current_mute_states_for_tracks(tracks: &[LocalTrack]) -> Vec<signal::TrackMuteState> {
    let mut by_type = HashMap::<i32, bool>::new();
    for track in tracks {
        by_type
            .entry(track.track_type() as i32)
            .and_modify(|muted| *muted &= track.is_muted())
            .or_insert_with(|| track.is_muted());
    }
    let mut states = by_type
        .into_iter()
        .map(|(track_type, muted)| signal::TrackMuteState { track_type, muted })
        .collect::<Vec<_>>();
    states.sort_by_key(|state| state.track_type);
    states
}

/// Parse an SFU media-stream id (`<track_lookup_prefix>:<track_type_num>`) into
/// its prefix and optional numeric track type (JS `Subscriber.handleOnTrack`).
fn parse_msid(stream_id: &str) -> (String, Option<i32>) {
    match stream_id.split_once(':') {
        Some((prefix, ttype)) => (prefix.to_owned(), ttype.trim().parse::<i32>().ok()),
        None => (stream_id.to_owned(), None),
    }
}

fn join_cancelled() -> RtcError {
    RtcError::IllegalState("join cancelled by leave".to_owned())
}

/// Install a process-default rustls `CryptoProvider` for the WebRTC + WSS
/// stack. Both `ring` (webrtc-rs DTLS) and `aws-lc-rs` (reqwest) are linked, so
/// rustls cannot auto-select a default and panics on first use. We pin `ring`
/// once; if a provider is already installed the call is a harmless no-op.
fn ensure_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn is_pc_healthy(state: RTCPeerConnectionState) -> bool {
    matches!(
        state,
        RTCPeerConnectionState::New
            | RTCPeerConnectionState::Connecting
            | RTCPeerConnectionState::Connected
    )
}

/// Milliseconds since the process started (monotonic-ish), for the rate limiter.
fn elapsed_ms() -> u64 {
    use std::sync::OnceLock;
    static BASE: OnceLock<Instant> = OnceLock::new();
    let base = BASE.get_or_init(Instant::now);
    base.elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests;
