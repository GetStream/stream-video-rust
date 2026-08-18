//! Minimal backend participant: create a user and call, join, then leave on
//! Ctrl+C.
//!
//! ```bash
//! cargo run --example join_call
//! ```
//!
//! Reads `STREAM_API_KEY` and `STREAM_API_SECRET` from the environment or a
//! local `.env`. Optional `EXAMPLE_USER_ID`, `EXAMPLE_CALL_TYPE`, and
//! `EXAMPLE_CALL_ID` variables override the generated defaults.

#![allow(dead_code)]

use anyhow::{Context, Result};
use getstream::Stream;
use getstream::models::{CallRequest, GetOrCreateCallRequest, MemberRequest, UserRequest};
use getstream::rtc::JoinCallData;
use getstream::video::Call;

/// Upsert `user_id`, get or create the call, and join it.
pub async fn join_call(
    client: &Stream,
    user_id: &str,
    call_type: &str,
    call_id: &str,
) -> Result<Call> {
    client
        .upsert_users([UserRequest::new(user_id)])
        .await
        .context("upsert user")?;

    let call = client.video().call(call_type, call_id);
    call.get_or_create(GetOrCreateCallRequest {
        data: Some(CallRequest {
            created_by_id: Some(user_id.to_owned()),
            members: Some(vec![MemberRequest::new(user_id)]),
            ..Default::default()
        }),
        ..Default::default()
    })
    .await
    .context("get or create call")?;
    call.join(JoinCallData::new(user_id))
        .await
        .context("join call")?;
    Ok(call)
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "getstream=info,join_call=info".into()),
        )
        .init();

    let client = Stream::from_env().context("STREAM_API_KEY / STREAM_API_SECRET must be set")?;
    let user_id = std::env::var("EXAMPLE_USER_ID").unwrap_or_else(|_| "backend-user".to_owned());
    let call_type = std::env::var("EXAMPLE_CALL_TYPE").unwrap_or_else(|_| "default".to_owned());
    let call_id = std::env::var("EXAMPLE_CALL_ID")
        .unwrap_or_else(|_| format!("rust-join-{}", uuid::Uuid::new_v4().simple()));

    let call = join_call(&client, &user_id, &call_type, &call_id).await?;
    println!("joined {}", call.cid());
    for participant in call.participants() {
        println!("participant: {}", participant.user_id);
    }

    println!("press Ctrl+C to leave");
    let wait_result = tokio::signal::ctrl_c().await.context("listen for Ctrl+C");
    let leave_result = call.leave().await.context("leave call");
    wait_result?;
    leave_result
}
