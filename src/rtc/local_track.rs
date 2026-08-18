//! Outbound media tracks ([`LocalAudioTrack`], [`LocalVideoTrack`]).
//!
//! These wrap a webrtc-rs [`TrackLocalStaticRTP`] plus an RTP packetizer, the
//! Rust analog of videosdk's `track.Local` (`WriteRTP` / `WriteSample` /
//! `StartWrite`). Three write paths feed the same outbound track:
//!
//! - [`LocalAudioTrack::write_pcm`] — raw [`PcmFrame`]s. Resampled to 48 kHz
//!   mono and paced into 20 ms Opus frames by a background task that emits
//!   silence on starve (stream-py `AudioStreamTrack` pacing). This is the PCM
//!   republish / TTS-bot path.
//! - [`LocalAudioTrack::write_sample`] / [`LocalVideoTrack::write_sample`] —
//!   already-encoded media (Opus/VP8/…) plus a frame duration; the SDK
//!   packetizes and writes. The caller controls pacing.
//! - `write_rtp` — forward an inbound [`RtpPacket`]. The SDK allocates a fresh
//!   SSRC (webrtc-rs binding) and **rewrites the sequence number**, dropping the
//!   inbound header extensions; callers must not reuse inbound headers.
//!
//! Outbound audio packets carry the RFC 6464 `ssrc-audio-level` extension: the
//! SFU marks a participant speaking only from that level, and remote UIs draw
//! their speaking indicator purely from the resulting SFU events. `write_pcm`
//! measures the level itself; the pre-encoded and RTP-forward paths cannot, so
//! they take it from the caller (`*_with_level`) and otherwise send none.
//!
//! `Clone` is a cheap handle to the *same* underlying track: cloning and
//! publishing the same `LocalAudioTrack` shares one media stream.

use std::collections::VecDeque;
use std::num::NonZeroU8;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use webrtc::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9};
use webrtc::rtp::extension::HeaderExtension;
use webrtc::rtp::extension::audio_level_extension::AudioLevelExtension;
use webrtc::rtp::packetizer::{Packetizer, new_packetizer};
use webrtc::rtp::sequence::new_random_sequencer;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::TrackLocalWriter;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

use super::error::{Result, RtcError};
use super::h264::{H264Encoder, validate_h264_encode_request};
use super::layers::{PlannedVideoLayer, simulcast_layers, single_layer};
use super::pcm::{FRAME_SAMPLES_20MS, OPUS_SAMPLE_RATE, PcmFrame, StreamResampler, rms_i16};
use super::proto::event::VideoLayerSetting;
use super::proto::models::{PublishOption, TrackType, VideoLayer};
use super::rtp_h264::H264RtpPacketizer;
use super::rtp_vpx::VpxRtpPacketizer;
use super::vpx::{Vp9SvcMode, VpxCodec, VpxEncoder, VpxSvcEncoder};

/// A raw RTP packet (`webrtc::rtp::packet::Packet`), used by the RTP-forward
/// republish path.
pub type RtpPacket = webrtc::rtp::packet::Packet;

/// Maximum RTP payload we packetize into (leaves room under a 1200-byte MTU).
const PACKET_MTU: usize = 1200;
/// Placeholder SSRC/PT for the packetizer — [`TrackLocalStaticRTP`] overrides
/// both per negotiated binding, so these values never reach the wire.
const PLACEHOLDER_SSRC: u32 = 0;
const PLACEHOLDER_PT: u8 = 0;

/// Target bitrate (kbps) for the VP8/VP9 encoder. A conservative default for the
/// backend publish path; the SFU measures the actual sent rate.
const VIDEO_BITRATE_KBPS: u32 = 1_000;
const MAX_LOCAL_VIDEO_EDGE: u32 = 3_840;
const MAX_LOCAL_VIDEO_PIXELS: u64 = 3_840 * 2_160;
const MAX_LOCAL_VIDEO_I420_BYTES: usize = 3_840 * 2_160 * 3 / 2;
const PCM_QUEUE_CAPACITY_SAMPLES: usize = FRAME_SAMPLES_20MS * 10;
const MAX_OPUS_PACKET_BYTES: usize = 1_500;
/// Force a fresh keyframe at least this often. The backend publisher does not
/// answer receiver PLI/FIR (webrtc-rs has no publisher keyframe-request hook),
/// and libvpx only emits a keyframe on the first frame or a scene change — so a
/// static image would produce a single keyframe that late subscribers miss. We
/// re-init the encoder on this interval so a new subscriber always gets a
/// decodable keyframe quickly.
const KEYFRAME_INTERVAL_MS: i64 = 1_000;

/// Sentinel for "no audio level for this write". RFC 6464 levels occupy
/// `0..=127`, so any value above that is free.
const AUDIO_LEVEL_UNSET: u8 = u8::MAX;

/// Convert a `[0, 1]`-normalized RMS amplitude (e.g. [`PcmFrame::rms`]) to an
/// RFC 6464 §3 audio level in -dBov: `0` is loudest (full scale) and `127` is
/// digital silence.
///
/// [`LocalAudioTrack::write_pcm`] applies this automatically; it is public for
/// callers of [`LocalAudioTrack::write_sample_with_level`], which publish
/// already-encoded audio the SDK cannot measure.
pub fn audio_level_dbov(rms: f64) -> u8 {
    if rms <= 0.0 {
        return 127;
    }
    (-(20.0 * rms.log10())).round().clamp(0.0, 127.0) as u8
}

/// Shared outbound-track machinery: the webrtc track, its packetizer, and the
/// forward-path sequence counter.
struct TrackCore {
    track: Arc<TrackLocalStaticRTP>,
    /// Packetizer for the encoded-sample paths (`write_pcm` pacer + `write_sample`).
    packetizer: StdMutex<Box<dyn Packetizer + Send + Sync>>,
    clock_rate: u32,
    mime_type: String,
    /// Monotonic sequence number for the RTP-forward path.
    fwd_seq: AtomicU16,
    fwd_init: AtomicBool,
    stopped: AtomicBool,
    muted: AtomicBool,
    quality_paused: AtomicBool,
    track_id: String,
    publish_option_id: AtomicI32,
    /// RFC 6464 level to stamp on the next outbound packet, or
    /// [`AUDIO_LEVEL_UNSET`]. Written by the audio paths just before they write
    /// and read back here (videosdk's `AudioSampleProvider.CurrentAudioLevel`),
    /// which keeps every public write signature unchanged. Video tracks never
    /// set it, so their packets carry no level.
    audio_level: AtomicU8,
}

impl TrackCore {
    fn new(codec: RTCRtpCodecCapability, track_id: String, stream_id: String) -> Result<Self> {
        Self::new_with_rid(codec, track_id, stream_id, None)
    }

    fn new_with_rid(
        codec: RTCRtpCodecCapability,
        track_id: String,
        stream_id: String,
        rid: Option<&str>,
    ) -> Result<Self> {
        let clock_rate = codec.clock_rate;
        let mime_type = codec.mime_type.clone();
        let payloader = codec
            .payloader_for_codec()
            .map_err(|e| RtcError::Media(format!("no payloader for codec: {e}")))?;
        let packetizer = new_packetizer(
            PACKET_MTU,
            PLACEHOLDER_PT,
            PLACEHOLDER_SSRC,
            payloader,
            Box::new(new_random_sequencer()),
            clock_rate,
        );
        let track = Arc::new(match rid {
            Some(rid) => TrackLocalStaticRTP::new_with_rid(
                codec,
                track_id.clone(),
                rid.to_owned(),
                stream_id,
            ),
            None => TrackLocalStaticRTP::new(codec, track_id.clone(), stream_id),
        });
        Ok(Self {
            track,
            packetizer: StdMutex::new(Box::new(packetizer)),
            clock_rate,
            mime_type,
            fwd_seq: AtomicU16::new(0),
            fwd_init: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            quality_paused: AtomicBool::new(false),
            track_id,
            publish_option_id: AtomicI32::new(-1),
            audio_level: AtomicU8::new(AUDIO_LEVEL_UNSET),
        })
    }

    /// Stage the RFC 6464 level for the writes that follow. `None` clears it, so
    /// a write without a level never inherits a stale one.
    fn set_audio_level(&self, level: Option<u8>) {
        self.audio_level
            .store(level.unwrap_or(AUDIO_LEVEL_UNSET), Ordering::Relaxed);
    }

    /// The staged audio-level extension, or `None` when unset.
    ///
    /// Degrading to "no extension" rather than fabricating a level keeps an
    /// un-instrumented caller at exactly its previous on-wire behavior.
    fn audio_level_extension(&self) -> Option<HeaderExtension> {
        match self.audio_level.load(Ordering::Relaxed) {
            AUDIO_LEVEL_UNSET => None,
            level => Some(HeaderExtension::AudioLevel(AudioLevelExtension {
                level,
                // The SFU's audio-level observer ignores the V bit, as do the
                // Go and aiortc publishers.
                voice: false,
            })),
        }
    }

    /// Packetize an encoded frame (`samples` at the codec clock rate) and write
    /// every resulting RTP packet to the bound senders.
    async fn write_encoded(&self, payload: &[u8], samples: u32) -> Result<()> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(RtcError::IllegalState(
                "write to a stopped track".to_owned(),
            ));
        }
        if self.muted.load(Ordering::SeqCst) || self.quality_paused.load(Ordering::SeqCst) {
            return Ok(());
        }
        let payload = Bytes::copy_from_slice(payload);
        let packets = {
            let mut p = self.packetizer.lock().unwrap_or_else(|e| e.into_inner());
            p.packetize(&payload, samples)
                .map_err(|e| RtcError::Media(format!("packetize: {e}")))?
        };
        // `write_rtp_with_extensions` resolves each URI to the id negotiated for
        // the binding, so extension ids are never ours to manage. An empty slice
        // is exactly `write_rtp`.
        let level = self.audio_level_extension();
        for pkt in &packets {
            // Ok(0) before the track binds (pre-ICE) is fine; a real transport
            // error propagates so publish failures are visible.
            self.track
                .write_rtp_with_extensions(pkt, level.as_slice())
                .await
                .map_err(RtcError::from)?;
        }
        Ok(())
    }

    /// Write a fully-formed RTP packet (SSRC + payload type are set per binding
    /// by [`TrackLocalStaticRTP`]; the sequence number, timestamp, marker, and
    /// payload are ours). Used by the video path, which builds the codec RTP
    /// descriptor itself.
    async fn write_packet(&self, pkt: &RtpPacket) -> Result<()> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(RtcError::IllegalState(
                "write to a stopped track".to_owned(),
            ));
        }
        if self.muted.load(Ordering::SeqCst) || self.quality_paused.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.track.write_rtp(pkt).await.map_err(RtcError::from)?;
        Ok(())
    }

    /// Forward an inbound RTP packet under a fresh SSRC (binding) with a rewritten
    /// sequence number and no inbound header extensions.
    ///
    /// Inbound extension ids are negotiated per-PeerConnection, so carrying them
    /// across would be meaningless; they are stripped and the sender re-adds the
    /// ids negotiated for the new PC. There is no inbound audio level to carry
    /// through either — the SFU's subscriber configuration advertises no audio
    /// extensions, so a forwarded stream only carries a level when the caller
    /// supplies one (see [`LocalAudioTrack::write_rtp_with_level`]).
    async fn forward_rtp(&self, packet: &RtpPacket) -> Result<()> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(RtcError::IllegalState(
                "write to a stopped track".to_owned(),
            ));
        }
        if self.muted.load(Ordering::SeqCst) || self.quality_paused.load(Ordering::SeqCst) {
            return Ok(());
        }
        let mut pkt = packet.clone();
        pkt.header.sequence_number = self.next_fwd_seq();
        pkt.header.extension = false;
        pkt.header.extension_profile = 0;
        pkt.header.extensions.clear();
        pkt.header.extensions_padding = 0;
        self.track
            .write_rtp_with_extensions(&pkt, self.audio_level_extension().as_slice())
            .await
            .map_err(RtcError::from)?;
        Ok(())
    }

    fn next_fwd_seq(&self) -> u16 {
        if !self.fwd_init.swap(true, Ordering::SeqCst) {
            let base = seed_u16();
            self.fwd_seq.store(base, Ordering::SeqCst);
            base
        } else {
            self.fwd_seq.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
        }
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::SeqCst);
    }

    fn is_muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }

    fn set_quality_paused(&self, paused: bool) -> bool {
        self.quality_paused.swap(paused, Ordering::SeqCst)
    }

    fn is_output_paused(&self) -> bool {
        self.muted.load(Ordering::SeqCst) || self.quality_paused.load(Ordering::SeqCst)
    }
}

/// A low-entropy 16-bit seed from the wall clock (no `rand` dependency), used to
/// randomize the forward-path sequence base so it does not always start at 0.
fn seed_u16() -> u16 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u16)
        .unwrap_or(0)
}

// Audio

struct AudioInner {
    core: TrackCore,
    /// Resampled 48 kHz mono PCM awaiting the 20 ms pacer.
    pcm: StdMutex<VecDeque<i16>>,
    resampler: StdMutex<StreamResampler>,
    encoder: StdMutex<super::opus::Encoder>,
    pacer: StdMutex<Option<JoinHandle<()>>>,
    pacer_started: AtomicBool,
    pcm_pacing: AtomicBool,
    write_guard: tokio::sync::Mutex<()>,
}

/// An outbound Opus audio track.
///
/// Feed it PCM ([`write_pcm`](Self::write_pcm)), pre-encoded Opus
/// ([`write_sample`](Self::write_sample)), or forward inbound RTP
/// ([`write_rtp`](Self::write_rtp)). `Clone` shares the same underlying track.
#[derive(Clone)]
pub struct LocalAudioTrack {
    inner: Arc<AudioInner>,
}

impl LocalAudioTrack {
    /// Build a mono Opus track (48 kHz, matching the SFU/webrtc-rs default codec).
    pub fn opus() -> Result<Self> {
        let codec = RTCRtpCodecCapability {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: OPUS_SAMPLE_RATE,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            rtcp_feedback: vec![],
        };
        let track_id = format!("audio-{}", uuid::Uuid::new_v4().simple());
        let core = TrackCore::new(codec, track_id, "stream-rust-audio".to_owned())?;
        let encoder = super::opus::Encoder::new_voip_mono().map_err(RtcError::Media)?;
        Ok(Self {
            inner: Arc::new(AudioInner {
                core,
                pcm: StdMutex::new(VecDeque::with_capacity(PCM_QUEUE_CAPACITY_SAMPLES)),
                resampler: StdMutex::new(StreamResampler::to_opus_mono()),
                encoder: StdMutex::new(encoder),
                pacer: StdMutex::new(None),
                pacer_started: AtomicBool::new(false),
                pcm_pacing: AtomicBool::new(true),
                write_guard: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// Queue a PCM frame for the paced 20 ms Opus encoder.
    ///
    /// The frame is resampled to 48 kHz mono and buffered for at most 200 ms; a
    /// background task emits one Opus packet every 20 ms, writing silence when
    /// the buffer runs dry. To keep interactive audio current, this method does
    /// not backpressure a producer: overflow drops the oldest queued samples and
    /// retains the newest audio. [`flush`](Self::flush) still drops all unsent
    /// samples immediately for barge-in.
    ///
    /// # Errors
    ///
    /// Returns [`RtcError::PcmQueueOverflow`] after retaining the newest audio
    /// when this write exceeds the 200 ms queue. The caller may continue writing;
    /// the typed error makes overload observable without allowing stale audio to
    /// accumulate.
    pub async fn write_pcm(&self, frame: PcmFrame) -> Result<()> {
        if self.inner.core.stopped.load(Ordering::SeqCst) {
            return Err(RtcError::IllegalState(
                "write to a stopped track".to_owned(),
            ));
        }
        self.inner.pcm_pacing.store(true, Ordering::SeqCst);
        let resampled = {
            let mut r = self
                .inner
                .resampler
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            r.push(&frame)
        };
        let dropped = {
            let mut buf = self.inner.pcm.lock().unwrap_or_else(|e| e.into_inner());
            let dropped = push_bounded_pcm(&mut buf, resampled);
            if dropped > 0 {
                tracing::debug!(
                    dropped_samples = dropped,
                    capacity_samples = PCM_QUEUE_CAPACITY_SAMPLES,
                    "stream.rtc.audio.pcm_queue_overflow"
                );
            }
            dropped
        };
        self.ensure_pacer();
        if dropped > 0 {
            Err(RtcError::PcmQueueOverflow {
                dropped_samples: dropped,
                capacity_samples: PCM_QUEUE_CAPACITY_SAMPLES,
            })
        } else {
            Ok(())
        }
    }

    /// Write an already-encoded Opus frame of `duration` (packetized immediately;
    /// the caller paces).
    ///
    /// The packets carry no audio level, so the SFU cannot mark this participant
    /// as speaking and remote UIs show no speaking indicator. Use
    /// [`write_sample_with_level`](Self::write_sample_with_level) when the level
    /// of the encoded audio is known.
    pub async fn write_sample(&self, data: &[u8], duration: Duration) -> Result<()> {
        self.write_sample_inner(data, duration, None).await
    }

    /// [`write_sample`](Self::write_sample) plus the RFC 6464 audio level of
    /// this frame (videosdk `SampleWriteOptions.AudioLevel`).
    ///
    /// `level` is in -dBov: `0` is full scale and `127` is silence. Use
    /// [`audio_level_dbov`] to derive it from a normalized RMS amplitude.
    /// Without a level the SFU never reports this participant as speaking, so a
    /// republish bot writing pre-encoded Opus is audible but shows no level.
    pub async fn write_sample_with_level(
        &self,
        data: &[u8],
        duration: Duration,
        level: u8,
    ) -> Result<()> {
        self.write_sample_inner(data, duration, Some(level.min(127)))
            .await
    }

    async fn write_sample_inner(
        &self,
        data: &[u8],
        duration: Duration,
        level: Option<u8>,
    ) -> Result<()> {
        self.inner.pcm_pacing.store(false, Ordering::SeqCst);
        let _write = self.inner.write_guard.lock().await;
        let samples = (duration.as_secs_f64() * f64::from(self.inner.core.clock_rate)) as u32;
        self.inner.core.set_audio_level(level);
        self.inner.core.write_encoded(data, samples).await
    }

    /// Forward an inbound RTP packet (same-codec republish). The SDK rewrites the
    /// SSRC (new binding) and sequence number and drops inbound extensions.
    ///
    /// The forwarded packets carry no audio level: the SFU does not forward one
    /// downstream, so there is nothing to preserve. Use
    /// [`write_rtp_with_level`](Self::write_rtp_with_level) to attach one.
    pub async fn write_rtp(&self, packet: RtpPacket) -> Result<()> {
        self.inner.pcm_pacing.store(false, Ordering::SeqCst);
        let _write = self.inner.write_guard.lock().await;
        self.inner.core.set_audio_level(None);
        self.inner.core.forward_rtp(&packet).await
    }

    /// [`write_rtp`](Self::write_rtp) plus the RFC 6464 audio level to stamp on
    /// the forwarded packet, in -dBov (`0` loudest, `127` silence).
    pub async fn write_rtp_with_level(&self, packet: RtpPacket, level: u8) -> Result<()> {
        self.inner.pcm_pacing.store(false, Ordering::SeqCst);
        let _write = self.inner.write_guard.lock().await;
        self.inner.core.set_audio_level(Some(level.min(127)));
        self.inner.core.forward_rtp(&packet).await
    }

    /// Drop any buffered-but-unsent PCM (barge-in). Does not stop the track.
    pub fn flush(&self) {
        let mut buf = self.inner.pcm.lock().unwrap_or_else(|e| e.into_inner());
        buf.clear();
    }

    /// Stop the pacer and reject further writes. Called by `stop_publish`/`leave`.
    pub(crate) fn stop(&self) {
        self.inner.core.stop();
        if let Some(handle) = self
            .inner
            .pacer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            handle.abort();
        }
    }

    /// The underlying webrtc-rs track, for attaching this track to a
    /// PeerConnection you manage yourself (`pc.add_track(...)`).
    ///
    /// [`Call::publish_audio`](crate::Call::publish_audio) does this for the
    /// SFU; you only need it to send the same audio to a second peer, such as an
    /// AI provider's Realtime endpoint. Every write path (`write_pcm` and
    /// friends) feeds all bound senders.
    pub fn webrtc_track(&self) -> Arc<TrackLocalStaticRTP> {
        self.inner.core.track.clone()
    }

    pub(crate) fn track_id(&self) -> String {
        self.inner.core.track_id.clone()
    }

    pub(crate) fn mime_type(&self) -> String {
        self.inner.core.mime_type.clone()
    }

    /// Spawn the 20 ms PCM/silence pacing task (idempotent).
    pub(crate) fn ensure_pacer(&self) {
        if self
            .inner
            .pacer_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let inner = self.inner.clone();
        let handle = tokio::spawn(async move { pace_audio(inner).await });
        *self.inner.pacer.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }
}

/// The 20 ms pacing loop: pull one Opus frame worth of PCM (or silence) every
/// tick, encode it, and packetize it onto the outbound track.
async fn pace_audio(inner: Arc<AudioInner>) {
    let mut interval = tokio::time::interval(Duration::from_millis(20));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut scratch = vec![0i16; FRAME_SAMPLES_20MS];
    let mut encoded = vec![0u8; MAX_OPUS_PACKET_BYTES];
    loop {
        interval.tick().await;
        if inner.core.stopped.load(Ordering::SeqCst) {
            return;
        }
        if !inner.pcm_pacing.load(Ordering::SeqCst) {
            continue;
        }
        let _write = inner.write_guard.lock().await;
        if !inner.pcm_pacing.load(Ordering::SeqCst) {
            continue;
        }
        {
            let mut buf = inner.pcm.lock().unwrap_or_else(|e| e.into_inner());
            for slot in scratch.iter_mut() {
                *slot = buf.pop_front().unwrap_or(0);
            }
        }
        // Measure what we are about to encode, including any silence fill, so a
        // starved pacer reports silence rather than the last spoken level.
        inner
            .core
            .set_audio_level(Some(audio_level_dbov(rms_i16(&scratch))));
        let encoded_len = {
            let mut enc = inner.encoder.lock().unwrap_or_else(|e| e.into_inner());
            encode_opus_into(&mut enc, &scratch, &mut encoded)
        };
        match encoded_len {
            Ok(len) => {
                if let Err(e) = inner
                    .core
                    .write_encoded(&encoded[..len], FRAME_SAMPLES_20MS as u32)
                    .await
                {
                    // A stopped track ends the loop; transient pre-ICE writes are Ok.
                    tracing::trace!(error = %e, "stream.rtc.audio.pace_write");
                    if inner.core.stopped.load(Ordering::SeqCst) {
                        return;
                    }
                }
            }
            Err(e) => tracing::debug!(error = %e, "stream.rtc.audio.encode_failed"),
        }
    }
}

fn push_bounded_pcm(queue: &mut VecDeque<i16>, samples: Vec<i16>) -> usize {
    let overflow = queue
        .len()
        .saturating_add(samples.len())
        .saturating_sub(PCM_QUEUE_CAPACITY_SAMPLES);
    let from_queue = overflow.min(queue.len());
    queue.drain(..from_queue);
    let from_samples = overflow - from_queue;
    queue.extend(samples.into_iter().skip(from_samples));
    overflow
}

fn encode_opus_into(
    encoder: &mut super::opus::Encoder,
    pcm: &[i16],
    output: &mut [u8],
) -> Result<usize> {
    encoder.encode(pcm, output).map_err(RtcError::Media)
}

// Video

#[derive(Clone, Copy)]
enum VideoCodec {
    Vpx(VpxCodec),
    H264,
}

enum VideoCodecState {
    Vpx {
        encoder: VpxEncoder,
        packetizer: VpxRtpPacketizer,
    },
    Vp9Svc {
        encoder: VpxSvcEncoder,
        packetizer: VpxRtpPacketizer,
    },
    H264 {
        encoder: Box<H264Encoder>,
        packetizer: H264RtpPacketizer,
        encoded: Vec<u8>,
    },
}

/// A video encoder plus its presentation clock, RTP packetizer, and outbound
/// RTP counters. Native codec access is serialized by the enclosing mutex.
struct VideoEncoder {
    codec: VideoCodecState,
    width: u32,
    height: u32,
    bitrate_kbps: u32,
    /// `next_pts` of the last keyframe; drives periodic keyframe forcing.
    last_key_pts: i64,
    /// Presentation time of the last frame emitted for max-framerate control.
    last_emit_pts: Option<i64>,
    /// Outbound RTP sequence number (wraps).
    seq: u16,
}

struct VideoClock {
    next_pts: i64,
    rtp_ts: u32,
}

struct VideoEncoding {
    core: TrackCore,
    encoder: StdMutex<Option<VideoEncoder>>,
    target_bitrate_kbps: AtomicU32,
    scale_resolution_bits: AtomicU32,
    max_framerate: AtomicU32,
    force_keyframe: AtomicBool,
}

impl VideoEncoding {
    fn scale_resolution_down_by(&self) -> f32 {
        f32::from_bits(self.scale_resolution_bits.load(Ordering::SeqCst)).max(1.0)
    }
}

struct VideoInner {
    encodings: Vec<VideoEncoding>,
    codec_id: VideoCodec,
    /// Serializes frame work before it enters Tokio's blocking pool. The permit
    /// moves into the worker so cancellation cannot build an unbounded queue.
    encode_gate: Arc<Semaphore>,
    clock: StdMutex<VideoClock>,
    active_encoding_count: AtomicU8,
    layering: VideoLayering,
    planned_width: AtomicU32,
    planned_height: AtomicU32,
    svc_spatial_layers: AtomicU8,
    svc_temporal_layers: AtomicU8,
    svc_max_spatial_layers: AtomicU8,
    svc_max_temporal_layers: AtomicU8,
}

/// Controls whether a local video track uses one encoding or lets the SFU
/// manage codec-appropriate video layering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum VideoLayering {
    /// Preserve the original one-track, one-encoding behavior.
    #[default]
    Single,
    /// Build server-managed layering, optionally capped locally.
    ///
    /// VP9 camera tracks use one-SSRC codec-native SVC. H264 camera and VP8
    /// screen-share tracks use independent RID simulcast encodings.
    ServerManaged {
        /// Maximum local spatial layers. `None` allows up to three.
        max_spatial_layers: Option<NonZeroU8>,
        /// Maximum codec-native temporal layers. `None` allows up to three.
        max_temporal_layers: Option<NonZeroU8>,
    },
}

/// Encoder settings for a locally encoded video track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct LocalVideoTrackConfig {
    /// Target encoder bitrate in bits per second.
    pub target_bitrate_bps: u32,
    /// Spatial/temporal layering policy. Defaults to [`VideoLayering::Single`].
    pub layering: VideoLayering,
}

impl Default for LocalVideoTrackConfig {
    fn default() -> Self {
        Self {
            target_bitrate_bps: VIDEO_BITRATE_KBPS * 1_000,
            layering: VideoLayering::Single,
        }
    }
}

impl LocalVideoTrackConfig {
    /// Configure a local encoder target bitrate in bits per second.
    pub fn new(target_bitrate_bps: u32) -> Self {
        Self {
            target_bitrate_bps,
            ..Self::default()
        }
    }

    /// Enable server-managed layering with up to three spatial and temporal layers.
    #[must_use]
    pub fn server_managed(mut self) -> Self {
        self.layering = VideoLayering::ServerManaged {
            max_spatial_layers: None,
            max_temporal_layers: None,
        };
        self
    }
}

/// An outbound video track (VP8, VP9, or H264).
///
/// Feed raw frames via [`write_i420`](Self::write_i420) (the SDK encodes to the
/// track's codec and packetizes), publish pre-encoded frames via
/// [`write_sample`](Self::write_sample), or forward inbound RTP via
/// [`write_rtp`](Self::write_rtp). `Clone` shares the track.
#[derive(Clone)]
pub struct LocalVideoTrack {
    inner: Arc<VideoInner>,
}

impl LocalVideoTrack {
    /// Build a VP8 video track (90 kHz clock). The Stream SFU accepts VP8 for
    /// **screen-share**; use [`LocalVideoTrack::vp9`] for a camera video track.
    pub fn vp8() -> Result<Self> {
        Self::vp8_with_config(LocalVideoTrackConfig::default())
    }

    /// Build a VP8 track with explicit local encoder settings.
    pub fn vp8_with_config(config: LocalVideoTrackConfig) -> Result<Self> {
        Self::with_codec(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_VP8.to_owned(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line: String::new(),
                rtcp_feedback: vec![],
            },
            VideoCodec::Vpx(VpxCodec::Vp8),
            config,
        )
    }

    /// Build a VP8 track configured for server-managed screen-share simulcast.
    /// Publish it with [`crate::Call::publish_screen_share`].
    pub fn vp8_simulcast() -> Result<Self> {
        Self::vp8_with_config(LocalVideoTrackConfig::default().server_managed())
    }

    /// Build a VP9 (profile 0) video track — the Stream SFU's default codec for
    /// the `Video` track type.
    pub fn vp9() -> Result<Self> {
        Self::vp9_with_config(LocalVideoTrackConfig::default())
    }

    /// Build a VP9 track with explicit local encoder settings.
    pub fn vp9_with_config(config: LocalVideoTrackConfig) -> Result<Self> {
        Self::with_codec(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_VP9.to_owned(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line: "profile-id=0".to_owned(),
                rtcp_feedback: vec![],
            },
            VideoCodec::Vpx(VpxCodec::Vp9),
            config,
        )
    }

    /// Build a VP9 camera track configured for server-managed one-SSRC SVC.
    ///
    /// The SFU publish option determines the logical spatial and temporal layer
    /// counts (up to `L3T3_KEY`). Only raw [`write_i420`](Self::write_i420)
    /// input is accepted for this layered track.
    pub fn vp9_svc() -> Result<Self> {
        Self::vp9_with_config(LocalVideoTrackConfig::default().server_managed())
    }

    /// Build an H264 Constrained Baseline, packetization-mode 1 video track.
    ///
    /// VP9 remains the SDK's default camera codec. Select H264 for peers that
    /// require it, including Safari-origin media and OpenAI Realtime video.
    /// H264 may be covered by patents in some jurisdictions; distributors must
    /// evaluate their own licensing obligations.
    pub fn h264() -> Result<Self> {
        Self::h264_with_config(LocalVideoTrackConfig::default())
    }

    /// Build an H264 track with explicit local encoder settings.
    pub fn h264_with_config(config: LocalVideoTrackConfig) -> Result<Self> {
        Self::with_codec(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line: super::publish_options::H264_FMTP.to_owned(),
                rtcp_feedback: vec![],
            },
            VideoCodec::H264,
            config,
        )
    }

    /// Build an H264 camera track configured for server-managed simulcast.
    /// Publish it with [`crate::Call::publish_video`].
    pub fn h264_simulcast() -> Result<Self> {
        Self::h264_with_config(LocalVideoTrackConfig::default().server_managed())
    }

    fn with_codec(
        codec: RTCRtpCodecCapability,
        codec_id: VideoCodec,
        config: LocalVideoTrackConfig,
    ) -> Result<Self> {
        if config.target_bitrate_bps == 0 {
            return Err(RtcError::Media(
                "video target bitrate must be greater than zero".to_owned(),
            ));
        }
        let track_id = format!("video-{}", uuid::Uuid::new_v4().simple());
        let stream_id = "stream-rust-video".to_owned();
        let rids: &[Option<&str>] = match (config.layering, codec_id) {
            (VideoLayering::Single, _) => &[None],
            (VideoLayering::ServerManaged { .. }, VideoCodec::Vpx(VpxCodec::Vp9)) => &[Some("q")],
            (VideoLayering::ServerManaged { .. }, _) => &[Some("q"), Some("h"), Some("f")],
        };
        let bitrate_kbps = config.target_bitrate_bps.saturating_add(999) / 1_000;
        let mut encodings = Vec::with_capacity(rids.len());
        for rid in rids {
            encodings.push(VideoEncoding {
                core: TrackCore::new_with_rid(
                    codec.clone(),
                    track_id.clone(),
                    stream_id.clone(),
                    *rid,
                )?,
                encoder: StdMutex::new(None),
                target_bitrate_kbps: AtomicU32::new(bitrate_kbps),
                scale_resolution_bits: AtomicU32::new(1.0_f32.to_bits()),
                max_framerate: AtomicU32::new(0),
                force_keyframe: AtomicBool::new(true),
            });
        }
        Ok(Self {
            inner: Arc::new(VideoInner {
                encodings,
                codec_id,
                encode_gate: Arc::new(Semaphore::new(1)),
                clock: StdMutex::new(VideoClock {
                    next_pts: 0,
                    rtp_ts: u32::from(seed_u16()),
                }),
                active_encoding_count: AtomicU8::new(1),
                layering: config.layering,
                planned_width: AtomicU32::new(0),
                planned_height: AtomicU32::new(0),
                svc_spatial_layers: AtomicU8::new(1),
                svc_temporal_layers: AtomicU8::new(1),
                svc_max_spatial_layers: AtomicU8::new(1),
                svc_max_temporal_layers: AtomicU8::new(1),
            }),
        })
    }

    /// Encode and publish a raw I420 (YUV 4:2:0 planar) frame.
    ///
    /// `data` is the packed I420 buffer (`width*height` luma bytes, then two
    /// `width/2*height/2` chroma planes; `len >= width*height*3/2`). `width` and
    /// `height` must be even. `duration` is the frame's on-wire duration and
    /// advances the RTP timestamp. The SDK encodes to the track's codec, forcing
    /// a periodic keyframe (see `KEYFRAME_INTERVAL_MS`) so late
    /// subscribers can decode, then packetizes and writes RTP.
    pub async fn write_i420(
        &self,
        data: &[u8],
        width: u32,
        height: u32,
        duration: Duration,
    ) -> Result<()> {
        if self.inner.encodings[0].core.stopped.load(Ordering::SeqCst) {
            return Err(RtcError::IllegalState(
                "write to a stopped track".to_owned(),
            ));
        }
        if self
            .inner
            .encodings
            .iter()
            .take(usize::from(
                self.inner.active_encoding_count.load(Ordering::SeqCst),
            ))
            .all(|encoding| encoding.core.is_output_paused())
        {
            return Ok(());
        }
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(RtcError::Media(format!(
                "i420 frame dimensions must be non-zero and even (got {width}x{height})"
            )));
        }
        if matches!(self.inner.layering, VideoLayering::ServerManaged { .. }) {
            let planned = (
                self.inner.planned_width.load(Ordering::SeqCst),
                self.inner.planned_height.load(Ordering::SeqCst),
            );
            if planned.0 > 0 && planned != (width, height) {
                return Err(RtcError::Media(format!(
                    "layered i420 input must match the announced full resolution {}x{} (got {width}x{height})",
                    planned.0, planned.1
                )));
            }
        }
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .filter(|pixels| {
                width <= MAX_LOCAL_VIDEO_EDGE
                    && height <= MAX_LOCAL_VIDEO_EDGE
                    && *pixels <= MAX_LOCAL_VIDEO_PIXELS
            })
            .ok_or_else(|| {
                RtcError::Media(format!(
                    "i420 frame exceeds local encode limit of {MAX_LOCAL_VIDEO_EDGE} per edge \
                     and {MAX_LOCAL_VIDEO_PIXELS} pixels (got {width}x{height})"
                ))
            })?;
        if matches!(self.inner.codec_id, VideoCodec::H264) {
            validate_h264_encode_request(width, height, duration)?;
        }
        let expected = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_add(pixels / 2))
            .ok_or_else(|| RtcError::Media("i420 frame size overflow".to_owned()))?;
        debug_assert_eq!(u64::try_from(expected).ok(), Some(pixels + pixels / 2));
        if expected > MAX_LOCAL_VIDEO_I420_BYTES {
            return Err(RtcError::Media(format!(
                "i420 frame exceeds local encode limit of {MAX_LOCAL_VIDEO_I420_BYTES} bytes \
                 (need {expected})"
            )));
        }
        if data.len() < expected {
            return Err(RtcError::Media(format!(
                "i420 buffer too small: {} bytes for {width}x{height} (need {expected})",
                data.len()
            )));
        }

        let dur_ms = i64::try_from(duration.as_millis().max(1)).unwrap_or(i64::MAX);
        let samples =
            (duration.as_secs_f64() * f64::from(self.inner.encodings[0].core.clock_rate)) as u32;

        let permit = self
            .inner
            .encode_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| RtcError::IllegalState("video encoder stopped".to_owned()))?;
        let inner = self.inner.clone();
        let frame = data[..expected].to_vec();
        let encoded_layers = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            encode_i420_layers(&inner, &frame, width, height, dur_ms, samples)
        })
        .await
        .map_err(|error| RtcError::Media(format!("video encode worker failed: {error}")))??;

        for (index, packets) in encoded_layers {
            for pkt in &packets {
                self.inner.encodings[index].core.write_packet(pkt).await?;
            }
        }
        Ok(())
    }

    /// Write an already-encoded video frame of `duration`.
    pub async fn write_sample(&self, data: &[u8], duration: Duration) -> Result<()> {
        self.ensure_single_layer_input("encoded samples")?;
        let samples =
            (duration.as_secs_f64() * f64::from(self.inner.encodings[0].core.clock_rate)) as u32;
        self.inner.encodings[0]
            .core
            .write_encoded(data, samples)
            .await
    }

    /// Forward an inbound RTP packet (same-codec republish).
    pub async fn write_rtp(&self, packet: RtpPacket) -> Result<()> {
        self.ensure_single_layer_input("RTP forwarding")?;
        self.inner.encodings[0].core.forward_rtp(&packet).await
    }

    pub(crate) fn stop(&self) {
        for encoding in &self.inner.encodings {
            encoding.core.stop();
        }
    }

    /// The underlying webrtc-rs track, for attaching this track to a
    /// PeerConnection you manage yourself (`pc.add_track(...)`). See
    /// [`LocalAudioTrack::webrtc_track`].
    pub fn webrtc_track(&self) -> Arc<TrackLocalStaticRTP> {
        self.inner.encodings[0].core.track.clone()
    }

    pub(crate) fn track_id(&self) -> String {
        self.inner.encodings[0].core.track_id.clone()
    }

    pub(crate) fn mime_type(&self) -> String {
        self.inner.encodings[0].core.mime_type.clone()
    }

    #[cfg(test)]
    fn set_target_bitrate(&self, bitrate_bps: u32) {
        if bitrate_bps == 0 {
            return;
        }
        let bitrate_kbps = bitrate_bps.saturating_add(999) / 1_000;
        for encoding in &self.inner.encodings {
            encoding
                .target_bitrate_kbps
                .store(bitrate_kbps, Ordering::SeqCst);
        }
    }

    fn ensure_single_layer_input(&self, input: &'static str) -> Result<()> {
        if matches!(self.inner.layering, VideoLayering::ServerManaged { .. }) {
            return Err(RtcError::UnsupportedLayeredInput { input });
        }
        Ok(())
    }

    pub(crate) fn configure_for_publish(
        &self,
        track_type: TrackType,
        option: &PublishOption,
    ) -> Result<Vec<VideoLayer>> {
        let plans = self.plan_for_publish(track_type, option)?;
        let svc = self.is_vp9_svc();
        self.inner
            .active_encoding_count
            .store(if svc { 1 } else { plans.len() as u8 }, Ordering::SeqCst);
        if matches!(self.inner.layering, VideoLayering::ServerManaged { .. })
            && let Some(full) = plans.last()
        {
            self.inner
                .planned_width
                .store(full.dimension.width, Ordering::SeqCst);
            self.inner
                .planned_height
                .store(full.dimension.height, Ordering::SeqCst);
        }
        if svc {
            let spatial_layers = u8::try_from(plans.len()).unwrap_or(3).clamp(1, 3);
            let temporal_layers = self.svc_temporal_layers_for(option);
            self.inner
                .svc_max_spatial_layers
                .store(spatial_layers, Ordering::SeqCst);
            self.inner
                .svc_max_temporal_layers
                .store(temporal_layers, Ordering::SeqCst);
            self.inner.svc_spatial_layers.store(
                if option.use_single_layer {
                    1
                } else {
                    spatial_layers
                },
                Ordering::SeqCst,
            );
            self.inner
                .svc_temporal_layers
                .store(temporal_layers, Ordering::SeqCst);
        }
        for (index, encoding) in self.inner.encodings.iter().enumerate() {
            let plan_index = if svc {
                plans.len().saturating_sub(1)
            } else {
                index
            };
            let Some(plan) = plans.get(plan_index) else {
                encoding.core.set_quality_paused(true);
                continue;
            };
            apply_planned_layer(encoding, plan);
        }
        Ok(plans.iter().map(PlannedVideoLayer::as_proto).collect())
    }

    fn plan_for_publish(
        &self,
        track_type: TrackType,
        option: &PublishOption,
    ) -> Result<Vec<PlannedVideoLayer>> {
        let plans = match self.inner.layering {
            VideoLayering::Single => vec![single_layer(option)],
            VideoLayering::ServerManaged {
                max_spatial_layers,
                max_temporal_layers: _,
            } => {
                let supported = (matches!(self.inner.codec_id, VideoCodec::H264)
                    && track_type == TrackType::Video)
                    || (matches!(self.inner.codec_id, VideoCodec::Vpx(VpxCodec::Vp8))
                        && track_type == TrackType::ScreenShare)
                    || (matches!(self.inner.codec_id, VideoCodec::Vpx(VpxCodec::Vp9))
                        && track_type == TrackType::Video);
                if !supported {
                    return Err(RtcError::UnsupportedVideoLayering {
                        codec: self.mime_type(),
                        track_type,
                    });
                }
                simulcast_layers(option, max_spatial_layers)
            }
        };
        Ok(plans)
    }

    pub(crate) fn planned_layers_for_publish(
        &self,
        track_type: TrackType,
        option: &PublishOption,
    ) -> Result<Vec<VideoLayer>> {
        self.plan_for_publish(track_type, option)
            .map(|plans| plans.iter().map(PlannedVideoLayer::as_proto).collect())
    }

    pub(crate) fn webrtc_tracks(&self) -> Vec<Arc<dyn TrackLocal + Send + Sync>> {
        let count = usize::from(self.inner.active_encoding_count.load(Ordering::SeqCst));
        self.inner
            .encodings
            .iter()
            .take(count)
            .map(|encoding| encoding.core.track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .collect()
    }

    pub(crate) fn apply_layer_setting(&self, setting: &VideoLayerSetting) {
        if self.is_vp9_svc() {
            self.apply_svc_layer_settings(std::slice::from_ref(setting));
            return;
        }
        let count = usize::from(self.inner.active_encoding_count.load(Ordering::SeqCst));
        let index = match setting.name.as_str() {
            "q" => 0,
            "h" => 1,
            "f" => 2,
            _ if count == 1 => 0,
            _ => return,
        };
        let Some(encoding) = self.inner.encodings.get(index).filter(|_| index < count) else {
            return;
        };
        if setting.max_bitrate > 0 {
            let bitrate_bps = u32::try_from(setting.max_bitrate).unwrap_or(u32::MAX);
            encoding
                .target_bitrate_kbps
                .store(bitrate_bps.saturating_add(999) / 1_000, Ordering::SeqCst);
        }
        if setting.max_framerate > 0 {
            encoding
                .max_framerate
                .store(setting.max_framerate, Ordering::SeqCst);
        }
        if setting.scale_resolution_down_by.is_finite() && setting.scale_resolution_down_by >= 1.0 {
            encoding
                .scale_resolution_bits
                .store(setting.scale_resolution_down_by.to_bits(), Ordering::SeqCst);
        }
        let was_paused = encoding.core.set_quality_paused(!setting.active);
        if setting.active && was_paused {
            encoding.force_keyframe.store(true, Ordering::SeqCst);
        }
    }

    pub(crate) fn apply_layer_settings(&self, settings: &[VideoLayerSetting]) {
        if self.is_vp9_svc() {
            self.apply_svc_layer_settings(settings);
            return;
        }
        let count = usize::from(self.inner.active_encoding_count.load(Ordering::SeqCst));
        for index in 0..count {
            let rid = ["q", "h", "f"][index];
            let setting = if count == 1 {
                settings.iter().find(|setting| setting.active)
            } else {
                settings
                    .iter()
                    .find(|setting| setting.active && setting.name == rid)
            };
            if let Some(setting) = setting {
                self.apply_layer_setting(setting);
            } else if let Some(encoding) = self.inner.encodings.get(index) {
                encoding.core.set_quality_paused(true);
            }
        }
    }

    pub(crate) fn set_muted(&self, muted: bool) {
        for encoding in &self.inner.encodings {
            encoding.core.set_muted(muted);
        }
    }

    pub(crate) fn is_muted(&self) -> bool {
        self.inner.encodings[0].core.is_muted()
    }

    pub(crate) fn force_keyframe(&self) {
        for encoding in &self.inner.encodings {
            encoding.force_keyframe.store(true, Ordering::SeqCst);
        }
    }

    fn is_vp9_svc(&self) -> bool {
        matches!(self.inner.codec_id, VideoCodec::Vpx(VpxCodec::Vp9))
            && matches!(self.inner.layering, VideoLayering::ServerManaged { .. })
    }

    fn svc_temporal_layers_for(&self, option: &PublishOption) -> u8 {
        let server = if option.max_temporal_layers <= 0 {
            3
        } else {
            u8::try_from(option.max_temporal_layers)
                .unwrap_or(3)
                .clamp(1, 3)
        };
        match self.inner.layering {
            VideoLayering::ServerManaged {
                max_temporal_layers,
                ..
            } => max_temporal_layers
                .map(NonZeroU8::get)
                .unwrap_or(3)
                .min(server)
                .clamp(1, 3),
            VideoLayering::Single => 1,
        }
    }

    fn apply_svc_layer_settings(&self, settings: &[VideoLayerSetting]) {
        let Some(encoding) = self.inner.encodings.first() else {
            return;
        };
        let Some(setting) = settings.iter().find(|setting| setting.active) else {
            encoding.core.set_quality_paused(true);
            return;
        };
        if setting.max_bitrate > 0 {
            let bitrate_bps = u32::try_from(setting.max_bitrate).unwrap_or(u32::MAX);
            encoding
                .target_bitrate_kbps
                .store(bitrate_bps.saturating_add(999) / 1_000, Ordering::SeqCst);
        }
        if setting.max_framerate > 0 {
            encoding
                .max_framerate
                .store(setting.max_framerate, Ordering::SeqCst);
        }
        if setting.scale_resolution_down_by.is_finite() && setting.scale_resolution_down_by >= 1.0 {
            encoding
                .scale_resolution_bits
                .store(setting.scale_resolution_down_by.to_bits(), Ordering::SeqCst);
        }
        if let Some((spatial, temporal)) = parse_vp9_scalability_mode(&setting.scalability_mode) {
            let spatial = spatial.min(self.inner.svc_max_spatial_layers.load(Ordering::SeqCst));
            let temporal = temporal.min(self.inner.svc_max_temporal_layers.load(Ordering::SeqCst));
            let spatial_changed = self
                .inner
                .svc_spatial_layers
                .swap(spatial, Ordering::SeqCst)
                != spatial;
            let temporal_changed = self
                .inner
                .svc_temporal_layers
                .swap(temporal, Ordering::SeqCst)
                != temporal;
            if spatial_changed || temporal_changed {
                encoding.force_keyframe.store(true, Ordering::SeqCst);
            }
        }
        let was_paused = encoding.core.set_quality_paused(false);
        if was_paused {
            encoding.force_keyframe.store(true, Ordering::SeqCst);
        }
    }

    #[cfg(test)]
    pub(crate) fn svc_mode(&self) -> Option<(u8, u8)> {
        self.is_vp9_svc().then(|| {
            (
                self.inner.svc_spatial_layers.load(Ordering::SeqCst),
                self.inner.svc_temporal_layers.load(Ordering::SeqCst),
            )
        })
    }

    #[cfg(test)]
    pub(crate) fn is_layer_paused(&self, rid: &str) -> Option<bool> {
        let index = match rid {
            "q" => 0,
            "h" => 1,
            "f" => 2,
            _ => return None,
        };
        self.inner
            .encodings
            .get(index)
            .map(|encoding| encoding.core.quality_paused.load(Ordering::SeqCst))
    }

    #[cfg(test)]
    fn layer_control_state(&self, rid: &str) -> Option<(bool, u32, u32, f32)> {
        let index = match rid {
            "q" => 0,
            "h" => 1,
            "f" => 2,
            _ => return None,
        };
        self.inner.encodings.get(index).map(|encoding| {
            (
                encoding.core.quality_paused.load(Ordering::SeqCst),
                encoding.target_bitrate_kbps.load(Ordering::SeqCst),
                encoding.max_framerate.load(Ordering::SeqCst),
                f32::from_bits(encoding.scale_resolution_bits.load(Ordering::SeqCst)),
            )
        })
    }
}

fn apply_planned_layer(encoding: &VideoEncoding, plan: &PlannedVideoLayer) {
    if plan.bitrate_bps > 0 {
        encoding.target_bitrate_kbps.store(
            plan.bitrate_bps.saturating_add(999) / 1_000,
            Ordering::SeqCst,
        );
    }
    encoding
        .max_framerate
        .store(plan.max_framerate, Ordering::SeqCst);
    encoding
        .scale_resolution_bits
        .store(plan.scale_resolution_down_by.to_bits(), Ordering::SeqCst);
    let was_paused = encoding.core.set_quality_paused(!plan.initially_active);
    if plan.initially_active && was_paused {
        encoding.force_keyframe.store(true, Ordering::SeqCst);
    }
}

fn parse_vp9_scalability_mode(value: &str) -> Option<(u8, u8)> {
    let (value, key_picture_dependency) = value
        .strip_suffix("_KEY")
        .map_or((value, false), |value| (value, true));
    let (spatial, temporal) = value.strip_prefix('L')?.split_once('T')?;
    let spatial = spatial.parse::<u8>().ok()?;
    let temporal = temporal.parse::<u8>().ok()?;
    ((1..=3).contains(&spatial)
        && (1..=3).contains(&temporal)
        && (spatial == 1 || key_picture_dependency))
        .then_some((spatial, temporal))
}

fn encode_i420_layers(
    inner: &VideoInner,
    data: &[u8],
    width: u32,
    height: u32,
    dur_ms: i64,
    samples: u32,
) -> Result<Vec<(usize, Vec<RtpPacket>)>> {
    let (pts, timestamp) = {
        let mut clock = inner
            .clock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let current = (clock.next_pts, clock.rtp_ts);
        clock.next_pts = clock.next_pts.saturating_add(dur_ms);
        clock.rtp_ts = clock.rtp_ts.wrapping_add(samples);
        current
    };
    let count = usize::from(inner.active_encoding_count.load(Ordering::SeqCst));
    let mut output = Vec::with_capacity(count);
    for (index, encoding) in inner.encodings.iter().take(count).enumerate() {
        if encoding.core.is_output_paused() {
            continue;
        }
        let scale = encoding.scale_resolution_down_by();
        let layer_width = scaled_even(width, scale);
        let layer_height = scaled_even(height, scale);
        if matches!(inner.codec_id, VideoCodec::H264) {
            validate_h264_encode_request(
                layer_width,
                layer_height,
                Duration::from_millis(dur_ms.max(1) as u64),
            )?;
        }
        let scaled;
        let layer_data = if layer_width == width && layer_height == height {
            data
        } else {
            scaled = scale_i420(data, width, height, layer_width, layer_height)?;
            scaled.as_slice()
        };
        let packets = encode_layer_packets(
            inner.codec_id,
            encoding,
            layer_data,
            layer_width,
            layer_height,
            pts,
            dur_ms,
            timestamp,
            if matches!(inner.codec_id, VideoCodec::Vpx(VpxCodec::Vp9))
                && matches!(inner.layering, VideoLayering::ServerManaged { .. })
            {
                Some(Vp9SvcMode::new(
                    inner.svc_spatial_layers.load(Ordering::SeqCst),
                    inner.svc_temporal_layers.load(Ordering::SeqCst),
                )?)
            } else {
                None
            },
        )?;
        output.push((index, packets));
    }
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
fn encode_layer_packets(
    codec_id: VideoCodec,
    encoding: &VideoEncoding,
    data: &[u8],
    width: u32,
    height: u32,
    pts: i64,
    dur_ms: i64,
    timestamp: u32,
    svc_mode: Option<Vp9SvcMode>,
) -> Result<Vec<RtpPacket>> {
    let mut guard = encoding.encoder.lock().unwrap_or_else(|e| e.into_inner());
    let bitrate_kbps = encoding.target_bitrate_kbps.load(Ordering::SeqCst);
    let needs_new = match guard.as_ref() {
        Some(state) => {
            let current_svc_mode = match &state.codec {
                VideoCodecState::Vp9Svc { encoder, .. } => Some(encoder.mode()),
                VideoCodecState::Vpx { .. } | VideoCodecState::H264 { .. } => None,
            };
            state.width != width
                || state.height != height
                || state.bitrate_kbps != bitrate_kbps
                || current_svc_mode != svc_mode
        }
        None => true,
    };
    if needs_new {
        let seq = guard
            .as_ref()
            .map(|state| state.seq)
            .unwrap_or_else(seed_u16);
        let prior_vp9_packetizer = guard.as_ref().and_then(|state| match &state.codec {
            VideoCodecState::Vp9Svc { packetizer, .. } => Some(packetizer.clone()),
            VideoCodecState::Vpx { .. } | VideoCodecState::H264 { .. } => None,
        });
        let codec = match (codec_id, svc_mode) {
            (VideoCodec::Vpx(VpxCodec::Vp9), Some(mode)) => VideoCodecState::Vp9Svc {
                encoder: VpxSvcEncoder::new(width, height, bitrate_kbps, mode)?,
                packetizer: prior_vp9_packetizer
                    .unwrap_or_else(|| VpxRtpPacketizer::new(VpxCodec::Vp9)),
            },
            (VideoCodec::Vpx(codec), None) => VideoCodecState::Vpx {
                encoder: VpxEncoder::new(codec, width, height, bitrate_kbps)?,
                packetizer: VpxRtpPacketizer::new(codec),
            },
            (VideoCodec::H264, None) => VideoCodecState::H264 {
                encoder: Box::new(H264Encoder::new(bitrate_kbps.saturating_mul(1_000))?),
                packetizer: H264RtpPacketizer::default(),
                encoded: Vec::new(),
            },
            (VideoCodec::Vpx(VpxCodec::Vp8), Some(_)) | (VideoCodec::H264, Some(_)) => {
                return Err(RtcError::Media(
                    "VP9 SVC mode supplied for a non-VP9 encoder".to_owned(),
                ));
            }
        };
        *guard = Some(VideoEncoder {
            codec,
            width,
            height,
            bitrate_kbps,
            // Force a keyframe on the first frame after (re)init.
            last_key_pts: pts - KEYFRAME_INTERVAL_MS,
            last_emit_pts: None,
            seq,
        });
    }
    let Some(state) = guard.as_mut() else {
        return Err(RtcError::Media("video encoder unavailable".to_owned()));
    };
    let max_framerate = encoding.max_framerate.load(Ordering::SeqCst);
    if let Some(interval_ms) = 1_000_u32.checked_div(max_framerate) {
        let interval_ms = i64::from(interval_ms.max(1));
        if state
            .last_emit_pts
            .is_some_and(|last| pts.saturating_sub(last) < interval_ms)
        {
            return Ok(Vec::new());
        }
    }
    state.last_emit_pts = Some(pts);
    let force_key = encoding.force_keyframe.swap(false, Ordering::SeqCst)
        || (pts - state.last_key_pts) >= KEYFRAME_INTERVAL_MS;
    if force_key {
        state.last_key_pts = pts;
    }
    let mut out = Vec::new();
    let mut seq = state.seq;
    let w16 = width.min(u32::from(u16::MAX)) as u16;
    let h16 = height.min(u32::from(u16::MAX)) as u16;
    match &mut state.codec {
        VideoCodecState::Vpx {
            encoder,
            packetizer,
        } => {
            let frames = encoder.encode(data, pts, dur_ms, force_key)?;
            for frame in frames {
                if frame.data.is_empty() {
                    continue;
                }
                tracing::trace!(
                    bytes = frame.data.len(),
                    key = frame.key,
                    mime = %encoding.core.mime_type,
                    "stream.rtc.video.encoded_frame"
                );
                for payload in packetizer.packetize(&frame.data, frame.key, w16, h16, PACKET_MTU) {
                    let header = webrtc::rtp::header::Header {
                        version: 2,
                        payload_type: PLACEHOLDER_PT,
                        sequence_number: seq,
                        timestamp,
                        ssrc: PLACEHOLDER_SSRC,
                        marker: payload.last,
                        ..Default::default()
                    };
                    out.push(RtpPacket {
                        header,
                        payload: Bytes::from(payload.data),
                    });
                    seq = seq.wrapping_add(1);
                }
            }
        }
        VideoCodecState::Vp9Svc {
            encoder,
            packetizer,
        } => {
            let frames = encoder.encode(data, pts, dur_ms, force_key)?;
            let encoded_bytes = frames.iter().map(|frame| frame.data.len()).sum::<usize>();
            tracing::trace!(
                bytes = encoded_bytes,
                spatial_layers = frames.len(),
                temporal_id = frames.first().map_or(0, |frame| frame.temporal_id),
                key = frames.first().is_some_and(|frame| frame.key),
                mime = %encoding.core.mime_type,
                "stream.rtc.video.encoded_svc_picture"
            );
            for payload in
                packetizer.packetize_svc_picture(&frames, encoder.mode(), w16, h16, PACKET_MTU)
            {
                let header = webrtc::rtp::header::Header {
                    version: 2,
                    payload_type: PLACEHOLDER_PT,
                    sequence_number: seq,
                    timestamp,
                    ssrc: PLACEHOLDER_SSRC,
                    marker: payload.last,
                    ..Default::default()
                };
                out.push(RtpPacket {
                    header,
                    payload: Bytes::from(payload.data),
                });
                seq = seq.wrapping_add(1);
            }
        }
        VideoCodecState::H264 {
            encoder,
            packetizer,
            encoded,
        } => {
            let key = encoder.encode_into(data, width, height, force_key, encoded)?;
            tracing::trace!(
                bytes = encoded.len(),
                key,
                mime = %encoding.core.mime_type,
                "stream.rtc.video.encoded_frame"
            );
            for payload in packetizer.packetize(encoded, PACKET_MTU)? {
                let header = webrtc::rtp::header::Header {
                    version: 2,
                    payload_type: PLACEHOLDER_PT,
                    sequence_number: seq,
                    timestamp,
                    ssrc: PLACEHOLDER_SSRC,
                    marker: payload.last,
                    ..Default::default()
                };
                out.push(RtpPacket {
                    header,
                    payload: payload.data,
                });
                seq = seq.wrapping_add(1);
            }
        }
    }
    state.seq = seq;
    Ok(out)
}

fn scaled_even(value: u32, scale: f32) -> u32 {
    (((value as f32 / scale).round() as u32).max(2)) & !1
}

fn scale_i420(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> Result<Vec<u8>> {
    let source_width = usize::try_from(source_width)
        .map_err(|_| RtcError::Media("source width does not fit usize".to_owned()))?;
    let source_height = usize::try_from(source_height)
        .map_err(|_| RtcError::Media("source height does not fit usize".to_owned()))?;
    let target_width = usize::try_from(target_width)
        .map_err(|_| RtcError::Media("target width does not fit usize".to_owned()))?;
    let target_height = usize::try_from(target_height)
        .map_err(|_| RtcError::Media("target height does not fit usize".to_owned()))?;
    let target_y_len = target_width
        .checked_mul(target_height)
        .ok_or_else(|| RtcError::Media("scaled i420 size overflow".to_owned()))?;
    let mut output = vec![0; target_y_len + target_y_len / 2];
    let source_y_len = source_width * source_height;
    scale_plane(
        &source[..source_y_len],
        source_width,
        source_height,
        &mut output[..target_y_len],
        target_width,
        target_height,
    );
    let source_chroma_len = source_y_len / 4;
    let target_chroma_len = target_y_len / 4;
    for plane in 0..2 {
        let source_start = source_y_len + plane * source_chroma_len;
        let target_start = target_y_len + plane * target_chroma_len;
        scale_plane(
            &source[source_start..source_start + source_chroma_len],
            source_width / 2,
            source_height / 2,
            &mut output[target_start..target_start + target_chroma_len],
            target_width / 2,
            target_height / 2,
        );
    }
    Ok(output)
}

fn scale_plane(
    source: &[u8],
    source_width: usize,
    source_height: usize,
    target: &mut [u8],
    target_width: usize,
    target_height: usize,
) {
    for (y, row) in target.chunks_exact_mut(target_width).enumerate() {
        let source_y = y * source_height / target_height;
        for (x, value) in row.iter_mut().enumerate() {
            let source_x = x * source_width / target_width;
            *value = source[source_y * source_width + source_x];
        }
    }
}

// Publish dispatch

/// A published local track plus the `TrackType` it was published as. Held by the
/// call for republish restore and `stop_publish`. Screen-share reuses the video
/// track with a `ScreenShare` type.
///
/// Obtain one via [`From`] (`audio_track.into()` / `video_track.into()`) to pass
/// to [`Call::stop_publish`](crate::Call::stop_publish). Because the publish
/// entry points take the concrete [`LocalAudioTrack`] / [`LocalVideoTrack`],
/// passing a [`RemoteTrack`](crate::rtc::RemoteTrack) is a compile error.
#[derive(Clone)]
pub enum LocalTrack {
    /// An outbound audio track.
    Audio(LocalAudioTrack),
    /// An outbound audio track associated with a screen share.
    ScreenShareAudio(LocalAudioTrack),
    /// An outbound video (or screen-share) track.
    Video {
        /// The wrapped video track.
        track: LocalVideoTrack,
        /// Whether it was published as `Video` or `ScreenShare`.
        track_type: TrackType,
    },
}

impl From<LocalAudioTrack> for LocalTrack {
    fn from(track: LocalAudioTrack) -> Self {
        LocalTrack::Audio(track)
    }
}

impl From<LocalVideoTrack> for LocalTrack {
    fn from(track: LocalVideoTrack) -> Self {
        LocalTrack::Video {
            track,
            track_type: TrackType::Video,
        }
    }
}

impl LocalTrack {
    pub(crate) fn webrtc_tracks(&self) -> Vec<Arc<dyn TrackLocal + Send + Sync>> {
        match self {
            LocalTrack::Audio(a) | LocalTrack::ScreenShareAudio(a) => vec![a.webrtc_track()],
            LocalTrack::Video { track, .. } => track.webrtc_tracks(),
        }
    }

    pub(crate) fn configure_for_publish(&self, option: &PublishOption) -> Result<Vec<VideoLayer>> {
        match self {
            LocalTrack::Video { track, track_type } => {
                track.configure_for_publish(*track_type, option)
            }
            LocalTrack::Audio(_) | LocalTrack::ScreenShareAudio(_) => Ok(Vec::new()),
        }
    }

    pub(crate) fn planned_layers_for_publish(
        &self,
        option: &PublishOption,
    ) -> Result<Vec<VideoLayer>> {
        match self {
            LocalTrack::Video { track, track_type } => {
                track.planned_layers_for_publish(*track_type, option)
            }
            LocalTrack::Audio(_) | LocalTrack::ScreenShareAudio(_) => Ok(Vec::new()),
        }
    }

    pub(crate) fn publish_option_id(&self) -> Option<i32> {
        let id = match self {
            LocalTrack::Audio(track) | LocalTrack::ScreenShareAudio(track) => {
                track.inner.core.publish_option_id.load(Ordering::SeqCst)
            }
            LocalTrack::Video { track, .. } => track.inner.encodings[0]
                .core
                .publish_option_id
                .load(Ordering::SeqCst),
        };
        (id != -1).then_some(id)
    }

    pub(crate) fn set_publish_option_id(&self, id: i32) {
        match self {
            LocalTrack::Audio(track) | LocalTrack::ScreenShareAudio(track) => {
                track
                    .inner
                    .core
                    .publish_option_id
                    .store(id, Ordering::SeqCst);
            }
            LocalTrack::Video { track, .. } => {
                for encoding in &track.inner.encodings {
                    encoding.core.publish_option_id.store(id, Ordering::SeqCst);
                }
            }
        }
    }

    pub(crate) fn track_type(&self) -> TrackType {
        match self {
            LocalTrack::Audio(_) => TrackType::Audio,
            LocalTrack::ScreenShareAudio(_) => TrackType::ScreenShareAudio,
            LocalTrack::Video { track_type, .. } => *track_type,
        }
    }

    pub(crate) fn track_id(&self) -> String {
        match self {
            LocalTrack::Audio(a) | LocalTrack::ScreenShareAudio(a) => a.track_id(),
            LocalTrack::Video { track, .. } => track.track_id(),
        }
    }

    /// The RTP MIME type this track publishes (e.g. `audio/opus`, `video/VP9`),
    /// used to pick the SFU publish option whose codec matches.
    pub(crate) fn mime_type(&self) -> String {
        match self {
            LocalTrack::Audio(a) | LocalTrack::ScreenShareAudio(a) => a.mime_type(),
            LocalTrack::Video { track, .. } => track.mime_type(),
        }
    }

    pub(crate) fn start_media(&self) {
        match self {
            LocalTrack::Audio(track) | LocalTrack::ScreenShareAudio(track) => {
                track.ensure_pacer();
            }
            LocalTrack::Video { .. } => {}
        }
    }

    pub(crate) fn stop(&self) {
        match self {
            LocalTrack::Audio(a) | LocalTrack::ScreenShareAudio(a) => a.stop(),
            LocalTrack::Video { track, .. } => track.stop(),
        }
    }

    pub(crate) fn set_muted(&self, muted: bool) {
        match self {
            LocalTrack::Audio(track) | LocalTrack::ScreenShareAudio(track) => {
                track.inner.core.set_muted(muted);
            }
            LocalTrack::Video { track, .. } => track.set_muted(muted),
        }
    }

    pub(crate) fn is_muted(&self) -> bool {
        match self {
            LocalTrack::Audio(track) | LocalTrack::ScreenShareAudio(track) => {
                track.inner.core.is_muted()
            }
            LocalTrack::Video { track, .. } => track.is_muted(),
        }
    }

    pub(crate) fn apply_video_layer_settings(&self, layers: &[VideoLayerSetting]) {
        if let LocalTrack::Video { track, .. } = self {
            track.apply_layer_settings(layers);
        }
    }

    pub(crate) fn force_video_keyframe(&self) {
        if let LocalTrack::Video { track, .. } = self {
            track.force_keyframe();
        }
    }

    #[cfg(test)]
    pub(crate) fn is_video_layer_paused(&self, rid: &str) -> Option<bool> {
        match self {
            LocalTrack::Video { track, .. } => track.is_layer_paused(rid),
            LocalTrack::Audio(_) | LocalTrack::ScreenShareAudio(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn video_layer_control_state(&self, rid: &str) -> Option<(bool, u32, u32, f32)> {
        match self {
            LocalTrack::Video { track, .. } => track.layer_control_state(rid),
            LocalTrack::Audio(_) | LocalTrack::ScreenShareAudio(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn opus_track_builds_and_writes_sample() {
        let track = LocalAudioTrack::opus().expect("opus track");
        // Before any PC binding, writes succeed as no-ops (no bindings yet).
        let silence = [0xf8u8, 0xff, 0xfe]; // a tiny Opus silence frame
        track
            .write_sample(&silence, Duration::from_millis(20))
            .await
            .expect("write_sample");
    }

    #[tokio::test]
    async fn write_pcm_paces_without_binding() {
        let track = LocalAudioTrack::opus().expect("opus track");
        let frame = PcmFrame::mono(vec![1000; FRAME_SAMPLES_20MS], OPUS_SAMPLE_RATE);
        track.write_pcm(frame).await.expect("write_pcm");
        // Give the pacer a couple of ticks; it must not panic writing silence.
        tokio::time::sleep(Duration::from_millis(50)).await;
        track.stop();
    }

    #[tokio::test]
    async fn publication_starts_paced_silence_with_truthful_level() {
        let track = LocalAudioTrack::opus().expect("opus track");
        LocalTrack::Audio(track.clone()).start_media();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(track.inner.pacer_started.load(Ordering::SeqCst));
        assert_eq!(track.inner.core.audio_level.load(Ordering::Relaxed), 127);
        track.stop();
    }

    #[test]
    fn pcm_queue_overflow_drops_oldest_and_flush_still_clears() {
        let track = LocalAudioTrack::opus().expect("opus track");
        {
            let mut queue = track
                .inner
                .pcm
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            queue.extend(std::iter::repeat_n(1, PCM_QUEUE_CAPACITY_SAMPLES - 2));
            assert_eq!(push_bounded_pcm(&mut queue, vec![2, 3, 4, 5]), 2);
            assert_eq!(queue.len(), PCM_QUEUE_CAPACITY_SAMPLES);
            assert_eq!(queue.back(), Some(&5));
        }
        track.flush();
        assert!(
            track
                .inner
                .pcm
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty()
        );
    }

    #[tokio::test]
    async fn write_pcm_reports_typed_overflow_after_retaining_newest_audio() {
        let track = LocalAudioTrack::opus().expect("opus track");
        let samples = vec![7; PCM_QUEUE_CAPACITY_SAMPLES + FRAME_SAMPLES_20MS];
        let result = track
            .write_pcm(PcmFrame::mono(samples, OPUS_SAMPLE_RATE))
            .await;
        assert!(matches!(
            result,
            Err(RtcError::PcmQueueOverflow {
                dropped_samples: FRAME_SAMPLES_20MS,
                capacity_samples: PCM_QUEUE_CAPACITY_SAMPLES,
            })
        ));
        let queue = track
            .inner
            .pcm
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(queue.len(), PCM_QUEUE_CAPACITY_SAMPLES);
        assert!(queue.iter().all(|sample| *sample == 7));
        drop(queue);
        track.stop();
    }

    #[test]
    fn audio_level_dbov_maps_rms_to_rfc6464_levels() {
        assert_eq!(audio_level_dbov(0.0), 127, "digital silence");
        assert_eq!(audio_level_dbov(-1.0), 127, "negative rms is silence");
        assert_eq!(audio_level_dbov(1.0), 0, "full scale is loudest");
        assert_eq!(audio_level_dbov(0.1), 20, "-20 dBFS");
        assert_eq!(audio_level_dbov(0.01), 40, "-40 dBFS");
        // Below -127 dBov the level saturates rather than wrapping.
        assert_eq!(audio_level_dbov(f64::MIN_POSITIVE), 127);
    }

    #[test]
    fn audio_level_dbov_crosses_the_sfu_speaking_threshold() {
        // The SFU only counts a participant as speaking below 35 -dBov.
        assert!(
            audio_level_dbov(0.259) < 35,
            "a -12 dBFS tone must count as speech"
        );
        assert!(audio_level_dbov(0.001) > 35, "a -60 dBFS whisper must not");
    }

    #[tokio::test]
    async fn write_pcm_stages_a_level_for_the_encoded_frame() {
        let track = LocalAudioTrack::opus().expect("opus track");
        assert_eq!(
            track.inner.core.audio_level.load(Ordering::Relaxed),
            AUDIO_LEVEL_UNSET,
            "a fresh track stamps no level"
        );
        let loud = PcmFrame::mono(vec![i16::MAX / 2; FRAME_SAMPLES_20MS * 4], OPUS_SAMPLE_RATE);
        track.write_pcm(loud).await.expect("write_pcm");
        tokio::time::sleep(Duration::from_millis(60)).await;
        let level = track.inner.core.audio_level.load(Ordering::Relaxed);
        track.stop();
        assert!(
            level < 35,
            "paced loud PCM must report a speaking level, got {level}"
        );
    }

    #[tokio::test]
    async fn write_sample_level_is_not_inherited_by_a_later_plain_write() {
        let track = LocalAudioTrack::opus().expect("opus track");
        let silence = [0xf8u8, 0xff, 0xfe];
        track
            .write_sample_with_level(&silence, Duration::from_millis(20), 12)
            .await
            .expect("write_sample_with_level");
        assert_eq!(track.inner.core.audio_level.load(Ordering::Relaxed), 12);
        track
            .write_sample(&silence, Duration::from_millis(20))
            .await
            .expect("write_sample");
        assert_eq!(
            track.inner.core.audio_level.load(Ordering::Relaxed),
            AUDIO_LEVEL_UNSET,
            "a write without a level must not reuse the previous one"
        );
    }

    #[tokio::test]
    async fn stopped_track_rejects_writes() {
        let track = LocalAudioTrack::opus().expect("opus track");
        track.stop();
        let err = track
            .write_sample(&[0xf8, 0xff, 0xfe], Duration::from_millis(20))
            .await;
        assert!(matches!(err, Err(RtcError::IllegalState(_))));
    }

    #[tokio::test]
    async fn muted_track_accepts_writes_without_becoming_stopped() {
        let track = LocalAudioTrack::opus().expect("opus track");
        let local = LocalTrack::Audio(track.clone());
        local.set_muted(true);
        track
            .write_sample(&[0xf8, 0xff, 0xfe], Duration::from_millis(20))
            .await
            .expect("muted writes are intentionally dropped");
        local.set_muted(false);
        track
            .write_sample(&[0xf8, 0xff, 0xfe], Duration::from_millis(20))
            .await
            .expect("the same track resumes after unmute");
    }

    #[tokio::test]
    async fn vp8_track_builds() {
        let track = LocalVideoTrack::vp8().expect("vp8 track");
        assert!(track.track_id().starts_with("video-"));
    }

    fn layered_config() -> LocalVideoTrackConfig {
        LocalVideoTrackConfig::default().server_managed()
    }

    fn layered_option(track_type: TrackType, codec: &str) -> PublishOption {
        PublishOption {
            id: 41,
            track_type: track_type as i32,
            codec: Some(super::super::proto::models::Codec {
                name: codec.to_owned(),
                ..Default::default()
            }),
            bitrate: 1_200_000,
            fps: 30,
            max_spatial_layers: 3,
            max_temporal_layers: 3,
            video_dimension: Some(super::super::proto::models::VideoDimension {
                width: 1280,
                height: 720,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn layered_h264_camera_builds_three_rid_encodings() {
        let track = LocalVideoTrack::h264_with_config(layered_config()).expect("layered H264");
        let layers = track
            .configure_for_publish(TrackType::Video, &layered_option(TrackType::Video, "H264"))
            .expect("supported H264 camera topology");
        assert_eq!(
            layers
                .iter()
                .map(|layer| layer.rid.as_str())
                .collect::<Vec<_>>(),
            ["q", "h", "f"]
        );
        assert_eq!(
            track
                .webrtc_tracks()
                .iter()
                .filter_map(|track| track.rid())
                .collect::<Vec<_>>(),
            ["q", "h", "f"]
        );
    }

    #[test]
    fn layered_vp8_screen_share_builds_three_rid_encodings() {
        let track = LocalVideoTrack::vp8_simulcast().expect("layered VP8");
        let layers = track
            .configure_for_publish(
                TrackType::ScreenShare,
                &layered_option(TrackType::ScreenShare, "VP8"),
            )
            .expect("supported VP8 screen-share topology");
        assert_eq!(
            layers
                .iter()
                .map(|layer| layer.rid.as_str())
                .collect::<Vec<_>>(),
            ["q", "h", "f"]
        );
    }

    #[test]
    fn layered_vp9_announces_three_layers_over_one_q_encoding() {
        let track = LocalVideoTrack::vp9_svc().expect("VP9 SVC config");
        let layers = track
            .configure_for_publish(TrackType::Video, &layered_option(TrackType::Video, "VP9"))
            .expect("supported VP9 camera SVC");
        assert_eq!(
            layers
                .iter()
                .map(|layer| layer.rid.as_str())
                .collect::<Vec<_>>(),
            ["q", "h", "f"]
        );
        assert_eq!(track.webrtc_tracks().len(), 1);
        assert_eq!(track.webrtc_tracks()[0].rid(), Some("q"));
        assert_eq!(track.svc_mode(), Some((3, 3)));
    }

    #[tokio::test]
    async fn layered_track_rejects_preencoded_and_rtp_input() {
        let track = LocalVideoTrack::vp9_svc().expect("layered VP9");
        let sample = track
            .write_sample(&[1, 2, 3], Duration::from_millis(33))
            .await
            .expect_err("encoded input cannot be split truthfully");
        assert!(matches!(sample, RtcError::UnsupportedLayeredInput { .. }));
        let rtp = track
            .write_rtp(RtpPacket::default())
            .await
            .expect_err("RTP input cannot be split truthfully");
        assert!(matches!(rtp, RtcError::UnsupportedLayeredInput { .. }));
    }

    #[test]
    fn vp9_svc_quality_control_changes_codec_layers_and_forces_a_key_picture() {
        let track = LocalVideoTrack::vp9_svc().expect("VP9 SVC");
        track
            .configure_for_publish(TrackType::Video, &layered_option(TrackType::Video, "VP9"))
            .expect("configure layers");
        track.inner.encodings[0]
            .force_keyframe
            .store(false, Ordering::SeqCst);

        track.apply_layer_settings(&[VideoLayerSetting {
            name: "q".to_owned(),
            active: true,
            max_bitrate: 400_000,
            max_framerate: 15,
            scale_resolution_down_by: 4.0,
            scalability_mode: "L1T3".to_owned(),
            ..Default::default()
        }]);

        assert_eq!(track.svc_mode(), Some((1, 3)));
        assert_eq!(
            track.layer_control_state("q").map(|state| state.1),
            Some(400)
        );
        assert_eq!(
            track.layer_control_state("q").map(|state| state.3),
            Some(4.0)
        );
        assert!(
            track.inner.encodings[0]
                .force_keyframe
                .load(Ordering::SeqCst)
        );
        assert!(!track.inner.encodings[0].core.is_output_paused());

        track.inner.encodings[0]
            .force_keyframe
            .store(false, Ordering::SeqCst);
        track.apply_layer_settings(&[VideoLayerSetting {
            name: "f".to_owned(),
            active: true,
            scalability_mode: "L3T2_KEY".to_owned(),
            ..Default::default()
        }]);
        assert_eq!(track.svc_mode(), Some((3, 2)));
        assert!(
            track.inner.encodings[0]
                .force_keyframe
                .load(Ordering::SeqCst)
        );
    }

    #[test]
    fn vp9_svc_pauses_when_all_layers_are_disabled_and_forces_a_key_picture_on_resume() {
        let track = LocalVideoTrack::vp9_svc().expect("VP9 SVC");
        track
            .configure_for_publish(TrackType::Video, &layered_option(TrackType::Video, "VP9"))
            .expect("configure layers");
        track.inner.encodings[0]
            .force_keyframe
            .store(false, Ordering::SeqCst);

        track.apply_layer_settings(&[
            VideoLayerSetting {
                name: "q".to_owned(),
                active: false,
                ..Default::default()
            },
            VideoLayerSetting {
                name: "h".to_owned(),
                active: false,
                ..Default::default()
            },
            VideoLayerSetting {
                name: "f".to_owned(),
                active: false,
                ..Default::default()
            },
        ]);

        assert!(track.inner.encodings[0].core.is_output_paused());
        assert!(
            !track.inner.encodings[0]
                .force_keyframe
                .load(Ordering::SeqCst),
            "pausing must not request a frame that cannot be sent"
        );

        track.apply_layer_settings(&[VideoLayerSetting {
            name: "q".to_owned(),
            active: true,
            max_bitrate: 400_000,
            max_framerate: 15,
            scale_resolution_down_by: 4.0,
            scalability_mode: "L1T3".to_owned(),
            ..Default::default()
        }]);

        assert!(!track.inner.encodings[0].core.is_output_paused());
        assert_eq!(track.svc_mode(), Some((1, 3)));
        assert_eq!(track.layer_control_state("q"), Some((false, 400, 15, 4.0)));
        assert!(
            track.inner.encodings[0]
                .force_keyframe
                .load(Ordering::SeqCst),
            "resuming must start with a decodable key picture"
        );
    }

    #[test]
    fn vp9_svc_reconfiguration_preserves_rtp_counters_and_emits_new_ss() {
        let track = LocalVideoTrack::vp9_svc().expect("VP9 SVC");
        let mut option = layered_option(TrackType::Video, "VP9");
        option.video_dimension = Some(super::super::proto::models::VideoDimension {
            width: 320,
            height: 240,
        });
        track
            .configure_for_publish(TrackType::Video, &option)
            .expect("configure layers");
        let frame = vec![128; 320 * 240 * 3 / 2];

        let first =
            encode_i420_layers(&track.inner, &frame, 320, 240, 100, 9_000).expect("first picture");
        let first_packets = &first[0].1;
        let first_last = first_packets.last().expect("first RTP packet");
        assert_eq!(
            first_packets
                .iter()
                .filter(|packet| packet.header.marker)
                .count(),
            1
        );

        track.apply_layer_settings(&[VideoLayerSetting {
            name: "q".to_owned(),
            active: true,
            scalability_mode: "L1T3".to_owned(),
            scale_resolution_down_by: 1.0,
            ..Default::default()
        }]);
        let second = encode_i420_layers(&track.inner, &frame, 320, 240, 100, 9_000)
            .expect("reconfigured picture");
        let second_first = second[0].1.first().expect("second RTP packet");

        assert_eq!(
            second_first.header.sequence_number,
            first_last.header.sequence_number.wrapping_add(1)
        );
        assert_eq!(
            second_first.header.timestamp,
            first_last.header.timestamp.wrapping_add(9_000)
        );
        assert_ne!(
            second_first.payload[0] & 0x02,
            0,
            "reconfigured key picture must carry a new scalability structure"
        );
    }

    #[test]
    fn vp9_scalability_mode_parser_accepts_supported_modes_only() {
        assert_eq!(parse_vp9_scalability_mode("L3T3_KEY"), Some((3, 3)));
        assert_eq!(parse_vp9_scalability_mode("L1T3"), Some((1, 3)));
        assert_eq!(parse_vp9_scalability_mode("L2T2"), None);
        assert_eq!(parse_vp9_scalability_mode("L0T3"), None);
        assert_eq!(parse_vp9_scalability_mode("S3T3"), None);
        assert_eq!(parse_vp9_scalability_mode("L3T3h"), None);
    }

    #[test]
    fn publish_quality_updates_only_the_named_rid_and_forces_keyframe_on_resume() {
        let track = LocalVideoTrack::h264_with_config(layered_config()).expect("layered H264");
        track
            .configure_for_publish(TrackType::Video, &layered_option(TrackType::Video, "H264"))
            .expect("configure layers");
        track.apply_layer_setting(&VideoLayerSetting {
            name: "h".to_owned(),
            active: false,
            max_bitrate: 450_000,
            max_framerate: 15,
            scale_resolution_down_by: 3.0,
            ..Default::default()
        });
        assert!(
            track.inner.encodings[1]
                .core
                .quality_paused
                .load(Ordering::SeqCst)
        );
        assert_eq!(
            track.inner.encodings[1]
                .target_bitrate_kbps
                .load(Ordering::SeqCst),
            450
        );
        assert_eq!(
            track.inner.encodings[1]
                .max_framerate
                .load(Ordering::SeqCst),
            15
        );
        track.inner.encodings[1]
            .force_keyframe
            .store(false, Ordering::SeqCst);
        track.apply_layer_setting(&VideoLayerSetting {
            name: "h".to_owned(),
            active: true,
            ..Default::default()
        });
        assert!(
            track.inner.encodings[1]
                .force_keyframe
                .load(Ordering::SeqCst)
        );
        assert!(
            !track.inner.encodings[0]
                .core
                .quality_paused
                .load(Ordering::SeqCst)
        );
    }

    #[test]
    fn video_config_rejects_zero_bitrate() {
        let result = LocalVideoTrack::vp9_with_config(LocalVideoTrackConfig {
            target_bitrate_bps: 0,
            ..LocalVideoTrackConfig::default()
        });
        assert!(matches!(result, Err(RtcError::Media(_))));
    }

    #[test]
    fn video_config_sets_and_updates_encoder_bitrate() {
        let track = LocalVideoTrack::vp9_with_config(LocalVideoTrackConfig {
            target_bitrate_bps: 750_001,
            ..LocalVideoTrackConfig::default()
        })
        .expect("configured VP9 track");
        assert_eq!(
            track.inner.encodings[0]
                .target_bitrate_kbps
                .load(Ordering::SeqCst),
            751
        );
        track.set_target_bitrate(500_000);
        assert_eq!(
            track.inner.encodings[0]
                .target_bitrate_kbps
                .load(Ordering::SeqCst),
            500
        );
    }

    #[tokio::test]
    async fn vp9_write_i420_encodes_blue_frame() {
        let track = LocalVideoTrack::vp9().expect("vp9 track");
        let (w, h) = (320u32, 240u32);
        let mut buf = vec![41u8; (w * h) as usize];
        buf.extend(std::iter::repeat_n(240u8, ((w / 2) * (h / 2)) as usize));
        buf.extend(std::iter::repeat_n(110u8, ((w / 2) * (h / 2)) as usize));
        for _ in 0..5 {
            track
                .write_i420(&buf, w, h, Duration::from_millis(100))
                .await
                .expect("write_i420 blue frame");
        }
    }

    #[tokio::test]
    async fn h264_write_i420_encodes_blue_frame() {
        let track = LocalVideoTrack::h264().expect("H264 track");
        let (w, h) = (320u32, 240u32);
        let mut buf = vec![41u8; (w * h) as usize];
        buf.extend(std::iter::repeat_n(240u8, ((w / 2) * (h / 2)) as usize));
        buf.extend(std::iter::repeat_n(110u8, ((w / 2) * (h / 2)) as usize));
        track
            .write_i420(&buf, w, h, Duration::from_millis(100))
            .await
            .expect("write_i420 H264 blue frame");
        assert_eq!(track.mime_type(), MIME_TYPE_H264);
    }

    #[tokio::test]
    async fn h264_write_i420_rejects_frames_beyond_level_3_1_before_copy() {
        let track = LocalVideoTrack::h264().expect("H264 track");
        let max_fs_error = track
            .write_i420(&[], 1_920, 1_080, Duration::from_millis(100))
            .await
            .expect_err("1080p exceeds level 3.1 MaxFS");
        assert!(max_fs_error.to_string().contains("level 3.1"));

        let frame_rate_error = track
            .write_i420(&[], 1_280, 720, Duration::from_millis(16))
            .await
            .expect_err("720p60 exceeds the configured level 3.1 rate");
        assert!(
            frame_rate_error
                .to_string()
                .contains("duration is too short")
        );
    }

    #[tokio::test]
    async fn vp9_write_i420_rejects_unbounded_dimensions_before_copy() {
        let track = LocalVideoTrack::vp9().expect("VP9 track");
        let edge_error = track
            .write_i420(&[], 4_096, 2_160, Duration::from_millis(100))
            .await
            .expect_err("local encoders must reject dimensions above the finite cap");
        assert!(edge_error.to_string().contains("local encode limit"));

        let expected_bytes_error = track
            .write_i420(&[], 3_840, 2_162, Duration::from_millis(100))
            .await
            .expect_err("local encoders must reject oversized expected frame bytes");
        assert!(
            expected_bytes_error
                .to_string()
                .contains("local encode limit")
        );
    }

    #[tokio::test]
    async fn vp9_write_i420_accepts_small_frame_from_oversized_backing_slice() {
        let track = LocalVideoTrack::vp9().expect("VP9 track");
        let backing = vec![128; MAX_LOCAL_VIDEO_I420_BYTES + 1];
        track
            .write_i420(&backing, 320, 240, Duration::from_millis(100))
            .await
            .expect("only the dimension-derived frame prefix is copied");
    }
}
