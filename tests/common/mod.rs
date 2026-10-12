//! Shared helpers for live integration tests.
//!
//! Tests hit the real Stream API and **skip cleanly** when
//! `STREAM_API_KEY` / `STREAM_API_SECRET` are absent (loaded from the repo
//! `.env` via dotenvy). They never assert against mocks.

use std::time::Duration;

use getstream::Stream;
use getstream::models::DeleteCallRequest;
use getstream::video::Call;

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

/// Runs `body` as a task for a maximum of `limit`, then hard-deletes the call of
/// `admin`. If `body` panics or times out, the test fails after the delete.
pub async fn run_then_delete<F>(admin: Call, limit: Duration, body: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut task = tokio::spawn(body);
    let outcome = tokio::time::timeout(limit, &mut task).await;
    if outcome.is_err() {
        task.abort();
    }
    let _ = admin.delete(DeleteCallRequest { hard: Some(true) }).await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Ok(Err(error)) => panic!("test task was cancelled: {error}"),
        Err(_) => panic!("test timed out"),
    }
}
