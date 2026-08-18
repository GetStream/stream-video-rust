//! Shared helpers for live integration tests.
//!
//! Tests hit the real Stream API and **skip cleanly** when
//! `STREAM_API_KEY` / `STREAM_API_SECRET` are absent (loaded from the repo
//! `.env` via dotenvy). They never assert against mocks.

use getstream::Stream;

/// Construct a live client from `.env` / environment, or return `None` to signal
/// the caller should skip (no credentials configured).
pub fn client_or_skip() -> Option<Stream> {
    // Best-effort: load .env if present. Missing file is fine (CI may inject env).
    let _ = dotenvy::dotenv();

    let has_key = std::env::var("STREAM_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let has_secret = std::env::var("STREAM_API_SECRET")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if !has_key || !has_secret {
        eprintln!("SKIP: STREAM_API_KEY/STREAM_API_SECRET not set; skipping live test");
        return None;
    }

    Some(Stream::from_env().expect("build live client from configured credentials"))
}

/// A unique call ID for this run, so reruns are idempotent.
pub fn unique_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}
