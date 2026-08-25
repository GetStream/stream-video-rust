# getstream-rtc-core

Python bindings for the Stream Video Rust SFU participant stack. The wheel
exposes `RtcSession` over pre-fetched coordinator credentials; Python keeps
location discovery, coordinator join, and coordinator watch.

Requires Python 3.10+ (abi3). The native extension owns a dedicated multi-thread
Tokio runtime and bridges it to asyncio with `pyo3-async-runtimes`.

## Public API

```python
from getstream_rtc_core import (
    IceServer,
    SfuCredentials,
    StatsOptions,
    RtcSession,
    LocalAudioTrack,
    LocalVideoTrack,
    event_queue,
)

credentials = SfuCredentials(
    edge_name=join.credentials.server.edge_name,
    url=join.credentials.server.url,
    ws_endpoint=join.credentials.server.ws_endpoint,
    token=join.credentials.token,
    ice_servers=[
        IceServer(urls=s["urls"], username=s.get("username"), password=s.get("password"))
        for s in join.credentials.ice_servers
    ],
)

session = await RtcSession.join(
    api_key,
    user_token,
    call.call_type,
    call.id,
    user_id,
    credentials,
    stats_options=StatsOptions(
        reporting_interval_ms=join.stats_options.get("reporting_interval_ms", 0),
        enable_rtc_stats=join.stats_options.get("enable_rtc_stats", False),
    ),
    own_capabilities=list(join.own_capabilities),
)

audio = LocalAudioTrack.opus()
await session.publish_audio(audio)
await audio.write_pcm(pcm_bytes, sample_rate=48000, channels=1)

async for event in session.events():
    print(event["kind"])

queue = event_queue(session)
stats = await session.stats()
await session.leave()
```

Remote tracks arrive via `await session.next_track()`. Each `RemoteTrack`
exposes `next_pcm()` / `next_video_frame()`, returning `PcmFrame` / `VideoFrame`
whose payloads are `bytes` objects (buffer protocol). Native zero-copy
`__getbuffer__` is not available on the Python 3.10 limited ABI.

## Build a local wheel

From this directory, with libvpx, cmake, and a C compiler installed:

```bash
pip install maturin
maturin build --release
```

The wheel is written to `target/wheels/` at the repository root (Cargo
workspace target) unless `--out` is passed. Install it with:

```bash
pip install --force-reinstall ../../target/wheels/getstream_rtc_core-*.whl
```

For an editable install while developing:

```bash
maturin develop --release
```
