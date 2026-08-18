//! A lightweight RTC event tracer, ported from stream-video-js
//! `packages/client/src/stats/rtc/Tracer.ts`.
//!
//! Each traced event is a [`TraceRecord`] — the 4-tuple `[tag, id, data, ts]`
//! the SFU's `OnParticipantSDKReportedStats` handler expects inside the
//! `SendStats.rtc_stats` JSON array. The buffer is bounded (oldest dropped,
//! leaving a `traceBufferOverflow` breadcrumb) so a long-lived call can never
//! grow it without limit. [`Tracer::take`] drains the buffer for the next
//! `SendStats`; [`Tracer::restore`] re-prepends a drained slice if the send
//! failed, so end-of-call events are not lost.
//!
//! The tracer is `Send + Sync` with interior mutability so it can be shared
//! (via `Arc`) into webrtc-rs event callbacks and the background stats loop.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// One traced event: `[tag, id, data, timestamp_ms]`.
///
/// Serializes as a JSON array (a tuple struct), wire-compatible with the JS
/// `TraceRecord` the server decodes. `id` distinguishes the source connection
/// (`"publisher"` / `"subscriber"`) or is `null` for signal/call-level traces.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TraceRecord(pub String, pub Option<String>, pub Value, pub i64);

impl TraceRecord {
    /// The event tag (e.g. `onicecandidate`, `ontrack`, `iceconnectionstatechange`).
    pub fn tag(&self) -> &str {
        &self.0
    }
}

/// Default trace buffer bound, matching the JS Tracer (2500 records).
pub const DEFAULT_MAX_BUFFER: usize = 2500;

/// A bounded, append-only buffer of [`TraceRecord`]s for one trace source.
#[derive(Debug)]
pub struct Tracer {
    id: Option<String>,
    max_buffer: usize,
    buffer: Mutex<Vec<TraceRecord>>,
}

impl Tracer {
    /// A tracer whose records carry `id` (or `None` for signal/call-level).
    pub fn new(id: Option<String>) -> Self {
        Self::with_capacity(id, DEFAULT_MAX_BUFFER)
    }

    /// A tracer with an explicit buffer bound (min 1).
    pub fn with_capacity(id: Option<String>, max_buffer: usize) -> Self {
        Self {
            id,
            max_buffer: max_buffer.max(1),
            buffer: Mutex::new(Vec::new()),
        }
    }

    /// The trace id shared by every record from this source.
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Append `[tag, id, data, now]` to the buffer, dropping the oldest records
    /// (with a `traceBufferOverflow` breadcrumb) if the bound is exceeded.
    pub fn trace(&self, tag: impl Into<String>, data: Value) {
        let record = TraceRecord(tag.into(), self.id.clone(), data, now_ms());
        let mut buffer = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        buffer.push(record);
        cap_buffer(&mut buffer, self.max_buffer, &self.id);
    }

    /// Drain and return every buffered record (the rollup for the next
    /// `SendStats`), leaving the buffer empty.
    pub fn take(&self) -> Vec<TraceRecord> {
        let mut buffer = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *buffer)
    }

    /// Re-prepend a previously [`take`](Self::take)n slice after a failed send,
    /// so its events are re-sent on the next interval (append-only, re-bounded).
    pub fn restore(&self, mut records: Vec<TraceRecord>) {
        if records.is_empty() {
            return;
        }
        let mut buffer = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        records.append(&mut buffer);
        *buffer = records;
        cap_buffer(&mut buffer, self.max_buffer, &self.id);
    }

    /// Number of buffered records (test/introspection).
    pub fn len(&self) -> usize {
        self.buffer.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Bound `buffer` to `max` records by dropping the oldest ones, leaving a
/// single `traceBufferOverflow` breadcrumb at the front so the consumer knows
/// records were dropped (JS `Tracer.capBuffer`).
fn cap_buffer(buffer: &mut Vec<TraceRecord>, max: usize, id: &Option<String>) {
    let overflow = buffer.len().saturating_sub(max);
    if overflow == 0 {
        return;
    }
    buffer.drain(0..overflow);
    if let Some(front) = buffer.first_mut() {
        *front = TraceRecord(
            "traceBufferOverflow".to_owned(),
            id.clone(),
            json!({ "dropped": overflow }),
            now_ms(),
        );
    }
}

/// Milliseconds since the Unix epoch (`Date.now()` in JS). Falls back to `0`
/// only if the system clock is before the epoch (never on a healthy host), so
/// no panic on a library path.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_records_carry_tag_id_and_data() {
        let tracer = Tracer::new(Some("subscriber".to_owned()));
        tracer.trace("iceconnectionstatechange", json!("connected"));
        let drained = tracer.take();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].tag(), "iceconnectionstatechange");
        assert_eq!(drained[0].1.as_deref(), Some("subscriber"));
        assert_eq!(drained[0].2, json!("connected"));
        assert!(drained[0].3 > 0, "timestamp should be a real epoch ms");
    }

    #[test]
    fn take_drains_the_buffer() {
        let tracer = Tracer::new(None);
        tracer.trace("a", Value::Null);
        tracer.trace("b", Value::Null);
        assert_eq!(tracer.len(), 2);
        let first = tracer.take();
        assert_eq!(first.len(), 2);
        assert!(tracer.is_empty(), "buffer empty after take");
        assert!(tracer.take().is_empty(), "second take yields nothing");
    }

    #[test]
    fn serializes_as_a_four_element_array() {
        let tracer = Tracer::new(Some("publisher".to_owned()));
        tracer.trace("ontrack", json!("audio:abc stream:xyz"));
        let drained = tracer.take();
        let value = serde_json::to_value(&drained).expect("serialize");
        let arr = value.as_array().expect("array of records");
        let record = arr[0].as_array().expect("record is an array");
        assert_eq!(record.len(), 4);
        assert_eq!(record[0], json!("ontrack"));
        assert_eq!(record[1], json!("publisher"));
        assert_eq!(record[2], json!("audio:abc stream:xyz"));
        assert!(record[3].is_i64());
    }

    #[test]
    fn buffer_is_bounded_and_leaves_overflow_breadcrumb() {
        let tracer = Tracer::with_capacity(Some("publisher".to_owned()), 4);
        for i in 0..10 {
            tracer.trace(format!("evt-{i}"), json!(i));
        }
        let drained = tracer.take();
        assert_eq!(drained.len(), 4, "buffer capped at max");
        // Capping runs on each push (JS parity), so it drops one at a time and
        // the front is always the latest overflow breadcrumb.
        assert_eq!(drained[0].tag(), "traceBufferOverflow");
        assert_eq!(drained[0].2, json!({ "dropped": 1 }));
        // The most-recent records survive.
        assert_eq!(drained[1].tag(), "evt-7");
        assert_eq!(drained[3].tag(), "evt-9");
    }

    #[test]
    fn restore_beyond_capacity_drops_and_breadcrumbs() {
        let tracer = Tracer::with_capacity(None, 3);
        // A large drained slice restored at once overflows in one shot.
        let restored: Vec<TraceRecord> = (0..5)
            .map(|i| TraceRecord(format!("r-{i}"), None, json!(i), 1))
            .collect();
        tracer.trace("live", json!("x"));
        tracer.restore(restored);
        let all = tracer.take();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].tag(), "traceBufferOverflow");
        assert_eq!(all[0].2, json!({ "dropped": 3 }));
    }

    #[test]
    fn restore_re_prepends_a_drained_slice() {
        let tracer = Tracer::new(None);
        tracer.trace("first", json!(1));
        tracer.trace("second", json!(2));
        let drained = tracer.take();
        // New events arrive after the failed send.
        tracer.trace("third", json!(3));
        tracer.restore(drained);
        let all = tracer.take();
        let tags: Vec<&str> = all.iter().map(|r| r.tag()).collect();
        assert_eq!(tags, vec!["first", "second", "third"]);
    }

    #[test]
    fn restore_of_empty_is_a_noop() {
        let tracer = Tracer::new(None);
        tracer.trace("x", Value::Null);
        tracer.restore(Vec::new());
        assert_eq!(tracer.len(), 1);
    }
}
