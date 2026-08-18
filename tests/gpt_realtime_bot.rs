//! Live end-to-end test for the `gpt_realtime_bot` example.
//!
//! A second SDK session publishes bursty audio and a blue video frame, then
//! verifies that the bot processes both media tracks and publishes an audible
//! response from OpenAI Realtime.
//!
//! Skips cleanly without `STREAM_API_*` (no client) or without `OPENAI_API_KEY`
//! (no OpenAI bridge). Nothing is mocked.

mod common;

#[allow(dead_code)]
#[path = "../examples/gpt_realtime_bot.rs"]
mod bot;

use std::f64::consts::PI;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use getstream::models::{
    CallRequest, DeleteCallRequest, GetOrCreateCallRequest, MemberRequest, UserRequest,
};
use getstream::rtc::proto::models::TrackType;
use getstream::rtc::{
    JoinCallData, LocalAudioTrack, LocalVideoTrack, PcmFrame, RemoteTrack, SubscriptionConfig,
};
use getstream::video::Call;
use tokio::sync::mpsc::{Receiver, channel};
use tokio::task::JoinHandle;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

const OPUS_SR: u32 = 48_000;
const FRAME_20MS: usize = (OPUS_SR as usize) / 50;
const TONE_HZ: f64 = 300.0;
const TONE_AMP: f64 = 12_000.0;
/// A comfortably non-silent RMS floor (silence is 0; our tone is ~0.26).
const NON_SILENT_RMS: f64 = 0.02;
const VIDEO_W: u32 = 320;
const VIDEO_H: u32 = 240;

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "getstream=info".into()),
        )
        .with_test_writer()
        .try_init();
}

fn tone_frame(n: &mut u64, amplitude: f64) -> PcmFrame {
    let mut samples = Vec::with_capacity(FRAME_20MS);
    for _ in 0..FRAME_20MS {
        let t = *n as f64 / f64::from(OPUS_SR);
        samples.push((amplitude * (2.0 * PI * TONE_HZ * t).sin()) as i16);
        *n += 1;
    }
    PcmFrame::mono(samples, OPUS_SR)
}

/// Feed the tone in ~1.2 s bursts separated by ~0.6 s of silence so the OpenAI
/// server VAD sees discrete turns and replies each time (a continuous tone never
/// yields a `speech_stopped`, so it would never trigger a response).
fn spawn_speech_tone(track: LocalAudioTrack) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut n: u64 = 0;
        let mut interval = tokio::time::interval(Duration::from_millis(20));
        loop {
            for (frames, amplitude) in [(60, TONE_AMP), (30, 0.0)] {
                for _ in 0..frames {
                    interval.tick().await;
                    if track
                        .write_pcm(tone_frame(&mut n, amplitude))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    })
}

/// A solid-blue frame in packed I420 (BT.601 limited-range).
fn solid_blue_i420(width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut buf = vec![41u8; w * h];
    buf.extend(std::iter::repeat_n(240u8, (w / 2) * (h / 2)));
    buf.extend(std::iter::repeat_n(110u8, (w / 2) * (h / 2)));
    buf
}

/// Publish a solid-blue I420 frame at ~10 fps until the track is stopped.
fn spawn_blue_video(track: LocalVideoTrack) -> JoinHandle<()> {
    tokio::spawn(async move {
        let frame = solid_blue_i420(VIDEO_W, VIDEO_H);
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            interval.tick().await;
            if track
                .write_i420(&frame, VIDEO_W, VIDEO_H, Duration::from_millis(100))
                .await
                .is_err()
            {
                return;
            }
        }
    })
}

/// Register an `on_track` sink forwarding each `RemoteTrack` to an mpsc channel.
fn track_sink(call: &Call) -> Receiver<RemoteTrack> {
    let (tx, rx) = channel(8);
    call.on_track(move |track| {
        let _ = tx.try_send(track);
    });
    rx
}

/// Wait up to `timeout` for a `RemoteTrack` from `user` of `track_type`.
async fn recv_track(
    rx: &mut Receiver<RemoteTrack>,
    user: &str,
    track_type: TrackType,
    timeout: Duration,
) -> Option<RemoteTrack> {
    tokio::time::timeout(timeout, async {
        while let Some(track) = rx.recv().await {
            if track.participant().user_id == user && track.track_type() == track_type {
                return Some(track);
            }
        }
        None
    })
    .await
    .unwrap_or(None)
}

async fn wait_until(timeout: Duration, condition: impl Fn() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    condition()
}

async fn stop_task(task: Option<JoinHandle<()>>) {
    if let Some(task) = task {
        task.abort();
        let _ = task.await;
    }
}

/// Read decoded PCM from `remote` in ~500 ms blocks for up to `overall`,
/// returning the **maximum** per-block RMS. Intermittent bot speech (greeting +
/// per-turn replies interleaved with pacer silence) makes a global average
/// misleading, so we detect the loudest window and stop as soon as it clears the
/// non-silent floor.
async fn max_block_rms(remote: &RemoteTrack, overall: Duration) -> f64 {
    let block = FRAME_20MS * 25; // ~500 ms
    let mut acc: Vec<i16> = Vec::with_capacity(block);
    let mut best = 0.0_f64;
    let result = tokio::time::timeout(overall, async {
        loop {
            match remote.next_pcm().await {
                Some(frame) => {
                    acc.extend(frame.samples);
                    while acc.len() >= block {
                        let chunk: Vec<i16> = acc.drain(..block).collect();
                        let rms = PcmFrame::mono(chunk, OPUS_SR).rms();
                        best = best.max(rms);
                        if best > NON_SILENT_RMS {
                            return;
                        }
                    }
                }
                None => return,
            }
        }
    })
    .await;
    let _ = result;
    if !acc.is_empty() {
        let rms = PcmFrame::mono(acc, OPUS_SR).rms();
        best = best.max(rms);
    }
    best
}

#[tokio::test]
async fn gpt_bot_hears_audio_video_and_replies() {
    let Some(client) = common::client_or_skip() else {
        return;
    };
    let Some(cfg) = bot::OpenAiConfig::from_env() else {
        eprintln!("SKIP: OPENAI_API_KEY not set; skipping GPT Realtime bot live test");
        return;
    };
    init_tracing();

    let bot_user = common::unique_id("bot");
    let speaker = common::unique_id("speaker");
    let call_id = common::unique_id("rust-gpt-call");

    // Pre-create the call with both members so the speaker can join + subscribe.
    client
        .upsert_users(vec![
            UserRequest::new(&bot_user),
            UserRequest::new(&speaker),
        ])
        .await
        .expect("upsert_users");
    let admin = client.video().call("default", &call_id);
    admin
        .get_or_create(GetOrCreateCallRequest {
            data: Some(CallRequest {
                created_by_id: Some(speaker.clone()),
                members: Some(vec![
                    MemberRequest::new(&bot_user),
                    MemberRequest::new(&speaker),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("get_or_create");

    let call_s = client.video().call("default", &call_id);
    let mut bot_handle = None;
    let mut tone = None;
    let mut blue = None;
    let outcome: Result<()> = tokio::time::timeout(Duration::from_secs(180), async {
        bot_handle = Some(
            bot::start_bot(&client, &cfg, &bot_user, "default", &call_id)
                .await
                .context("start bot")?,
        );
        let bot = bot_handle.as_ref().expect("bot was just stored");
        let mut rx_s = track_sink(&call_s);
        call_s
            .join(JoinCallData::new(&speaker))
            .await
            .context("speaker join")?;

        let audio = LocalAudioTrack::opus().context("speaker Opus track")?;
        call_s
            .publish_audio(audio.clone())
            .await
            .context("speaker publish audio")?;
        tone = Some(spawn_speech_tone(audio));

        let video = LocalVideoTrack::vp9().context("speaker VP9 track")?;
        call_s
            .publish_video(video.clone())
            .await
            .context("speaker publish video")?;
        blue = Some(spawn_blue_video(video));

        call_s
            .update_subscriptions(SubscriptionConfig::audio_video())
            .await
            .context("speaker subscriptions")?;

        ensure!(
            wait_until(Duration::from_secs(60), || bot.audio_seen()).await,
            "bot on_track never fired for AUDIO (subscription/ICE/RTP stage)"
        );

        ensure!(
            wait_until(Duration::from_secs(60), || bot.video_frames_decoded() >= 3).await,
            "bot received video but decoded only {} frames (reassembly/keyframe/decode stage)",
            bot.video_frames_decoded()
        );

        ensure!(
            wait_until(Duration::from_secs(30), || bot.video_frames_encoded() >= 2).await,
            "bot decoded video but only H264-encoded {} frames for OpenAI \
             (downscale/OpenH264/track-write stage)",
            bot.video_frames_encoded()
        );

        let bot_audio = recv_track(
            &mut rx_s,
            &bot_user,
            TrackType::Audio,
            Duration::from_secs(45),
        )
        .await
        .context("second session never received the bot's audio track")?;
        let rms = max_block_rms(&bot_audio, Duration::from_secs(35)).await;
        ensure!(
            rms > NON_SILENT_RMS,
            "bot produced only silence (max block rms={rms:.4})"
        );

        let codec = bot
            .openai_video_codec()
            .await
            .context("OpenAI's SDP answer carried no video m-line")?;
        ensure!(
            codec.contains("H264"),
            "expected OpenAI to negotiate H264 video, got {codec}"
        );
        ensure!(
            bot.openai_connection_state() == RTCPeerConnectionState::Connected,
            "the OpenAI PeerConnection is not connected: {:?}",
            bot.openai_connection_state()
        );
        Ok(())
    })
    .await
    .context("GPT bot test timed out")
    .and_then(|result| result);

    stop_task(tone).await;
    stop_task(blue).await;
    let speaker_cleanup = call_s.leave().await;
    let bot_cleanup = match bot_handle {
        Some(bot) => bot.shutdown().await,
        None => Ok(()),
    };
    let call_cleanup = admin.delete(DeleteCallRequest { hard: Some(true) }).await;
    if let Err(error) = outcome {
        panic!(
            "{error:#}; speaker cleanup: {speaker_cleanup:?}; bot cleanup: {bot_cleanup:?}; \
             call cleanup: {call_cleanup:?}"
        );
    }
    speaker_cleanup.expect("speaker cleanup");
    bot_cleanup.expect("bot cleanup");
    call_cleanup.expect("call cleanup");
}
