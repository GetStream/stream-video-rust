mod common;

use std::process::Command;
use std::time::Duration;

use getstream::models::{
    CallRequest, DeleteCallRequest, GetOrCreateCallRequest, MemberRequest, UserRequest,
};
use getstream::rtc::JoinCallData;

const DEFAULT_ITERATIONS: usize = 10;
const MAX_RSS_GROWTH_BYTES: u64 = 64 * 1024 * 1024;

fn process_rss_bytes() -> u64 {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("run ps for RSS measurement");
    assert!(output.status.success(), "ps RSS measurement failed");
    let kibibytes = String::from_utf8(output.stdout)
        .expect("ps RSS output is UTF-8")
        .trim()
        .parse::<u64>()
        .expect("parse ps RSS output");
    kibibytes * 1024
}

#[tokio::test]
async fn repeated_join_leave_has_bounded_rss_growth() {
    let Some(client) = common::client_or_skip() else {
        return;
    };
    let iterations = std::env::var("STREAM_SOAK_ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_ITERATIONS)
        .clamp(1, 100);
    let user_id = common::unique_id("rust-soak");
    let call_id = common::unique_id("rust-soak-call");
    client
        .upsert_users([UserRequest::new(&user_id)])
        .await
        .expect("upsert soak user");

    let call = client.video().call("default", &call_id);
    call.get_or_create(GetOrCreateCallRequest {
        data: Some(CallRequest {
            created_by_id: Some(user_id.clone()),
            members: Some(vec![MemberRequest::new(&user_id)]),
            ..Default::default()
        }),
        ..Default::default()
    })
    .await
    .expect("create soak call");

    let outcome = tokio::time::timeout(Duration::from_secs(30), async {
        call.join(JoinCallData::new(&user_id)).await?;
        call.leave().await
    })
    .await
    .expect("warm-up join/leave timed out");
    outcome.expect("warm-up join/leave failed");
    tokio::time::sleep(Duration::from_millis(250)).await;
    let start_rss = process_rss_bytes();

    for iteration in 1..=iterations {
        let outcome = tokio::time::timeout(Duration::from_secs(30), async {
            call.join(JoinCallData::new(&user_id)).await?;
            call.leave().await
        })
        .await
        .unwrap_or_else(|_| panic!("join/leave iteration {iteration} timed out"));
        outcome.unwrap_or_else(|error| panic!("join/leave iteration {iteration} failed: {error}"));
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    let end_rss = process_rss_bytes();
    let growth = end_rss.saturating_sub(start_rss);
    let _ = call.delete(DeleteCallRequest { hard: Some(true) }).await;

    eprintln!(
        "SOAK_RESULT iterations={iterations} start_rss_bytes={start_rss} \
         end_rss_bytes={end_rss} growth_bytes={growth}"
    );
    assert!(
        growth <= MAX_RSS_GROWTH_BYTES,
        "RSS grew by {growth} bytes across {iterations} join/leave cycles"
    );
}
