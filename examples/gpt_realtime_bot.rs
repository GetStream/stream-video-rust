//! Stream ↔ OpenAI Realtime voice bot over **WebRTC on both sides**.
//!
//! ```bash
//! cargo run --example gpt_realtime_bot
//! ```
//!
//! The bot joins a Stream call, connects to OpenAI Realtime, and bridges audio
//! in both directions. It also sends downscaled H264 video frames to OpenAI.
//!
//! The OpenAI video track uses H264. [`LocalVideoTrack`] performs the encode and
//! RTP packetization in-process through OpenH264. OpenH264 is BSD-2-Clause;
//! applications distributing H264 functionality must evaluate their own patent
//! obligations.
//!
//! # OpenAI Realtime handshake
//!
//! `POST https://api.openai.com/v1/realtime/calls` with
//! `Authorization: Bearer <OPENAI_API_KEY>` — the server-side API key works
//! directly, since ephemeral `client_secrets` are a browser concern. The body is
//! `multipart/form-data` carrying `sdp` (the offer) and a `session` JSON part
//! (`{"type":"realtime","model":…,"instructions":…,"audio":…}`). A success is
//! `201 Created` with a `Location: /v1/realtime/calls/rtc_…` header and the SDP
//! answer as `application/sdp`.
//!
//! The beta shapes are retired: `POST /v1/realtime?model=…` fails with
//! `400 beta_api_shape_disabled`, and no `OpenAI-Beta` header is sent. The
//! endpoint is WHIP-like — one shot, no trickle — so the offer is posted only
//! after ICE gathering completes. OpenAI advertises host candidates only, so
//! that PeerConnection needs no ICE servers.
//!
//! # OpenAI video is H264-only
//!
//! Offering VP8, VP9, AV1, H265, and H264 returns an answer with nine H264
//! payload types and nothing else (`packetization-mode` 0/1, profiles `42001f`,
//! `42e01f`, `640028`–`640033`). With a video transceiver in the offer and no
//! H264 encoder, applying that answer fails with `unable to start track, codec
//! is not supported by remote` and takes the *audio* leg down with it. OpenAI
//! cannot renegotiate video mid-session without losing session state, so this is
//! decided at connect time — hence the in-process H264 encode below.
//!
//! Env: `STREAM_API_KEY`, `STREAM_API_SECRET`, `OPENAI_API_KEY`,
//! `OPENAI_REALTIME_MODEL` (fallback `OPENAI_MODEL`, default `gpt-realtime`),
//! optional `EXAMPLE_BASE_URL` / `EXAMPLE_CALL_TYPE` / `EXAMPLE_CALL_ID`.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use getstream::models::{CallRequest, GetOrCreateCallRequest, MemberRequest, UserRequest};
use getstream::rtc::proto::models::TrackType;
use getstream::rtc::{
    JoinCallData, LocalAudioTrack, LocalVideoTrack, RemoteTrack, RtcError, SubscriptionConfig,
    VideoFrame,
};
use getstream::video::Call;
use getstream::{Stream, TokenOptions};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

const DEFAULT_BASE_URL: &str = "https://getstream.io/video/demos";
const DEFAULT_MODEL: &str = "gpt-realtime";
const OPENAI_CALLS_URL: &str = "https://api.openai.com/v1/realtime/calls";
const OPENAI_EVENT_CHANNEL: &str = "oai-events";
const OPENAI_CONNECT_TIMEOUT: Duration = Duration::from_secs(45);
const TRACK_EVENT_CAPACITY: usize = 32;
const BRIDGE_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const VIDEO_SEND_INTERVAL: Duration = Duration::from_secs(1);
const VIDEO_SEND_MAX_EDGE: u32 = 512;
/// OpenAI Realtime connection settings resolved from the environment.
#[derive(Clone)]
pub struct OpenAiConfig {
    api_key: String,
    pub model: String,
}

impl OpenAiConfig {
    /// Load OpenAI credentials and model selection from the environment.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())?;
        let model = std::env::var("OPENAI_REALTIME_MODEL")
            .ok()
            .filter(|m| !m.is_empty())
            .or_else(|| std::env::var("OPENAI_MODEL").ok().filter(|m| !m.is_empty()))
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        Some(Self { api_key, model })
    }
}

/// A running bot with its joined [`Call`], media status, and bridge tasks.
pub struct BotHandle {
    pub call: Call,
    progress: Arc<MediaProgress>,
    openai: OpenAiSession,
    bridge: MediaBridge,
}

impl BotHandle {
    /// Whether the bot has received a remote audio track.
    pub fn audio_seen(&self) -> bool {
        self.progress.audio_seen.load(Ordering::Relaxed)
    }

    /// Number of inbound video frames decoded by the bot.
    pub fn video_frames_decoded(&self) -> u64 {
        self.progress.frames_decoded.load(Ordering::Relaxed)
    }

    /// Number of video frames encoded for OpenAI.
    pub fn video_frames_encoded(&self) -> u64 {
        self.progress.frames_encoded.load(Ordering::Relaxed)
    }

    /// The OpenAI PeerConnection's connection state (ICE + DTLS).
    pub fn openai_connection_state(&self) -> RTCPeerConnectionState {
        self.openai.pc.connection_state()
    }

    /// The codec OpenAI negotiated on the video m-line, or `None` if its answer
    /// carried no video at all.
    pub async fn openai_video_codec(&self) -> Option<String> {
        let remote = self.openai.pc.remote_description().await?;
        negotiated_video_codec(&remote.sdp)
    }

    /// Cancel and reap bridge tasks, close OpenAI, and leave the Stream call.
    pub async fn shutdown(self) -> Result<()> {
        let bridge_result = self.bridge.shutdown().await;
        let openai_result = self.openai.shutdown().await;
        let leave_result = self.call.leave().await.context("leave");
        combine_cleanup_results(bridge_result, openai_result, leave_result)
    }
}

struct OpenAiSession {
    pc: Arc<RTCPeerConnection>,
    _events: Arc<RTCDataChannel>,
    mic: LocalAudioTrack,
    camera: LocalVideoTrack,
    tasks: TaskGroup,
}

impl OpenAiSession {
    async fn shutdown(self) -> Result<()> {
        let close_result = self.pc.close().await.context("close OpenAI PeerConnection");
        self.tasks.shutdown().await;
        close_result
    }
}

#[derive(Clone)]
struct TaskGroup(Arc<StdMutex<Option<JoinSet<()>>>>);

impl TaskGroup {
    fn new() -> Self {
        Self(Arc::new(StdMutex::new(Some(JoinSet::new()))))
    }

    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        if let Some(tasks) = self.0.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            tasks.spawn(task);
        }
    }

    async fn shutdown(&self) {
        let tasks = self.0.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(mut tasks) = tasks else {
            return;
        };
        tasks.abort_all();
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result
                && !error.is_cancelled()
            {
                tracing::warn!(%error, "gpt_realtime_bot: background task failed");
            }
        }
    }
}

fn combine_cleanup_results(
    bridge: Result<()>,
    openai: Result<()>,
    stream: Result<()>,
) -> Result<()> {
    let errors = [("bridge", bridge), ("OpenAI", openai), ("Stream", stream)]
        .into_iter()
        .filter_map(|(name, result)| result.err().map(|error| format!("{name}: {error:#}")))
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        bail!(errors.join("; "))
    }
}

/// Build an authenticated browser URL for a human participant.
pub fn demo_join_url(
    base_url: &str,
    call_id: &str,
    api_key: &str,
    token: &str,
    user_name: &str,
) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("api_key", api_key)
        .append_pair("token", token)
        .append_pair("skip_lobby", "true")
        .append_pair("user_name", user_name)
        .append_pair("bitrate", "1500000")
        .append_pair("w", "1280")
        .append_pair("h", "720")
        .finish();
    format!("{}/join/{call_id}?{query}", base_url.trim_end_matches('/'))
}

fn session_config(cfg: &OpenAiConfig, instructions: &str) -> Value {
    json!({
        "type": "realtime",
        "model": cfg.model,
        "instructions": instructions,
        "audio": {
            "input": { "turn_detection": { "type": "server_vad" } },
            "output": { "voice": "marin" }
        }
    })
}

async fn exchange_sdp(cfg: &OpenAiConfig, offer_sdp: &str, session: &Value) -> Result<String> {
    let boundary = format!("----getstream-rust-{}", uuid::Uuid::new_v4().simple());
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\n{offer_sdp}\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"session\"\r\n\r\n{session}\r\n\
         --{boundary}--\r\n"
    );

    let response = reqwest::Client::new()
        .post(OPENAI_CALLS_URL)
        .bearer_auth(&cfg.api_key)
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("Accept", "application/sdp")
        .body(body)
        .send()
        .await
        .context("POST /v1/realtime/calls")?;

    let status = response.status();
    let text = response.text().await.context("read SDP answer")?;
    if !status.is_success() {
        bail!("OpenAI Realtime SDP exchange failed ({status}): {text}");
    }
    Ok(text)
}

async fn connect_openai(
    cfg: &OpenAiConfig,
    stream_audio: LocalAudioTrack,
) -> Result<OpenAiSession> {
    let mut media_engine = MediaEngine::default();
    media_engine
        .register_default_codecs()
        .context("register default codecs")?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)
        .context("register interceptors")?;
    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();
    let pc = Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .context("create OpenAI PeerConnection")?,
    );
    let tasks = TaskGroup::new();

    let result = match timeout(
        OPENAI_CONNECT_TIMEOUT,
        configure_openai(cfg, stream_audio, pc.clone(), tasks.clone()),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow!(
            "OpenAI Realtime negotiation exceeded {} seconds",
            OPENAI_CONNECT_TIMEOUT.as_secs()
        )),
    };
    if result.is_err() {
        let _ = pc.close().await;
        tasks.shutdown().await;
    }
    result
}

async fn configure_openai(
    cfg: &OpenAiConfig,
    stream_audio: LocalAudioTrack,
    pc: Arc<RTCPeerConnection>,
    tasks: TaskGroup,
) -> Result<OpenAiSession> {
    let events = pc
        .create_data_channel(OPENAI_EVENT_CHANNEL, None)
        .await
        .context("create oai-events data channel")?;

    let mic = LocalAudioTrack::opus().context("opus track for OpenAI")?;
    spawn_rtcp_drain(
        &tasks,
        pc.add_track(mic.webrtc_track())
            .await
            .context("add audio track to the OpenAI PeerConnection")?,
    );

    let camera = LocalVideoTrack::h264().context("H264 track for OpenAI")?;
    spawn_rtcp_drain(
        &tasks,
        pc.add_track(camera.webrtc_track())
            .await
            .context("add video track to the OpenAI PeerConnection")?,
    );

    let weak_pc = Arc::downgrade(&pc);
    let to_stream = stream_audio.clone();
    let audio_tasks = tasks.clone();
    pc.on_track(Box::new(move |track, _receiver, _transceiver| {
        let weak_pc = weak_pc.clone();
        let to_stream = to_stream.clone();
        let audio_tasks = audio_tasks.clone();
        Box::pin(async move {
            let mime = track.codec().capability.mime_type;
            tracing::info!(%mime, "gpt_realtime_bot: OpenAI audio track");
            let Some(pc) = weak_pc.upgrade() else { return };
            let remote = RemoteTrack::from_webrtc(track, TrackType::Audio, &pc);
            audio_tasks.spawn(async move {
                while let Some(pcm) = remote.next_pcm().await {
                    match to_stream.write_pcm(pcm).await {
                        Ok(()) | Err(RtcError::PcmQueueOverflow { .. }) => {}
                        Err(error) => {
                            tracing::warn!(
                                error = %error,
                                "gpt_realtime_bot: stopped OpenAI audio bridge"
                            );
                            return;
                        }
                    }
                }
            });
        })
    }));

    let barge_in = stream_audio.clone();
    events.on_message(Box::new(move |msg: DataChannelMessage| {
        let barge_in = barge_in.clone();
        Box::pin(async move {
            handle_openai_event(&msg.data, &barge_in);
        })
    }));

    let greeter = events.clone();
    events.on_open(Box::new(move || {
        let greeter = greeter.clone();
        Box::pin(async move {
            if let Err(e) = send_event(&greeter, &json!({ "type": "response.create" })).await {
                tracing::warn!(error = %e, "gpt_realtime_bot: failed to request the greeting");
            }
        })
    }));

    let instructions = "You are a friendly voice assistant on a live video call. \
        Greet the caller warmly in one short sentence, then answer questions concisely.";

    let offer = pc.create_offer(None).await.context("create offer")?;
    let mut gathered = pc.gathering_complete_promise().await;
    pc.set_local_description(offer)
        .await
        .context("set local description")?;
    let _ = gathered.recv().await;
    let offer_sdp = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("no local description after ICE gathering"))?
        .sdp;

    let answer = exchange_sdp(cfg, &offer_sdp, &session_config(cfg, instructions)).await?;
    match negotiated_video_codec(&answer) {
        Some(codec) => tracing::info!(%codec, "gpt_realtime_bot: OpenAI negotiated video"),
        None => tracing::warn!(
            "gpt_realtime_bot: OpenAI's answer carries no video m-line — the model will not see the caller"
        ),
    }
    pc.set_remote_description(
        RTCSessionDescription::answer(answer).context("parse OpenAI SDP answer")?,
    )
    .await
    .context("apply OpenAI SDP answer")?;

    tracing::info!(model = %cfg.model, "gpt_realtime_bot: OpenAI PeerConnection negotiated");
    Ok(OpenAiSession {
        pc,
        _events: events,
        mic,
        camera,
        tasks,
    })
}

fn spawn_rtcp_drain(
    tasks: &TaskGroup,
    sender: Arc<webrtc::rtp_transceiver::rtp_sender::RTCRtpSender>,
) {
    tasks.spawn(async move {
        let mut buf = vec![0u8; 1500];
        while sender.read(&mut buf).await.is_ok() {}
    });
}

/// The codec on the answer's video m-line, e.g. `H264/90000 (payload type 100)`.
/// `None` means the answer has no video at all.
fn negotiated_video_codec(sdp: &str) -> Option<String> {
    let m_line = sdp.lines().find(|l| l.starts_with("m=video"))?;
    let payload_type = m_line.split_whitespace().nth(3)?;
    let rtpmap = format!("a=rtpmap:{payload_type} ");
    let codec = sdp
        .lines()
        .find_map(|l| l.strip_prefix(&rtpmap))
        .unwrap_or("unknown");
    Some(format!("{codec} (payload type {payload_type})"))
}

async fn send_event(channel: &Arc<RTCDataChannel>, event: &Value) -> Result<()> {
    channel
        .send_text(event.to_string())
        .await
        .context("send event on oai-events")?;
    Ok(())
}

fn handle_openai_event(payload: &[u8], stream_audio: &LocalAudioTrack) {
    let Ok(event) = serde_json::from_slice::<Value>(payload) else {
        return;
    };
    match event["type"].as_str().unwrap_or_default() {
        "input_audio_buffer.speech_started" => stream_audio.flush(),
        "response.output_audio_transcript.delta" => {
            tracing::trace!(delta = %event["delta"], "gpt_realtime_bot: transcript");
        }
        "error" => {
            tracing::warn!(error = %event["error"], "gpt_realtime_bot: OpenAI error event");
        }
        other => tracing::debug!(event = other, "gpt_realtime_bot: OpenAI event"),
    }
}

#[derive(Default)]
struct MediaProgress {
    audio_seen: AtomicBool,
    frames_decoded: AtomicU64,
    frames_encoded: AtomicU64,
    latest_frame: StdMutex<Option<VideoFrame>>,
}

async fn pump_audio_in(
    remote: RemoteTrack,
    mic: LocalAudioTrack,
    mut cancel: watch::Receiver<bool>,
) {
    loop {
        let frame = tokio::select! {
            _ = cancel.changed() => return,
            frame = remote.next_pcm() => frame,
        };
        let Some(frame) = frame else { return };
        match mic.write_pcm(frame).await {
            Ok(()) | Err(RtcError::PcmQueueOverflow { .. }) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "gpt_realtime_bot: stopped inbound audio bridge"
                );
                return;
            }
        }
    }
}

async fn pump_video_in(
    remote: RemoteTrack,
    progress: Arc<MediaProgress>,
    mut cancel: watch::Receiver<bool>,
) {
    let user = remote.participant().user_id.clone();
    tracing::info!(%user, codec = %remote.codec().mime_type, "gpt_realtime_bot: video track");

    loop {
        let frame = tokio::select! {
            _ = cancel.changed() => return,
            frame = remote.next_video_frame() => frame,
        };
        let Some(frame) = frame else { break };
        let n = progress.frames_decoded.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n.is_multiple_of(30) {
            tracing::info!(
                %user,
                frames = n,
                width = frame.width,
                height = frame.height,
                "gpt_realtime_bot: decoded video"
            );
        }
        *progress
            .latest_frame
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(frame);
    }
    tracing::info!(%user, "gpt_realtime_bot: video track ended");
}

async fn pump_video_encoder(
    progress: Arc<MediaProgress>,
    camera: LocalVideoTrack,
    mut cancel: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(VIDEO_SEND_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancel.changed() => return,
            _ = interval.tick() => {}
        }
        let Some(frame) = progress
            .latest_frame
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            continue;
        };
        let frame = frame.downscale_to_fit(VIDEO_SEND_MAX_EDGE);

        match camera
            .write_i420(&frame.data, frame.width, frame.height, VIDEO_SEND_INTERVAL)
            .await
        {
            Ok(()) => {
                let n = progress.frames_encoded.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 {
                    tracing::info!(
                        width = frame.width,
                        height = frame.height,
                        "gpt_realtime_bot: first H264 frame for OpenAI"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "gpt_realtime_bot: H264 encode failed");
                return;
            }
        }
    }
}

struct MediaBridge {
    cancel: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl MediaBridge {
    fn spawn(
        mut track_rx: mpsc::Receiver<RemoteTrack>,
        progress: Arc<MediaProgress>,
        mic: LocalAudioTrack,
        camera: LocalVideoTrack,
    ) -> Self {
        let (cancel, mut cancel_rx) = watch::channel(false);
        let encoder_cancel = cancel_rx.clone();
        let encoder_progress = progress.clone();
        let task = tokio::spawn(async move {
            let mut pumps = JoinSet::new();
            pumps.spawn(pump_video_encoder(encoder_progress, camera, encoder_cancel));

            loop {
                tokio::select! {
                    _ = cancel_rx.changed() => break,
                    maybe_track = track_rx.recv() => {
                        let Some(track) = maybe_track else { break };
                        match track.track_type() {
                            TrackType::Audio | TrackType::ScreenShareAudio => {
                                progress.audio_seen.store(true, Ordering::Relaxed);
                                pumps.spawn(pump_audio_in(track, mic.clone(), cancel_rx.clone()));
                            }
                            TrackType::Video | TrackType::ScreenShare => {
                                pumps.spawn(pump_video_in(
                                    track,
                                    progress.clone(),
                                    cancel_rx.clone(),
                                ));
                            }
                            TrackType::Unspecified => {}
                        }
                    }
                    result = pumps.join_next(), if !pumps.is_empty() => {
                        if let Some(Err(error)) = result
                            && !error.is_cancelled()
                        {
                            tracing::warn!(%error, "gpt_realtime_bot: media pump failed");
                        }
                    }
                }
            }

            pumps.abort_all();
            while let Some(result) = pumps.join_next().await {
                if let Err(error) = result
                    && !error.is_cancelled()
                {
                    tracing::warn!(%error, "gpt_realtime_bot: media pump failed during shutdown");
                }
            }
        });
        Self { cancel, task }
    }

    async fn shutdown(mut self) -> Result<()> {
        let _ = self.cancel.send(true);
        match timeout(BRIDGE_STOP_TIMEOUT, &mut self.task).await {
            Ok(result) => result.context("bridge supervisor failed"),
            Err(_) => {
                self.task.abort();
                let _ = self.task.await;
                bail!("timed out stopping bridge tasks");
            }
        }
    }
}

async fn cleanup_failed_bot_setup(
    call: &Call,
    openai: Option<OpenAiSession>,
    mut error: anyhow::Error,
) -> anyhow::Error {
    if let Some(openai) = openai
        && let Err(cleanup_error) = openai.shutdown().await
    {
        error = error.context(format!("also failed to close OpenAI: {cleanup_error:#}"));
    }
    if let Err(cleanup_error) = call.leave().await {
        error = error.context(format!("also failed to leave Stream call: {cleanup_error}"));
    }
    error
}

async fn publish_bot_audio(call: &Call) -> Result<LocalAudioTrack> {
    let track = LocalAudioTrack::opus().context("opus track")?;
    call.publish_audio(track.clone())
        .await
        .context("publish_audio")?;
    Ok(track)
}

/// Join the call as `bot`, publish audio, connect the OpenAI PeerConnection, and
/// subscribe to audio + video. Returns a [`BotHandle`]; the caller stays
/// connected and calls [`BotHandle::shutdown`] to leave.
pub async fn start_bot(
    client: &Stream,
    cfg: &OpenAiConfig,
    user_id: &str,
    call_type: &str,
    call_id: &str,
) -> Result<BotHandle> {
    client
        .upsert_users(vec![UserRequest::new(user_id)])
        .await
        .context("upsert bot user")?;

    let call = client.video().call(call_type, call_id);
    call.get_or_create(GetOrCreateCallRequest {
        data: Some(CallRequest {
            created_by_id: Some(user_id.to_owned()),
            members: Some(vec![MemberRequest::new(user_id)]),
            ..Default::default()
        }),
        ..Default::default()
    })
    .await
    .context("get_or_create")?;

    call.join(JoinCallData::create(user_id))
        .await
        .context("join")?;

    let local_audio = match publish_bot_audio(&call).await {
        Ok(track) => track,
        Err(error) => return Err(cleanup_failed_bot_setup(&call, None, error).await),
    };

    let openai = match connect_openai(cfg, local_audio).await {
        Ok(openai) => openai,
        Err(error) => return Err(cleanup_failed_bot_setup(&call, None, error).await),
    };

    let progress = Arc::new(MediaProgress::default());

    let (track_tx, track_rx) = mpsc::channel::<RemoteTrack>(TRACK_EVENT_CAPACITY);
    call.on_track(move |track| match track_tx.try_send(track) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(track)) => {
            tracing::warn!(
                user = %track.participant().user_id,
                track_type = ?track.track_type(),
                "gpt_realtime_bot: dropping track event because the bounded queue is full"
            );
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    });

    if let Err(error) = call
        .update_subscriptions(SubscriptionConfig {
            video_dimension: Some((640, 360)),
            ..SubscriptionConfig::audio_video()
        })
        .await
        .context("update_subscriptions")
    {
        return Err(cleanup_failed_bot_setup(&call, Some(openai), error).await);
    }

    let bridge = MediaBridge::spawn(
        track_rx,
        progress.clone(),
        openai.mic.clone(),
        openai.camera.clone(),
    );

    Ok(BotHandle {
        call,
        progress,
        openai,
        bridge,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "getstream=info,gpt_realtime_bot=info".into()),
        )
        .init();

    let client = Stream::from_env().context("STREAM_API_KEY / STREAM_API_SECRET must be set")?;
    let cfg = OpenAiConfig::from_env()
        .context("OPENAI_API_KEY must be set to run the GPT Realtime bot")?;

    let user_id = std::env::var("EXAMPLE_BOT_USER_ID").unwrap_or_else(|_| "bot".to_owned());
    let call_type = std::env::var("EXAMPLE_CALL_TYPE").unwrap_or_else(|_| "default".to_owned());
    let call_id = std::env::var("EXAMPLE_CALL_ID")
        .unwrap_or_else(|_| format!("rust-gpt-bot-{}", uuid::Uuid::new_v4().simple()));
    let base_url =
        std::env::var("EXAMPLE_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());

    let human_id =
        std::env::var("EXAMPLE_HUMAN_USER_ID").unwrap_or_else(|_| "user-demo-human".to_owned());
    let human_name = "Human User";
    client
        .upsert_users(vec![UserRequest {
            name: Some(human_name.to_owned()),
            ..UserRequest::new(human_id.as_str())
        }])
        .await
        .context("upsert human user")?;
    let token = client
        .create_token_with(
            &human_id,
            TokenOptions {
                expiration: Some(Duration::from_secs(3600)),
                ..Default::default()
            },
        )
        .context("mint human browser token")?;
    let url = demo_join_url(&base_url, &call_id, client.api_key(), &token, human_name);

    let bot = start_bot(&client, &cfg, &user_id, &call_type, &call_id).await?;

    println!("bot joined call {}", bot.call.cid());
    println!("model:      {}", cfg.model);
    println!("open in browser: {url}");
    println!("\nbridging Stream ↔ OpenAI Realtime over WebRTC — press Ctrl+C to leave.");

    let wait_result = tokio::signal::ctrl_c()
        .await
        .context("failed to listen for Ctrl+C");

    println!("\nleaving …");
    let shutdown_result = bot.shutdown().await;
    wait_result?;
    shutdown_result?;
    println!("left cleanly.");
    Ok(())
}
