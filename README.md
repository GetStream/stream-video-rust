# 🦀 Official Rust SDK for [Stream Video](https://getstream.io/video/) (Preview)

[![crates.io](https://img.shields.io/crates/v/getstream.svg)](https://crates.io/crates/getstream)
[![docs.rs](https://img.shields.io/docsrs/getstream)](https://docs.rs/getstream)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org/)
[![Stream License](https://img.shields.io/badge/license-Stream-blue.svg)](https://github.com/GetStream/stream-video-rust/blob/main/LICENSE)

The `getstream` crate is Stream's server-side Rust SDK for building rich video
applications and agents. It combines the Stream Video REST API with an SFU
WebRTC participant: manage users and calls, join calls from your backend, read
remote audio and video, transform it, and publish media back into the call.

## Quick links

- [Stream Video](https://getstream.io/video/)
- [Create a Stream account](https://getstream.io/try-for-free/)
- [Stream Dashboard](https://dashboard.getstream.io/)
- [Video API documentation](https://getstream.io/video/docs/api/)
- [Rust API reference (docs.rs)](https://docs.rs/getstream)
- [Examples](#examples)
- [Contributing](https://github.com/GetStream/stream-video-rust/blob/main/CONTRIBUTING.md)

## Features

- Create and manage Stream users and user authentication tokens.
- Create, query, update, end, and delete video calls.
- Manage call members, permissions, recording, transcription, captions,
  livestreaming, custom events, and reactions.
- Join a call as a server-side SFU participant with retry, reconnect, and
  migration handling.
- Subscribe globally or by participant session to remote audio, video, and
  screen-share tracks.
- Observe typed participant, connection-quality, pin, grant, and inbound-pause
  state from the SFU.
- Read Opus audio as PCM, decode VP8/VP9/H264 video as I420, or work with raw
  RTP packets.
- Resample and rechannel PCM, convert it to 32-bit float, raw bytes, WAV, or
  G.711, and slice it into chunks and sliding windows.
- Transform and republish audio or video through local tracks.
- Temporarily mute publications, publish screen-share audio, configure local
  video bitrate, and control SFU-side noise cancellation.
- Emit structured, secret-redacted diagnostics through `tracing`.

## Crate map

crates.io has no organization namespace. This package is Stream's official
Rust crate, published as [`getstream`](https://crates.io/crates/getstream).

| Item | Role |
| --- | --- |
| [`Stream`](https://docs.rs/getstream/latest/getstream/struct.Stream.html) | Server client: users, tokens, and webhook verification |
| [`Call`](https://docs.rs/getstream/latest/getstream/struct.Call.html) / [`VideoClient`](https://docs.rs/getstream/latest/getstream/struct.VideoClient.html) | Video REST and `Call::join` |
| [`rtc`](https://docs.rs/getstream/latest/getstream/rtc/index.html) | SFU participant, local/remote tracks, and PCM utilities |
| [`models`](https://docs.rs/getstream/latest/getstream/models/index.html) | REST request and response types |
| [`ClientConfig`](https://docs.rs/getstream/latest/getstream/struct.ClientConfig.html) | HTTP timeouts, retries, and payload limits |
| [`webhook`](https://docs.rs/getstream/latest/getstream/webhook/index.html) | Signature verification and typed events |

The wire-level `rtc` transport modules (`proto`, `peer`, `sfu_ws`, `signal`,
`publisher`, `tracer`, `coordinator_ws`) are public because they track Stream's
SFU protocol, but they are exempt from compatibility guarantees.

## Requirements

The WebRTC media stack is part of every build. Install Rust 1.88 or newer, a C
compiler, CMake, `pkg-config`, and `libvpx` before building:

```bash
# macOS
brew install libvpx cmake pkg-config

# Debian / Ubuntu
sudo apt install libvpx-dev cmake pkg-config build-essential
```

`protoc`, a system libopus package, and external media programs such as
`ffmpeg` are not required.

## Installation

```bash
cargo add getstream@0.1.0-preview.2 tokio tracing
```

Or in `Cargo.toml`:

```toml
[dependencies]
getstream = "0.1.0-preview.2"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal"] }
tracing = "0.1"
```

`getstream = "0.1"` will not match this preview. Cargo only selects a pre-release
when the version requirement includes one.

The API reference is published at [docs.rs/getstream](https://docs.rs/getstream);
from a checkout, generate it locally with `cargo doc --open`.

This is a `0.x` preview, so minor releases may contain breaking changes. The
wire-level `rtc` transport modules (`proto`, `peer`, `sfu_ws`, `signal`,
`publisher`, `tracer`, `coordinator_ws`) track Stream's SFU protocol directly and
are exempt from compatibility guarantees at any version bump.

## Getting started

Set `STREAM_API_KEY` and `STREAM_API_SECRET` in the server environment. Never
ship the API secret to a browser or mobile client.

The following example creates a user and token, creates a call, joins it as a
backend participant, sends a reaction, and leaves cleanly:

```rust,no_run
use getstream::models::{
    CallRequest, GetOrCreateCallRequest, MemberRequest, SendVideoReactionRequest,
    UserRequest,
};
use getstream::rtc::JoinCallData;
use getstream::Stream;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Stream::from_env()?;
    let user_id = "video-agent";

    client
        .upsert_users([UserRequest::new(user_id)])
        .await?;

    // Return this token to a trusted client that connects as `user_id`.
    // The server-side SDK keeps STREAM_API_SECRET on the backend.
    let _user_token = client.create_token(user_id)?;

    let call = client.video().call("default", "agent-demo");
    call.get_or_create(GetOrCreateCallRequest {
        data: Some(CallRequest {
            created_by_id: Some(user_id.to_owned()),
            members: Some(vec![MemberRequest::new(user_id)]),
            ..Default::default()
        }),
        ..Default::default()
    })
    .await?;

    call.join(JoinCallData::new(user_id)).await?;
    call.send_reaction(SendVideoReactionRequest {
        reaction_type: "raise-hand".to_owned(),
        emoji_code: Some("✋".to_owned()),
        ..Default::default()
    })
    .await?;

    call.leave().await?;
    Ok(())
}
```

`Call::join` authenticates the backend participant internally. The token from
`create_token` is for a user connecting through another trusted client.

## Accessing and transforming media tracks

Remote tracks expose participant and codec metadata as well as decoded and raw
media. The example below subscribes to audio and video, doubles PCM amplitude,
downscales video to a 512-pixel longest edge, and republishes both streams. A
bounded channel absorbs short track-event bursts, and a `JoinSet` owns every
media task so shutdown can cancel and reap them.

```rust,no_run
use std::time::Duration;

use getstream::rtc::proto::models::TrackType;
use getstream::rtc::{
    LocalAudioTrack, LocalVideoTrack, RemoteTrack, RtcResult, SubscriptionConfig,
};
use getstream::Call;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

async fn transform_track(
    track: RemoteTrack,
    outbound_audio: LocalAudioTrack,
    outbound_video: LocalVideoTrack,
) -> RtcResult<()> {
    match track.track_type() {
        TrackType::Audio | TrackType::ScreenShareAudio => {
            while let Some(mut frame) = track.next_pcm().await {
                for sample in &mut frame.samples {
                    *sample = sample.saturating_mul(2);
                }
                outbound_audio.write_pcm(frame).await?;
            }
        }
        TrackType::Video | TrackType::ScreenShare => {
            while let Some(frame) = track.next_video_frame().await {
                let frame = frame.downscale_to_fit(512);
                outbound_video
                    .write_i420(
                        &frame.data,
                        frame.width,
                        frame.height,
                        Duration::from_millis(33),
                    )
                    .await?;
            }
        }
        TrackType::Unspecified => {}
    }
    Ok(())
}

async fn run_media_bridge(call: &Call) -> Result<(), Box<dyn std::error::Error>> {
    call.update_subscriptions(SubscriptionConfig::audio_video())
        .await?;

    let outbound_audio = LocalAudioTrack::opus()?;
    let outbound_video = LocalVideoTrack::vp9()?;
    call.publish_audio(outbound_audio.clone()).await?;
    call.publish_video(outbound_video.clone()).await?;

    let (track_tx, mut track_rx) = mpsc::channel(16);
    call.on_track(move |track| {
        if track_tx.try_send(track).is_err() {
            tracing::warn!("dropping track event because the media queue is full");
        }
    });

    let mut track_tasks = JoinSet::new();
    loop {
        tokio::select! {
            Some(track) = track_rx.recv() => {
                let outbound_audio = outbound_audio.clone();
                let outbound_video = outbound_video.clone();
                track_tasks.spawn(transform_track(track, outbound_audio, outbound_video));
            }
            Some(result) = track_tasks.join_next(), if !track_tasks.is_empty() => {
                result??;
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    track_tasks.abort_all();
    while track_tasks.join_next().await.is_some() {}
    call.leave().await?;
    Ok(())
}
```

For a complete bridge with cancellation, barge-in, audio and video processing,
and deterministic cleanup, see [`gpt_realtime_bot`](https://github.com/GetStream/stream-video-rust/blob/main/examples/gpt_realtime_bot.rs).

For selective agents, use `Call::update_subscription_targets` with
`SubscriptionTarget` values instead of subscribing to every participant. A
temporary `mute_track` / `unmute_track` preserves the same local track and
sender; `stop_publish` remains terminal for that local track handle. The latest
SFU view is available synchronously through `Call::call_state`.

Layered publishing is opt-in. `LocalVideoTrack::vp9_svc()` provides camera SVC
with up to three spatial and temporal layers on one SSRC.
`LocalVideoTrack::h264_simulcast()` supports camera video and
`LocalVideoTrack::vp8_simulcast()` supports screen share with a `q`/`h`/`f` RID
ladder on one m-line. All three follow SFU quality updates. Feed raw I420 at the
full resolution announced by the SFU publish option; a mismatch is rejected
instead of advertising dimensions that are not sent. Pre-encoded samples and
forwarded RTP remain single-layer-only.

Tracks carry 48 kHz signed 16-bit PCM, which is rarely what a model or telephony
API wants. The [`rtc::pcm`](https://docs.rs/getstream/latest/getstream/rtc/pcm/)
module converts the rate and channel count, converts to 32-bit float, raw bytes,
WAV, or G.711, and slices audio into chunks and sliding windows.

## Examples

From the SDK checkout, create your local environment file and add credentials:

```bash
cp .env.example .env
```

| Variable | Required by | Purpose |
| --- | --- | --- |
| `STREAM_API_KEY` | Both examples | Public Stream application key |
| `STREAM_API_SECRET` | Both examples | Server-only Stream application secret |
| `OPENAI_API_KEY` | `gpt_realtime_bot` | Server-side OpenAI Realtime authentication |
| `OPENAI_REALTIME_MODEL` | `gpt_realtime_bot` | Realtime model; defaults to `gpt-realtime` |
| `EXAMPLE_BASE_URL` | `gpt_realtime_bot` only | Base URL used for its printed browser join link |

Join a call as a backend participant:

```bash
cargo run --example join_call
```

Run the Stream-to-OpenAI Realtime audio/video bridge:

```bash
cargo run --example gpt_realtime_bot
```

Both examples load `.env` for local development. Never commit that file or
print the Stream API secret or OpenAI API key.

## Logging, retries, and security

The library emits structured events through
[`tracing`](https://docs.rs/tracing) and never installs a global subscriber.
Known secret fields are redacted, and HTTP bodies are excluded by default.
Applications must opt in to body logging carefully because custom payloads can
still contain sensitive information.

HTTP retries are opt-in and limited to idempotent reads on rate limits or
transport failures. SFU joins use Stream's reconnect and migration state
machine rather than HTTP retry behavior.

Webhook verification must receive the exact raw request body and the
`X-Signature` header. Verify before parsing or deduplicating the event:

```rust,no_run
# fn handle(
#     client: &getstream::Stream,
#     raw_body: &[u8],
#     x_signature: &str,
# ) -> getstream::Result<()> {
let event = client.parse_webhook(raw_body, x_signature)?;
tracing::info!(event_type = event.event_type(), "verified Stream webhook");
# Ok(())
# }
```

H264 can be subject to patent obligations in some jurisdictions. Applications
that distribute H264 functionality must assess their own requirements.

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](https://github.com/GetStream/stream-video-rust/blob/main/CONTRIBUTING.md) for local
setup, tests, benchmarks, and the checks to run before opening a pull request.

## License

Copyright (c) 2014-2026 Stream.io Inc. All rights reserved.

Licensed under the [Stream License](https://github.com/GetStream/stream-video-rust/blob/main/LICENSE). You may not use this software
except in compliance with that license. The software is distributed on an
"AS IS" basis, without warranties or conditions of any kind.
