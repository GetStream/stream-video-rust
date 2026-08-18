//! Live Phase 4 media integration tests: publish, subscribe, and republish.
//!
//! Requires live credentials (repo `.env`). Without credentials each test
//! prints a SKIP line and passes without touching the network. Nothing is
//! mocked: a media failure is reported at the stage it occurred (publish RPC,
//! subscription, ICE/DTLS, or RTP read).
//!
//! Audio is synthesized in-test (a 440 Hz sine), published through
//! `LocalAudioTrack` (which Opus-encodes + paces), so no binary fixture is
//! committed.

mod common;

use std::collections::BTreeSet;
use std::f64::consts::PI;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use getstream::models::UserRequest;
use getstream::models::{CallRequest, DeleteCallRequest, GetOrCreateCallRequest, MemberRequest};
use getstream::rtc::proto::models::TrackType;
use getstream::rtc::{
    CallEvent, ClientPublishOptions, JoinCallData, LocalAudioTrack, LocalTrack, LocalVideoTrack,
    PcmFrame, PreferredVideoCodec, RemoteTrack, RtcError, SubscriptionConfig, VideoFrame,
};
use getstream::video::Call;
use tokio::sync::mpsc::{Receiver, channel};
use tokio::task::JoinHandle;
use webrtc::rtp::codecs::vp9::Vp9Packet;
use webrtc::rtp::packetizer::Depacketizer;

const OPUS_SR: u32 = 48_000;
const FRAME_20MS: usize = (OPUS_SR as usize) / 50;
const TONE_HZ: f64 = 440.0;
const TONE_AMP: f64 = 12_000.0;
/// Near-full-scale tone for the audio-level test. The SFU's `ActiveLevel: 35`
/// discards anything quieter than -35 dBov, so a quiet tone would legitimately
/// never be reported as speaking; this sits at roughly -4 dBov.
const LOUD_TONE_AMP: f64 = 26_000.0;
const QUIET_SPEECH_AMP: f64 = 2_000.0;
/// A comfortably-non-silent RMS floor (a full-scale sine is ~0.7; our tone is
/// ~0.26; silence is 0). Well clear of Opus quantization noise.
const NON_SILENT_RMS: f64 = 0.03;

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "getstream=info".into()),
        )
        .with_test_writer()
        .try_init();
}

/// Generate `count` mono 48 kHz samples of a continuous 440 Hz sine at `amp`,
/// advancing the phase counter `n` so consecutive blocks join seamlessly.
fn tone_block(n: &mut u64, count: usize, amp: f64) -> PcmFrame {
    let mut samples = Vec::with_capacity(count);
    for _ in 0..count {
        let t = *n as f64 / f64::from(OPUS_SR);
        samples.push((amp * (2.0 * PI * TONE_HZ * t).sin()) as i16);
        *n += 1;
    }
    PcmFrame::mono(samples, OPUS_SR)
}

/// Feed a continuous tone into `track` in ~real time until the track is stopped
/// (write returns an error). Pre-fills ~200 ms so the pacer never starves.
fn spawn_tone(track: LocalAudioTrack) -> JoinHandle<()> {
    spawn_tone_amp(track, TONE_AMP)
}

fn spawn_tone_amp(track: LocalAudioTrack, amp: f64) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut n: u64 = 0;
        match track
            .write_pcm(tone_block(&mut n, FRAME_20MS * 10, amp))
            .await
        {
            Ok(()) | Err(RtcError::PcmQueueOverflow { .. }) => {}
            Err(_) => return,
        }
        let mut interval = tokio::time::interval(Duration::from_millis(20));
        loop {
            interval.tick().await;
            match track.write_pcm(tone_block(&mut n, FRAME_20MS, amp)).await {
                Ok(()) | Err(RtcError::PcmQueueOverflow { .. }) => {}
                Err(_) => return,
            }
        }
    })
}

/// Alternate a learned quiet floor with loud speech. The SFU's dominant-speaker
/// detector needs more than 10 dB of variation plus a one-second activity
/// window; a constant loud tone is speaking but cannot win a later election.
fn spawn_speech_tone(track: LocalAudioTrack) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut n: u64 = 0;
        let mut frame_index = 0usize;
        let mut interval = tokio::time::interval(Duration::from_millis(20));
        loop {
            interval.tick().await;
            let amp = if frame_index < 25 {
                QUIET_SPEECH_AMP
            } else {
                LOUD_TONE_AMP
            };
            match track.write_pcm(tone_block(&mut n, FRAME_20MS, amp)).await {
                Ok(()) | Err(RtcError::PcmQueueOverflow { .. }) => {}
                Err(error) => panic!("speech tone write failed: {error}"),
            }
            frame_index = (frame_index + 1) % 85;
        }
    })
}

const VIDEO_W: u32 = 320;
const VIDEO_H: u32 = 240;

/// A solid-blue frame in packed I420 (BT.601 limited-range): Y≈41, U≈240, V≈110.
fn solid_blue_i420(width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut buf = vec![41u8; w * h]; // Y plane
    buf.extend(std::iter::repeat_n(240u8, (w / 2) * (h / 2))); // U plane
    buf.extend(std::iter::repeat_n(110u8, (w / 2) * (h / 2))); // V plane
    buf
}

/// Publish a solid-blue I420 frame at ~10 fps until the track is stopped. The
/// SDK encodes each frame with the track's codec and writes RTP.
fn spawn_blue_video(track: LocalVideoTrack) -> JoinHandle<()> {
    spawn_blue_video_at(track, VIDEO_W, VIDEO_H)
}

fn spawn_blue_video_at(track: LocalVideoTrack, width: u32, height: u32) -> JoinHandle<()> {
    tokio::spawn(async move {
        let frame = solid_blue_i420(width, height);
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            interval.tick().await;
            if track
                .write_i420(&frame, width, height, Duration::from_millis(100))
                .await
                .is_err()
            {
                return;
            }
        }
    })
}

/// Parse the fixed-pattern RFC 9628 descriptor emitted by the SDK's VP9 SVC
/// packetizer. Returning a diagnostic instead of silently dropping malformed
/// descriptors keeps the live test failure tied to the violated wire contract.
fn parse_vp9_svc_descriptor(payload: &Bytes) -> Result<Vp9Packet, String> {
    let mut descriptor = Vp9Packet::default();
    descriptor
        .depacketize(payload)
        .map_err(|error| format!("malformed VP9 payload descriptor: {error}"))?;
    if !descriptor.i || !descriptor.l {
        return Err(format!(
            "VP9 SVC descriptor must carry picture and layer IDs: {descriptor:?}"
        ));
    }
    if descriptor.f {
        return Err("VP9 SVC test expected the SDK's fixed temporal pattern".to_owned());
    }
    Ok(descriptor)
}

#[derive(Debug, Default)]
struct Vp9SvcPicture {
    timestamp: Option<u32>,
    ssrcs: BTreeSet<u32>,
    picture_ids: BTreeSet<u16>,
    spatial_ids: BTreeSet<u8>,
    temporal_ids: BTreeSet<u8>,
    tl0_picture_ids: BTreeSet<u8>,
    began_layers: BTreeSet<u8>,
    ended_layers: BTreeSet<u8>,
    marker_layers: BTreeSet<u8>,
    predicted_values: BTreeSet<bool>,
    scalability_dimensions: BTreeSet<Vec<(u16, u16)>>,
}

impl Vp9SvcPicture {
    fn is_complete_for(&self, expected_layers: &BTreeSet<u8>) -> bool {
        let marker_layers = expected_layers
            .last()
            .copied()
            .into_iter()
            .collect::<BTreeSet<_>>();
        self.ssrcs.len() == 1
            && self.picture_ids.len() == 1
            && self.temporal_ids.len() == 1
            && self.tl0_picture_ids.len() == 1
            && self.spatial_ids == *expected_layers
            && self.began_layers == *expected_layers
            && self.ended_layers == *expected_layers
            && self.marker_layers == marker_layers
    }
}

async fn next_vp9_svc_picture(remote: &RemoteTrack) -> Result<Vp9SvcPicture, String> {
    let mut picture = Vp9SvcPicture::default();
    loop {
        let packet = remote
            .read_rtp()
            .await
            .ok_or_else(|| "VP9 SVC remote track ended".to_owned())?;
        if packet.payload.is_empty() {
            continue;
        }
        if picture
            .timestamp
            .is_some_and(|timestamp| timestamp != packet.header.timestamp)
        {
            // Packet loss can hide a marker. Start at the next RTP picture
            // rather than letting observations leak across timestamps.
            picture = Vp9SvcPicture::default();
        }
        picture.timestamp = Some(packet.header.timestamp);

        let parsed = parse_vp9_svc_descriptor(&packet.payload)?;
        picture.ssrcs.insert(packet.header.ssrc);
        picture.picture_ids.insert(parsed.picture_id);
        picture.spatial_ids.insert(parsed.sid);
        picture.temporal_ids.insert(parsed.tid);
        picture.tl0_picture_ids.insert(parsed.tl0picidx);
        picture.predicted_values.insert(parsed.p);
        if parsed.b {
            picture.began_layers.insert(parsed.sid);
        }
        if parsed.e {
            picture.ended_layers.insert(parsed.sid);
        }
        if packet.header.marker {
            picture.marker_layers.insert(parsed.sid);
        }
        if parsed.v && parsed.y {
            picture
                .scalability_dimensions
                .insert(parsed.width.into_iter().zip(parsed.height).collect());
        }
        if packet.header.marker {
            return Ok(picture);
        }
    }
}

async fn await_vp9_svc_layers(
    remote: &RemoteTrack,
    expected_layers: &[u8],
    expected_dimensions: Option<&[(u16, u16)]>,
    consecutive_pictures: usize,
    timeout: Duration,
    all_ssrcs: &mut BTreeSet<u32>,
) -> Result<Vp9SvcPicture, String> {
    let expected_layers = expected_layers.iter().copied().collect::<BTreeSet<_>>();
    let expected_dimensions = expected_dimensions.map(<[_]>::to_vec);
    let mut consecutive = 0usize;

    tokio::time::timeout(timeout, async {
        loop {
            let picture = next_vp9_svc_picture(remote).await?;
            all_ssrcs.extend(&picture.ssrcs);
            let dimensions_match = expected_dimensions
                .as_ref()
                .is_none_or(|dimensions| picture.scalability_dimensions.contains(dimensions));
            if picture.is_complete_for(&expected_layers) && dimensions_match {
                consecutive += 1;
                if consecutive >= consecutive_pictures {
                    return Ok(picture);
                }
            } else {
                consecutive = 0;
            }
        }
    })
    .await
    .map_err(|_| {
        format!(
            "timed out waiting for {consecutive_pictures} complete VP9 SVC picture(s) with \
             layers {expected_layers:?} and dimensions {expected_dimensions:?}"
        )
    })?
}

fn assert_packed_blue_frame(frame: &VideoFrame) {
    assert!(
        frame.width > 0 && frame.height > 0,
        "decoded frame had no dimensions ({}x{})",
        frame.width,
        frame.height
    );
    assert_eq!(
        frame.data.len(),
        (frame.width as usize) * (frame.height as usize) * 3 / 2,
        "decoded buffer is not packed I420 for {}x{}",
        frame.width,
        frame.height
    );

    let rgb = frame.to_rgb8();
    assert_eq!(
        rgb.len(),
        (frame.width as usize) * (frame.height as usize) * 3
    );
    let (r, g, b) = (u32::from(rgb[0]), u32::from(rgb[1]), u32::from(rgb[2]));
    assert!(
        b > 200 && b > r && b > g,
        "decoded pixel was not the published blue: rgb=({r},{g},{b})"
    );
}

/// An `on_track` sink: registers a callback forwarding each `RemoteTrack` to an
/// mpsc channel the test drains.
fn track_sink(call: &Call) -> Receiver<RemoteTrack> {
    let (tx, rx) = channel(8);
    call.on_track(move |track| {
        let _ = tx.try_send(track);
    });
    rx
}

/// Wait up to `timeout` for a `RemoteTrack` from `user` of `track_type`,
/// discarding (and thereby unsubscribing) any others.
async fn recv_track(
    rx: &mut Receiver<RemoteTrack>,
    user: &str,
    track_type: TrackType,
    timeout: Duration,
) -> Option<RemoteTrack> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => return None,
            maybe = rx.recv() => match maybe {
                Some(track) => {
                    if track.participant().user_id == user && track.track_type() == track_type {
                        return Some(track);
                    }
                    // Not the one we want: dropping it unsubscribes it.
                }
                None => return None,
            }
        }
    }
}

/// Read decoded PCM from `remote` until `target` samples are gathered or the
/// overall deadline elapses, returning the RMS of everything collected.
async fn drain_rms(remote: &RemoteTrack, target: usize, overall: Duration) -> f64 {
    let mut all: Vec<i16> = Vec::new();
    let result = tokio::time::timeout(overall, async {
        while all.len() < target {
            match remote.next_pcm().await {
                Some(frame) => all.extend(frame.samples),
                None => break,
            }
        }
    })
    .await;
    // A timeout just means we assess whatever we managed to decode.
    let _ = result;
    PcmFrame::mono(all, OPUS_SR).rms()
}

/// Wait up to `timeout` for a `TrackPublished` (`published == true`) or
/// `TrackUnpublished` (`published == false`) event for `user`/`track_type`,
/// draining unrelated events. Returns whether the event was observed.
async fn await_track_event(
    events: &mut tokio::sync::broadcast::Receiver<CallEvent>,
    user: &str,
    track_type: TrackType,
    published: bool,
    timeout: Duration,
) -> bool {
    use tokio::sync::broadcast::error::RecvError;
    let want = track_type as i32;
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => return false,
            recv = events.recv() => match recv {
                Ok(CallEvent::TrackPublished { user_id, track_type: tt, .. })
                    if published && user_id == user && tt == want => return true,
                Ok(CallEvent::TrackUnpublished { user_id, track_type: tt, .. })
                    if !published && user_id == user && tt == want => return true,
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return false,
            }
        }
    }
}

/// Standard call bootstrap: upsert users and pre-create the call with members.
async fn setup_call(client: &getstream::Stream, users: &[&str]) -> (Call, String) {
    let call_id = common::unique_id("rust-media-call");
    let user_reqs: Vec<UserRequest> = users.iter().map(|u| UserRequest::new(*u)).collect();
    client
        .upsert_users(user_reqs)
        .await
        .expect("upsert_users failed");

    let admin = client.video().call("default", &call_id);
    let members: Vec<MemberRequest> = users.iter().map(|u| MemberRequest::new(*u)).collect();
    admin
        .get_or_create(GetOrCreateCallRequest {
            data: Some(CallRequest {
                created_by_id: Some(users[0].to_owned()),
                members: Some(members),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("get_or_create failed");
    (admin, call_id)
}

/// Test 1: A publishes; B subscribes and receives RTP + decoded PCM
#[tokio::test]
async fn a_publishes_tone_b_receives_rtp_and_pcm() {
    let Some(client) = common::client_or_skip() else {
        return;
    };
    init_tracing();

    let user_a = common::unique_id("a");
    let user_b = common::unique_id("b");
    let (admin, call_id) = setup_call(&client, &[&user_a, &user_b]).await;

    let outcome = tokio::time::timeout(Duration::from_secs(150), async {
        let call_a = client.video().call("default", &call_id);
        let call_b = client.video().call("default", &call_id);

        call_a
            .join(JoinCallData::new(&user_a))
            .await
            .expect("A join");
        let audio_a = LocalAudioTrack::opus().expect("opus track");
        call_a
            .publish_audio(audio_a.clone())
            .await
            .expect("A publish_audio");
        let feeder = spawn_tone(audio_a);

        let mut rx_b = track_sink(&call_b);
        call_b
            .join(JoinCallData::new(&user_b))
            .await
            .expect("B join");
        call_b
            .update_subscriptions(SubscriptionConfig::audio_all())
            .await
            .expect("B update_subscriptions");

        let remote = recv_track(
            &mut rx_b,
            &user_a,
            TrackType::Audio,
            Duration::from_secs(45),
        )
        .await
        .expect("B did not receive A's audio track (subscription/ICE/RTP stage)");

        assert_eq!(remote.participant().user_id, user_a);
        assert!(remote.codec().mime_type.to_lowercase().contains("opus"));

        // Raw RTP: a handful of non-empty packets.
        let mut rtp_packets = 0usize;
        for _ in 0..200 {
            match tokio::time::timeout(Duration::from_secs(10), remote.read_rtp()).await {
                Ok(Some(pkt)) => {
                    if !pkt.payload.is_empty() {
                        rtp_packets += 1;
                        if rtp_packets >= 5 {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(
            rtp_packets >= 5,
            "expected inbound RTP packets, got {rtp_packets}"
        );

        // Decoded PCM: ~500 ms, non-silent.
        let rms = drain_rms(&remote, FRAME_20MS * 25, Duration::from_secs(20)).await;
        assert!(
            rms > NON_SILENT_RMS,
            "decoded PCM was silent (rms={rms:.4})"
        );

        let hold: u64 = std::env::var("LIVE_METRICS_HOLD_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if hold > 0 {
            tokio::time::sleep(Duration::from_secs(hold)).await;
        }
        feeder.abort();
        call_a.leave().await.expect("A leave");
        call_b.leave().await.expect("B leave");
    })
    .await;

    if std::env::var("LIVE_KEEP_CALL").is_err() {
        let _ = admin.delete(DeleteCallRequest { hard: Some(true) }).await;
    }
    outcome.expect("test 1 timed out");
}

/// Test 2: A -> B (PCM bridge) -> C receives non-silent audio (KEY)
#[tokio::test]
async fn republish_pcm_bridge_a_to_b_to_c() {
    let Some(client) = common::client_or_skip() else {
        return;
    };
    init_tracing();

    let user_a = common::unique_id("a");
    let user_b = common::unique_id("b");
    let user_c = common::unique_id("c");
    let (admin, call_id) = setup_call(&client, &[&user_a, &user_b, &user_c]).await;

    let outcome = tokio::time::timeout(Duration::from_secs(180), async {
        let call_a = client.video().call("default", &call_id);
        let call_b = client.video().call("default", &call_id);
        let call_c = client.video().call("default", &call_id);

        // A publishes a tone.
        call_a
            .join(JoinCallData::new(&user_a))
            .await
            .expect("A join");
        let audio_a = LocalAudioTrack::opus().expect("opus a");
        call_a
            .publish_audio(audio_a.clone())
            .await
            .expect("A publish");
        let feeder = spawn_tone(audio_a);

        // B subscribes to A and PCM-bridges onto its own local track.
        let mut rx_b = track_sink(&call_b);
        call_b
            .join(JoinCallData::new(&user_b))
            .await
            .expect("B join");
        call_b
            .update_subscriptions(SubscriptionConfig::audio_all())
            .await
            .expect("B subscribe");
        let remote_a = recv_track(
            &mut rx_b,
            &user_a,
            TrackType::Audio,
            Duration::from_secs(45),
        )
        .await
        .expect("B did not receive A's audio");

        let b_local = LocalAudioTrack::opus().expect("opus b");
        call_b
            .publish_audio(b_local.clone())
            .await
            .expect("B publish");
        let bridge = tokio::spawn(async move {
            let remote_a = Arc::new(remote_a);
            while let Some(pcm) = remote_a.next_pcm().await {
                match b_local.write_pcm(pcm).await {
                    Ok(()) | Err(RtcError::PcmQueueOverflow { .. }) => {}
                    Err(_) => break,
                }
            }
        });

        // C subscribes and must hear B's (bridged) audio: non-silent.
        let mut rx_c = track_sink(&call_c);
        call_c
            .join(JoinCallData::new(&user_c))
            .await
            .expect("C join");
        call_c
            .update_subscriptions(SubscriptionConfig::audio_all())
            .await
            .expect("C subscribe");
        let remote_b = recv_track(
            &mut rx_c,
            &user_b,
            TrackType::Audio,
            Duration::from_secs(60),
        )
        .await
        .expect("C did not receive B's bridged audio");

        let rms = drain_rms(&remote_b, FRAME_20MS * 25, Duration::from_secs(30)).await;
        assert!(
            rms > NON_SILENT_RMS,
            "C received silent audio from B's PCM bridge (rms={rms:.4})"
        );

        bridge.abort();
        feeder.abort();
        call_a.leave().await.expect("A leave");
        call_b.leave().await.expect("B leave");
        call_c.leave().await.expect("C leave");
    })
    .await;

    let _ = admin.delete(DeleteCallRequest { hard: Some(true) }).await;
    outcome.expect("test 2 (PCM bridge) timed out");
}

/// Test 3: A -> B (RTP forward, same codec) -> C receives audio
#[tokio::test]
async fn republish_rtp_forward_a_to_b_to_c() {
    let Some(client) = common::client_or_skip() else {
        return;
    };
    init_tracing();

    let user_a = common::unique_id("a");
    let user_b = common::unique_id("b");
    let user_c = common::unique_id("c");
    let (admin, call_id) = setup_call(&client, &[&user_a, &user_b, &user_c]).await;

    let outcome = tokio::time::timeout(Duration::from_secs(180), async {
        let call_a = client.video().call("default", &call_id);
        let call_b = client.video().call("default", &call_id);
        let call_c = client.video().call("default", &call_id);

        call_a
            .join(JoinCallData::new(&user_a))
            .await
            .expect("A join");
        let audio_a = LocalAudioTrack::opus().expect("opus a");
        call_a
            .publish_audio(audio_a.clone())
            .await
            .expect("A publish");
        let feeder = spawn_tone(audio_a);

        let mut rx_b = track_sink(&call_b);
        call_b
            .join(JoinCallData::new(&user_b))
            .await
            .expect("B join");
        call_b
            .update_subscriptions(SubscriptionConfig::audio_all())
            .await
            .expect("B subscribe");
        let remote_a = recv_track(
            &mut rx_b,
            &user_a,
            TrackType::Audio,
            Duration::from_secs(45),
        )
        .await
        .expect("B did not receive A's audio");

        // Same-codec RTP forward: SDK rewrites SSRC/seq and drops extensions.
        let b_local = LocalAudioTrack::opus().expect("opus b");
        call_b
            .publish_audio(b_local.clone())
            .await
            .expect("B publish");
        let forward = tokio::spawn(async move {
            let remote_a = Arc::new(remote_a);
            while let Some(pkt) = remote_a.read_rtp().await {
                if b_local.write_rtp(pkt).await.is_err() {
                    break;
                }
            }
        });

        let mut rx_c = track_sink(&call_c);
        call_c
            .join(JoinCallData::new(&user_c))
            .await
            .expect("C join");
        call_c
            .update_subscriptions(SubscriptionConfig::audio_all())
            .await
            .expect("C subscribe");
        let remote_b = recv_track(
            &mut rx_c,
            &user_b,
            TrackType::Audio,
            Duration::from_secs(60),
        )
        .await
        .expect("C did not receive B's forwarded audio");

        let rms = drain_rms(&remote_b, FRAME_20MS * 25, Duration::from_secs(30)).await;
        assert!(
            rms > NON_SILENT_RMS,
            "C received silent audio from B's RTP forward (rms={rms:.4})"
        );

        forward.abort();
        feeder.abort();
        call_a.leave().await.expect("A leave");
        call_b.leave().await.expect("B leave");
        call_c.leave().await.expect("C leave");
    })
    .await;

    let _ = admin.delete(DeleteCallRequest { hard: Some(true) }).await;
    outcome.expect("test 3 (RTP forward) timed out");
}

/// Test 5: no video on_track without a video subscription
#[tokio::test]
async fn no_video_on_track_without_video_subscription() {
    let Some(client) = common::client_or_skip() else {
        return;
    };
    init_tracing();

    let user_a = common::unique_id("a");
    let user_b = common::unique_id("b");
    let (admin, call_id) = setup_call(&client, &[&user_a, &user_b]).await;

    let outcome = tokio::time::timeout(Duration::from_secs(150), async {
        let call_a = client.video().call("default", &call_id);
        let call_b = client.video().call("default", &call_id);

        // A publishes both audio and video.
        call_a
            .join(JoinCallData::new(&user_a))
            .await
            .expect("A join");
        let audio_a = LocalAudioTrack::opus().expect("opus a");
        call_a
            .publish_audio(audio_a.clone())
            .await
            .expect("A publish audio");
        let feeder = spawn_tone(audio_a);
        // The SFU's `Video` publish option is VP9 (VP8 is screen-share only).
        let video_a = getstream::rtc::LocalVideoTrack::vp9().expect("vp9 a");
        call_a
            .publish_video(video_a)
            .await
            .expect("A publish video");

        // B subscribes to audio only (the default).
        let mut rx_b = track_sink(&call_b);
        call_b
            .join(JoinCallData::new(&user_b))
            .await
            .expect("B join");
        call_b
            .update_subscriptions(SubscriptionConfig::audio_all())
            .await
            .expect("B subscribe");

        // Audio must arrive (proves subscription works)...
        let audio = recv_track(
            &mut rx_b,
            &user_a,
            TrackType::Audio,
            Duration::from_secs(45),
        )
        .await;
        assert!(audio.is_some(), "B never received A's audio track");

        // ...but no video track should be delivered over the next window.
        let video = recv_track(
            &mut rx_b,
            &user_a,
            TrackType::Video,
            Duration::from_secs(12),
        )
        .await;
        assert!(
            video.is_none(),
            "a video on_track fired without a video subscription"
        );

        feeder.abort();
        call_a.leave().await.expect("A leave");
        call_b.leave().await.expect("B leave");
    })
    .await;

    let _ = admin.delete(DeleteCallRequest { hard: Some(true) }).await;
    outcome.expect("test 5 (no-video) timed out");
}

/// Test 6: A publishes video; B reads RTP and C decodes a frame
#[tokio::test]
async fn publish_blue_video_reaches_raw_rtp_and_i420_decoder() {
    let Some(client) = common::client_or_skip() else {
        return;
    };
    init_tracing();

    let user_a = common::unique_id("a");
    let user_b = common::unique_id("b");
    let user_c = common::unique_id("c");
    let (admin, call_id) = setup_call(&client, &[&user_a, &user_b, &user_c]).await;

    let outcome = tokio::time::timeout(Duration::from_secs(160), async {
        let call_a = client.video().call("default", &call_id);
        let call_b = client.video().call("default", &call_id);
        let call_c = client.video().call("default", &call_id);

        call_a
            .join(JoinCallData::new(&user_a))
            .await
            .expect("A join");
        let video_a = LocalVideoTrack::vp9().expect("vp9 track");
        call_a
            .publish_video(video_a.clone())
            .await
            .expect("A publish_video");
        let feeder = spawn_blue_video(video_a);

        let mut rx_b = track_sink(&call_b);
        call_b
            .join(JoinCallData::new(&user_b))
            .await
            .expect("B join");
        call_b
            .update_subscriptions(SubscriptionConfig::audio_video())
            .await
            .expect("B update_subscriptions");
        let mut rx_c = track_sink(&call_c);
        call_c
            .join(JoinCallData::new(&user_c))
            .await
            .expect("C join");
        call_c
            .update_subscriptions(SubscriptionConfig::audio_video())
            .await
            .expect("C update_subscriptions");

        let raw_remote = recv_track(
            &mut rx_b,
            &user_a,
            TrackType::Video,
            Duration::from_secs(60),
        )
        .await
        .expect("B did not receive A's video track (encode/publish/subscription/RTP stage)");
        let decoded_remote = recv_track(
            &mut rx_c,
            &user_a,
            TrackType::Video,
            Duration::from_secs(60),
        )
        .await
        .expect("C did not receive A's video track (encode/publish/subscription/RTP stage)");

        assert_eq!(raw_remote.track_type(), TrackType::Video);
        assert_eq!(raw_remote.participant().user_id, user_a);
        let mime = raw_remote.codec().mime_type.to_lowercase();
        assert!(
            mime.contains("vp9") || mime.contains("vp8"),
            "unexpected video codec {mime}"
        );

        let mut rtp_packets = 0usize;
        for _ in 0..400 {
            match tokio::time::timeout(Duration::from_secs(15), raw_remote.read_rtp()).await {
                Ok(Some(packet)) if !packet.payload.is_empty() => {
                    rtp_packets += 1;
                    if rtp_packets >= 5 {
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        assert!(
            rtp_packets >= 5,
            "expected inbound video RTP packets, got {rtp_packets}"
        );

        let frame =
            tokio::time::timeout(Duration::from_secs(45), decoded_remote.next_video_frame())
                .await
                .expect("timed out before a frame decoded (reassembly/keyframe/decode stage)")
                .expect("video track ended before a frame decoded");

        assert_packed_blue_frame(&frame);

        // Downscaling the decoded frame must stay a valid I420 buffer (the
        // shape a bot re-encodes or hands to a vision model).
        let small = frame.downscale_to_fit(160);
        assert_eq!(small.width.max(small.height), 160);
        assert_eq!(
            small.data.len(),
            (small.width as usize) * (small.height as usize) * 3 / 2
        );

        feeder.abort();
        call_a.leave().await.expect("A leave");
        call_b.leave().await.expect("B leave");
        call_c.leave().await.expect("C leave");
    })
    .await;

    let _ = admin.delete(DeleteCallRequest { hard: Some(true) }).await;
    outcome.expect("VP9 RTP/decode test timed out");
}

#[tokio::test]
async fn vp9_svc_preserves_one_ssrc_and_adapts_all_spatial_layers() {
    let Some(client) = common::client_or_skip() else {
        return;
    };
    init_tracing();

    let user_a = common::unique_id("svc-a");
    let user_b = common::unique_id("svc-b");
    let (admin, call_id) = setup_call(&client, &[&user_a, &user_b]).await;
    let call_a = client.video().call("default", &call_id);
    let call_b = client.video().call("default", &call_id);
    let mut feeder = None;

    let outcome = tokio::time::timeout(Duration::from_secs(240), async {
        call_a
            .join(JoinCallData::new(&user_a))
            .await
            .map_err(|error| format!("VP9 SVC publisher join failed: {error}"))?;
        let video = LocalVideoTrack::vp9_svc()
            .map_err(|error| format!("VP9 SVC track creation failed: {error}"))?;
        call_a
            .publish_video(video.clone())
            .await
            .map_err(|error| format!("VP9 SVC publish failed: {error}"))?;
        // The current SFU camera PublishOption advertises 1280x720 as its full
        // layer. Layered raw input intentionally has to match that truthful
        // announced dimension.
        feeder = Some(spawn_blue_video_at(video, 1280, 720));

        let mut tracks = track_sink(&call_b);
        call_b
            .join(JoinCallData::new(&user_b))
            .await
            .map_err(|error| format!("VP9 SVC subscriber join failed: {error}"))?;
        call_b
            .update_subscriptions(SubscriptionConfig::audio_video())
            .await
            .map_err(|error| format!("VP9 SVC subscription failed: {error}"))?;
        let remote = recv_track(
            &mut tracks,
            &user_a,
            TrackType::Video,
            Duration::from_secs(60),
        )
        .await
        .ok_or_else(|| "subscriber did not receive the VP9 SVC track".to_owned())?;
        if !remote.codec().mime_type.eq_ignore_ascii_case("video/vp9") {
            return Err(format!(
                "expected VP9 SVC, negotiated {}",
                remote.codec().mime_type
            ));
        }

        let full_dimensions = [(320, 180), (640, 360), (1280, 720)];
        let mut all_ssrcs = BTreeSet::new();

        // High quality must carry a complete q/h/f key picture. Besides seeing
        // every SID, this verifies that the three spatial frames share one RTP
        // timestamp, picture ID, temporal ID, and TL0PICIDX; each layer has
        // proper B/E boundaries; and only f carries the marker.
        let initial_high = await_vp9_svc_layers(
            &remote,
            &[0, 1, 2],
            Some(&full_dimensions),
            1,
            Duration::from_secs(60),
            &mut all_ssrcs,
        )
        .await?;
        if initial_high.predicted_values != BTreeSet::from([false]) {
            return Err(format!(
                "VP9 scalability structure must arrive on a key picture, observed {initial_high:?}"
            ));
        }

        // Ask the SFU for the q dimension. Require three consecutive complete
        // base-only pictures so queued high-quality RTP cannot satisfy the
        // transition accidentally.
        call_b
            .update_subscriptions(SubscriptionConfig {
                audio: false,
                video: true,
                screen_share: false,
                video_dimension: Some((320, 180)),
            })
            .await
            .map_err(|error| format!("VP9 SVC low-quality subscription failed: {error}"))?;
        await_vp9_svc_layers(
            &remote,
            &[0],
            None,
            3,
            Duration::from_secs(60),
            &mut all_ssrcs,
        )
        .await?;

        // Restoring the full dimension must restore q/h/f and emit a new key
        // picture with truthful SS dimensions after the encoder reconfiguration.
        call_b
            .update_subscriptions(SubscriptionConfig {
                audio: false,
                video: true,
                screen_share: false,
                video_dimension: Some((1280, 720)),
            })
            .await
            .map_err(|error| format!("VP9 SVC high-quality subscription failed: {error}"))?;
        let restored_high = await_vp9_svc_layers(
            &remote,
            &[0, 1, 2],
            Some(&full_dimensions),
            1,
            Duration::from_secs(60),
            &mut all_ssrcs,
        )
        .await?;
        if restored_high.predicted_values != BTreeSet::from([false]) {
            return Err(format!(
                "restored VP9 SVC layers must start with a key picture, observed {restored_high:?}"
            ));
        }
        if all_ssrcs.len() != 1 {
            return Err(format!(
                "VP9 SVC must retain one SSRC across quality changes, observed {all_ssrcs:?}"
            ));
        }
        Ok::<(), String>(())
    })
    .await;

    if let Some(feeder) = feeder {
        feeder.abort();
        let _ = feeder.await;
    }
    let _ = call_a.leave().await;
    let _ = call_b.leave().await;
    let _ = admin.delete(DeleteCallRequest { hard: Some(true) }).await;

    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(error)) => panic!("{error}"),
        Err(_) => panic!("VP9 SVC live media test timed out"),
    }
}

#[tokio::test]
async fn publish_h264_video_b_decodes_i420_frame() {
    let Some(client) = common::client_or_skip() else {
        return;
    };
    init_tracing();

    let user_a = common::unique_id("h264-a");
    let user_b = common::unique_id("h264-b");
    let (admin, call_id) = setup_call(&client, &[&user_a, &user_b]).await;

    let outcome = tokio::time::timeout(Duration::from_secs(160), async {
        let call_a = client.video().call("default", &call_id);
        let call_b = client.video().call("default", &call_id);

        call_a.update_publish_options(ClientPublishOptions::new(PreferredVideoCodec::H264));
        call_a
            .join(JoinCallData::new(&user_a))
            .await
            .expect("H264 publisher join");
        let video_a = LocalVideoTrack::h264().expect("H264 track");

        // No public accessor exposes the join response's advertised
        // publish_options, so attempting the publish is the structured skip
        // signal the API gives us: `publish_video` validates the requested codec
        // against the SFU's advertised options before any SetPublisher RPC, so a
        // VP-only edge (all app edges are `-vp`) returns a narrow "did not
        // advertise" media error. Skip cleanly only on that, surface anything
        // else, and run the full publish -> subscribe -> decode assertion path
        // on an H264-advertising edge.
        match call_a.publish_video(video_a.clone()).await {
            Ok(()) => {}
            Err(RtcError::Media(message)) if message.contains("did not advertise") => {
                eprintln!(
                    "SKIP: publish_h264_video_b_decodes_i420_frame: SFU edge does not \
                     advertise H264 ({message})"
                );
                call_a
                    .leave()
                    .await
                    .expect("H264 publisher leave after skip");
                return;
            }
            Err(error) => panic!("unexpected H264 publish_video failure: {error}"),
        }
        let feeder = spawn_blue_video(video_a);

        let mut rx_b = track_sink(&call_b);
        call_b
            .join(JoinCallData::new(&user_b))
            .await
            .expect("H264 subscriber join");
        call_b
            .update_subscriptions(SubscriptionConfig::audio_video())
            .await
            .expect("subscribe to H264 video");

        let remote = recv_track(
            &mut rx_b,
            &user_a,
            TrackType::Video,
            Duration::from_secs(60),
        )
        .await
        .expect("subscriber did not receive the H264 track");
        assert!(
            remote.codec().mime_type.eq_ignore_ascii_case("video/h264"),
            "expected H264, negotiated {}",
            remote.codec().mime_type
        );

        let frame = tokio::time::timeout(Duration::from_secs(45), remote.next_video_frame())
            .await
            .expect("timed out reassembling and decoding H264")
            .expect("H264 track ended before a frame decoded");
        assert_packed_blue_frame(&frame);

        feeder.abort();
        call_a.leave().await.expect("H264 publisher leave");
        call_b.leave().await.expect("H264 subscriber leave");
    })
    .await;

    let _ = admin.delete(DeleteCallRequest { hard: Some(true) }).await;
    outcome.expect("H264 live media test timed out");
}

// Test 8: the SFU reports A taking over as speaking / dominant
//
// Verified through **signaling**, not inbound RTP: the SFU's subscriber
// configuration advertises no audio header extensions, so a second SDK session
// never sees the level on the wire even when everything works. What a browser
// actually renders comes from `AudioLevelChanged` / `DominantSpeakerChanged`,
// which the SFU only emits for publishers whose RTP carries the RFC 6464
// extension — so these events are the real end-to-end signal.

/// Await both an `AudioLevelChanged` naming `session` as speaking and a
/// `DominantSpeakerChanged` naming it, within `timeout`.
async fn await_speaking(
    events: &mut tokio::sync::broadcast::Receiver<CallEvent>,
    session: &str,
    timeout: Duration,
) -> (bool, bool) {
    let (mut level_seen, mut dominant_seen) = (false, false);
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        if level_seen && dominant_seen {
            return (true, true);
        }
        tokio::select! {
            () = &mut deadline => return (level_seen, dominant_seen),
            received = events.recv() => match received {
                Ok(CallEvent::AudioLevelChanged(levels)) => {
                    if levels
                        .iter()
                        .any(|l| l.session_id == session && l.is_speaking)
                    {
                        level_seen = true;
                    }
                }
                Ok(CallEvent::DominantSpeakerChanged { session_id, .. }) => {
                    if session_id == session {
                        dominant_seen = true;
                    }
                }
                Ok(_) => {}
                // Lagged just means we fell behind; keep listening.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return (level_seen, dominant_seen);
                }
            }
        }
    }
}

#[tokio::test]
async fn loud_publisher_is_reported_speaking_and_dominant() {
    let Some(client) = common::client_or_skip() else {
        return;
    };
    init_tracing();

    let user_a = common::unique_id("a");
    let user_b = common::unique_id("b");
    let (admin, call_id) = setup_call(&client, &[&user_a, &user_b]).await;

    let outcome = tokio::time::timeout(Duration::from_secs(150), async {
        let call_a = client.video().call("default", &call_id);
        let call_b = client.video().call("default", &call_id);

        call_a
            .join(JoinCallData::new(&user_a))
            .await
            .expect("A join");
        let session_a = call_a.session_id().await.expect("A session id");
        let audio_a = LocalAudioTrack::opus().expect("opus track");

        // Join and subscribe before any participant publishes audio. A speaker
        // selected before B joins is present in JoinResponse, and the SFU does
        // not replay the earlier DominantSpeakerChanged event to B.
        let mut events_b = call_b.subscribe();
        call_b
            .join(JoinCallData::new(&user_b))
            .await
            .expect("B join");
        call_b
            .update_subscriptions(SubscriptionConfig::audio_all())
            .await
            .expect("B update_subscriptions");
        let session_b = call_b.session_id().await.expect("B session id");
        let audio_b = LocalAudioTrack::opus().expect("B opus track");
        call_b
            .publish_audio(audio_b.clone())
            .await
            .expect("B publish_audio");
        let feeder_b = spawn_tone_amp(audio_b, LOUD_TONE_AMP);

        // Establish B as the sole published speaker first. Publishing A only
        // after B's election forces a B-to-A transition instead of depending on
        // whether A's initial election raced B's join.
        let (b_level_seen, b_dominant_seen) =
            await_speaking(&mut events_b, &session_b, Duration::from_secs(45)).await;
        feeder_b.abort();

        call_a
            .publish_audio(audio_a.clone())
            .await
            .expect("A publish_audio");
        let feeder_a = spawn_speech_tone(audio_a);
        let (level_seen, dominant_seen) =
            await_speaking(&mut events_b, &session_a, Duration::from_secs(45)).await;

        feeder_a.abort();
        call_a.leave().await.expect("A leave");
        call_b.leave().await.expect("B leave");

        assert!(
            b_level_seen,
            "SFU never reported prior speaker {session_b} as speaking in AudioLevelChanged"
        );
        assert!(
            b_dominant_seen,
            "SFU never established prior speaker {session_b} before the transition"
        );
        assert!(
            level_seen,
            "SFU never reported {session_a} as speaking in AudioLevelChanged: \
             outbound audio carries no RFC 6464 ssrc-audio-level extension"
        );
        assert!(
            dominant_seen,
            "SFU never transitioned dominant speaker from {session_b} to {session_a}"
        );
    })
    .await;

    let _ = admin.delete(DeleteCallRequest { hard: Some(true) }).await;
    outcome.expect("test 8 (audio level) timed out");
}

/// Test 9: stopping the sole publication is accepted and un-announced
///
/// Live proof that stopping the *sole* publication no longer trips the SFU's
/// "Invalid SetPublisher request (envelope mismatch)" rejection.
///
/// The publisher keeps its transceiver in the negotiated envelope and signals
/// the stop through `UpdateMuteStates` (matching `stream-video-js`), so no
/// publisher renegotiation with an empty track set is attempted. B watches the
/// typed event stream: it must see A's audio `TrackPublished`, then — once A
/// stops the track — A's audio `TrackUnpublished`, confirming the SFU accepted
/// the request and broadcast the corrected publication state to peers.
#[tokio::test]
async fn stop_publish_of_sole_audio_is_accepted_and_unannounced() {
    let Some(client) = common::client_or_skip() else {
        return;
    };
    init_tracing();

    let user_a = common::unique_id("a");
    let user_b = common::unique_id("b");
    let (admin, call_id) = setup_call(&client, &[&user_a, &user_b]).await;

    let outcome = tokio::time::timeout(Duration::from_secs(150), async {
        let call_a = client.video().call("default", &call_id);
        let call_b = client.video().call("default", &call_id);

        call_b
            .join(JoinCallData::new(&user_b))
            .await
            .expect("B join");
        call_b
            .update_subscriptions(SubscriptionConfig::audio_all())
            .await
            .expect("B update_subscriptions");
        let mut events_b = call_b.subscribe();

        call_a
            .join(JoinCallData::new(&user_a))
            .await
            .expect("A join");
        let audio_a = LocalAudioTrack::opus().expect("opus track");
        call_a
            .publish_audio(audio_a.clone())
            .await
            .expect("A publish_audio");
        let audio_feeder = spawn_tone(audio_a.clone());

        let audio_published = await_track_event(
            &mut events_b,
            &user_a,
            TrackType::Audio,
            true,
            Duration::from_secs(45),
        )
        .await;
        assert!(
            audio_published,
            "B never received A's audio TrackPublished before the stop"
        );

        // Stop the sole publication. Before the fix this renegotiated the
        // publisher with an empty track set and the SFU returned "Invalid
        // SetPublisher request (envelope mismatch)"; now it must be accepted.
        call_a
            .stop_publish(LocalTrack::Audio(audio_a))
            .await
            .expect("A stop_publish accepted by SFU (no SetPublisher envelope mismatch)");
        audio_feeder.abort();

        let audio_unpublished = await_track_event(
            &mut events_b,
            &user_a,
            TrackType::Audio,
            false,
            Duration::from_secs(45),
        )
        .await;
        assert!(
            audio_unpublished,
            "SFU never reported A's audio TrackUnpublished after stop_publish \
             (mute state not propagated to peers)"
        );

        call_a.leave().await.expect("A leave");
        call_b.leave().await.expect("B leave");
    })
    .await;

    let _ = admin.delete(DeleteCallRequest { hard: Some(true) }).await;
    outcome.expect("test 9 (stop sole publication) timed out");
}
