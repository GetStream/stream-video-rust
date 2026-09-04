//! Live integration tests for the advanced call-stats REST surface.
//!
//! Run with credentials present (repo `.env`): `cargo test`. Without credentials
//! the tests print a SKIP line and pass without touching the API. The call
//! created for the session-scoped queries is deleted on every exit path.
//!
//! Stats are computed asynchronously by the server, so the per-session queries
//! tolerate a `404` (stats not yet available) rather than asserting on analytics
//! values; when a payload is returned, its identity fields must echo the call.

mod common;

use getstream::models::{
    CallRequest, DeleteCallRequest, GetOrCreateCallRequest, QueryCallParticipantSessionsRequest,
    QueryCallSessionParticipantStatsRequest, UserRequest,
};
use getstream::rtc::JoinCallData;

/// Active-calls status is an application-level read with structural invariants
/// that do not depend on analytics timing.
#[tokio::test]
async fn active_calls_status_has_consistent_summary() {
    let Some(client) = common::client_or_skip() else {
        return;
    };

    let status = client
        .video()
        .get_active_calls_status()
        .await
        .expect("get_active_calls_status failed");

    if let Some(summary) = status.summary {
        assert!(
            summary.active_calls >= 0,
            "active_calls must be non-negative"
        );
        assert!(
            summary.participants >= 0,
            "participants must be non-negative"
        );
        assert!(
            summary.active_publishers >= 0 && summary.active_subscribers >= 0,
            "publisher/subscriber counts must be non-negative"
        );
    }
}

/// Create a call, join a real session, then query its per-session participant
/// stats. Identity fields must echo the call; analytics-not-ready is tolerated.
#[tokio::test]
async fn session_scoped_participant_stats_echo_call_identity() {
    let Some(client) = common::client_or_skip() else {
        return;
    };

    let user_id = common::unique_id("rust-it-stats-user");
    let call_id = common::unique_id("rust-it-stats-call");
    client
        .upsert_users([UserRequest::new(&user_id)])
        .await
        .expect("upsert_users failed");

    let call = client.video().call("default", &call_id);
    call.get_or_create(GetOrCreateCallRequest {
        data: Some(CallRequest {
            created_by_id: Some(user_id.clone()),
            ..Default::default()
        }),
        ..Default::default()
    })
    .await
    .expect("get_or_create failed");

    let outcome: Result<(), String> = async {
        call.join(JoinCallData::new(&user_id))
            .await
            .map_err(|error| format!("join failed: {error}"))?;
        let session_id = call
            .session_id()
            .await
            .ok_or_else(|| "joined call did not expose a session id".to_owned())?;
        call.leave()
            .await
            .map_err(|error| format!("leave failed: {error}"))?;
        call.end()
            .await
            .map_err(|error| format!("end failed: {error}"))?;

        if let Some(stats) = allow_stats_pending_skip(
            "query_call_session_participant_stats",
            call.query_call_session_participant_stats(
                &session_id,
                QueryCallSessionParticipantStatsRequest::default(),
            )
            .await,
        )? {
            assert_eq!(stats.call_id, call_id, "participant stats call_id mismatch");
            assert_eq!(
                stats.call_type, "default",
                "participant stats type mismatch"
            );
            assert_eq!(
                stats.call_session_id, session_id,
                "participant stats session mismatch"
            );
        }

        if let Some(sessions) = allow_stats_pending_skip(
            "query_call_participant_sessions",
            call.query_call_participant_sessions(
                &session_id,
                QueryCallParticipantSessionsRequest::default(),
            )
            .await,
        )? {
            assert_eq!(
                sessions.call_id, call_id,
                "participant sessions call_id mismatch"
            );
            assert_eq!(
                sessions.call_type, "default",
                "participant sessions type mismatch"
            );
            assert_eq!(
                sessions.call_session_id, session_id,
                "participant sessions session mismatch"
            );
        }

        Ok(())
    }
    .await;

    let leave_cleanup = call.leave().await;
    let delete_cleanup = call.delete(DeleteCallRequest { hard: Some(true) }).await;
    if let Err(error) = outcome {
        panic!("{error}; leave cleanup: {leave_cleanup:?}; delete cleanup: {delete_cleanup:?}");
    }
    let _ = leave_cleanup;
    delete_cleanup.expect("delete cleanup failed");
}

/// Treat a `404` as "stats not yet computed" (async analytics pipeline), passing
/// the test without asserting on values; surface any other error.
fn allow_stats_pending_skip<T>(
    endpoint: &str,
    result: Result<T, getstream::Error>,
) -> Result<Option<T>, String> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error)
            if error
                .as_api_error()
                .is_some_and(|api_error| api_error.status == 404) =>
        {
            eprintln!("SKIP stats pending: {endpoint}: {error}");
            Ok(None)
        }
        Err(error) => Err(format!("{endpoint} failed: {error}")),
    }
}
