//! Generated SFU wire types (prost) for the `stream.video.sfu.*` packages.
//!
//! The Rust here is emitted at build time by [`build.rs`](../../build.rs) from
//! the vendored `.proto` sources under `proto/` and included verbatim. The
//! include file rebuilds the protobuf package hierarchy
//! (`stream::video::sfu::{models, event, signal}`) so cross-package references
//! resolve; the [`models`], [`event`], and [`signal`] re-exports below are the
//! stable paths the rest of the crate should use.
#![allow(clippy::all)]
#![allow(clippy::pedantic)]
#![allow(clippy::nursery)]
#![allow(missing_docs)]
#![allow(rustdoc::all)]

include!(concat!(env!("OUT_DIR"), "/_sfu_proto.rs"));

#[doc(inline)]
pub use self::stream::video::sfu::{event, models, signal};
