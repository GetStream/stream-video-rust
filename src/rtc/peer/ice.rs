//! ICE candidate exchange with the SFU: trickling local candidates and
//! buffering remote ones until a PeerConnection can accept them.

use std::sync::{Arc, Mutex as StdMutex};

use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::peer_connection::RTCPeerConnection;

use crate::rtc::error::Result;
use crate::rtc::proto::models::{self, PeerType};
use crate::rtc::signal::SignalClient;
use crate::rtc::tracer::Tracer;

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
pub struct PendingIce {
    publisher: CandidateQueue,
    subscriber: CandidateQueue,
}

impl PendingIce {
    /// Add a remote ICE candidate to the publisher or subscriber PeerConnection,
    /// buffering it if the remote description is not set yet.
    pub async fn add_remote(
        &self,
        subscriber: &Arc<RTCPeerConnection>,
        publisher: &Arc<RTCPeerConnection>,
        trickle: models::IceTrickle,
    ) -> Result<()> {
        let init: RTCIceCandidateInit = serde_json::from_str(&trickle.ice_candidate)?;
        let (target, queue) = if trickle.peer_type == PeerType::Subscriber as i32 {
            (subscriber, &self.subscriber)
        } else {
            (publisher, &self.publisher)
        };
        for candidate in queue.offer(init) {
            target.add_ice_candidate(candidate).await?;
        }
        Ok(())
    }

    /// Release every buffered candidate now that `pc`'s remote description is set.
    pub async fn flush(&self, pc: &Arc<RTCPeerConnection>, peer_type: PeerType) {
        let queue = if peer_type == PeerType::Subscriber {
            &self.subscriber
        } else {
            &self.publisher
        };
        for candidate in queue.mark_ready() {
            if let Err(e) = pc.add_ice_candidate(candidate).await {
                tracing::debug!(error = %e, "stream.rtc.ice.flush_add_failed");
            }
        }
    }
}

/// Register a subscriber/publisher `on_ice_candidate` handler that trickles
/// gathered candidates to the SFU over Twirp.
pub fn register_ice_trickle(
    pc: &Arc<RTCPeerConnection>,
    signal: SignalClient,
    session_id: String,
    peer_type: PeerType,
    tracer: Arc<Tracer>,
) {
    pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
        let signal = signal.clone();
        let session_id = session_id.clone();
        let tracer = tracer.clone();
        Box::pin(async move {
            let Some(candidate) = candidate else { return };
            let init = match candidate.to_json() {
                Ok(init) => init,
                Err(e) => {
                    tracing::debug!(error = %e, "stream.rtc.ice.to_json_failed");
                    return;
                }
            };
            // Match JS `onicecandidate`: trace the candidate init object.
            tracer.trace(
                "onicecandidate",
                serde_json::to_value(&init).unwrap_or(serde_json::Value::Null),
            );
            let ice_candidate = match serde_json::to_string(&init) {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(error = %e, "stream.rtc.ice.serialize_failed");
                    return;
                }
            };
            let trickle = models::IceTrickle {
                peer_type: peer_type as i32,
                ice_candidate,
                session_id,
            };
            match signal.ice_trickle(trickle).await {
                Ok(_) => tracing::debug!(?peer_type, "stream.rtc.ice.trickle_sent"),
                Err(e) => tracing::debug!(error = %e, "stream.rtc.ice.trickle_failed"),
            }
        })
    }));
}
