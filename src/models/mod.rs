//! Serde request/response models, hand-ported from getstream-go `models.go`.
//!
//! Field names track the Go SDK's JSON tags so a later OpenAPI codegen pass can
//! replace these by hand-written types transparently. Response types derive
//! `Default` and `#[serde(default)]` so partial payloads deserialize cleanly and
//! unknown fields are ignored.

mod call;
mod shared;
mod user;

pub use call::*;
pub use shared::*;
pub use user::*;
