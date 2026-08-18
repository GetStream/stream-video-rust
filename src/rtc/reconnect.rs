//! Pure decision logic for the join loop and the reconnect state machine.
//!
//! Ported from JS `Call.ts` + `coordinator/connection/utils.ts`. Everything in
//! this module is deterministic (or jitter-only) and side-effect free so it can
//! be unit-tested without a live SFU: backoff intervals, the rejoin rate
//! limiter, the ICE / negotiation failure caps, the join-retry decision, and
//! the FAST→REJOIN escalation rule. The orchestration that *acts* on these
//! decisions lives in [`super::join`].

use std::collections::VecDeque;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::proto::models::WebsocketReconnectStrategy;

/// Default cap on initial join attempts (JS `maxJoinRetries`).
pub const DEFAULT_MAX_JOIN_RETRIES: u32 = 3;
/// Rejoin/migrate rate-limit window count (JS `SlidingWindowRateLimiter(10, …)`).
pub const REJOIN_RATE_LIMIT: usize = 10;
/// Rejoin/migrate rate-limit window (JS 120_000 ms).
pub const REJOIN_RATE_WINDOW: Duration = Duration::from_secs(120);
/// ICE-never-connected failure cap before giving up (JS `maxIceFailuresWithoutConnect`).
pub const MAX_ICE_FAILURES_WITHOUT_CONNECT: u32 = 2;
/// Consecutive-negotiation-failure cap (JS `maxConsecutiveNegotiationFailures`).
pub const MAX_CONSECUTIVE_NEGOTIATION_FAILURES: u32 = 3;

/// Leave reason: rejoin/migrate rate limit exceeded.
pub const REASON_REJOIN_LIMIT: &str = "rejoin_attempt_limit_exceeded";
/// Leave reason: ICE never reached connected.
pub const REASON_ICE_UNSUPPORTED: &str = "webrtc_unsupported_network";
/// Leave reason: too many consecutive negotiation failures.
pub const REASON_NEGOTIATION_FAILURES: &str = "repeated_negotiation_failures";

/// Client-driven reconnect strategy (JS `WebsocketReconnectStrategy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectStrategy {
    /// Reuse the existing session; only re-open an unhealthy WS.
    Fast,
    /// New `session_id`, drop PCs, restore published/subscribed tracks.
    Rejoin,
    /// Join a new SFU with `migrating_from`, then close the old one.
    Migrate,
    /// Do not reconnect; leave.
    Disconnect,
}

impl ReconnectStrategy {
    /// Map from the proto enum the SFU sends in `Error` / `GoAway` events.
    pub fn from_proto(value: i32) -> Option<Self> {
        match value {
            v if v == WebsocketReconnectStrategy::Fast as i32 => Some(Self::Fast),
            v if v == WebsocketReconnectStrategy::Rejoin as i32 => Some(Self::Rejoin),
            v if v == WebsocketReconnectStrategy::Migrate as i32 => Some(Self::Migrate),
            v if v == WebsocketReconnectStrategy::Disconnect as i32 => Some(Self::Disconnect),
            _ => None,
        }
    }

    /// The wire value for this strategy (used in `ReconnectDetails`).
    pub fn as_proto(self) -> i32 {
        match self {
            Self::Fast => WebsocketReconnectStrategy::Fast as i32,
            Self::Rejoin => WebsocketReconnectStrategy::Rejoin as i32,
            Self::Migrate => WebsocketReconnectStrategy::Migrate as i32,
            Self::Disconnect => WebsocketReconnectStrategy::Disconnect as i32,
        }
    }

    /// REJOIN and MIGRATE are counted by the rejoin rate limiter and increment
    /// `reconnect_attempts`; FAST is not (JS).
    pub fn is_rate_limited(self) -> bool {
        matches!(self, Self::Rejoin | Self::Migrate)
    }
}

/// Full-jitter backoff between retries (JS `retryInterval`):
///
/// ```text
/// max = min(500 + n * 2000, 5000)
/// min = min(max(250, (n - 1) * 2000), 5000)
/// delay = uniform(min, max)
/// ```
///
/// Used for both the initial-join loop and reconnect sleeps.
pub fn retry_interval(n: u32) -> Duration {
    let n = i64::from(n);
    let max = (500 + n * 2000).min(5000);
    let min = ((n - 1) * 2000).clamp(250, 5000);
    let span = (max - min).max(0) as u64;
    let delay = min as u64 + full_jitter(span);
    Duration::from_millis(delay)
}

/// Uniform pseudo-random value in `[0, span]`, seeded from the wall clock so we
/// avoid pulling in a `rand` dependency (same trick as the HTTP retry backoff).
fn full_jitter(span: u64) -> u64 {
    if span == 0 {
        return 0;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    nanos % (span + 1)
}

/// Outcome of evaluating a failed **initial join** attempt (JS `Call.join`
/// for-loop). Pure: no sleeping happens here, the caller sleeps for `delay`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinAttemptOutcome {
    /// Unrecoverable — rethrow immediately, do not sleep.
    Abort,
    /// Retry budget exhausted — surface the last error.
    Exhausted,
    /// Sleep `delay`, then retry. `switch_sfu` forces `migrating_from` so the
    /// coordinator hands back a different edge.
    Retry {
        /// How long to back off before the next attempt.
        delay: Duration,
        /// Whether to migrate away from the current edge.
        switch_sfu: bool,
    },
}

/// Decide what to do after join attempt `attempt` (0-based) failed.
///
/// - `unrecoverable`: the error is `ErrorFromResponse.unrecoverable` or an SFU
///   `DISCONNECT` — abort with no sleep.
/// - `is_join_error_code`: SFU_FULL / SFU_SHUTTING_DOWN / limit — force an edge switch.
/// - `edge_failures`: failures seen on the current edge *including this one*;
///   ≥ 2 also forces a switch.
pub fn evaluate_join_failure(
    unrecoverable: bool,
    is_join_error_code: bool,
    edge_failures: u32,
    attempt: u32,
    max_retries: u32,
) -> JoinAttemptOutcome {
    if unrecoverable {
        return JoinAttemptOutcome::Abort;
    }
    let max_retries = max_retries.max(1);
    if attempt + 1 >= max_retries {
        return JoinAttemptOutcome::Exhausted;
    }
    JoinAttemptOutcome::Retry {
        delay: retry_interval(attempt + 1),
        switch_sfu: is_join_error_code || edge_failures >= 2,
    }
}

/// Sliding-window rate limiter (JS `SlidingWindowRateLimiter`).
///
/// Registrations are timestamped in milliseconds; `try_register` succeeds only
/// if fewer than `max` registrations fall inside the trailing `window`. The
/// clock is passed in (`now_ms`) so the limiter is fully deterministic in tests.
#[derive(Debug, Clone)]
pub struct SlidingWindowRateLimiter {
    max: usize,
    window_ms: u64,
    events: VecDeque<u64>,
}

impl SlidingWindowRateLimiter {
    /// A limiter allowing `max` events per `window`.
    pub fn new(max: usize, window: Duration) -> Self {
        Self {
            max: max.max(1),
            window_ms: window.as_millis() as u64,
            events: VecDeque::new(),
        }
    }

    /// The JS default: 10 rejoin/migrate attempts per 120s.
    pub fn rejoin_default() -> Self {
        Self::new(REJOIN_RATE_LIMIT, REJOIN_RATE_WINDOW)
    }

    /// Try to register an event at `now_ms`. Returns `true` if allowed (and the
    /// event is recorded), `false` if the window is saturated.
    pub fn try_register(&mut self, now_ms: u64) -> bool {
        // Evict events older than the trailing window. Comparing the age
        // (`now - front`) avoids the `now < window` underflow that a
        // `now - window` cutoff would hit for early timestamps.
        while let Some(&front) = self.events.front() {
            if now_ms.saturating_sub(front) >= self.window_ms {
                self.events.pop_front();
            } else {
                break;
            }
        }
        if self.events.len() >= self.max {
            return false;
        }
        self.events.push_back(now_ms);
        true
    }
}

/// Tracks the failure caps that force the reconnect loop to give up (JS
/// `iceFailuresWithoutConnect` / `consecutiveNegotiationFailures`).
#[derive(Debug, Clone)]
pub struct FailureCaps {
    ice_failures_without_connect: u32,
    consecutive_negotiation_failures: u32,
    max_ice_failures: u32,
    max_consecutive_negotiation: u32,
}

impl Default for FailureCaps {
    fn default() -> Self {
        Self {
            ice_failures_without_connect: 0,
            consecutive_negotiation_failures: 0,
            max_ice_failures: MAX_ICE_FAILURES_WITHOUT_CONNECT,
            max_consecutive_negotiation: MAX_CONSECUTIVE_NEGOTIATION_FAILURES,
        }
    }
}

impl FailureCaps {
    /// Record an ICE-never-connected failure. Returns `true` when the cap (2) is
    /// reached and the caller must `leave` with `webrtc_unsupported_network`.
    pub fn record_ice_never_connected(&mut self) -> bool {
        self.ice_failures_without_connect += 1;
        self.ice_failures_without_connect >= self.max_ice_failures
    }

    /// A successful ICE connect clears the ICE-failure counter.
    pub fn reset_ice(&mut self) {
        self.ice_failures_without_connect = 0;
    }

    /// Record a negotiation failure. Returns `true` when the cap (3) is reached
    /// and the caller must `leave` with `repeated_negotiation_failures`.
    pub fn record_negotiation_failure(&mut self) -> bool {
        self.consecutive_negotiation_failures += 1;
        self.consecutive_negotiation_failures >= self.max_consecutive_negotiation
    }

    /// A successful reconnect clears the consecutive-negotiation counter.
    pub fn reset_negotiation(&mut self) {
        self.consecutive_negotiation_failures = 0;
    }
}

/// Decide the strategy for the *next* reconnect attempt after the current one
/// failed (JS `shouldRejoin` escalation). Once we fall back to `REJOIN` we stay
/// there.
///
/// Escalate FAST→REJOIN when any of: past the fast-reconnect deadline, we were
/// migrating, we've tried ≥ 3 times, or a PeerConnection is unhealthy.
pub fn escalate_strategy(
    elapsed: Duration,
    fast_reconnect_deadline: Duration,
    was_migrating: bool,
    attempt: u32,
    publisher_healthy: bool,
    subscriber_healthy: bool,
) -> ReconnectStrategy {
    let should_rejoin = elapsed > fast_reconnect_deadline
        || was_migrating
        || attempt >= 3
        || !publisher_healthy
        || !subscriber_healthy;
    if should_rejoin {
        ReconnectStrategy::Rejoin
    } else {
        ReconnectStrategy::Fast
    }
}

/// The strategy to start a reconnect with after a signaling-WS close, given the
/// PeerConnection health (JS `handleSfuSignalClose`).
pub fn strategy_after_signal_close(
    publisher_healthy: bool,
    subscriber_healthy: bool,
) -> ReconnectStrategy {
    if publisher_healthy && subscriber_healthy {
        ReconnectStrategy::Fast
    } else {
        ReconnectStrategy::Rejoin
    }
}

/// Whether the reconnect deadline has elapsed. A zero timeout is unlimited.
pub(crate) fn disconnection_timed_out(elapsed: Duration, timeout: Duration) -> bool {
    !timeout.is_zero() && elapsed > timeout
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_interval_matches_js_bounds() {
        // n=0: min=250, max=500
        for _ in 0..100 {
            let d = retry_interval(0).as_millis() as i64;
            assert!((250..=500).contains(&d), "n=0 out of range: {d}");
        }
        // n=1: min=250 (max(250,0)), max=2500
        for _ in 0..100 {
            let d = retry_interval(1).as_millis() as i64;
            assert!((250..=2500).contains(&d), "n=1 out of range: {d}");
        }
        // n=3: min=min(4000,5000)=4000, max=min(6500,5000)=5000
        for _ in 0..100 {
            let d = retry_interval(3).as_millis() as i64;
            assert!((4000..=5000).contains(&d), "n=3 out of range: {d}");
        }
        // large n is clamped to 5000 both ways
        assert_eq!(retry_interval(100).as_millis(), 5000);
    }

    #[test]
    fn join_abort_is_immediate_and_no_sleep() {
        let outcome = evaluate_join_failure(true, false, 1, 0, 3);
        assert_eq!(outcome, JoinAttemptOutcome::Abort);
    }

    #[test]
    fn join_exhausts_after_max_retries() {
        // attempt index 2 (3rd attempt) with max 3 -> exhausted
        assert_eq!(
            evaluate_join_failure(false, false, 1, 2, 3),
            JoinAttemptOutcome::Exhausted
        );
        // max clamped to >= 1
        assert_eq!(
            evaluate_join_failure(false, false, 1, 0, 0),
            JoinAttemptOutcome::Exhausted
        );
    }

    #[test]
    fn join_switches_sfu_on_join_error_code_or_two_edge_failures() {
        match evaluate_join_failure(false, true, 1, 0, 3) {
            JoinAttemptOutcome::Retry { switch_sfu, .. } => assert!(switch_sfu),
            other => panic!("expected retry, got {other:?}"),
        }
        match evaluate_join_failure(false, false, 2, 0, 3) {
            JoinAttemptOutcome::Retry { switch_sfu, .. } => assert!(switch_sfu),
            other => panic!("expected retry, got {other:?}"),
        }
        match evaluate_join_failure(false, false, 1, 0, 3) {
            JoinAttemptOutcome::Retry { switch_sfu, .. } => assert!(!switch_sfu),
            other => panic!("expected retry, got {other:?}"),
        }
    }

    #[test]
    fn rate_limiter_allows_max_then_blocks_within_window() {
        let mut rl = SlidingWindowRateLimiter::new(10, Duration::from_secs(120));
        for i in 0..10 {
            assert!(rl.try_register(i * 100), "registration {i} should pass");
        }
        // 11th within the window is rejected
        assert!(!rl.try_register(1000));
        // after the window slides past the first events, room frees up
        assert!(rl.try_register(120_001));
    }

    #[test]
    fn ice_cap_trips_on_second_failure() {
        let mut caps = FailureCaps::default();
        assert!(!caps.record_ice_never_connected());
        assert!(caps.record_ice_never_connected());
        // reset clears it
        caps.reset_ice();
        assert!(!caps.record_ice_never_connected());
    }

    #[test]
    fn negotiation_cap_trips_on_third_failure() {
        let mut caps = FailureCaps::default();
        assert!(!caps.record_negotiation_failure());
        assert!(!caps.record_negotiation_failure());
        assert!(caps.record_negotiation_failure());
        caps.reset_negotiation();
        assert!(!caps.record_negotiation_failure());
    }

    #[test]
    fn escalation_prefers_rejoin_on_deadline_or_unhealthy() {
        // healthy, within deadline, low attempt -> stay FAST
        assert_eq!(
            escalate_strategy(
                Duration::from_secs(1),
                Duration::from_secs(5),
                false,
                0,
                true,
                true
            ),
            ReconnectStrategy::Fast
        );
        // past deadline -> REJOIN
        assert_eq!(
            escalate_strategy(
                Duration::from_secs(6),
                Duration::from_secs(5),
                false,
                0,
                true,
                true
            ),
            ReconnectStrategy::Rejoin
        );
        // unhealthy publisher -> REJOIN
        assert_eq!(
            escalate_strategy(
                Duration::from_secs(1),
                Duration::from_secs(5),
                false,
                0,
                false,
                true
            ),
            ReconnectStrategy::Rejoin
        );
    }

    #[test]
    fn signal_close_strategy_depends_on_pc_health() {
        assert_eq!(
            strategy_after_signal_close(true, true),
            ReconnectStrategy::Fast
        );
        assert_eq!(
            strategy_after_signal_close(true, false),
            ReconnectStrategy::Rejoin
        );
    }

    #[test]
    fn only_rejoin_and_migrate_are_rate_limited() {
        assert!(ReconnectStrategy::Rejoin.is_rate_limited());
        assert!(ReconnectStrategy::Migrate.is_rate_limited());
        assert!(!ReconnectStrategy::Fast.is_rate_limited());
        assert!(!ReconnectStrategy::Disconnect.is_rate_limited());
    }

    #[test]
    fn zero_disconnection_timeout_is_unlimited() {
        assert!(!disconnection_timed_out(
            Duration::from_secs(86_400),
            Duration::ZERO
        ));
        assert!(!disconnection_timed_out(
            Duration::from_secs(5),
            Duration::from_secs(5)
        ));
        assert!(disconnection_timed_out(
            Duration::from_secs(6),
            Duration::from_secs(5)
        ));
    }
}
