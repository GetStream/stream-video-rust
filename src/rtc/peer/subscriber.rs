//! Subscriber-side SDP negotiation: answering the SFU's `SubscriberOffer`.

use std::sync::Arc;

use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use super::ice::PendingIce;
use crate::rtc::error::{NegotiationError, Result, RtcError};
use crate::rtc::proto::event::SubscriberOffer;
use crate::rtc::proto::models::PeerType;
use crate::rtc::proto::signal;
use crate::rtc::signal::SignalClient;

/// Answer an SFU subscriber offer and post the answer over Twirp.
pub async fn negotiate_subscriber(
    subscriber: &Arc<RTCPeerConnection>,
    signal: &SignalClient,
    session_id: &str,
    offer: SubscriberOffer,
    pending_ice: &Arc<PendingIce>,
) -> Result<()> {
    let remote = RTCSessionDescription::offer(offer.sdp)
        .map_err(|e| RtcError::Negotiation(NegotiationError(e.to_string())))?;
    subscriber
        .set_remote_description(remote)
        .await
        .map_err(|e| RtcError::Negotiation(NegotiationError(e.to_string())))?;
    // The remote description now exists: release any candidates the SFU trickled
    // before this offer arrived.
    pending_ice.flush(subscriber, PeerType::Subscriber).await;
    let answer = subscriber
        .create_answer(None)
        .await
        .map_err(|e| RtcError::Negotiation(NegotiationError(e.to_string())))?;
    subscriber
        .set_local_description(answer.clone())
        .await
        .map_err(|e| RtcError::Negotiation(NegotiationError(e.to_string())))?;

    signal
        .send_answer(signal::SendAnswerRequest {
            peer_type: PeerType::Subscriber as i32,
            sdp: answer.sdp,
            session_id: session_id.to_owned(),
            negotiation_id: offer.negotiation_id,
        })
        .await?;
    tracing::debug!(session_id, "stream.rtc.subscriber.answer_sent");
    Ok(())
}
