//! Inbound media tracks ([`RemoteTrack`]) delivered by the subscriber
//! PeerConnection after a subscription lands.
//!
//! Ported from videosdk's subscriber `OnTrackReceived` and stream-py's inbound
//! decode path. A `RemoteTrack` carries the publishing participant, the
//! [`TrackType`], the negotiated [`Codec`], and three read paths:
//!
//! - [`RemoteTrack::read_rtp`] — the raw inbound RTP packet (RTP-forward path).
//! - [`RemoteTrack::next_pcm`] — decoded 48 kHz mono [`PcmFrame`] (audio only;
//!   Opus decode, for the PCM bridge / bots).
//! - [`RemoteTrack::next_video_frame`] — decoded packed-I420 [`VideoFrame`]
//!   (VP8/VP9 video, for bots that need to *see* the call).
//!
//! Read operations on one track are serialized. Do not mix raw and decoded
//! reads: each RTP packet is consumed by whichever read operation acquires the
//! track first, so splitting one stream between paths makes both incomplete.
//!
//! Dropping a `RemoteTrack` unsubscribes it from the SFU (best-effort) so the
//! server stops forwarding a stream the caller no longer reads.

use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use webrtc::media::io::sample_builder::SampleBuilder;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::packet::Packet as RtcpPacket;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp::codecs::vp8::Vp8Packet;
use webrtc::rtp::codecs::vp9::Vp9Packet;
use webrtc::rtp::packetizer::Depacketizer;
use webrtc::track::track_remote::TrackRemote;

use super::local::RtpPacket;
use crate::rtc::codecs::vpx::{VpxCodec, VpxDecoder};
use crate::rtc::error::{Result, RtcError};
use crate::rtc::pcm::{FRAME_SAMPLES_20MS, OPUS_SAMPLE_RATE, PcmFrame};
use crate::rtc::proto::models::{self, TrackType};
use crate::rtc::video_frame::VideoFrame;

/// How many packets the video [`SampleBuilder`] buffers while waiting for gaps
/// to be filled (by NACK/RTX or a reordered arrival) before giving up on a
/// frame. ~1 s of 30 fps video fragmented at MTU.
const VIDEO_MAX_LATE: u16 = 200;
/// Longest run of missing audio packets this track fills in. A longer gap counts
/// as a break in the stream and is not filled.
const AUDIO_MAX_FILLED_PACKETS: u16 = 10;
/// The longest Opus frame at 48 kHz mono is 120 ms.
const MAX_OPUS_FRAME_SAMPLES: usize = 5_760;
/// The RTP clock for all WebRTC video.
const VIDEO_CLOCK_RATE: u32 = 90_000;
/// Floor between automatic PLIs. A keyframe is expensive for the publisher, and
/// one is in flight for at least a round trip, so asking faster only wastes
/// uplink.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_secs(1);
/// Interval between `stream.rtc.remote.video_receive_stats` debug logs.
const VIDEO_STATS_INTERVAL: Duration = Duration::from_secs(5);

/// The publishing participant a [`RemoteTrack`] belongs to.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct RemoteParticipant {
    /// The publisher's user id.
    pub user_id: String,
    /// The publisher's SFU session id.
    pub session_id: String,
    /// Track kinds the participant currently publishes.
    pub published_tracks: Vec<TrackType>,
    /// Time the participant joined the SFU session.
    pub joined_at: Option<prost_types::Timestamp>,
    /// Current SFU-reported connection quality.
    pub connection_quality: models::ConnectionQuality,
    /// Whether the participant is currently speaking.
    pub is_speaking: bool,
    /// Whether the participant is the dominant speaker.
    pub is_dominant_speaker: bool,
    /// Normalized audio level in the `0.0..=1.0` range.
    pub audio_level: f32,
    /// Participant display name.
    pub name: String,
    /// Participant image URL.
    pub image: String,
    /// Participant-defined custom data.
    pub custom: Option<prost_types::Struct>,
    /// Roles assigned to this participant.
    pub roles: Vec<String>,
    /// How the participant entered the call.
    pub source: models::ParticipantSource,
    /// Track kinds the SFU currently reports as paused for this subscriber.
    pub paused_tracks: Vec<TrackType>,
}

impl RemoteParticipant {
    pub(crate) fn from_proto(
        participant: &models::Participant,
        paused_tracks: impl IntoIterator<Item = i32>,
    ) -> Self {
        Self {
            user_id: participant.user_id.clone(),
            session_id: participant.session_id.clone(),
            published_tracks: participant
                .published_tracks
                .iter()
                .filter_map(|value| TrackType::try_from(*value).ok())
                .collect(),
            joined_at: participant.joined_at,
            connection_quality: models::ConnectionQuality::try_from(participant.connection_quality)
                .unwrap_or(models::ConnectionQuality::Unspecified),
            is_speaking: participant.is_speaking,
            is_dominant_speaker: participant.is_dominant_speaker,
            audio_level: participant.audio_level,
            name: participant.name.clone(),
            image: participant.image.clone(),
            custom: participant.custom.clone(),
            roles: participant.roles.clone(),
            source: models::ParticipantSource::try_from(participant.source)
                .unwrap_or(models::ParticipantSource::WebrtcUnspecified),
            paused_tracks: paused_tracks
                .into_iter()
                .filter_map(|value| TrackType::try_from(value).ok())
                .collect(),
        }
    }
}

/// The negotiated codec of an inbound track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Codec {
    /// e.g. `audio/opus`, `video/VP8`.
    pub mime_type: String,
    /// The negotiated RTP payload type.
    pub payload_type: u8,
    /// RTP clock rate (48000 for Opus, 90000 for video).
    pub clock_rate: u32,
    /// Channel count (Opus rtpmap reports 2).
    pub channels: u16,
}

/// Reassembles RTP into whole frames and decodes them.
///
/// [`SampleBuilder`] is generic over its depacketizer, so the codec choice is an
/// enum rather than a trait object. It also restores packet order, which
/// [`RemoteTrack::read_rtp`] does not — feeding `read_rtp` output straight to a
/// decoder corrupts any frame that arrives out of order.
enum VideoSamples {
    Vp8(SampleBuilder<Vp8Packet>),
    Vp9(SampleBuilder<Vp9Depacketizer>),
}

/// [`Vp9Packet`] reset before each packet. `Vp9Packet::depacketize` keeps the
/// reference indices of earlier packets and fails once three accumulate, which
/// rejects every flexible-mode inter frame after the third.
#[derive(Default)]
struct Vp9Depacketizer(Vp9Packet);

impl Depacketizer for Vp9Depacketizer {
    fn depacketize(&mut self, packet: &Bytes) -> std::result::Result<Bytes, webrtc::rtp::Error> {
        self.0 = Vp9Packet::default();
        self.0.depacketize(packet)
    }

    fn is_partition_head(&self, payload: &Bytes) -> bool {
        self.0.is_partition_head(payload)
    }

    fn is_partition_tail(&self, marker: bool, payload: &Bytes) -> bool {
        self.0.is_partition_tail(marker, payload)
    }
}

impl VideoSamples {
    fn new(codec: VideoCodec) -> Self {
        match codec {
            VideoCodec::Vp8 => Self::Vp8(SampleBuilder::new(
                VIDEO_MAX_LATE,
                Vp8Packet::default(),
                VIDEO_CLOCK_RATE,
            )),
            VideoCodec::Vp9 => Self::Vp9(SampleBuilder::new(
                VIDEO_MAX_LATE,
                Vp9Depacketizer::default(),
                VIDEO_CLOCK_RATE,
            )),
        }
    }

    fn push(&mut self, packet: RtpPacket) {
        match self {
            Self::Vp8(b) => b.push(packet),
            Self::Vp9(b) => b.push(packet),
        }
    }

    fn pop(&mut self) -> Option<webrtc::media::Sample> {
        match self {
            Self::Vp8(b) => b.pop(),
            Self::Vp9(b) => b.pop(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoCodec {
    Vp8,
    Vp9,
}

/// Inbound video reassembly + decode state, plus frames already decoded but not
/// yet handed to the caller (one sample can yield more than one frame).
struct VideoDecode {
    samples: VideoSamples,
    decoder: VpxDecoder,
    ready: VecDeque<VideoFrame>,
    last_resolution: Option<(u32, u32)>,
    /// Payload type of the last packet, starting at the negotiated one.
    payload_type: u8,
    last_sequence_number: Option<u16>,
    stats: VideoStats,
    stats_since: Instant,
}

/// Inbound video counts since the last stats log. [`SampleBuilder`] discards
/// its build errors, so packets without completed samples are the only sign of
/// a frame that could not be assembled.
#[derive(Default)]
struct VideoStats {
    packets: usize,
    /// Packets whose sequence number does not follow the previous packet's.
    sequence_gaps: usize,
    markers: usize,
    samples: usize,
    frames: usize,
    decode_errors: usize,
    last_payload_byte: Option<u8>,
}

/// Inbound audio decode state, plus frames already decoded but not yet handed to
/// the caller (one packet can yield several when it fills in a lost one).
struct AudioDecode {
    decoder: opus::Decoder,
    last_seq: Option<u16>,
    ready: VecDeque<PcmFrame>,
    /// Length of the last frame decoded from a real packet. libopus makes a
    /// rebuilt frame as long as the output buffer. A lost packet states no
    /// length, so the stream's own frame size is the best value to use.
    frame_samples: usize,
    /// Decode output, reused for every packet. Queued frames are exact copies.
    scratch: Vec<i16>,
}

/// How this track's payload is turned into something the caller can use.
enum Decode {
    /// Opus → 48 kHz mono PCM.
    Audio(StdMutex<AudioDecode>),
    /// VP8/VP9 RTP → packed I420 frames. Shared with the bounded blocking
    /// decode work; a [`SampleBuilder`] carries a full sequence-number window.
    Video(Arc<StdMutex<VideoDecode>>),
    /// No decoder: a codec we cannot decode (for example AV1), or a decoder that
    /// failed to initialize. [`RemoteTrack::read_rtp`] still works.
    None,
}

/// An inbound media track from a remote participant.
///
/// Not `Clone`: it owns the inbound stream and unsubscribes on drop. Wrap it in
/// an `Arc` if you need shared read handles. When the publisher publishes a
/// dropped track again, `on_track` delivers a new `RemoteTrack` for it.
pub struct RemoteTrack {
    track: Arc<TrackRemote>,
    participant: RemoteParticipant,
    track_type: TrackType,
    codec: Codec,
    decode: Decode,
    /// Serializes raw and decoded consumers without holding a std mutex across
    /// network reads or blocking codec work.
    read_gate: AsyncMutex<()>,
    /// Remains owned by a native video decode job even if its async caller is
    /// cancelled, preventing detached `spawn_blocking` work from accumulating.
    video_decode_gate: Arc<Semaphore>,
    /// The subscriber PeerConnection, for sending RTCP keyframe requests. Weak
    /// so a live track never keeps a torn-down connection alive.
    subscriber: Weak<RTCPeerConnection>,
    /// When the last automatic PLI went out (rate limiting).
    last_keyframe_request: StdMutex<Option<Instant>>,
    /// Invoked once on drop to unsubscribe from the SFU.
    on_drop: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
    /// A packet read before this track was built, returned by the first read.
    first_packet: StdMutex<Option<RtpPacket>>,
}

impl RemoteTrack {
    /// Build a `RemoteTrack` from a subscriber `on_track` event.
    ///
    /// `subscriber` is the PeerConnection the track arrived on, used to send
    /// RTCP keyframe requests. `on_drop` is invoked exactly once when the track
    /// is dropped so the call can retract the subscription. `first_packet` is
    /// returned before the packets of `track`.
    pub(crate) fn new(
        track: Arc<TrackRemote>,
        participant: RemoteParticipant,
        track_type: TrackType,
        subscriber: Weak<RTCPeerConnection>,
        on_drop: Box<dyn FnOnce() + Send>,
        first_packet: Option<RtpPacket>,
    ) -> Self {
        let params = track.codec();
        let codec = Codec {
            mime_type: params.capability.mime_type.clone(),
            payload_type: params.payload_type,
            clock_rate: params.capability.clock_rate,
            channels: params.capability.channels,
        };
        tracing::debug!(
            user_id = %participant.user_id,
            ?track_type,
            mime_type = %codec.mime_type,
            payload_type = codec.payload_type,
            ssrc = track.ssrc(),
            "stream.rtc.remote.track_codec"
        );

        let decode = build_decoder(track_type, &codec);

        Self {
            track,
            participant,
            track_type,
            codec,
            decode,
            read_gate: AsyncMutex::new(()),
            video_decode_gate: Arc::new(Semaphore::new(1)),
            subscriber,
            last_keyframe_request: StdMutex::new(None),
            on_drop: StdMutex::new(Some(on_drop)),
            first_packet: StdMutex::new(first_packet),
        }
    }

    /// Wrap an inbound webrtc-rs track from a PeerConnection you manage
    /// yourself, so the SDK's decoders ([`next_pcm`](Self::next_pcm),
    /// [`next_video_frame`](Self::next_video_frame)) work on it too.
    ///
    /// Tracks from a Stream call arrive through
    /// [`Call::on_track`](crate::Call::on_track) already built; this is for the
    /// other direction — a second peer, such as an AI provider's Realtime
    /// endpoint, whose media you want to bridge back into a call. [`participant`](Self::participant)
    /// is empty, and dropping the track does not unsubscribe anything.
    pub fn from_webrtc(
        track: Arc<TrackRemote>,
        track_type: TrackType,
        peer: &Arc<RTCPeerConnection>,
    ) -> Self {
        Self::new(
            track,
            RemoteParticipant::default(),
            track_type,
            Arc::downgrade(peer),
            Box::new(|| {}),
            None,
        )
    }

    /// The publishing participant.
    pub fn participant(&self) -> &RemoteParticipant {
        &self.participant
    }

    /// The track kind (audio / video / screen-share).
    pub fn track_type(&self) -> TrackType {
        self.track_type
    }

    /// The negotiated inbound codec.
    pub fn codec(&self) -> &Codec {
        &self.codec
    }

    /// The inbound SSRC.
    pub fn ssrc(&self) -> u32 {
        self.track.ssrc()
    }

    /// Read the next raw inbound RTP packet.
    ///
    /// Returns `None` once the track ends (the subscriber stopped forwarding).
    /// Use this for the same-codec RTP-forward republish path. Packets arrive in
    /// network order; use [`next_video_frame`](Self::next_video_frame) if you
    /// need whole, ordered frames. Reads on one track are serialized; do not
    /// concurrently mix this raw path with either decoded path.
    pub async fn read_rtp(&self) -> Option<RtpPacket> {
        let _read_guard = self.read_gate.lock().await;
        self.read_rtp_inner().await
    }

    async fn read_rtp_inner(&self) -> Option<RtpPacket> {
        let first = self
            .first_packet
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if first.is_some() {
            return first;
        }
        match self.track.read_rtp().await {
            Ok((pkt, _attr)) => Some(pkt),
            Err(e) => {
                tracing::debug!(error = %e, "stream.rtc.remote.read_rtp_ended");
                None
            }
        }
    }

    /// Decode and return the next audio frame as 48 kHz mono s16 PCM, with
    /// [`PcmFrame::pts`] set to the RTP timestamp of its first sample.
    ///
    /// Skips empty/comfort-noise packets and returns `None` only when the track
    /// ends. Returns `None` immediately for non-audio tracks. Concurrent reads
    /// on this track are serialized; do not mix decoded and raw reads.
    pub async fn next_pcm(&self) -> Option<PcmFrame> {
        let Decode::Audio(state) = &self.decode else {
            return None;
        };
        let _read_guard = self.read_gate.lock().await;
        loop {
            if let Some(frame) = state.lock().unwrap_or_else(|e| e.into_inner()).take_frame() {
                return Some(frame);
            }
            let pkt = self.read_rtp_inner().await?;
            if pkt.payload.is_empty() {
                continue;
            }
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            state.push_packet(
                pkt.header.sequence_number,
                pkt.header.timestamp,
                &pkt.payload,
            );
            if let Some(frame) = state.take_frame() {
                return Some(frame);
            }
        }
    }

    /// Decode and return the next video frame as packed I420.
    ///
    /// Reads RTP until a whole frame is reassembled and decoded, so it blocks
    /// for roughly one frame interval. Returns `None` once the track ends, and
    /// immediately for tracks with no video decoder — audio or an unsupported
    /// video codec such as AV1.
    ///
    /// Packet loss and joining mid-stream both leave the decoder without a valid
    /// reference frame; this asks the publisher for a fresh keyframe (RTCP PLI,
    /// at most one per second) whenever that happens, so the stream recovers
    /// instead of stalling silently. Concurrent calls are serialized, including
    /// their native blocking decode work; do not mix this with raw reads.
    pub async fn next_video_frame(&self) -> Option<VideoFrame> {
        let Decode::Video(state) = &self.decode else {
            return None;
        };
        let _read_guard = self.read_gate.lock().await;
        loop {
            if let Some(frame) = state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .ready
                .pop_front()
            {
                return Some(frame);
            }

            let packet = self.read_rtp_inner().await?;

            // Packet reordering/depacketization is cheap and stays on the async
            // task. Native VPx decode is measured in milliseconds at
            // 720p, so only complete samples cross into Tokio's blocking pool.
            let samples = {
                let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                s.push_packet(packet)
            };
            if samples.is_empty() {
                continue;
            }
            let worker_permit = match Arc::clone(&self.video_decode_gate).acquire_owned().await {
                Ok(permit) => permit,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "stream.rtc.remote.video_decode_gate_closed"
                    );
                    return None;
                }
            };
            let worker_state = Arc::clone(state);
            let needs_keyframe = match tokio::task::spawn_blocking(move || {
                let _worker_permit = worker_permit;
                worker_state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .decode_samples(samples)
            })
            .await
            {
                Ok(needs_keyframe) => needs_keyframe,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "stream.rtc.remote.video_decode_worker_failed"
                    );
                    true
                }
            };
            if needs_keyframe {
                self.request_keyframe_throttled().await;
            }
        }
    }

    /// Ask the publisher to send a fresh keyframe (RTCP Picture Loss
    /// Indication).
    ///
    /// [`next_video_frame`](Self::next_video_frame) already does this on loss,
    /// so calling it by hand is only needed when *you* drive decoding (e.g. via
    /// [`read_rtp`](Self::read_rtp)). Unlike the automatic path this is not rate
    /// limited — sending PLIs faster than about one per second wastes uplink.
    pub async fn request_keyframe(&self) -> Result<()> {
        let pli = PictureLossIndication {
            sender_ssrc: 0,
            media_ssrc: self.track.ssrc(),
        };
        self.write_rtcp(&[Box::new(pli)]).await?;
        tracing::debug!(
            ssrc = self.track.ssrc(),
            "stream.rtc.remote.keyframe_requested"
        );
        Ok(())
    }

    /// Send RTCP feedback to the publisher through the subscriber connection.
    pub async fn write_rtcp(&self, packets: &[Box<dyn RtcpPacket + Send + Sync>]) -> Result<usize> {
        let peer = self.subscriber.upgrade().ok_or_else(|| {
            RtcError::IllegalState("RTCP write on a closed connection".to_owned())
        })?;
        Ok(peer.write_rtcp(packets).await?)
    }

    /// The automatic path: at most one PLI per [`KEYFRAME_REQUEST_INTERVAL`],
    /// best-effort (a failure here must not end the frame loop).
    async fn request_keyframe_throttled(&self) {
        {
            let mut last = self
                .last_keyframe_request
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            if last.is_some_and(|t| now.duration_since(t) < KEYFRAME_REQUEST_INTERVAL) {
                return;
            }
            *last = Some(now);
        }
        if let Err(e) = self.request_keyframe().await {
            tracing::debug!(error = %e, "stream.rtc.remote.keyframe_request_failed");
        }
    }
}

impl AudioDecode {
    fn new(decoder: opus::Decoder) -> Self {
        Self {
            decoder,
            last_seq: None,
            ready: VecDeque::new(),
            frame_samples: FRAME_SAMPLES_20MS,
            scratch: vec![0; MAX_OPUS_FRAME_SAMPLES],
        }
    }

    /// Decode one Opus packet and queue every frame it yields.
    ///
    /// A gap in the RTP sequence means packets were lost. libopus never sees
    /// that — it only gets the buffers we hand it — so without this the lost
    /// audio drops out of the timeline and the decoder's prediction state
    /// desyncs for the frames that follow. Each lost packet gets a frame here.
    ///
    /// Only the frame directly before `sequence_number` can be rebuilt from real
    /// audio: in-band FEC puts a low-quality copy of a frame into the *next*
    /// packet, so anything lost earlier had its copy in a lost packet too.
    fn push_packet(&mut self, sequence_number: u16, rtp_timestamp: u32, payload: &[u8]) {
        let missing = match self.last_seq {
            None => 0,
            Some(last) => {
                let ahead = sequence_number.wrapping_sub(last);
                // Sequence numbers wrap: a packet behind the mark reads as a
                // distance over half the range. Its frame already went out,
                // rebuilt from the packet that overtook it.
                if ahead == 0 || ahead > u16::MAX / 2 {
                    return;
                }
                let missing = ahead - 1;
                if missing > AUDIO_MAX_FILLED_PACKETS {
                    0
                } else {
                    missing
                }
            }
        };
        self.last_seq = Some(sequence_number);

        // Lost frames count back from the packet that arrived: DTX skips
        // timestamps over silence but not sequence numbers.
        let frame_samples = self.frame_samples as u32;
        for back in (2..=missing).rev() {
            self.decode_frame(
                &[],
                false,
                rtp_timestamp.wrapping_sub(u32::from(back) * frame_samples),
            );
        }
        if missing > 0 {
            self.decode_frame(payload, true, rtp_timestamp.wrapping_sub(frame_samples));
        }
        self.decode_frame(payload, false, rtp_timestamp);
    }

    /// Decode one frame that starts at RTP timestamp `pts` and queue it. An
    /// empty `payload` makes libopus build a replacement for a lost frame.
    /// `fec` takes the copy of the previous frame out of `payload` instead of
    /// decoding `payload` itself.
    fn decode_frame(&mut self, payload: &[u8], fec: bool, pts: u32) {
        let rebuilt = fec || payload.is_empty();
        // A real packet states its own length and the buffer is only an upper
        // bound. For a rebuilt frame the buffer length is the length libopus
        // produces, so it must match the frame that was lost.
        let capacity = if rebuilt {
            self.frame_samples
        } else {
            MAX_OPUS_FRAME_SAMPLES
        };
        match self
            .decoder
            .decode(payload, &mut self.scratch[..capacity], fec)
        {
            Ok(samples) => {
                if samples == 0 {
                    return;
                }
                if !rebuilt {
                    self.frame_samples = samples;
                }
                let mut frame = PcmFrame::mono(self.scratch[..samples].to_vec(), OPUS_SAMPLE_RATE);
                frame.pts = Some(pts);
                self.ready.push_back(frame);
            }
            Err(error) => {
                tracing::debug!(error = %error, "stream.rtc.remote.opus_decode_failed");
            }
        }
    }

    fn take_frame(&mut self) -> Option<PcmFrame> {
        self.ready.pop_front()
    }
}

impl VideoDecode {
    /// Feed one RTP packet into the reassembler and return every sample it
    /// completes. This path performs no native decode work.
    fn push_packet(&mut self, packet: RtpPacket) -> Vec<webrtc::media::Sample> {
        let ssrc = packet.header.ssrc;
        let payload_type = packet.header.payload_type;
        if payload_type != self.payload_type {
            tracing::debug!(
                ssrc,
                from = self.payload_type,
                to = payload_type,
                "stream.rtc.remote.video_payload_type_changed"
            );
            self.payload_type = payload_type;
        }
        self.stats.packets += 1;
        let sequence_number = packet.header.sequence_number;
        if self
            .last_sequence_number
            .is_some_and(|last| sequence_number != last.wrapping_add(1))
        {
            self.stats.sequence_gaps += 1;
        }
        self.last_sequence_number = Some(sequence_number);
        if packet.header.marker {
            self.stats.markers += 1;
        }
        self.stats.last_payload_byte = packet.payload.first().copied();

        let mut completed = Vec::with_capacity(1);
        self.samples.push(packet);
        while let Some(sample) = self.samples.pop() {
            completed.push(sample);
        }
        self.stats.samples += completed.len();

        if self.stats_since.elapsed() >= VIDEO_STATS_INTERVAL {
            let stats = std::mem::take(&mut self.stats);
            tracing::debug!(
                ssrc,
                payload_type,
                packets = stats.packets,
                sequence_gaps = stats.sequence_gaps,
                markers = stats.markers,
                samples = stats.samples,
                frames = stats.frames,
                decode_errors = stats.decode_errors,
                last_payload_byte = ?stats.last_payload_byte.map(|b| format!("{b:#04x}")),
                "stream.rtc.remote.video_receive_stats"
            );
            self.stats_since = Instant::now();
        }
        completed
    }

    /// Decode completed samples into `ready` on a blocking worker. Returns
    /// whether the publisher should be asked for a fresh keyframe.
    fn decode_samples(&mut self, samples: Vec<webrtc::media::Sample>) -> bool {
        let mut needs_keyframe = false;
        for sample in samples {
            if sample.prev_dropped_packets > 0 {
                // A hole in the frame means the reference chain is broken: every
                // later inter-frame decodes against state we never received.
                tracing::debug!(
                    dropped = sample.prev_dropped_packets,
                    "stream.rtc.remote.video_packets_dropped"
                );
                needs_keyframe = true;
            }

            match self.decoder.decode(&sample.data, sample.packet_timestamp) {
                Ok(frames) => {
                    self.stats.frames += frames.len();
                    for frame in frames {
                        let resolution = (frame.width, frame.height);
                        if self.last_resolution != Some(resolution) {
                            tracing::debug!(
                                width = frame.width,
                                height = frame.height,
                                "stream.rtc.remote.video_resolution_changed"
                            );
                            self.last_resolution = Some(resolution);
                        }
                        self.ready.push_back(frame);
                    }
                }
                Err(e) => {
                    self.stats.decode_errors += 1;
                    tracing::debug!(error = %e, "stream.rtc.remote.video_decode_failed");
                    needs_keyframe = true;
                }
            }
        }
        needs_keyframe
    }
}

impl Drop for RemoteTrack {
    fn drop(&mut self) {
        if let Some(f) = self
            .on_drop
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            f();
        }
    }
}

fn is_audio(track_type: TrackType) -> bool {
    matches!(track_type, TrackType::Audio | TrackType::ScreenShareAudio)
}

/// Pick a decoder from the track kind + negotiated codec.
///
/// A decoder-init failure is logged and leaves the corresponding `next_*`
/// returning `None` rather than panicking; `read_rtp` keeps working either way.
fn build_decoder(track_type: TrackType, codec: &Codec) -> Decode {
    if is_audio(track_type) {
        return match opus::Decoder::new(OPUS_SAMPLE_RATE, opus::Channels::Mono) {
            Ok(decoder) => Decode::Audio(StdMutex::new(AudioDecode::new(decoder))),
            Err(e) => {
                tracing::warn!(error = %e, "stream.rtc.remote.opus_decoder_init_failed");
                Decode::None
            }
        };
    }

    let Some(video_codec) = video_codec_for(&codec.mime_type) else {
        tracing::warn!(
            mime_type = %codec.mime_type,
            "stream.rtc.remote.video_codec_not_decodable: next_video_frame will return None; \
             VP8 and VP9 are decodable (read_rtp still works)"
        );
        return Decode::None;
    };
    let decoder = VpxDecoder::new(match video_codec {
        VideoCodec::Vp8 => VpxCodec::Vp8,
        VideoCodec::Vp9 => VpxCodec::Vp9,
    });
    match decoder {
        Ok(decoder) => Decode::Video(Arc::new(StdMutex::new(VideoDecode {
            samples: VideoSamples::new(video_codec),
            decoder,
            ready: VecDeque::new(),
            last_resolution: None,
            payload_type: codec.payload_type,
            last_sequence_number: None,
            stats: VideoStats::default(),
            stats_since: Instant::now(),
        }))),
        Err(e) => {
            tracing::warn!(
                error = %e,
                mime_type = %codec.mime_type,
                "stream.rtc.remote.video_decoder_init_failed"
            );
            Decode::None
        }
    }
}

/// Map an RTP mime type onto an in-process video decoder.
fn video_codec_for(mime_type: &str) -> Option<VideoCodec> {
    let mime = mime_type.to_ascii_lowercase();
    match () {
        () if mime.ends_with("/vp8") => Some(VideoCodec::Vp8),
        () if mime.ends_with("/vp9") => Some(VideoCodec::Vp9),
        () => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtc::codecs::vpx::{EncodedFrame, VpxEncoder};

    #[test]
    fn supported_video_mime_types_map_to_a_decoder() {
        assert_eq!(video_codec_for("video/VP8"), Some(VideoCodec::Vp8));
        assert_eq!(video_codec_for("video/vp9"), Some(VideoCodec::Vp9));
        assert_eq!(video_codec_for("video/H264"), None);
    }

    /// RTP packets for one VP9 frame in flexible mode, as browsers send VP9
    /// SVC: a 15-bit picture ID and, on an inter frame, one reference index.
    fn vp9_flexible_mode_packets(
        frame: &EncodedFrame,
        picture_id: u16,
        first_sequence_number: u16,
    ) -> Vec<RtpPacket> {
        let chunks: Vec<&[u8]> = frame.data.chunks(200).collect();
        let last = chunks.len() - 1;
        chunks
            .into_iter()
            .enumerate()
            .map(|(index, chunk)| {
                // I (picture ID) + F (flexible mode).
                let mut payload = vec![0x90, 0x80 | (picture_id >> 8) as u8, picture_id as u8];
                if !frame.key {
                    payload[0] |= 0x40;
                    // P_DIFF = 1, N = 0.
                    payload.push(1 << 1);
                }
                if index == 0 {
                    payload[0] |= 0x08;
                }
                if index == last {
                    payload[0] |= 0x04;
                }
                payload.extend_from_slice(chunk);
                RtpPacket {
                    header: webrtc::rtp::header::Header {
                        version: 2,
                        payload_type: 98,
                        sequence_number: first_sequence_number.wrapping_add(index as u16),
                        timestamp: u32::from(picture_id) * 3_000,
                        marker: index == last,
                        ..Default::default()
                    },
                    payload: Bytes::from(payload),
                }
            })
            .collect()
    }

    #[test]
    fn every_frame_of_a_flexible_mode_vp9_stream_is_decoded() {
        let (width, height) = (160, 120);
        let mut encoder = VpxEncoder::new(VpxCodec::Vp9, width, height, 400).expect("vp9 encoder");
        let image = vec![128u8; (width * height * 3 / 2) as usize];
        let codec = Codec {
            mime_type: "video/VP9".to_owned(),
            payload_type: 98,
            clock_rate: VIDEO_CLOCK_RATE,
            channels: 0,
        };
        let Decode::Video(state) = build_decoder(TrackType::Video, &codec) else {
            panic!("VP9 must have a decoder");
        };
        let mut state = state.lock().expect("video decode state");

        let mut pictures: u16 = 0;
        let mut sequence_number: u16 = 0;
        for index in 0..10 {
            let encoded = encoder
                .encode(&image, index * 33, 33, index == 0)
                .expect("encode");
            for frame in &encoded {
                let packets = vp9_flexible_mode_packets(frame, pictures, sequence_number);
                pictures += 1;
                sequence_number = sequence_number.wrapping_add(packets.len() as u16);
                for packet in packets {
                    let samples = state.push_packet(packet);
                    state.decode_samples(samples);
                }
            }
        }

        // The last frame completes only when a packet of a later frame arrives.
        assert!(pictures > 3, "the encoder must emit inter frames");
        assert_eq!(state.ready.len(), usize::from(pictures) - 1);
    }

    /// One 20 ms frame of 440 Hz tone, loud enough that a silent or badly
    /// rebuilt frame is obvious.
    fn tone_20ms() -> Vec<i16> {
        (0..FRAME_SAMPLES_20MS)
            .map(|index| {
                let time = index as f64 / f64::from(OPUS_SAMPLE_RATE);
                (12_000.0 * (std::f64::consts::TAU * 440.0 * time).sin()) as i16
            })
            .collect()
    }

    /// Encode `count` copies of a tone and return one Opus payload per packet.
    fn tone_packets(count: usize, inband_fec: bool) -> Vec<Vec<u8>> {
        let mut encoder = opus::Encoder::new(
            OPUS_SAMPLE_RATE,
            opus::Channels::Mono,
            opus::Application::Voip,
        )
        .expect("encoder");
        encoder.set_inband_fec(inband_fec).expect("set fec");
        encoder.set_packet_loss_perc(20).expect("set loss");
        let pcm = tone_20ms();
        (0..count)
            .map(|_| {
                let mut packet = vec![0u8; 1_500];
                let length = encoder.encode(&pcm, &mut packet).expect("encode");
                packet.truncate(length);
                packet
            })
            .collect()
    }

    fn audio_decode() -> AudioDecode {
        AudioDecode::new(
            opus::Decoder::new(OPUS_SAMPLE_RATE, opus::Channels::Mono).expect("decoder"),
        )
    }

    fn peak(frame: &[i16]) -> i16 {
        frame
            .iter()
            .map(|sample| sample.saturating_abs())
            .max()
            .unwrap_or(0)
    }

    /// The RTP timestamp of packet `sequence_number` in a 20 ms stream.
    fn rtp(sequence_number: u16) -> u32 {
        u32::from(sequence_number) * FRAME_SAMPLES_20MS as u32
    }

    #[test]
    fn an_unbroken_sequence_yields_one_frame_per_packet() {
        let packets = tone_packets(4, true);
        let mut state = audio_decode();

        for (index, packet) in packets.iter().enumerate() {
            state.push_packet(index as u16, rtp(index as u16), packet);
            let frame = state.take_frame();
            assert_eq!(
                frame.as_ref().map(|frame| frame.samples.len()),
                Some(FRAME_SAMPLES_20MS),
                "packet {index} should yield exactly one frame"
            );
            assert_eq!(frame.and_then(|frame| frame.pts), Some(rtp(index as u16)));
            assert!(
                state.take_frame().is_none(),
                "packet {index} queued extra frames"
            );
        }
    }

    #[test]
    fn a_decoded_frame_holds_only_its_samples() {
        let packets = tone_packets(1, true);
        let mut state = audio_decode();

        state.push_packet(0, rtp(0), &packets[0]);

        let frame = state.take_frame().expect("decoded frame");
        assert_eq!(frame.samples.capacity(), frame.samples.len());
    }

    #[test]
    fn a_lost_packet_is_rebuilt_from_the_next_one() {
        let packets = tone_packets(3, true);
        let mut state = audio_decode();

        state.push_packet(0, rtp(0), &packets[0]);
        assert!(state.take_frame().is_some());
        // Packet 1 never arrives; packet 2 carries a copy of frame 1.
        state.push_packet(2, rtp(2), &packets[2]);

        let rebuilt = state.take_frame().expect("rebuilt frame");
        let current = state.take_frame().expect("current frame");
        assert!(state.take_frame().is_none(), "only two frames are owed");
        assert_eq!(rebuilt.samples.len(), FRAME_SAMPLES_20MS);
        assert_eq!(current.samples.len(), FRAME_SAMPLES_20MS);
        assert_eq!((rebuilt.pts, current.pts), (Some(rtp(1)), Some(rtp(2))));
        assert!(
            peak(&rebuilt.samples) > 1_000,
            "rebuilt frame is silent (peak {})",
            peak(&rebuilt.samples)
        );
    }

    #[test]
    fn timestamps_continue_over_a_lost_packet_and_the_rtp_wrap() {
        let packets = tone_packets(3, true);
        let mut state = audio_decode();
        let first = u32::MAX - (FRAME_SAMPLES_20MS as u32 - 1);

        state.push_packet(0, first, &packets[0]);
        // Packet 1 is lost; its timestamp wraps to 0.
        state.push_packet(2, first.wrapping_add(rtp(2)), &packets[2]);

        let mut timestamps = Vec::new();
        while let Some(frame) = state.take_frame() {
            timestamps.push(frame.pts);
        }
        assert_eq!(
            timestamps,
            [Some(first), Some(0), Some(FRAME_SAMPLES_20MS as u32)]
        );
    }

    #[test]
    fn frames_lost_after_dtx_silence_take_their_timestamps_from_the_next_packet() {
        let packets = tone_packets(4, true);
        let mut state = audio_decode();
        // DTX skips 400 ms of timestamps and keeps the sequence numbers.
        let after_silence = rtp(20);

        state.push_packet(0, rtp(0), &packets[0]);
        assert!(state.take_frame().is_some());
        // Packets 1 and 2, the first ones after the silence, are lost.
        state.push_packet(3, after_silence + rtp(2), &packets[3]);

        let mut timestamps = Vec::new();
        while let Some(frame) = state.take_frame() {
            timestamps.push(frame.pts);
        }
        assert_eq!(
            timestamps,
            [
                Some(after_silence),
                Some(after_silence + rtp(1)),
                Some(after_silence + rtp(2))
            ]
        );
    }

    #[test]
    fn a_sender_without_fec_still_keeps_the_timeline() {
        let packets = tone_packets(3, false);
        let mut state = audio_decode();

        state.push_packet(0, rtp(0), &packets[0]);
        assert!(state.take_frame().is_some());
        state.push_packet(2, rtp(2), &packets[2]);

        assert_eq!(
            (
                state.take_frame().map(|frame| frame.samples.len()),
                state.take_frame().map(|frame| frame.samples.len())
            ),
            (Some(FRAME_SAMPLES_20MS), Some(FRAME_SAMPLES_20MS)),
            "a lost packet still owes two frames without FEC"
        );
    }

    /// Drain the queue and count what came out.
    fn drain(state: &mut AudioDecode) -> usize {
        let mut frames = 0;
        while state.take_frame().is_some() {
            frames += 1;
        }
        frames
    }

    #[test]
    fn a_late_packet_is_dropped_instead_of_repeated() {
        let packets = tone_packets(4, true);
        let mut state = audio_decode();

        state.push_packet(0, rtp(0), &packets[0]);
        // Packet 1 is overtaken by 2, so its frame is rebuilt here.
        state.push_packet(2, rtp(2), &packets[2]);
        let before_late = drain(&mut state);
        state.push_packet(1, rtp(1), &packets[1]);
        let late = drain(&mut state);
        state.push_packet(3, rtp(3), &packets[3]);
        let after_late = drain(&mut state);

        assert_eq!(late, 0, "a late packet must not repeat a frame");
        assert_eq!(
            before_late + late + after_late,
            4,
            "four packets on the wire owe four frames"
        );
    }

    #[test]
    fn a_duplicate_packet_is_dropped() {
        let packets = tone_packets(1, true);
        let mut state = audio_decode();

        state.push_packet(7, rtp(7), &packets[0]);
        let first = drain(&mut state);
        state.push_packet(7, rtp(7), &packets[0]);
        let second = drain(&mut state);

        assert_eq!((first, second), (1, 0));
    }

    #[test]
    fn several_lost_packets_each_get_a_frame() {
        let packets = tone_packets(5, true);
        let mut state = audio_decode();

        state.push_packet(0, rtp(0), &packets[0]);
        assert!(state.take_frame().is_some());
        // Packets 1, 2 and 3 are lost.
        state.push_packet(4, rtp(4), &packets[4]);

        let mut frames = 0;
        while state.take_frame().is_some() {
            frames += 1;
        }
        assert_eq!(frames, 4, "three lost packets plus the one that arrived");
    }

    #[test]
    fn a_gap_beyond_the_limit_yields_only_the_packet_that_arrived() {
        let packets = tone_packets(2, true);
        let mut state = audio_decode();

        state.push_packet(0, rtp(0), &packets[0]);
        assert!(state.take_frame().is_some());
        let far = AUDIO_MAX_FILLED_PACKETS + 2;
        state.push_packet(far, rtp(far), &packets[1]);

        assert!(state.take_frame().is_some(), "the arriving packet decodes");
        assert!(
            state.take_frame().is_none(),
            "a gap beyond the limit must not be filled"
        );
    }

    #[test]
    fn a_backwards_sequence_number_queues_nothing() {
        let packets = tone_packets(3, true);
        let mut state = audio_decode();

        state.push_packet(9, rtp(9), &packets[0]);
        assert_eq!(drain(&mut state), 1);
        state.push_packet(4, rtp(4), &packets[1]);

        assert_eq!(drain(&mut state), 0);
    }

    #[test]
    fn a_corrupt_payload_queues_nothing() {
        let mut state = audio_decode();

        state.push_packet(0, rtp(0), &[0xff; 4]);

        assert!(state.take_frame().is_none());
    }

    #[test]
    fn undecodable_video_codecs_have_no_decoder() {
        assert_eq!(video_codec_for("video/AV1"), None);
        assert_eq!(video_codec_for("audio/opus"), None);
    }

    #[test]
    fn participant_snapshot_preserves_sfu_metadata_and_paused_tracks() {
        let participant = models::Participant {
            user_id: "alice".to_owned(),
            session_id: "session-a".to_owned(),
            published_tracks: vec![TrackType::Audio as i32, TrackType::Video as i32],
            connection_quality: models::ConnectionQuality::Good as i32,
            is_speaking: true,
            audio_level: 0.75,
            name: "Alice".to_owned(),
            roles: vec!["host".to_owned()],
            source: models::ParticipantSource::Sip as i32,
            ..Default::default()
        };
        let snapshot =
            RemoteParticipant::from_proto(&participant, [TrackType::Video as i32, i32::MAX]);

        assert_eq!(snapshot.user_id, "alice");
        assert_eq!(snapshot.connection_quality, models::ConnectionQuality::Good);
        assert_eq!(snapshot.published_tracks.len(), 2);
        assert_eq!(snapshot.paused_tracks, vec![TrackType::Video]);
        assert_eq!(snapshot.source, models::ParticipantSource::Sip);
    }
}
