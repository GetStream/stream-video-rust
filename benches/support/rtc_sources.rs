//! Crate-private RTC sources compiled into the Criterion target.
//!
//! Keeping this wrapper named `rtc` preserves the source modules' normal
//! `crate::rtc` and `super` paths while avoiding any benchmark-only public API in
//! the library.

#![allow(dead_code, unused_imports)]

pub(crate) mod error {
    pub(crate) use getstream::rtc::{RtcError, RtcResult as Result};
}

#[path = "../../src/rtc/codecs/mod.rs"]
pub(crate) mod codecs;
#[path = "../../src/rtc/video_frame.rs"]
pub(crate) mod video_frame;
