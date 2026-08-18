//! Live Phase 3 integration test: two SDK sessions join the same SFU call.
//!
//! Requires live credentials (repo `.env`). Without credentials it prints a SKIP
//! line and passes without touching the network. It never mocks the SFU: a
//! failure to connect is reported as a real error.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use getstream::TokenOptions;
use getstream::models::UserRequest;
use getstream::models::{CallRequest, DeleteCallRequest, GetOrCreateCallRequest, MemberRequest};
use getstream::rtc::{CallEvent, JoinCallData, RtcClient};
use tokio::sync::broadcast::Receiver;

/// Wait (up to `timeout`) for a `ParticipantJoined` whose `user_id` matches
/// `other`. Returns `true` if observed.
async fn observe_participant(
    mut rx: Receiver<CallEvent>,
    other: String,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => return false,
            event = rx.recv() => match event {
                Ok(CallEvent::ParticipantJoined(p)) if p.user_id == other => return true,
                Ok(_) => continue,
                // Lagged: keep waiting; the join event may still arrive.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return false,
            },
        }
    }
}

fn tamper_signature(token: &str) -> Result<String, String> {
    let (unsigned, signature) = token
        .rsplit_once('.')
        .filter(|(unsigned, signature)| unsigned.matches('.').count() == 1 && !signature.is_empty())
        .ok_or_else(|| "minted token did not contain three non-empty JWT segments".to_owned())?;
    let Some(first) = signature.as_bytes().first() else {
        return Err("minted token did not contain three non-empty JWT segments".to_owned());
    };
    let replacement = if *first == b'A' { 'B' } else { 'A' };
    Ok(format!("{unsigned}.{replacement}{}", &signature[1..]))
}

/// Two sessions join the same call; each observes the other; both leave cleanly.
/// A second `join()` on an already-joined handle returns a typed error.
#[tokio::test]
async fn two_sessions_join_and_observe_each_other() {
    let Some(client) = common::client_or_skip() else {
        return;
    };

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "getstream=debug".into()),
        )
        .with_test_writer()
        .try_init();

    let user_a = common::unique_id("rust-rtc-a");
    let user_b = common::unique_id("rust-rtc-b");
    let call_id = common::unique_id("rust-rtc-call");

    client
        .upsert_users([UserRequest::new(&user_a), UserRequest::new(&user_b)])
        .await
        .expect("upsert_users failed");

    // Pre-create the call with both users as members so either may join.
    let call_admin = client.video().call("default", &call_id);
    call_admin
        .get_or_create(GetOrCreateCallRequest {
            data: Some(CallRequest {
                created_by_id: Some(user_a.clone()),
                members: Some(vec![
                    MemberRequest::new(&user_a),
                    MemberRequest::new(&user_b),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("get_or_create failed");

    // The whole join/observe/leave dance must finish well within a minute.
    let outcome = tokio::time::timeout(Duration::from_secs(90), async {
        let call_a = client.video().call("default", &call_id);
        let call_b = client.video().call("default", &call_id);

        // Subscribe BEFORE joining so no participant event is missed.
        let rx_a = call_a.subscribe();
        let rx_b = call_b.subscribe();

        // Session A joins first.
        call_a
            .join(JoinCallData::new(&user_a))
            .await
            .expect("session A join failed");

        // A second join on the same handle is illegal.
        let double = call_a.join(JoinCallData::new(&user_a)).await;
        assert!(
            double.is_err(),
            "second join() on a joined handle must return a typed error"
        );

        // Session B joins the same call.
        call_b
            .join(JoinCallData::new(&user_b))
            .await
            .expect("session B join failed");

        // A should see B arrive as an event; B should see A via the initial roster.
        let saw_b = observe_participant(rx_a, user_b.clone(), Duration::from_secs(30));
        let saw_a = observe_participant(rx_b, user_a.clone(), Duration::from_secs(30));
        let (saw_b, saw_a) = tokio::join!(saw_b, saw_a);
        assert!(saw_b, "session A did not observe participant {user_b}");
        assert!(saw_a, "session B did not observe participant {user_a}");

        call_a.leave().await.expect("session A leave failed");
        call_b.leave().await.expect("session B leave failed");
    })
    .await;

    // Best-effort cleanup regardless of outcome.
    let _ = call_admin
        .delete(DeleteCallRequest { hard: Some(true) })
        .await;

    outcome.expect("two-session join test timed out (possible hang)");
}

#[tokio::test]
async fn provider_backed_client_loads_token_and_joins() {
    let Some(client) = common::client_or_skip() else {
        return;
    };
    let user_id = common::unique_id("rust-provider");
    let call_id = common::unique_id("rust-provider-call");
    client
        .upsert_users([UserRequest::new(&user_id)])
        .await
        .expect("upsert provider user");
    let admin = client.video().call("default", &call_id);
    admin
        .get_or_create(GetOrCreateCallRequest {
            data: Some(CallRequest {
                created_by_id: Some(user_id.clone()),
                members: Some(vec![MemberRequest::new(&user_id)]),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("create provider call");

    let loads = Arc::new(AtomicUsize::new(0));
    let provider_loads = loads.clone();
    let provider_client = client.clone();
    let provider_user = user_id.clone();
    let rtc = RtcClient::with_token_provider(client.api_key(), move || {
        provider_loads.fetch_add(1, Ordering::SeqCst);
        let client = provider_client.clone();
        let user_id = provider_user.clone();
        async move { client.create_token(&user_id) }
    })
    .expect("provider-backed RTC client");

    let outcome = tokio::time::timeout(Duration::from_secs(90), async {
        let call = rtc
            .join("default", &call_id, JoinCallData::new(&user_id))
            .await
            .expect("provider-backed join");
        call.leave().await.expect("provider-backed leave");
    })
    .await;
    let _ = admin.delete(DeleteCallRequest { hard: Some(true) }).await;
    outcome.expect("provider-backed join timed out");
    assert_eq!(loads.load(Ordering::SeqCst), 1);
}

/// Local decoding is not sufficient proof of authenticity: Stream must reject
/// a validly shaped participant token whose HS256 signature was altered.
#[tokio::test]
async fn participant_token_signature_is_enforced() {
    let Some(client) = common::client_or_skip() else {
        return;
    };
    let user_id = common::unique_id("rust-token-auth");
    let call_id = common::unique_id("rust-token-signature");
    client
        .upsert_users([UserRequest::new(&user_id)])
        .await
        .expect("upsert token authorization user");

    let admin = client.video().call("default", &call_id);
    admin
        .get_or_create(GetOrCreateCallRequest {
            data: Some(CallRequest {
                created_by_id: Some(user_id.clone()),
                members: Some(vec![MemberRequest::new(&user_id)]),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("create token signature call");

    let token = client
        .create_token_with(
            &user_id,
            TokenOptions {
                expiration: Some(Duration::from_secs(600)),
                call_cids: Some(vec![format!("default:{call_id}")]),
                ..Default::default()
            },
        )
        .expect("mint participant token");
    let tampered = tamper_signature(&token).expect("tamper JWT signature");

    let outcome: Result<(), String> = tokio::time::timeout(Duration::from_secs(120), async {
        let allowed = RtcClient::new(client.api_key(), token)
            .map_err(|error| format!("build RTC client: {error}"))?
            .join("default", &call_id, JoinCallData::new(&user_id))
            .await
            .map_err(|error| format!("valid token failed to join: {error}"))?;
        allowed
            .leave()
            .await
            .map_err(|error| format!("leave valid-token call: {error}"))?;

        if let Ok(unexpected) = RtcClient::new(client.api_key(), tampered)
            .map_err(|error| format!("build tampered-token RTC client: {error}"))?
            .join("default", &call_id, JoinCallData::new(&user_id))
            .await
        {
            let leave = unexpected.leave().await;
            return Err(format!(
                "Stream accepted a participant token with a tampered signature; \
                 leave result: {leave:?}"
            ));
        }
        Ok(())
    })
    .await
    .map_err(|_| "participant token signature test timed out".to_owned())
    .and_then(|result| result);

    let cleanup = admin.delete(DeleteCallRequest { hard: Some(true) }).await;
    if let Err(error) = outcome {
        panic!("{error}; cleanup: {cleanup:?}");
    }
    cleanup.expect("delete token signature test call");
}

/// A completed leave starts a new lifecycle generation, so the same call handle
/// can join successfully again.
#[tokio::test]
async fn join_leave_join_same_handle() {
    let Some(client) = common::client_or_skip() else {
        return;
    };
    let user_id = common::unique_id("rust-rtc-rejoin");
    let call_id = common::unique_id("rust-rtc-rejoin-call");
    client
        .upsert_users([UserRequest::new(&user_id)])
        .await
        .expect("upsert user");

    let call = client.video().call("default", &call_id);
    call.get_or_create(GetOrCreateCallRequest {
        data: Some(CallRequest {
            created_by_id: Some(user_id.clone()),
            ..Default::default()
        }),
        ..Default::default()
    })
    .await
    .expect("create call");

    let outcome = tokio::time::timeout(Duration::from_secs(90), async {
        call.join(JoinCallData::new(&user_id))
            .await
            .expect("first join");
        call.leave().await.expect("first leave");
        call.join(JoinCallData::new(&user_id))
            .await
            .expect("second join");
        call.leave().await.expect("second leave");
    })
    .await;

    let _ = call.delete(DeleteCallRequest { hard: Some(true) }).await;
    outcome.expect("join/leave/join test timed out");
}
