//! Live integration tests for the advanced call-stats REST surface.
//!
//! Run with credentials present (repo `.env`): `cargo test`. Without credentials
//! the tests print a SKIP line and pass without touching the API. The call
//! created for the session-scoped queries is deleted on every exit path.
//!
//! The per-session queries are keyed by the *coordinator* call session id
//! (`get().call.session.id`), not by `Call::session_id()` -- the latter is this
//! participant's SFU session, which these endpoints report as `user_session_id`
//! nested inside the payload. Passing the wrong one returns `404 call session
//! not found`. Stats can lag the call by a few seconds, so the queries retry on
//! `404` for a bounded window and then fail rather than skipping.

mod common;

use std::future::Future;
use std::time::Duration;

use getstream::models::{
    CallRequest, CustomData, DeleteCallRequest, GetOrCreateCallRequest,
    QueryCallParticipantSessionsRequest, QueryCallSessionParticipantStatsRequest, UserRequest,
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
/// stats. Identity fields must echo the call, and the participant session must
/// carry this join's SFU session id.
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

        // `session_id()` is this participant's SFU session; the stats endpoints
        // are keyed by the coordinator's call session, which is a different id.
        let user_session_id = call
            .session_id()
            .await
            .ok_or_else(|| "joined call did not expose an SFU session id".to_owned())?;
        let session_id = call
            .get(Default::default())
            .await
            .map_err(|error| format!("get failed: {error}"))?
            .call
            .session
            .map(|session| session.id)
            .ok_or_else(|| "joined call did not expose a call session".to_owned())?;

        call.leave()
            .await
            .map_err(|error| format!("leave failed: {error}"))?;
        call.end()
            .await
            .map_err(|error| format!("end failed: {error}"))?;

        // Independent reads of the same ended session: no need to serialise
        // their retry windows.
        let (stats, sessions) = tokio::try_join!(
            await_stats("query_call_session_participant_stats", || {
                call.query_call_session_participant_stats(
                    &session_id,
                    QueryCallSessionParticipantStatsRequest {
                        // Populated so the query encoding is validated against
                        // the server, not just against its own unit test.
                        limit: Some(5),
                        filter_conditions: participant_filter(&user_id),
                        ..Default::default()
                    },
                )
            }),
            await_stats("query_call_participant_sessions", || {
                call.query_call_participant_sessions(
                    &session_id,
                    QueryCallParticipantSessionsRequest::default(),
                )
            }),
        )?;
        assert_eq!(stats.call_id, call_id, "participant stats call_id mismatch");
        assert_eq!(
            stats.call_type, "default",
            "participant stats type mismatch"
        );
        assert_eq!(
            stats.call_session_id, session_id,
            "participant stats session mismatch"
        );

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

        // The join above is the only participant session, and it must be
        // reported under the SFU session id -- guarding the two ids from being
        // conflated again.
        let reported: Vec<&str> = sessions
            .participants_sessions
            .iter()
            .filter_map(|entry| entry.get("user_session_id")?.as_str())
            .collect();
        assert!(
            reported.contains(&user_session_id.as_str()),
            "participant sessions {reported:?} missing this join's SFU session {user_session_id}"
        );

        Ok(())
    }
    .await;

    let leave_cleanup = call.leave().await;
    let delete_cleanup = call.delete(DeleteCallRequest { hard: Some(true) }).await;
    if let Err(error) = outcome {
        panic!("{error}; leave cleanup: {leave_cleanup:?}; delete cleanup: {delete_cleanup:?}");
    }
    delete_cleanup.expect("delete cleanup failed");
}

/// Restrict a participant-stats query to one user, exercising the
/// `filter_conditions` query encoding against the server rather than only
/// against the encoder's own unit test.
fn participant_filter(user_id: &str) -> CustomData {
    CustomData::from([("user_id".to_owned(), serde_json::Value::from(user_id))])
}

/// Analytics can trail the call by a few seconds, so retry a `404` for a bounded
/// window. Unlike an unconditional skip, a persistent `404` still fails the test.
async fn await_stats<T, F, Fut>(endpoint: &str, mut query: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, getstream::Error>>,
{
    const DEADLINE: Duration = Duration::from_secs(30);
    const INTERVAL: Duration = Duration::from_secs(3);

    let mut waited = Duration::ZERO;
    loop {
        match query().await {
            Ok(value) => return Ok(value),
            Err(error)
                if waited < DEADLINE
                    && error
                        .as_api_error()
                        .is_some_and(|api_error| api_error.status == 404) =>
            {
                tokio::time::sleep(INTERVAL).await;
                waited += INTERVAL;
            }
            Err(error) => {
                return Err(format!("{endpoint} failed after {waited:?}: {error}"));
            }
        }
    }
}
