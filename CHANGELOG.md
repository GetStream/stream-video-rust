# Unreleased

## Breaking Changes

### Coordinator WebSocket URL is configured separately

`ClientConfig::coordinator_ws_url` sets the coordinator connect WebSocket URL.
It accepts only `ws` and `wss` URLs and defaults to
`DEFAULT_COORDINATOR_WS_URL`. The URL no longer follows `base_url`: a client
for a staging or local environment must set both fields. Code that builds
`ClientConfig` with a struct literal must set the new field or use
`..ClientConfig::default()`. `DEFAULT_COORDINATOR_WS_URL` moved from
`rtc::coordinator::ws` to the crate root.

### Call events come in one stream for each source

`Call`, `RtcCall` and `RtcCore` replace `subscribe()`, `on()`, `off()` and the
`CallEvent` enum with three streams:

- `sfu_events()` gives `SfuCallEvent`: the events from the SFU, including
  `CallEnded { reason }` for the SFU `call_ended`.
- `coordinator_events()` gives `CoordinatorEvent`: the call-scoped coordinator
  events, including `call.ended`.
- `client_events()` gives `ClientCallEvent`: `CallingStateChanged`.

The SDK leaves the call on the SFU `call_ended` or the coordinator
`call.ended`; the other one may then not arrive. `CallingStateChanged(Left)` is
the reliable end of the call. Each stream has its own buffer, so a lagging
receiver loses events only from its own stream.

### Track events carry the SFU data

`SfuCallEvent::TrackPublished` and `SfuCallEvent::TrackUnpublished` give
`track_type` as a `TrackType`, not an `i32`, and add `participant`.
`TrackUnpublished` also adds `cause`. Patterns that match these variants must
use the new fields or `..`.

### Decoded audio frames carry their RTP timestamp

`PcmFrame` adds `pts: Option<u32>`: the RTP timestamp of the first sample, in
units of 1/48000 s, wrapping like RTP. `RemoteTrack::next_pcm` sets it; a frame
rebuilt for a lost packet counts back from the next packet that arrives. Frames
that the application or a conversion builds have `None`, and `write_pcm` ignores
the field. Code that builds `PcmFrame` with a struct literal must set `pts` or
use `PcmFrame::new` / `PcmFrame::mono`.

### Subscription config has the stream-py shape

`SubscriptionConfig` replaces `audio`, `video`, `screen_share` and
`video_dimension` with the fields of the stream-py `SubscriptionConfig`:

- `default: TrackSubscriptionConfig` gives `track_types`, `video_dimension`
  and `screenshare_dimension`. Screen-share video and screen-share audio are
  now separate track types, and screen share has its own dimension.
- `role_filters` gives a rule by participant role. The first role of the
  participant that has a rule selects it; other participants use `default`.
- `max_subscriptions` limits the number of tracks. The tracks of the
  participants that the call learned about first are kept.

`SubscriptionConfig::default()` now subscribes to nothing, and
`SubscriptionConfig::matches` is removed. The presets `audio_all`,
`audio_video`, `all` and `none` stay. The default video and screen-share
dimension is now 1920×1080 (it was 1280×720), also for a `SubscriptionTarget`
without a dimension. `Call::participants` gives the participants in the order
the call learned about them. `set_incoming_video_enabled` keeps the configured
video dimension and changes only the video track type of the current config.
Before an `update_subscriptions` call, `set_incoming_video_enabled(true)` now
subscribes to video only (it was audio and video), and `false` subscribes to
nothing (it was audio).

### Leave stops the published tracks

`leave` stops every published local track, as JS does with `stopOnLeave`. A
write to a stopped track returns `RtcError::IllegalState`, so a later join must
publish new tracks.

### The PCM queue holds up to 60 s

`write_pcm` queues up to `LocalAudioTrackConfig::pcm_queue_capacity`, 60 s by
default (it was 200 ms). A producer that writes faster than real time gets
`PcmQueueOverflow` only when the queue is full. The capacity must hold at least
one 20 ms frame; `LocalAudioTrack::opus_with_config` returns `RtcError::Media`
for a smaller value.

### H.264 is removed

The SDK no longer depends on OpenH264, so it neither encodes nor decodes H.264.
`LocalVideoTrack::h264`, `h264_with_config`, `h264_simulcast` and
`PreferredVideoCodec::H264` are removed, and `"h264"` no longer parses as a
`PreferredVideoCodec`. The publisher and the subscriber negotiate only VP8 and
VP9 video, so a publisher that sends only H.264 gives the agent no video track.
Use `vp9` or `vp9_svc` for camera video and `vp8` or `vp8_simulcast` for screen
share. The `gpt_realtime_bot` example is voice-only, because OpenAI Realtime
accepts only H.264 video.

## New Features

### Pacing control for PCM tracks

`LocalAudioTrackConfig::pace` (default `true`) starts pacing at the first
`write_pcm`, as before. With `with_pace(false)`, `write_pcm` only queues until
`LocalAudioTrack::start_pacing`; `pause_pacing` stops pacing and keeps the
queue. `Call::publish_audio` holds the queue of a new publication until the SFU
publisher first connects, so the audio written before the connect is not lost.
Pacing does not pause on a later disconnect: audio written during an outage is
lost, and a reconnect adds no delay.

### A stopped track can be replaced by a new track

After `stop_publish`, a new audio, video, or screen-share track of the same kind
and publish option takes over the sender that the stop kept, as JS
`replaceTrack` does. Remote participants keep their remote track and get the
media of the new track. Before, a second video publish failed with
"participant not found". A simulcast track (`vp8_simulcast`) cannot replace or
be replaced: `publish` returns `RtcError::SimulcastReplace`. Turn such a track
off and on with `mute_track` and `unmute_track`.

### A dropped remote track comes back after a republish

Dropping a `RemoteTrack` unsubscribes it. When the publisher publishes the
track again (for example after an unmute), `on_track` now delivers a new
`RemoteTrack` for it, also when the SFU sends the media on the receiver that
the dropped track used. Before, the track did not come back. A track that is
kept through a mute gets the media again and is not delivered a second time.

### A token-only client can prepare a call before the join

`RtcClient::call` returns an `RtcCall` that is not joined yet, and
`RtcCall::join` joins it. Register `on_track` and subscribe before the join to
get the join events and tracks. `Call::rtc` gives the same `RtcCall` type for a
client with an API secret; both handles share one session. `RtcCall` also adds
`update_publish_options` and `set_disconnection_timeout`.

### Stable call event names

`SfuCallEvent::name` gives the stable `SfuEvent` field name of the source event
(for example `participant_joined` or `call_ended`), and
`participant_count_changed`. `ClientCallEvent::name` gives
`calling_state_changed`. A `CoordinatorEvent` has its coordinator `event_type`
(for example `call.created`).

### Configurable call event buffer

`ClientConfig::call_event_capacity` sets how many events each call event
stream keeps for a slow receiver. The default stays 256. A larger value makes a
lag less likely, but each call allocates all slots of its three streams. Code
that builds `ClientConfig` with a struct literal must set the new field or use
`..ClientConfig::default()`.

### Video REST: advanced call statistics and reporting

Application-level stats on `VideoClient` (`get_active_calls_status`,
`query_aggregate_call_stats`, `query_call_session_stats`, `get_daily_digest`,
`query_user_feedback`, `report_client_call_event`) and call-session-scoped stats
on `Call` (`get_call_participant_session_metrics`,
`query_call_participant_sessions`, `get_call_session_participant_stats_details`,
`query_call_session_participant_stats`,
`get_call_session_participant_stats_timeline`).

## Fixes

- After a join or a reconnect, `sfu_events()` gives `ParticipantJoined`,
  `ParticipantUpdated` and `ParticipantLeft` only for the participants that
  changed since the previous join response. Before, each join response sent
  `ParticipantJoined` for every participant. A change of speaking state, audio
  level, connection quality or dominant speaker does not count: its own event
  reports it.
- Each join starts with an empty participant list and call state, also after
  a join that failed or was dropped. A new join no longer reports
  `ParticipantLeft` for the participants of the earlier attempt.
- `ParticipantCountChanged` comes only when the count changes, not with each
  SFU health check.
- A dropped `RemoteTrack` unsubscribes its track only when it is the latest
  track for that participant and track type. It can be dropped on a thread
  without a Tokio runtime.
- A dropped `join` future sets the call back to `Idle`, so the next `join` is
  accepted. A dropped `leave` future still sends the leave, closes the
  connection and sets `Left`.
- The first join retry waits 250–500 ms and a later one up to 2.5 s, as in
  stream-video-js.
- The pacer task of a `LocalAudioTrack` ends when the track is dropped. A
  decoded `PcmFrame` holds only its own samples.
- A muted audio track no longer encodes its PCM.
- While a track is muted, its RTP clock keeps running, for audio and video. The
  first packet after an unmute shows the length of the mute (RFC 3550). Before,
  the timestamps continued from the last packet before the mute.
- `RemoteTrack::next_video_frame` decodes the VP9 SVC that browsers send in
  flexible mode. Before, it gave the first frames and then no more frames: the
  depacketizer kept the reference indices of earlier packets and rejected each
  inter frame after the third reference index.

# v0.1.0-preview.2

docs.rs builds on current nightly. `doc_auto_cfg` was removed in 1.92 and
merged into `doc_cfg`; the crate no longer enables that feature.

# v0.1.0-preview.1

First public preview of `getstream`, the server-side Stream Video SDK for Rust:
the Stream Video REST API plus an SFU WebRTC participant that joins calls, reads
and transforms remote media, and publishes media back into the call.

This is a `0.x` preview, so minor releases may include breaking changes. The
wire-level `rtc` transport modules (`proto`, `peer`, `sfu_ws`, `signal`,
`publisher`, `tracer`, `coordinator_ws`) track Stream's SFU protocol directly
and are exempt from compatibility guarantees at any version bump.

## New Features

### Server client and authentication

`Stream` server client with API key/secret configuration and `from_env`
construction, tunable connection settings and payload limits, user management
(`upsert_users`, `query_users`), and user authentication tokens with optional
expiry and custom claims.

### Video REST

Create, query, update, end, and delete calls; manage members, permissions,
recording, transcription, captions, livestreaming, custom events, and reactions.

### SFU WebRTC participant

`Call::join` / `Call::leave` backed by Stream's retry, reconnect, and migration
state machine (ported from `stream-video-js`). Global and per-session
subscription to remote audio, video, and screen-share tracks, with typed
participant, connection-quality, pin, grant, and inbound-pause state.

### Media access and transforms

Opus audio as PCM, VP8/VP9/H264 video decoded to I420, and raw RTP packets. PCM
utilities in `rtc::pcm` (ported from stream-py's `track_util`): `Resampler` and
streaming `StreamResampler` for rate and channel conversion with exact output
lengths; 32-bit float, raw byte, WAV, and G.711 μ-law / A-law conversion; and
`chunks`, `sliding_windows`, `head`, `tail`, `append`, and `concat` on
`PcmFrame`. G.711 output is byte-identical to FFmpeg's `pcm_mulaw` / `pcm_alaw`.

### Publishing

Local audio and video tracks, publication mute/unmute, screen-share audio, local
video bitrate configuration, and SFU-side noise cancellation. Layered video
publishing: VP9 SVC (`LocalVideoTrack::vp9_svc`), H264 camera simulcast
(`h264_simulcast`), and VP8 screen-share simulcast (`vp8_simulcast`), each
following SFU quality updates.

### Webhooks and observability

Webhook signature verification and typed event parsing, plus structured,
secret-redacted diagnostics through `tracing`. Ships with two examples:
`join_call` and `gpt_realtime_bot` (a Stream-to-OpenAI Realtime audio/video
bridge).

## Known Limitations

- `webrtc-rs` ships no publisher-side congestion controller (TWCC sender / GCC)
  and no RTX/NACK retransmission sender. This does not affect Opus audio, but
  high-bitrate video publishing has no bandwidth estimation or retransmission.
- AV1 is not supported.
- Pre-encoded samples and forwarded RTP are single-layer only.
