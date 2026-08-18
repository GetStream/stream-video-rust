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
