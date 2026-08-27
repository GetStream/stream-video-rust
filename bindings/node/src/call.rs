//! [`NativeCall`] — the Node-facing SFU participant handle.
//!
//! This is a thin projection of [`getstream::Call`]: join, leave, reconnect,
//! token minting, and media handling all stay in the Rust core. The only state
//! owned here is the pair of pull queues (events, inbound tracks) that let
//! JavaScript await native work without a callback bridge.

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use napi::bindgen_prelude::{Either, Env, Null, PromiseRaw};
use napi_derive::napi;
use serde::Deserialize;
use tokio::sync::{Mutex as TokioMutex, broadcast, mpsc, watch};

use getstream::Call;
use getstream::models::{CallRequest, RequestPermissionRequest};
use getstream::rtc::proto::models::TrackType;
use getstream::rtc::{
    CallEvent, ClientPublishOptions, JoinCallData, LocalTrack, PreferredVideoCodec, RemoteTrack,
    SubscriptionConfig, SubscriptionTarget,
};

use crate::error::{invalid_argument, rtc_error, sdk_error};
use crate::json::{event_json, state_json, stats_json};
use crate::parse_track_type;
use crate::tracks::{NativeLocalAudioTrack, NativeLocalVideoTrack, NativeRemoteTrack};

/// Inbound tracks buffered before JavaScript pulls them. Once full, the newly
/// arriving track is dropped and counted in `droppedRemoteTracks`.
const TRACK_QUEUE_CAP: usize = 32;
const MAX_JS_SAFE_INTEGER: u64 = (1 << 53) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnqueueResult {
    Enqueued,
    DroppedFull { total: u64 },
    DroppedClosed,
}

fn saturating_add_counter(counter: &AtomicU64, increment: u64) -> u64 {
    let previous = counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(increment).min(MAX_JS_SAFE_INTEGER))
        })
        .unwrap_or_else(|current| current);
    previous.saturating_add(increment).min(MAX_JS_SAFE_INTEGER)
}

fn enqueue_bounded<T>(
    sender: &mpsc::Sender<T>,
    value: T,
    dropped_full: &AtomicU64,
) -> EnqueueResult {
    match sender.try_send(value) {
        Ok(()) => EnqueueResult::Enqueued,
        Err(mpsc::error::TrySendError::Full(_)) => EnqueueResult::DroppedFull {
            total: saturating_add_counter(dropped_full, 1),
        },
        Err(mpsc::error::TrySendError::Closed(_)) => EnqueueResult::DroppedClosed,
    }
}

async fn recv_broadcast_or_closed<T: Clone>(
    receiver: &mut broadcast::Receiver<T>,
    closed: &mut watch::Receiver<bool>,
) -> Option<Result<T, broadcast::error::RecvError>> {
    tokio::select! {
        biased;
        received = receiver.recv() => Some(received),
        _ = closed.wait_for(|value| *value) => None,
    }
}

async fn recv_mpsc_or_closed<T>(
    receiver: &mut mpsc::Receiver<T>,
    closed: &mut watch::Receiver<bool>,
) -> Option<T> {
    tokio::select! {
        biased;
        value = receiver.recv() => value,
        _ = closed.wait_for(|value| *value) => None,
    }
}

fn queue_overflow_event(queue: &str, dropped: u64, total_dropped: u64) -> String {
    serde_json::json!({
        "type": "queueOverflow",
        "queue": queue,
        "dropped": dropped,
        "totalDropped": total_dropped,
    })
    .to_string()
}

fn nullable<T>(value: Option<T>) -> Either<T, Null> {
    match value {
        Some(value) => Either::A(value),
        None => Either::B(Null),
    }
}

#[napi]
pub struct NativeCall {
    call: Call,
    events: Arc<TokioMutex<broadcast::Receiver<CallEvent>>>,
    tracks: Arc<TokioMutex<mpsc::Receiver<RemoteTrack>>>,
    dropped_remote_tracks: Arc<AtomicU64>,
    lagged_events: Arc<AtomicU64>,
    /// Flipped by `leave` so pending `nextEvent` / `nextRemoteTrack` promises
    /// resolve instead of outliving the call: the core's event sender lives as
    /// long as this handle, so its queues never close on their own.
    closed: watch::Sender<bool>,
}

impl NativeCall {
    pub(crate) fn new(call: Call) -> Self {
        // Subscribe before the call is ever joined so no event between join
        // start and the first `nextEvent()` can be missed.
        let events = call.subscribe();
        let (tx, rx) = mpsc::channel(TRACK_QUEUE_CAP);
        let dropped_remote_tracks = Arc::new(AtomicU64::new(0));
        let overflow = dropped_remote_tracks.clone();
        call.on_track(move |track| match enqueue_bounded(&tx, track, &overflow) {
            EnqueueResult::Enqueued => {}
            EnqueueResult::DroppedFull { total } => tracing::warn!(
                dropped_new_arrivals = 1,
                total_dropped = total,
                capacity = TRACK_QUEUE_CAP,
                drop_policy = "drop_new_arrival",
                "stream.rtc.node.remote_track_queue_overflow"
            ),
            EnqueueResult::DroppedClosed => {
                tracing::debug!("stream.rtc.node.remote_track_queue_closed");
            }
        });
        Self {
            call,
            events: Arc::new(TokioMutex::new(events)),
            tracks: Arc::new(TokioMutex::new(rx)),
            dropped_remote_tracks,
            lagged_events: Arc::new(AtomicU64::new(0)),
            closed: watch::channel(false).0,
        }
    }

    /// A receiver that resolves once the call has been left.
    fn closed(&self) -> watch::Receiver<bool> {
        self.closed.subscribe()
    }
}

impl Drop for NativeCall {
    fn drop(&mut self) {
        // Finalizers cannot await core teardown; only wake detached readers.
        self.closed.send_replace(true);
    }
}

/// `JoinCallOptions` as sent by `StreamCall.join`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct JoinOptions {
    user_id: String,
    create: bool,
    data: Option<CallRequest>,
    ring: bool,
    notify: bool,
    video: bool,
    location: Option<String>,
    /// Applied through `updatePublishOptions` before join; accepted here so the
    /// same options object round-trips without a deserialization error.
    #[allow(dead_code)]
    preferred_video_codec: Option<String>,
    max_join_retries: Option<u32>,
    join_response_timeout_ms: Option<f64>,
    rpc_request_timeout_ms: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct SubscriptionConfigOptions {
    audio: Option<bool>,
    video: Option<bool>,
    screen_share: Option<bool>,
    video_dimension: Option<Dimension>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubscriptionTargetOptions {
    session_id: String,
    track_type: String,
    dimension: Option<Dimension>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Dimension {
    width: u32,
    height: u32,
}

impl Dimension {
    fn validate(self, field: &str) -> napi::Result<(u32, u32)> {
        if self.width == 0 || self.height == 0 {
            return Err(invalid_argument(format!(
                "{field} width and height must both be greater than zero"
            )));
        }
        Ok((self.width, self.height))
    }
}

fn timeout_from_ms(value: f64, field: &str) -> napi::Result<Duration> {
    if !value.is_finite() || value <= 0.0 {
        return Err(invalid_argument(format!(
            "{field} must be a finite number greater than zero"
        )));
    }
    let duration = Duration::try_from_secs_f64(value / 1_000.0).map_err(|_| {
        invalid_argument(format!("{field} must fit in the supported duration range"))
    })?;
    if duration.is_zero() {
        return Err(invalid_argument(format!(
            "{field} must resolve to a duration greater than zero"
        )));
    }
    Ok(duration)
}

#[napi]
impl NativeCall {
    /// Join as an SFU participant. The user token is minted inside the Rust
    /// core from the server secret and refreshed there for the join's lifetime.
    #[napi]
    pub fn join<'env>(
        &self,
        env: &'env Env,
        options_json: String,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let options: JoinOptions = serde_json::from_str(&options_json)
            .map_err(|error| invalid_argument(format!("invalid join options: {error}")))?;
        if options.user_id.is_empty() {
            return Err(invalid_argument("join requires a non-empty userId"));
        }

        let mut data = JoinCallData::new(options.user_id);
        data.create = options.create;
        data.data = options.data;
        data.ring = options.ring;
        data.notify = options.notify;
        data.video = options.video;
        data.location = options.location;
        if let Some(retries) = options.max_join_retries {
            data.max_join_retries = retries;
        }
        if let Some(value) = options.join_response_timeout_ms {
            data.join_response_timeout = timeout_from_ms(value, "joinResponseTimeoutMs")?;
        }
        if let Some(value) = options.rpc_request_timeout_ms {
            data.rpc_request_timeout = timeout_from_ms(value, "rpcRequestTimeoutMs")?;
        }

        let call = self.call.clone();
        env.spawn_future(async move { call.join(data).await.map_err(rtc_error) })
    }

    /// Leave the call and tear down both PeerConnections. Safe from any state,
    /// including mid-join, and resolves every pending native read.
    #[napi]
    pub fn leave<'env>(&self, env: &'env Env) -> napi::Result<PromiseRaw<'env, ()>> {
        let call = self.call.clone();
        let closed = self.closed.clone();
        env.spawn_future(async move {
            let result = call.leave().await.map_err(rtc_error);
            // Signal after teardown so the final `callEnded` /
            // `callingStateChanged` events still reach JavaScript.
            closed.send_replace(true);
            result
        })
    }

    /// The authoritative `RtcCallStateSnapshot`, as JSON.
    ///
    /// Asynchronous because the live session id lives behind the connection
    /// lock; the Node side caches the result so `call.state` stays synchronous.
    #[napi]
    pub async fn state_json(&self) -> String {
        let session_id = self.call.session_id().await;
        state_json(
            &self.call.call_state(),
            self.call.calling_state(),
            session_id,
        )
        .to_string()
    }

    /// A publisher/subscriber `getStats` snapshot, or `None` when disconnected.
    #[napi]
    pub async fn stats_json(&self) -> Option<String> {
        let snapshot = self.call.stats_snapshot().await?;
        Some(
            stats_json(
                &snapshot.publisher,
                &snapshot.subscriber,
                self.dropped_remote_tracks.load(Ordering::Relaxed),
            )
            .to_string(),
        )
    }

    /// Await the next call event as JSON, or `None` once the stream closes.
    ///
    /// A subscriber that falls behind the broadcast buffer receives a synthetic
    /// `queueOverflow` event rather than a silent gap.
    #[napi]
    pub fn next_event<'env>(
        &self,
        env: &'env Env,
    ) -> napi::Result<PromiseRaw<'env, Either<String, Null>>> {
        let events = Arc::clone(&self.events);
        let lagged_events = Arc::clone(&self.lagged_events);
        let mut closed = self.closed();
        env.spawn_future(async move {
            let mut events = events.lock().await;
            let Some(received) = recv_broadcast_or_closed(&mut events, &mut closed).await else {
                return Ok(Either::B(Null));
            };
            match received {
                Ok(event) => Ok(Either::A(event_json(&event).to_string())),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    let total = saturating_add_counter(&lagged_events, skipped);
                    tracing::warn!(skipped, total, "stream.rtc.node.event_queue_overflow");
                    Ok(Either::A(queue_overflow_event("events", skipped, total)))
                }
                Err(broadcast::error::RecvError::Closed) => Ok(Either::B(Null)),
            }
        })
    }

    /// Await the next inbound remote track, or `None` once the call is gone.
    #[napi]
    pub fn next_remote_track<'env>(
        &self,
        env: &'env Env,
    ) -> napi::Result<PromiseRaw<'env, Either<NativeRemoteTrack, Null>>> {
        let tracks = Arc::clone(&self.tracks);
        let mut closed = self.closed();
        env.spawn_future(async move {
            let mut tracks = tracks.lock().await;
            Ok(nullable(
                recv_mpsc_or_closed(&mut tracks, &mut closed)
                    .await
                    .map(NativeRemoteTrack::new),
            ))
        })
    }

    /// Ask the call owner for additional capabilities. Observable by other
    /// participants as a coordinator `call.permission_request` event.
    #[napi]
    pub fn request_permissions<'env>(
        &self,
        env: &'env Env,
        permissions: Vec<String>,
    ) -> napi::Result<PromiseRaw<'env, String>> {
        let call = self.call.clone();
        env.spawn_future(async move {
            let response = call
                .request_permissions(RequestPermissionRequest { permissions })
                .await
                .map_err(sdk_error)?;
            Ok(serde_json::json!({ "duration": response.duration }).to_string())
        })
    }

    /// Maximum reconnect duration. Zero reconnects indefinitely.
    #[napi]
    pub fn set_disconnection_timeout(&self, timeout_seconds: f64) -> napi::Result<()> {
        if !timeout_seconds.is_finite() || timeout_seconds < 0.0 {
            return Err(invalid_argument(
                "timeoutSeconds must be a finite, non-negative number",
            ));
        }
        let duration = Duration::try_from_secs_f64(timeout_seconds).map_err(|_| {
            invalid_argument("timeoutSeconds must fit in the supported duration range")
        })?;
        self.call.set_disconnection_timeout(duration);
        Ok(())
    }

    /// Publishing preferences for the next join generation.
    #[napi]
    pub fn update_publish_options(
        &self,
        preferred_video_codec: Option<String>,
    ) -> napi::Result<()> {
        let options = match preferred_video_codec {
            Some(codec) => {
                ClientPublishOptions::new(PreferredVideoCodec::from_str(&codec).map_err(rtc_error)?)
            }
            None => ClientPublishOptions::default(),
        };
        self.call.update_publish_options(options);
        Ok(())
    }

    /// Coarse subscription policy applied to every remote participant. The SFU
    /// forwards no media until this runs; the default policy is audio-only.
    #[napi]
    pub fn update_subscriptions<'env>(
        &self,
        env: &'env Env,
        config_json: String,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let options: SubscriptionConfigOptions = serde_json::from_str(&config_json)
            .map_err(|error| invalid_argument(format!("invalid subscription config: {error}")))?;
        let video_dimension = options
            .video_dimension
            .map(|dimension| dimension.validate("videoDimension"))
            .transpose()?;
        let config = SubscriptionConfig {
            audio: options.audio.unwrap_or(true),
            video: options.video.unwrap_or(false),
            screen_share: options.screen_share.unwrap_or(false),
            video_dimension,
        };
        let call = self.call.clone();
        env.spawn_future(async move { call.update_subscriptions(config).await.map_err(rtc_error) })
    }

    /// Subscribe to an exact set of participant-session tracks.
    #[napi]
    pub fn update_subscription_targets<'env>(
        &self,
        env: &'env Env,
        targets_json: String,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let parsed: Vec<SubscriptionTargetOptions> = serde_json::from_str(&targets_json)
            .map_err(|error| invalid_argument(format!("invalid subscription targets: {error}")))?;
        let targets = parsed
            .into_iter()
            .map(|target| {
                if target.session_id.is_empty() {
                    return Err(invalid_argument(
                        "subscription target requires a non-empty sessionId",
                    ));
                }
                let mut value = SubscriptionTarget::new(
                    target.session_id,
                    parse_track_type(&target.track_type)?,
                );
                if let Some(dimension) = target.dimension {
                    let (width, height) = dimension.validate("dimension")?;
                    value = value.with_dimension(width, height);
                }
                Ok(value)
            })
            .collect::<napi::Result<Vec<_>>>()?;
        let call = self.call.clone();
        env.spawn_future(async move {
            call.update_subscription_targets(targets)
                .await
                .map_err(rtc_error)
        })
    }

    /// Enable or disable incoming video from every remote participant.
    #[napi]
    pub fn set_incoming_video_enabled<'env>(
        &self,
        env: &'env Env,
        enabled: bool,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let call = self.call.clone();
        env.spawn_future(async move {
            call.set_incoming_video_enabled(enabled)
                .await
                .map_err(rtc_error)
        })
    }

    #[napi]
    pub fn publish_audio<'env>(
        &self,
        env: &'env Env,
        track: &NativeLocalAudioTrack,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let call = self.call.clone();
        let track = track.inner.clone();
        env.spawn_future(async move { call.publish_audio(track).await.map_err(rtc_error) })
    }

    #[napi]
    pub fn publish_video<'env>(
        &self,
        env: &'env Env,
        track: &NativeLocalVideoTrack,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let call = self.call.clone();
        let track = track.inner.clone();
        env.spawn_future(async move { call.publish_video(track).await.map_err(rtc_error) })
    }

    #[napi]
    pub fn publish_screen_share<'env>(
        &self,
        env: &'env Env,
        track: &NativeLocalVideoTrack,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let call = self.call.clone();
        let track = track.inner.clone();
        env.spawn_future(async move { call.publish_screen_share(track).await.map_err(rtc_error) })
    }

    #[napi]
    pub fn publish_screen_share_audio<'env>(
        &self,
        env: &'env Env,
        track: &NativeLocalAudioTrack,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let call = self.call.clone();
        let track = track.inner.clone();
        env.spawn_future(async move {
            call.publish_screen_share_audio(track)
                .await
                .map_err(rtc_error)
        })
    }

    /// Stop an audio publication. `trackType` distinguishes microphone audio
    /// from screen-share audio.
    #[napi]
    pub fn stop_publish_audio<'env>(
        &self,
        env: &'env Env,
        track: &NativeLocalAudioTrack,
        track_type: String,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let inner = track.inner.clone();
        let local = match parse_track_type(&track_type)? {
            TrackType::Audio => LocalTrack::Audio(inner),
            TrackType::ScreenShareAudio => LocalTrack::ScreenShareAudio(inner),
            other => {
                return Err(invalid_argument(format!(
                    "cannot stop an audio track as {}",
                    crate::track_type_name(other)
                )));
            }
        };
        let call = self.call.clone();
        env.spawn_future(async move { call.stop_publish(local).await.map_err(rtc_error) })
    }

    /// Stop a video publication as either camera video or screen share.
    #[napi]
    pub fn stop_publish_video<'env>(
        &self,
        env: &'env Env,
        track: &NativeLocalVideoTrack,
        track_type: String,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let inner = track.inner.clone();
        let track_type = match parse_track_type(&track_type)? {
            TrackType::Video => TrackType::Video,
            TrackType::ScreenShare => TrackType::ScreenShare,
            other => {
                return Err(invalid_argument(format!(
                    "cannot stop a video track as {}",
                    crate::track_type_name(other)
                )));
            }
        };
        let call = self.call.clone();
        env.spawn_future(async move {
            call.stop_publish(LocalTrack::Video {
                track: inner,
                track_type,
            })
            .await
            .map_err(rtc_error)
        })
    }

    #[napi]
    pub fn mute_track<'env>(
        &self,
        env: &'env Env,
        track_type: String,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let track_type = parse_track_type(&track_type)?;
        let call = self.call.clone();
        env.spawn_future(async move { call.mute_track(track_type).await.map_err(rtc_error) })
    }

    #[napi]
    pub fn unmute_track<'env>(
        &self,
        env: &'env Env,
        track_type: String,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let track_type = parse_track_type(&track_type)?;
        let call = self.call.clone();
        env.spawn_future(async move { call.unmute_track(track_type).await.map_err(rtc_error) })
    }

    #[napi]
    pub fn start_noise_cancellation<'env>(
        &self,
        env: &'env Env,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let call = self.call.clone();
        env.spawn_future(async move { call.start_noise_cancellation().await.map_err(rtc_error) })
    }

    #[napi]
    pub fn stop_noise_cancellation<'env>(
        &self,
        env: &'env Env,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let call = self.call.clone();
        env.spawn_future(async move { call.stop_noise_cancellation().await.map_err(rtc_error) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn bounded_queue_drops_the_new_arrival_and_counts_only_full_drops() {
        let (sender, mut receiver) = mpsc::channel(1);
        let dropped = AtomicU64::new(0);

        assert_eq!(
            enqueue_bounded(&sender, 1_u8, &dropped),
            EnqueueResult::Enqueued
        );
        assert_eq!(
            enqueue_bounded(&sender, 2_u8, &dropped),
            EnqueueResult::DroppedFull { total: 1 }
        );
        assert_eq!(receiver.try_recv(), Ok(1));
        assert_eq!(dropped.load(Ordering::Relaxed), 1);

        drop(receiver);
        assert_eq!(
            enqueue_bounded(&sender, 3_u8, &dropped),
            EnqueueResult::DroppedClosed
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn overflow_counters_saturate_instead_of_wrapping() {
        let counter = AtomicU64::new(MAX_JS_SAFE_INTEGER - 1);
        assert_eq!(saturating_add_counter(&counter, 5), MAX_JS_SAFE_INTEGER);
        assert_eq!(counter.load(Ordering::Relaxed), MAX_JS_SAFE_INTEGER);
    }

    #[tokio::test]
    async fn closed_mpsc_reader_drains_buffered_values_before_ending() {
        let (sender, mut receiver) = mpsc::channel(2);
        sender.send(1_u8).await.expect("queue first value");
        sender.send(2_u8).await.expect("queue second value");
        let (closed, _) = watch::channel(false);
        let mut closed_receiver = closed.subscribe();
        closed.send_replace(true);

        assert_eq!(
            recv_mpsc_or_closed(&mut receiver, &mut closed_receiver).await,
            Some(1)
        );
        assert_eq!(
            recv_mpsc_or_closed(&mut receiver, &mut closed_receiver).await,
            Some(2)
        );
        assert_eq!(
            recv_mpsc_or_closed(&mut receiver, &mut closed_receiver).await,
            None
        );
    }

    #[tokio::test]
    async fn closed_broadcast_reader_drains_buffered_events_before_ending() {
        let (sender, mut receiver) = broadcast::channel(2);
        sender.send(1_u8).expect("queue event");
        let (closed, _) = watch::channel(false);
        let mut closed_receiver = closed.subscribe();
        closed.send_replace(true);

        assert_eq!(
            recv_broadcast_or_closed(&mut receiver, &mut closed_receiver).await,
            Some(Ok(1))
        );
        assert_eq!(
            recv_broadcast_or_closed(&mut receiver, &mut closed_receiver).await,
            None
        );
    }

    #[tokio::test]
    async fn pending_reader_is_woken_by_terminal_close() {
        let (_sender, mut receiver) = mpsc::channel::<u8>(1);
        let (closed, _) = watch::channel(false);
        let mut closed_receiver = closed.subscribe();
        let pending =
            tokio::spawn(
                async move { recv_mpsc_or_closed(&mut receiver, &mut closed_receiver).await },
            );
        tokio::task::yield_now().await;

        closed.send_replace(true);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), pending)
                .await
                .expect("pending read should wake")
                .expect("reader task"),
            None
        );
    }

    #[tokio::test]
    async fn close_state_is_retained_for_reads_started_after_leave() {
        let (_sender, mut receiver) = mpsc::channel::<u8>(1);
        let (closed, initial_receiver) = watch::channel(false);
        drop(initial_receiver);
        closed.send_replace(true);
        let mut late_receiver = closed.subscribe();

        assert_eq!(
            recv_mpsc_or_closed(&mut receiver, &mut late_receiver).await,
            None
        );
    }

    #[tokio::test]
    async fn cancelling_a_pending_read_releases_the_queue_lock() {
        let (_sender, receiver) = mpsc::channel::<u8>(1);
        let receiver = Arc::new(TokioMutex::new(receiver));
        let task_receiver = Arc::clone(&receiver);
        let (closed, _) = watch::channel(false);
        let mut closed_receiver = closed.subscribe();
        let (locked, locked_receiver) = tokio::sync::oneshot::channel();
        let pending = tokio::spawn(async move {
            let mut receiver = task_receiver.lock().await;
            locked.send(()).expect("report acquired lock");
            recv_mpsc_or_closed(&mut receiver, &mut closed_receiver).await
        });
        locked_receiver.await.expect("reader acquired lock");

        pending.abort();
        assert!(
            pending
                .await
                .expect_err("read should be cancelled")
                .is_cancelled()
        );
        let guard = tokio::time::timeout(Duration::from_secs(1), receiver.lock())
            .await
            .expect("cancelled reader must release the queue lock");
        drop(guard);
    }

    #[test]
    fn queue_overflow_event_preserves_the_typed_camel_case_contract() {
        let value: Value =
            serde_json::from_str(&queue_overflow_event("events", 3, 8)).expect("overflow JSON");
        assert_eq!(value["type"], "queueOverflow");
        assert_eq!(value["queue"], "events");
        assert_eq!(value["dropped"], 3);
        assert_eq!(value["totalDropped"], 8);
    }

    #[test]
    fn join_options_reject_unknown_fields_and_invalid_timeouts() {
        assert!(serde_json::from_str::<JoinOptions>(r#"{"userId":"u","typo":true}"#).is_err());
        for value in [f64::MIN_POSITIVE, f64::MAX] {
            assert!(timeout_from_ms(value, "timeout").is_err());
        }
    }

    #[test]
    fn dimensions_must_be_non_zero() {
        assert!(
            Dimension {
                width: 0,
                height: 720,
            }
            .validate("videoDimension")
            .is_err()
        );
        assert_eq!(
            Dimension {
                width: 1280,
                height: 720,
            }
            .validate("videoDimension")
            .expect("valid dimension"),
            (1280, 720)
        );
    }
}
