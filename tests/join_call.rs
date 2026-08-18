//! Live test for the `join_call` example (Phase 5, box 81).
//!
//! Exercises the example's `join_call` core directly (no SIGINT), then
//! asserts the joined session appears in its own participant list and leaves
//! cleanly. Skips without live credentials.

mod common;

// Reuse the example's join/subscribe core so the test covers the real code path.
#[path = "../examples/join_call.rs"]
mod example;

use std::time::Duration;

use getstream::models::DeleteCallRequest;

#[tokio::test]
async fn join_call_joins_sees_self_and_leaves() {
    let Some(client) = common::client_or_skip() else {
        return;
    };

    let user = common::unique_id("join-user");
    let call_id = common::unique_id("rust-join-call");

    let outcome = tokio::time::timeout(Duration::from_secs(120), async {
        let call = example::join_call(&client, &user, "default", &call_id)
            .await
            .expect("join_call failed (upsert/get_or_create/join stage)");

        assert_eq!(call.cid(), format!("default:{call_id}"));

        let participants = call.participants();
        assert!(
            participants.iter().any(|p| p.user_id == user),
            "joined session not in participant list: {participants:?}"
        );

        call.leave().await.expect("leave failed");
    })
    .await;

    let admin = client.video().call("default", &call_id);
    let _ = admin.delete(DeleteCallRequest { hard: Some(true) }).await;
    outcome.expect("join_call test timed out");
}
