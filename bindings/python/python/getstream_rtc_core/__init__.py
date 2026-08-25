"""Python bindings for the Stream Video Rust RTC stack."""

from __future__ import annotations

import asyncio
from typing import Any

from getstream_rtc_core._native import (
    EventStream,
    IceServer,
    LocalAudioTrack,
    LocalVideoTrack,
    PcmFrame,
    RemoteTrack,
    RtcError,
    RtcSession,
    SfuCredentials,
    StatsOptions,
    VideoFrame,
    __version__,
)

__all__ = [
    "EventStream",
    "IceServer",
    "LocalAudioTrack",
    "LocalVideoTrack",
    "PcmFrame",
    "RemoteTrack",
    "RtcError",
    "RtcSession",
    "SfuCredentials",
    "StatsOptions",
    "VideoFrame",
    "__version__",
    "event_queue",
]


def event_queue(
    session: RtcSession, maxsize: int = 256
) -> asyncio.Queue[dict[str, Any] | None]:
    """Return an asyncio.Queue filled from the session's CallEvent stream.

    A `None` item is placed on the queue when the stream closes.
    """

    queue: asyncio.Queue[dict[str, Any] | None] = asyncio.Queue(maxsize=maxsize)

    async def _pump() -> None:
        try:
            async for event in session.events():
                await queue.put(event)
        finally:
            await queue.put(None)

    queue._pump_task = asyncio.create_task(_pump())  # type: ignore[attr-defined]
    return queue
