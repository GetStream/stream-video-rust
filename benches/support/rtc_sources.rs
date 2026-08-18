//! Crate-private RTC sources compiled into the Criterion target.
//!
//! Keeping this wrapper named `rtc` preserves the source modules' normal
//! `crate::rtc` and `super` paths while avoiding any benchmark-only public API in
//! the library.

#![allow(dead_code, unused_imports)]

pub(crate) mod error {
    pub(crate) use getstream::rtc::{RtcError, RtcResult as Result};
}

#[path = "../../src/rtc/h264.rs"]
pub(crate) mod h264;
#[path = "../../src/rtc/opus.rs"]
pub(crate) mod opus;
#[path = "../../src/rtc/rtp_h264.rs"]
pub(crate) mod rtp_h264;
#[path = "../../src/rtc/rtp_vpx.rs"]
pub(crate) mod rtp_vpx;
#[path = "../../src/rtc/video_frame.rs"]
pub(crate) mod video_frame;
#[path = "../../src/rtc/vpx.rs"]
pub(crate) mod vpx;
#[path = "../../src/rtc/vpx_decode.rs"]
pub(crate) mod vpx_decode;
