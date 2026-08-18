//! Client telemetry: periodic `SendStats` carrying the tracer rollup plus
//! per-PeerConnection `getStats`, so the Stream dashboard's per-participant
//! Events Timeline populates.
//!
//! Ported from stream-video-js `stats/SfuStatsReporter.ts`. The reporter drains
//! the signal / publisher / subscriber [`Tracer`]s into the `rtc_stats` JSON
//! array (ICE / track / PC-state / RPC events), samples each PeerConnection's
//! `get_stats()` into `{subscriber,publisher}_stats` plus a `getstats` trace
//! record, and posts a fully-populated [`SendStatsRequest`] over the Twirp
//! signal client. The loop is interval-driven (no busy-wait) and is drained one
//! final time on leave / reconnect so end-of-call events are recorded.
//!
//! Cadence is driven by the coordinator's cached `stats_options`
//! (`reporting_interval_ms`); when the coordinator leaves it unset we default it
//! ON at a low interval (see [`DEFAULT_REPORTING_INTERVAL_MS`]) so a backend SDK
//! session still emits telemetry.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Mutex as TokioMutex;
use webrtc::peer_connection::RTCPeerConnection;

use super::coordinator::StatsOptions;
use super::error::Result;
use super::identity;
use super::proto::signal::SendStatsRequest;
use super::signal::SignalClient;
use super::tracer::{TraceRecord, Tracer, now_ms};

/// Default stats reporting cadence (ms) used when the coordinator does not
/// enable periodic reporting (`stats_options.reporting_interval_ms <= 0`).
///
/// The JS SDK only reports when the coordinator sets a positive interval. This
/// SDK deliberately defaults it ON at a low cadence so the dashboard's
/// per-participant Events Timeline populates for backend sessions even when the
/// coordinator leaves the interval unset. The loop stays fully async
/// (interval-driven, no busy-wait) regardless.
pub const DEFAULT_REPORTING_INTERVAL_MS: u64 = 2000;

/// Bound on a single `get_stats()` call so a degraded or closing
/// PeerConnection cannot block a report or the final flush (JS time-boxes the
/// flush sampling to 2s).
const GET_STATS_TIMEOUT: Duration = Duration::from_secs(2);

/// Choose the stats reporting cadence from the cached coordinator options.
///
/// Uses `reporting_interval_ms` when it is positive; otherwise defaults ON at
/// [`DEFAULT_REPORTING_INTERVAL_MS`] (see the const docs for the rationale).
pub fn reporting_interval(options: &StatsOptions) -> Duration {
    if options.reporting_interval_ms > 0 {
        Duration::from_millis(options.reporting_interval_ms as u64)
    } else {
        Duration::from_millis(DEFAULT_REPORTING_INTERVAL_MS)
    }
}

/// The pieces the [`StatsReporter`] needs from one live SFU connection.
pub(crate) struct StatsReporterParts {
    pub signal: SignalClient,
    pub publisher: Arc<RTCPeerConnection>,
    pub subscriber: Arc<RTCPeerConnection>,
    pub signal_tracer: Arc<Tracer>,
    pub publisher_tracer: Arc<Tracer>,
    pub subscriber_tracer: Arc<Tracer>,
    pub session_id: String,
    pub unified_session_id: String,
    pub interval: Duration,
}

/// Drains the tracers + samples getStats and posts `SendStats` on a cadence.
pub(crate) struct StatsReporter {
    signal: SignalClient,
    publisher: Arc<RTCPeerConnection>,
    subscriber: Arc<RTCPeerConnection>,
    signal_tracer: Arc<Tracer>,
    publisher_tracer: Arc<Tracer>,
    subscriber_tracer: Arc<Tracer>,
    session_id: String,
    unified_session_id: String,
    interval: Duration,
    stopped: AtomicBool,
    /// Serializes the periodic loop against an explicit flush so overlapping
    /// sends never race on the trace buffers.
    send_lock: TokioMutex<()>,
    /// Accumulated telemetry-of-telemetry for the live integration test: the
    /// tags sent in successful reports, the success count, and the last SFU
    /// error. Test-only so production carries no extra bookkeeping.
    #[cfg(test)]
    observed: std::sync::Mutex<TestObservations>,
}

/// What the live test inspects about the reports this reporter has sent.
#[cfg(test)]
#[derive(Default, Clone)]
pub(crate) struct TestObservations {
    /// Distinct `rtc_stats` tags across all successful sends.
    pub sent_tags: std::collections::HashSet<String>,
    /// Number of `SendStats` RPCs that returned success (200, no Twirp/app error).
    pub success_count: usize,
    /// The most recent `SendStats` error, if any (the exact Twirp/app message).
    pub last_error: Option<String>,
}

impl StatsReporter {
    pub(crate) fn new(parts: StatsReporterParts) -> Self {
        Self {
            signal: parts.signal,
            publisher: parts.publisher,
            subscriber: parts.subscriber,
            signal_tracer: parts.signal_tracer,
            publisher_tracer: parts.publisher_tracer,
            subscriber_tracer: parts.subscriber_tracer,
            session_id: parts.session_id,
            unified_session_id: parts.unified_session_id,
            interval: parts.interval,
            stopped: AtomicBool::new(false),
            send_lock: TokioMutex::new(()),
            #[cfg(test)]
            observed: std::sync::Mutex::new(TestObservations::default()),
        }
    }

    /// Snapshot of what has been reported so far (live integration test only).
    #[cfg(test)]
    pub(crate) fn observations(&self) -> TestObservations {
        self.observed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The reporting cadence.
    pub(crate) fn interval(&self) -> Duration {
        self.interval
    }

    /// Stop the periodic loop (checked at the next tick). Idempotent.
    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Sample both PeerConnections, drain the tracers, and post one
    /// `SendStats`. On send failure the drained event traces are restored so
    /// they ride the next report (append-only rollback, matching JS); the
    /// getStats records are recomputed each cycle and need no rollback.
    pub(crate) async fn report_once(&self) -> Result<()> {
        let _guard = self.send_lock.lock().await;

        tracing::debug!(session_id = %self.session_id, "stream.rtc.stats.report_start");
        let (subscriber_stats, subscriber_flat) = collect_stats(&self.subscriber).await;
        let (publisher_stats, publisher_flat) = collect_stats(&self.publisher).await;
        let ts = now_ms();

        let signal_traces = self.signal_tracer.take();
        let publisher_traces = self.publisher_tracer.take();
        let subscriber_traces = self.subscriber_tracer.take();

        let mut rtc: Vec<TraceRecord> = Vec::with_capacity(
            signal_traces.len() + publisher_traces.len() + subscriber_traces.len() + 2,
        );
        rtc.extend(signal_traces.iter().cloned());
        rtc.extend(publisher_traces.iter().cloned());
        rtc.extend(subscriber_traces.iter().cloned());
        rtc.push(getstats_record(
            self.publisher_tracer.id(),
            publisher_flat,
            ts,
        ));
        rtc.push(getstats_record(
            self.subscriber_tracer.id(),
            subscriber_flat,
            ts,
        ));

        let rtc_stats = serde_json::to_string(&rtc).unwrap_or_else(|_| "[]".to_owned());

        let request = SendStatsRequest {
            session_id: self.session_id.clone(),
            unified_session_id: self.unified_session_id.clone(),
            subscriber_stats,
            publisher_stats,
            webrtc_version: identity::WEBRTC_VERSION.to_owned(),
            sdk: identity::sdk_name().to_owned(),
            sdk_version: identity::sdk_version(),
            rtc_stats,
            ..Default::default()
        };

        match self.signal.send_stats(request).await {
            Ok(_) => {
                #[cfg(test)]
                {
                    let mut obs = self.observed.lock().unwrap_or_else(|e| e.into_inner());
                    obs.success_count += 1;
                    for record in &rtc {
                        obs.sent_tags.insert(record.tag().to_owned());
                    }
                }
                Ok(())
            }
            Err(e) => {
                #[cfg(test)]
                {
                    self.observed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .last_error = Some(e.to_string());
                }
                self.subscriber_tracer.restore(subscriber_traces);
                self.publisher_tracer.restore(publisher_traces);
                self.signal_tracer.restore(signal_traces);
                Err(e)
            }
        }
    }

    /// Best-effort final flush (leave / reconnect swap): drains remaining
    /// traces and posts one last `SendStats`. Never propagates errors — call
    /// teardown must not hinge on the SFU accepting a final report.
    pub(crate) async fn flush(&self) {
        if let Err(e) = self.report_once().await {
            tracing::debug!(error = %e, "stream.rtc.stats.flush_failed");
        }
    }
}

/// The periodic reporting loop. Interval-driven (no busy-wait); one report per
/// tick, stopping cleanly after [`StatsReporter::stop`]. Task-aborted on
/// connection teardown as a backstop.
pub(crate) async fn run(reporter: Arc<StatsReporter>) {
    tracing::debug!(
        interval_ms = reporter.interval().as_millis() as u64,
        "stream.rtc.stats.loop_started"
    );
    let mut interval = tokio::time::interval(reporter.interval());
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first tick so the first report lands after one
    // interval, by which time some ICE / track events have accrued.
    interval.tick().await;
    loop {
        interval.tick().await;
        if reporter.is_stopped() {
            return;
        }
        if let Err(e) = reporter.report_once().await {
            tracing::debug!(error = %e, "stream.rtc.stats.report_failed");
        }
    }
}

/// Build a `["getstats", id, <flattened stats>, ts]` record. The SFU applies
/// the payload onto its running stats accumulator each cycle.
fn getstats_record(id: Option<&str>, flattened: Value, ts: i64) -> TraceRecord {
    TraceRecord("getstats".to_owned(), id.map(str::to_owned), flattened, ts)
}

/// Collect `pc.get_stats()` (time-boxed) as `(json_string, flattened_value)`.
///
/// The string form fills `SendStats.{subscriber,publisher}_stats`; the
/// flattened value rides in the `getstats` `rtc_stats` record. `flatten`
/// mirrors JS: the stats-report map becomes an array of stat objects.
async fn collect_stats(pc: &Arc<RTCPeerConnection>) -> (String, Value) {
    let report = match tokio::time::timeout(GET_STATS_TIMEOUT, pc.get_stats()).await {
        Ok(report) => report,
        Err(_) => {
            tracing::debug!("stream.rtc.stats.get_stats_timed_out");
            return ("[]".to_owned(), Value::Array(Vec::new()));
        }
    };
    let value = serde_json::to_value(&report).unwrap_or(Value::Null);
    let flattened = match value {
        Value::Object(map) => Value::Array(map.into_values().collect()),
        other => other,
    };
    let as_string = serde_json::to_string(&flattened).unwrap_or_else(|_| "[]".to_owned());
    (as_string, flattened)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cadence_uses_coordinator_interval_when_positive() {
        let options = StatsOptions {
            reporting_interval_ms: 5000,
            enable_rtc_stats: true,
        };
        assert_eq!(reporting_interval(&options), Duration::from_millis(5000));
    }

    #[test]
    fn cadence_defaults_on_when_coordinator_disables() {
        // reporting_interval_ms == 0 (the videosdk default) still yields a
        // sane, positive cadence so telemetry is emitted.
        let options = StatsOptions::default();
        assert_eq!(options.reporting_interval_ms, 0);
        assert_eq!(
            reporting_interval(&options),
            Duration::from_millis(DEFAULT_REPORTING_INTERVAL_MS)
        );
    }

    #[test]
    fn cadence_defaults_on_for_negative_interval() {
        let options = StatsOptions {
            reporting_interval_ms: -1,
            enable_rtc_stats: false,
        };
        assert_eq!(
            reporting_interval(&options),
            Duration::from_millis(DEFAULT_REPORTING_INTERVAL_MS)
        );
    }

    #[test]
    fn getstats_record_shape_matches_wire_format() {
        let record = getstats_record(Some("publisher"), json!([{ "type": "outbound-rtp" }]), 4242);
        let value = serde_json::to_value(&record).expect("serialize");
        assert_eq!(value[0], json!("getstats"));
        assert_eq!(value[1], json!("publisher"));
        assert_eq!(value[2], json!([{ "type": "outbound-rtp" }]));
        assert_eq!(value[3], json!(4242));
    }

    /// Live end-to-end telemetry probe (skips cleanly without credentials).
    ///
    /// A publishes an Opus tone; B joins, subscribes, and receives it. After the
    /// stats loop has run for a few seconds we assert that:
    ///   1. the periodic `SendStats` RPC succeeded at least once (200 / no Twirp
    ///      or application error) on both the publisher and subscriber sessions;
    ///   2. the subscriber's tracer rollup captured ICE **and** track events
    ///      (`iceconnectionstatechange` / `onicecandidate` + `ontrack`).
    ///
    /// If the live SFU rejects `SendStats`, the exact error is surfaced (panic)
    /// rather than hidden. The call/session ids are printed so the user can
    /// confirm the dashboard per-participant Events Timeline populated.
    #[tokio::test]
    async fn live_send_stats_populates_dashboard_timeline() {
        use std::f64::consts::PI;
        use std::time::Duration;

        use crate::rtc::proto::models::TrackType;
        use crate::rtc::{
            JoinCallData, LocalAudioTrack, PcmFrame, RemoteTrack, SubscriptionConfig,
        };

        let _ = dotenvy::dotenv();
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
            )
            .with_test_writer()
            .try_init();
        let key = std::env::var("STREAM_API_KEY").unwrap_or_default();
        let secret = std::env::var("STREAM_API_SECRET").unwrap_or_default();
        if key.is_empty() || secret.is_empty() {
            eprintln!("SKIP: STREAM creds absent; skipping live SendStats telemetry test");
            return;
        }

        let stream = crate::Stream::new(key, secret).expect("client");
        let user_a = format!("rust-stats-a-{}", uuid::Uuid::new_v4().simple());
        let user_b = format!("rust-stats-b-{}", uuid::Uuid::new_v4().simple());
        let call_id = format!("rust-stats-call-{}", uuid::Uuid::new_v4().simple());

        stream
            .upsert_users([
                crate::models::UserRequest::new(&user_a),
                crate::models::UserRequest::new(&user_b),
            ])
            .await
            .expect("upsert_users");

        let admin = stream.video().call("default", &call_id);
        admin
            .get_or_create(crate::models::GetOrCreateCallRequest {
                data: Some(crate::models::CallRequest {
                    created_by_id: Some(user_a.clone()),
                    members: Some(vec![
                        crate::models::MemberRequest::new(&user_a),
                        crate::models::MemberRequest::new(&user_b),
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .expect("get_or_create");

        let call_a = stream.video().call("default", &call_id);
        let call_b = stream.video().call("default", &call_id);

        let outcome = tokio::time::timeout(Duration::from_secs(120), async {
            // A publishes a continuous 440 Hz tone.
            call_a
                .join(JoinCallData::new(&user_a))
                .await
                .expect("A join");
            let audio = LocalAudioTrack::opus().expect("opus track");
            call_a
                .publish_audio(audio.clone())
                .await
                .expect("A publish");
            let feeder = tokio::spawn(async move {
                const SR: u32 = 48_000;
                const FRAME: usize = (SR as usize) / 50;
                let mut n: u64 = 0;
                let mut next = |count: usize| {
                    let mut s = Vec::with_capacity(count);
                    for _ in 0..count {
                        let t = n as f64 / f64::from(SR);
                        s.push((12_000.0 * (2.0 * PI * 440.0 * t).sin()) as i16);
                        n += 1;
                    }
                    PcmFrame::mono(s, SR)
                };
                if audio.write_pcm(next(FRAME * 10)).await.is_err() {
                    return;
                }
                let mut interval = tokio::time::interval(Duration::from_millis(20));
                loop {
                    interval.tick().await;
                    if audio.write_pcm(next(FRAME)).await.is_err() {
                        return;
                    }
                }
            });

            // B subscribes and keeps received tracks alive so media keeps
            // flowing (and ICE stays up) while the stats loop reports.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RemoteTrack>();
            call_b.on_track(move |t| {
                let _ = tx.send(t);
            });
            call_b
                .join(JoinCallData::new(&user_b))
                .await
                .expect("B join");
            call_b
                .update_subscriptions(SubscriptionConfig::audio_all())
                .await
                .expect("B update_subscriptions");

            // Wait for B to actually receive A's audio track.
            let mut held: Vec<RemoteTrack> = Vec::new();
            let got_track = tokio::time::timeout(Duration::from_secs(45), async {
                while let Some(track) = rx.recv().await {
                    let is_target = track.participant().user_id == user_a
                        && track.track_type() == TrackType::Audio;
                    held.push(track);
                    if is_target {
                        return true;
                    }
                }
                false
            })
            .await
            .unwrap_or(false);
            assert!(
                got_track,
                "B never received A's audio (subscription/ICE/RTP stage)"
            );

            // Give ICE/track traces a moment to settle, then drive one report on
            // each side deterministically (the periodic loop also runs, but its
            // cadence is coordinator-driven, so we don't rely on it firing within
            // the test window). Any SFU rejection surfaces here verbatim.
            tokio::time::sleep(Duration::from_secs(2)).await;
            let a_report = tokio::time::timeout(
                Duration::from_secs(15),
                call_a.rtc_core().force_stats_report(),
            )
            .await
            .expect("publisher SendStats timed out")
            .expect("A connected");
            let b_report = tokio::time::timeout(
                Duration::from_secs(15),
                call_b.rtc_core().force_stats_report(),
            )
            .await
            .expect("subscriber SendStats timed out")
            .expect("B connected");

            let cid = format!("default:{call_id}");
            let a_session = call_a.session_id().await.unwrap_or_default();
            let b_session = call_b.session_id().await.unwrap_or_default();
            let a_obs = call_a
                .rtc_core()
                .stats_observations()
                .await
                .expect("A connected");
            let b_obs = call_b
                .rtc_core()
                .stats_observations()
                .await
                .expect("B connected");

            feeder.abort();
            drop(held);
            (cid, a_session, b_session, a_report, b_report, a_obs, b_obs)
        })
        .await;

        let _ = call_a.leave().await;
        let _ = call_b.leave().await;
        let _ = admin
            .delete(crate::models::DeleteCallRequest { hard: Some(true) })
            .await;

        let (cid, a_session, b_session, a_report, b_report, a_obs, b_obs) =
            outcome.expect("telemetry test timed out (120s guard)");

        eprintln!(
            "DASHBOARD CHECK: cid={cid} publisher_session={a_session} subscriber_session={b_session}"
        );
        eprintln!(
            "  A(publisher): report={a_report:?} sends_ok={} last_error={:?}",
            a_obs.success_count, a_obs.last_error
        );
        eprintln!(
            "  B(subscriber): report={b_report:?} sends_ok={} last_error={:?} tags={:?}",
            b_obs.success_count, b_obs.last_error, b_obs.sent_tags
        );

        // Surface any live SFU rejection verbatim rather than hiding it.
        a_report.expect("publisher SendStats rejected by live SFU");
        b_report.expect("subscriber SendStats rejected by live SFU");

        assert!(
            a_obs.success_count >= 1,
            "publisher SendStats never succeeded"
        );
        assert!(
            b_obs.success_count >= 1,
            "subscriber SendStats never succeeded"
        );

        // The subscriber's rollup must carry ICE + track events.
        let has_ice = b_obs.sent_tags.contains("iceconnectionstatechange")
            || b_obs.sent_tags.contains("onicecandidate");
        assert!(
            has_ice,
            "subscriber rollup missing ICE events; tags={:?}",
            b_obs.sent_tags
        );
        assert!(
            b_obs.sent_tags.contains("ontrack"),
            "subscriber rollup missing `ontrack`; tags={:?}",
            b_obs.sent_tags
        );
    }

    /// Live proof of the shipped stats default.
    ///
    /// The SDK follows the coordinator's `reporting_interval_ms` and, when the
    /// coordinator omits it, defaults reporting ON at
    /// [`DEFAULT_REPORTING_INTERVAL_MS`] so a backend participant still populates
    /// the dashboard. This joins a live call, records the coordinator's returned
    /// `stats_options`, and proves the periodic loop sends at least one
    /// `SendStats` on its own cadence — it never calls `force_stats_report`.
    #[tokio::test]
    async fn live_default_stats_cadence_reports_without_forcing() {
        use std::f64::consts::PI;
        use std::time::Duration;

        use crate::rtc::{JoinCallData, LocalAudioTrack, PcmFrame};

        let _ = dotenvy::dotenv();
        let key = std::env::var("STREAM_API_KEY").unwrap_or_default();
        let secret = std::env::var("STREAM_API_SECRET").unwrap_or_default();
        if key.is_empty() || secret.is_empty() {
            eprintln!("SKIP: STREAM creds absent; skipping live default-cadence stats test");
            return;
        }

        let stream = crate::Stream::new(key, secret).expect("client");
        let user_a = format!("rust-statsdef-a-{}", uuid::Uuid::new_v4().simple());
        let call_id = format!("rust-statsdef-call-{}", uuid::Uuid::new_v4().simple());
        stream
            .upsert_users([crate::models::UserRequest::new(&user_a)])
            .await
            .expect("upsert_users");
        let admin = stream.video().call("default", &call_id);
        admin
            .get_or_create(crate::models::GetOrCreateCallRequest {
                data: Some(crate::models::CallRequest {
                    created_by_id: Some(user_a.clone()),
                    members: Some(vec![crate::models::MemberRequest::new(&user_a)]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .expect("get_or_create");

        let call_a = stream.video().call("default", &call_id);
        let outcome = tokio::time::timeout(Duration::from_secs(90), async {
            call_a
                .join(JoinCallData::new(&user_a))
                .await
                .expect("A join");
            let audio = LocalAudioTrack::opus().expect("opus track");
            call_a
                .publish_audio(audio.clone())
                .await
                .expect("A publish");
            let feeder = tokio::spawn(async move {
                const SR: u32 = 48_000;
                const FRAME: usize = (SR as usize) / 50;
                let mut n: u64 = 0;
                let mut next = |count: usize| {
                    let mut s = Vec::with_capacity(count);
                    for _ in 0..count {
                        let t = n as f64 / f64::from(SR);
                        s.push((12_000.0 * (2.0 * PI * 440.0 * t).sin()) as i16);
                        n += 1;
                    }
                    PcmFrame::mono(s, SR)
                };
                if audio.write_pcm(next(FRAME * 10)).await.is_err() {
                    return;
                }
                let mut interval = tokio::time::interval(Duration::from_millis(20));
                loop {
                    interval.tick().await;
                    if audio.write_pcm(next(FRAME)).await.is_err() {
                        return;
                    }
                }
            });

            let options = call_a.rtc_core().stats_options();
            let effective = reporting_interval(&options);
            // The shipped default keeps stats ON: the effective cadence stays
            // positive even when the coordinator omits (0) the interval.
            assert!(
                !effective.is_zero(),
                "effective stats cadence must be positive (default ON); \
                 coordinator reporting_interval_ms={}",
                options.reporting_interval_ms
            );

            // Wait for the periodic loop to emit on its own — never force it. The
            // first report lands ~one interval after join.
            let wait = (effective * 2 + Duration::from_secs(2)).min(Duration::from_secs(30));
            let deadline = tokio::time::Instant::now() + wait;
            let mut success = 0usize;
            while tokio::time::Instant::now() < deadline {
                success = call_a
                    .rtc_core()
                    .stats_observations()
                    .await
                    .map(|observations| observations.success_count)
                    .unwrap_or(0);
                if success >= 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }

            feeder.abort();
            (options.reporting_interval_ms, effective, success)
        })
        .await;

        let _ = call_a.leave().await;
        let _ = admin
            .delete(crate::models::DeleteCallRequest { hard: Some(true) })
            .await;

        let (coordinator_interval_ms, effective, success) =
            outcome.expect("default-cadence stats test timed out (90s guard)");
        eprintln!(
            "STATS DEFAULT: coordinator reporting_interval_ms={coordinator_interval_ms} \
             effective_ms={} periodic_success={success}",
            effective.as_millis()
        );
        assert!(
            success >= 1,
            "periodic stats loop never sent on its own default cadence \
             (coordinator reporting_interval_ms={coordinator_interval_ms}, effective_ms={})",
            effective.as_millis()
        );
    }
}
