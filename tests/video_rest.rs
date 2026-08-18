//! Live integration tests for the Phase 1 server REST surface.
//!
//! Run with credentials present (repo `.env`): `cargo test`. Without
//! credentials the tests print a SKIP line and pass without touching the API.

mod common;

use getstream::models::{
    CallRequest, CollectUserFeedbackRequest, DeleteCallRequest, GetCallReportRequest,
    GetCallRequest, GetCallStatsMapRequest, GetOrCreateCallRequest, GoLiveRequest, MemberRequest,
    QueryCallParticipantsRequest, QueryCallsRequest, RequestPermissionRequest, RingCallRequest,
    SendCallEventRequest, SendClosedCaptionRequest, SendVideoReactionRequest,
    StartClosedCaptionsRequest, StartFrameRecordingRequest, StopClosedCaptionsRequest,
    UpdateCallMembersRequest, UserRequest,
};
use getstream::rtc::{CallEvent, JoinCallData, LocalAudioTrack, LocalTrack, RtcError};
use std::time::Duration;

/// End-to-end call lifecycle: create → get → update members → query → end → delete.
#[tokio::test]
async fn call_crud_lifecycle() {
    let Some(client) = common::client_or_skip() else {
        return;
    };

    let owner_id = common::unique_id("rust-it-owner");
    let member_id = common::unique_id("rust-it-member");
    let call_id = common::unique_id("rust-it-call");

    // Upsert the users we reference so the API accepts them as call members.
    client
        .upsert_users([UserRequest::new(&owner_id), UserRequest::new(&member_id)])
        .await
        .expect("upsert_users failed");

    let call = client.video().call("default", &call_id);

    // create (get_or_create)
    let created = call
        .get_or_create(GetOrCreateCallRequest {
            data: Some(CallRequest {
                created_by_id: Some(owner_id.clone()),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("get_or_create failed");
    assert!(created.created, "expected a freshly created call");
    assert_eq!(created.call.id, call_id);
    assert_eq!(created.call.cid, format!("default:{call_id}"));

    // get
    let got = call
        .get(GetCallRequest::default())
        .await
        .expect("get failed");
    assert_eq!(got.call.cid, format!("default:{call_id}"));

    // update members
    let updated = call
        .update_call_members(UpdateCallMembersRequest {
            update_members: vec![MemberRequest::new(&member_id)],
            remove_members: vec![],
        })
        .await
        .expect("update_call_members failed");
    assert!(
        updated.members.iter().any(|m| m.user_id == member_id),
        "expected member {member_id} in members list"
    );

    // query_calls: our call should be findable by CID filter
    let queried = client
        .video()
        .query_calls(QueryCallsRequest {
            filter_conditions: serde_json::json!({ "cid": format!("default:{call_id}") })
                .as_object()
                .cloned()
                .unwrap()
                .into_iter()
                .collect(),
            limit: Some(1),
            ..Default::default()
        })
        .await
        .expect("query_calls failed");
    assert!(
        queried.calls.iter().any(|c| c.call.id == call_id),
        "expected queried calls to include {call_id}"
    );

    // end
    call.end().await.expect("end failed");

    // delete (hard) — also our cleanup so reruns stay idempotent
    call.delete(DeleteCallRequest { hard: Some(true) })
        .await
        .expect("delete failed");
}

#[tokio::test]
async fn scoped_call_endpoints_lifecycle() {
    let Some(client) = common::client_or_skip() else {
        return;
    };

    let owner_id = common::unique_id("rust-parity-owner");
    let member_id = common::unique_id("rust-parity-member");
    let call_id = common::unique_id("rust-parity-call");
    client
        .upsert_users([UserRequest::new(&owner_id), UserRequest::new(&member_id)])
        .await
        .expect("upsert_users failed");

    let call = client.video().call("default", &call_id);
    call.get_or_create(GetOrCreateCallRequest {
        data: Some(CallRequest {
            created_by_id: Some(owner_id),
            members: Some(vec![MemberRequest::new(&member_id)]),
            ..Default::default()
        }),
        ..Default::default()
    })
    .await
    .expect("get_or_create failed");

    let outcome: Result<(), String> = async {
        let mut filter_conditions = getstream::models::CustomData::new();
        filter_conditions.insert("user_id".to_owned(), serde_json::json!(member_id.clone()));
        call.query_participants(QueryCallParticipantsRequest {
            limit: Some(10),
            filter_conditions,
        })
        .await
        .map_err(|error| format!("query_participants failed: {error}"))?;

        call.ring(RingCallRequest {
            video: Some(false),
            members_ids: vec![member_id.clone()],
        })
        .await
        .map_err(|error| format!("ring failed: {error}"))?;

        call.list_recordings()
            .await
            .map_err(|error| format!("list_recordings failed: {error}"))?;
        call.list_transcriptions()
            .await
            .map_err(|error| format!("list_transcriptions failed: {error}"))?;

        Ok(())
    }
    .await;

    let cleanup = call.delete(DeleteCallRequest { hard: Some(true) }).await;
    if let Err(error) = outcome {
        panic!("{error}; cleanup result: {cleanup:?}");
    }
    cleanup.expect("delete cleanup failed");
}

#[tokio::test]
async fn audio_room_send_audio_permission_controls_publishing() {
    let Some(client) = common::client_or_skip() else {
        return;
    };

    let owner_id = common::unique_id("rust-permission-owner");
    let publisher_id = common::unique_id("rust-permission-publisher");
    let call_id = common::unique_id("rust-permission-call");
    client
        .upsert_users([UserRequest::new(&owner_id), UserRequest::new(&publisher_id)])
        .await
        .expect("upsert permission test users");

    let admin = client.video().call("audio_room", &call_id);
    admin
        .get_or_create(GetOrCreateCallRequest {
            data: Some(CallRequest {
                created_by_id: Some(owner_id),
                members: Some(vec![MemberRequest::new(&publisher_id)]),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("create permission test call");
    admin
        .go_live(GoLiveRequest::default())
        .await
        .expect("take permission test audio room live");
    let participant = client.video().call("audio_room", &call_id);
    let outcome: Result<(), String> = tokio::time::timeout(Duration::from_secs(120), async {
        let mut events = participant.subscribe();
        participant
            .join(JoinCallData::new(&publisher_id))
            .await
            .map_err(|error| format!("join permission test participant: {error}"))?;
        let denied_track = LocalAudioTrack::opus()
            .map_err(|error| format!("create denied audio track: {error}"))?;
        match participant.publish_audio(denied_track).await {
            Err(RtcError::PermissionDenied {
                capability: "send-audio",
            }) => {}
            Err(error) => {
                return Err(format!(
                    "publish with send-audio revoked returned the wrong error: {error}"
                ));
            }
            Ok(()) => {
                return Err("participant published audio while send-audio was revoked".to_owned());
            }
        }
        admin
            .grant_permissions(&publisher_id, vec!["send-audio".to_owned()])
            .await
            .map_err(|error| format!("grant send-audio: {error}"))?;
        wait_for_audio_permission(&mut events, true).await?;
        let allowed_track = LocalAudioTrack::opus()
            .map_err(|error| format!("create allowed audio track: {error}"))?;
        participant
            .publish_audio(allowed_track.clone())
            .await
            .map_err(|error| format!("publish after send-audio grant: {error}"))?;
        participant
            .stop_publish(LocalTrack::Audio(allowed_track))
            .await
            .map_err(|error| format!("stop granted audio publication: {error}"))?;
        participant
            .leave()
            .await
            .map_err(|error| format!("leave after allowed publish: {error}"))?;
        Ok(())
    })
    .await
    .map_err(|_| "permission behavior test timed out".to_owned())
    .and_then(|result| result);

    let leave_cleanup = participant.leave().await;
    let delete_cleanup = admin.delete(DeleteCallRequest { hard: Some(true) }).await;
    if let Err(error) = outcome {
        panic!("{error}; leave cleanup: {leave_cleanup:?}; delete cleanup: {delete_cleanup:?}");
    }
    leave_cleanup.expect("permission test leave cleanup");
    delete_cleanup.expect("permission test delete cleanup");
}

async fn wait_for_audio_permission(
    events: &mut tokio::sync::broadcast::Receiver<CallEvent>,
    expected: bool,
) -> Result<(), String> {
    let mut last_update = None;
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let event = events
                .recv()
                .await
                .map_err(|error| format!("permission event stream closed: {error}"))?;
            let CallEvent::Coordinator(event) = event else {
                continue;
            };
            if event.event_type != "call.permissions_updated" {
                continue;
            }
            last_update = Some(format!("{:?}", event.raw));
            let has_audio = event
                .raw
                .get("own_capabilities")
                .and_then(|value| value.as_array())
                .is_some_and(|capabilities| {
                    capabilities
                        .iter()
                        .any(|capability| capability.as_str() == Some("send-audio"))
                });
            if has_audio == expected {
                return Ok(());
            }
        }
    })
    .await;
    result.map_err(|_| {
        format!(
            "timed out waiting for send-audio={expected}; last permission update: {}",
            last_update.as_deref().unwrap_or("none")
        )
    })?
}

#[tokio::test]
async fn scoped_participant_service_lifecycle() {
    let Some(client) = common::client_or_skip() else {
        return;
    };

    let user_id = common::unique_id("rust-parity-participant");
    let call_id = common::unique_id("rust-parity-media");
    client
        .upsert_users([UserRequest::new(&user_id)])
        .await
        .expect("upsert_users failed");

    let call = client.video().call("default", &call_id);
    let server_call = client.video().call("default", &call_id);
    call.get_or_create(GetOrCreateCallRequest {
        data: Some(CallRequest {
            created_by_id: Some(user_id.clone()),
            ..Default::default()
        }),
        ..Default::default()
    })
    .await
    .expect("get_or_create failed");
    assert!(
        server_call.session_id().await.is_none(),
        "server REST handle unexpectedly has participant state"
    );

    let outcome: Result<(), String> = async {
        call.join(getstream::rtc::JoinCallData::new(&user_id))
            .await
            .map_err(|error| format!("join failed: {error}"))?;
        let session_id = call
            .session_id()
            .await
            .ok_or_else(|| "joined call did not expose a session id".to_owned())?;

        call.set_incoming_video_enabled(false)
            .await
            .map_err(|error| format!("disable incoming video failed: {error}"))?;
        call.set_incoming_video_enabled(true)
            .await
            .map_err(|error| format!("enable incoming video failed: {error}"))?;

        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let handler = call.on(move |event| {
            let _ = event_tx.send(event);
        });
        let mut custom = getstream::models::CustomData::new();
        custom.insert("source".to_owned(), serde_json::json!("rust-live-test"));
        server_call
            .send_custom_event(SendCallEventRequest {
                user_id: Some(user_id.clone()),
                custom,
                user: None,
            })
            .await
            .map_err(|error| format!("send_custom_event failed: {error}"))?;
        let custom_event = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = event_rx.recv().await {
                if let getstream::rtc::CallEvent::Coordinator(event) = event
                    && event.event_type == "custom"
                    && event
                        .raw
                        .pointer("/custom/source")
                        .and_then(serde_json::Value::as_str)
                        == Some("rust-live-test")
                {
                    return Ok(());
                }
            }
            Err("call event handler closed before receiving custom event".to_owned())
        })
        .await
        .map_err(|_| "timed out waiting for custom coordinator event".to_owned())
        .and_then(|result| result);
        custom_event?;

        let caption_result: Result<(), String> = async {
            let caption_text = format!("rust caption {session_id}");
            server_call
                .send_closed_caption(SendClosedCaptionRequest {
                    speaker_id: user_id.clone(),
                    text: caption_text.clone(),
                    start_time: Some(serde_json::json!("2026-08-17T18:00:00Z")),
                    end_time: Some(serde_json::json!("2026-08-17T18:00:01Z")),
                    language: Some("en".to_owned()),
                    service: Some("manual".to_owned()),
                    user_id: Some(user_id.clone()),
                    ..Default::default()
                })
                .await
                .map_err(|error| format!("send_closed_caption failed: {error}"))?;
            tokio::time::timeout(Duration::from_secs(10), async {
                while let Some(event) = event_rx.recv().await {
                    if let getstream::rtc::CallEvent::Coordinator(event) = event
                        && event.event_type == "call.closed_caption"
                        && event
                            .raw
                            .pointer("/closed_caption/text")
                            .and_then(serde_json::Value::as_str)
                            == Some(caption_text.as_str())
                    {
                        return Ok(());
                    }
                }
                Err("call event handler closed before receiving closed caption".to_owned())
            })
            .await
            .map_err(|_| "timed out waiting for closed caption event".to_owned())
            .and_then(|result| result)
        }
        .await;
        call.off(&handler);
        caption_result?;

        call.send_reaction(SendVideoReactionRequest {
            reaction_type: "raise-hand".to_owned(),
            emoji_code: Some("✋".to_owned()),
            custom: None,
        })
        .await
        .map_err(|error| format!("send_reaction failed: {error}"))?;

        allow_entitlement_skip(
            "request_permissions",
            call.request_permissions(RequestPermissionRequest {
                permissions: vec!["send-video".to_owned()],
            })
            .await,
        )?;

        let captions_started = allow_entitlement_skip(
            "start_closed_captions",
            call.start_closed_captions(StartClosedCaptionsRequest {
                language: Some("en".to_owned()),
                ..Default::default()
            })
            .await,
        )?
        .is_some();
        if captions_started {
            call.stop_closed_captions(StopClosedCaptionsRequest::default())
                .await
                .map_err(|error| format!("stop_closed_captions failed: {error}"))?;
        }
        let transcriptions = call
            .list_transcriptions()
            .await
            .map_err(|error| format!("list_transcriptions failed: {error}"))?;
        if let Some(transcription) = transcriptions.transcriptions.first() {
            call.delete_transcription(&transcription.session_id, &transcription.filename)
                .await
                .map_err(|error| format!("delete_transcription failed: {error}"))?;
        } else {
            eprintln!("SKIP artifact cleanup: delete_transcription had no generated artifact");
        }

        let frame_recording_started = allow_entitlement_skip(
            "start_frame_recording",
            call.start_frame_recording(StartFrameRecordingRequest::default())
                .await,
        )?
        .is_some();
        if frame_recording_started {
            call.stop_frame_recording()
                .await
                .map_err(|error| format!("stop_frame_recording failed: {error}"))?;
        }

        call.submit_feedback(CollectUserFeedbackRequest {
            rating: 5,
            sdk: "stream-rust".to_owned(),
            sdk_version: env!("CARGO_PKG_VERSION").to_owned(),
            user_session_id: Some(session_id.clone()),
            ..Default::default()
        })
        .await
        .map_err(|error| format!("submit_feedback failed: {error}"))?;

        call.end()
            .await
            .map_err(|error| format!("end failed: {error}"))?;
        call.leave()
            .await
            .map_err(|error| format!("leave failed: {error}"))?;

        allow_stats_pending_skip("get_call_stats", call.get_call_stats(&session_id).await)?;
        allow_stats_pending_skip(
            "get_call_report",
            call.get_call_report(GetCallReportRequest {
                session_id: Some(session_id.clone()),
            })
            .await,
        )?;
        allow_stats_pending_skip(
            "get_call_stats_map",
            call.get_call_stats_map(&session_id, GetCallStatsMapRequest::default())
                .await,
        )?;
        Ok(())
    }
    .await;

    let leave_cleanup = call.leave().await;
    let delete_cleanup = call.delete(DeleteCallRequest { hard: Some(true) }).await;
    if let Err(error) = outcome {
        panic!("{error}; leave cleanup: {leave_cleanup:?}; delete cleanup: {delete_cleanup:?}");
    }
    leave_cleanup.expect("leave cleanup failed");
    delete_cleanup.expect("delete cleanup failed");
}

fn allow_entitlement_skip<T>(
    endpoint: &str,
    result: Result<T, getstream::Error>,
) -> Result<Option<T>, String> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if is_entitlement_error(&error) => {
            eprintln!("SKIP entitlement: {endpoint}: {error}");
            Ok(None)
        }
        Err(error) => Err(format!("{endpoint} failed: {error}")),
    }
}

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

fn is_entitlement_error(error: &getstream::Error) -> bool {
    error.as_api_error().is_some_and(|api_error| {
        let message = api_error.message.to_ascii_lowercase();
        message.contains("not enabled")
            || message.contains("not configured")
            || message.contains("not available")
            || message.contains("disabled")
    })
}
