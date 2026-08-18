# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While the crate is `0.x`, minor releases may contain breaking changes. The
wire-level `rtc` transport modules (`proto`, `peer`, `sfu_ws`, `signal`,
`publisher`, `tracer`, `coordinator_ws`) track Stream's SFU protocol and are
exempt from compatibility guarantees at any version bump.

## [Unreleased]

## [0.1.0]

First public release.

### Added

- Server-side Stream client (`Stream`) with API key/secret configuration,
  `from_env` construction, and tunable connection settings and payload limits.
- User management and user authentication tokens, with optional expiry and
  custom claims.
- Video coordinator REST: create, query, update, end, and delete calls; manage
  members, permissions, recording, transcription, captions, livestreaming,
  custom events, and reactions.
- SFU WebRTC participant: `Call::join` / `Call::leave` with Stream's retry,
  reconnect, and migration state machine, ported from `stream-video-js`.
- Global and per-session subscription to remote audio, video, and screen-share
  tracks, plus typed participant, connection-quality, pin, grant, and
  inbound-pause state.
- Media access: Opus audio as PCM, VP8/VP9/H264 video decoded to I420, and raw
  RTP packets.
- PCM audio utilities in `rtc::pcm`, ported from stream-py's `track_util`:
  `Resampler` (per-block rate and channel conversion with exact output lengths)
  alongside the streaming `StreamResampler`; 32-bit float, raw byte, WAV, and
  G.711 μ-law / A-law conversion; and `chunks`, `sliding_windows`, `head`,
  `tail`, `append`, and `concat` on `PcmFrame`. G.711 output is byte-identical
  to FFmpeg's `pcm_mulaw` / `pcm_alaw`, which is what stream-py companders
  through.
- Publishing: local audio and video tracks, publication mute/unmute,
  screen-share audio, local video bitrate configuration, and SFU-side noise
  cancellation control.
- Layered video publishing: VP9 SVC (`LocalVideoTrack::vp9_svc`), H264 camera
  simulcast (`h264_simulcast`), and VP8 screen-share simulcast
  (`vp8_simulcast`), each following SFU quality updates.
- Webhook signature verification and typed event parsing.
- Structured, secret-redacted diagnostics through `tracing`.
- Examples: `join_call` and `gpt_realtime_bot` (Stream ↔ OpenAI Realtime
  audio/video bridge).

### Known limitations

- `webrtc-rs` ships no publisher-side congestion controller (TWCC sender / GCC)
  and no RTX/NACK retransmission sender. This does not affect Opus audio, but
  high-bitrate video publishing has no bandwidth estimation or retransmission.
- AV1 is not supported.
- Pre-encoded samples and forwarded RTP are single-layer only.

[Unreleased]: https://github.com/GetStream/stream-video-rust/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/GetStream/stream-video-rust/releases/tag/v0.1.0
