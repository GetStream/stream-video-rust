//! A handle to a single call and its REST operations.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Method;

use crate::client::Client;
use crate::error::{Error, Result};
use crate::models::*;

const CALL_BASE: &str = "/api/v2/video/call/{type}/{id}";
const INTERNAL_RTC_TOKEN_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// A handle to a specific call (`<type>:<id>`). Cheap to construct; no request is
/// made until a method is invoked. Obtain via [`crate::video::VideoClient::call`].
///
/// The same handle exposes the SFU participant path ([`Call::join`] /
/// [`Call::leave`]); clones share the join session.
#[derive(Clone)]
pub struct Call {
    client: Arc<Client>,
    call_type: String,
    call_id: String,
    rtc: Arc<crate::rtc::RtcCore>,
}

impl Call {
    pub(crate) fn new(client: Arc<Client>, call_type: String, call_id: String) -> Self {
        let rtc = crate::rtc::RtcCore::new(client.clone(), call_type.clone(), call_id.clone());
        Self {
            client,
            call_type,
            call_id,
            rtc,
        }
    }

    /// The call type (e.g. `default`, `livestream`).
    pub fn call_type(&self) -> &str {
        &self.call_type
    }

    /// The call ID.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// The call CID (`<type>:<id>`).
    pub fn cid(&self) -> String {
        format!("{}:{}", self.call_type, self.call_id)
    }

    fn path(&self, suffix: &str, extra: &[(&str, &str)]) -> String {
        let template = format!("{CALL_BASE}{suffix}");
        let mut params: Vec<(&str, &str)> = vec![("type", &self.call_type), ("id", &self.call_id)];
        params.extend_from_slice(extra);
        Client::build_path(&template, &params)
    }

    fn participant_auth(&self, operation: &str) -> Result<(String, Vec<(String, String)>)> {
        self.rtc.user_auth().ok_or_else(|| {
            Error::IllegalState(format!(
                "{operation} requires an active participant connection"
            ))
        })
    }

    // get / create / update / delete / end

    /// Get the call and its state (`GET .../{type}/{id}`).
    pub async fn get(&self, request: GetCallRequest) -> Result<GetCallResponse> {
        let mut query: Vec<(String, String)> = Vec::new();
        if let Some(v) = request.members_limit {
            query.push(("members_limit".into(), v.to_string()));
        }
        if let Some(v) = request.ring {
            query.push(("ring".into(), v.to_string()));
        }
        if let Some(v) = request.notify {
            query.push(("notify".into(), v.to_string()));
        }
        if let Some(v) = request.video {
            query.push(("video".into(), v.to_string()));
        }
        self.client
            .request::<(), _>(Method::GET, &self.path("", &[]), &query, None)
            .await
    }

    /// Get or create the call (`POST .../{type}/{id}`).
    pub async fn get_or_create(
        &self,
        request: GetOrCreateCallRequest,
    ) -> Result<GetOrCreateCallResponse> {
        self.client
            .request(Method::POST, &self.path("", &[]), &[], Some(&request))
            .await
    }

    /// Alias for [`Call::get_or_create`] (JS `create` semantics).
    pub async fn create(&self, request: GetOrCreateCallRequest) -> Result<GetOrCreateCallResponse> {
        self.get_or_create(request).await
    }

    /// Update the call (`PATCH .../{type}/{id}`).
    pub async fn update(&self, request: UpdateCallRequest) -> Result<UpdateCallResponse> {
        self.client
            .request(Method::PATCH, &self.path("", &[]), &[], Some(&request))
            .await
    }

    /// Delete the call (`POST .../{type}/{id}/delete`).
    pub async fn delete(&self, request: DeleteCallRequest) -> Result<DeleteCallResponse> {
        self.client
            .request(
                Method::POST,
                &self.path("/delete", &[]),
                &[],
                Some(&request),
            )
            .await
    }

    /// End the call (`POST .../{type}/{id}/mark_ended`).
    pub async fn end(&self) -> Result<EndCallResponse> {
        self.client
            .request::<(), _>(Method::POST, &self.path("/mark_ended", &[]), &[], None)
            .await
    }

    // members

    /// Add/update/remove members (`POST .../{type}/{id}/members`).
    pub async fn update_call_members(
        &self,
        request: UpdateCallMembersRequest,
    ) -> Result<UpdateCallMembersResponse> {
        self.client
            .request(
                Method::POST,
                &self.path("/members", &[]),
                &[],
                Some(&request),
            )
            .await
    }

    /// Query members (`POST /api/v2/video/call/members`). `id`/`type` are filled
    /// from this handle.
    pub async fn query_call_members(
        &self,
        mut request: QueryCallMembersRequest,
    ) -> Result<QueryCallMembersResponse> {
        request.id = self.call_id.clone();
        request.call_type = self.call_type.clone();
        self.client
            .request(
                Method::POST,
                "/api/v2/video/call/members",
                &[],
                Some(&request),
            )
            .await
    }

    /// Query participants currently connected to this call.
    pub async fn query_participants(
        &self,
        request: QueryCallParticipantsRequest,
    ) -> Result<QueryCallParticipantsResponse> {
        let query = request
            .limit
            .map(|limit| vec![("limit".to_owned(), limit.to_string())])
            .unwrap_or_default();
        self.client
            .request(
                Method::POST,
                &self.path("/participants", &[]),
                &query,
                Some(&request),
            )
            .await
    }

    // moderation

    /// Mute users (`POST .../{type}/{id}/mute_users`).
    pub async fn mute_users(&self, request: MuteUsersRequest) -> Result<MuteUsersResponse> {
        self.client
            .request(
                Method::POST,
                &self.path("/mute_users", &[]),
                &[],
                Some(&request),
            )
            .await
    }

    /// Kick a user (`POST .../{type}/{id}/kick`).
    pub async fn kick_user(&self, request: KickUserRequest) -> Result<KickUserResponse> {
        self.client
            .request(Method::POST, &self.path("/kick", &[]), &[], Some(&request))
            .await
    }

    /// Block a user (`POST .../{type}/{id}/block`).
    pub async fn block_user(&self, request: BlockUserRequest) -> Result<BlockUserResponse> {
        self.client
            .request(Method::POST, &self.path("/block", &[]), &[], Some(&request))
            .await
    }

    /// Unblock a user (`POST .../{type}/{id}/unblock`).
    pub async fn unblock_user(&self, request: UnblockUserRequest) -> Result<UnblockUserResponse> {
        self.client
            .request(
                Method::POST,
                &self.path("/unblock", &[]),
                &[],
                Some(&request),
            )
            .await
    }

    /// Ring call members who are not already connected.
    pub async fn ring(&self, request: RingCallRequest) -> Result<RingCallResponse> {
        self.client
            .request(Method::POST, &self.path("/ring", &[]), &[], Some(&request))
            .await
    }

    /// Send a custom event to every connected participant.
    pub async fn send_custom_event(
        &self,
        request: SendCallEventRequest,
    ) -> Result<SendCallEventResponse> {
        self.client
            .request(Method::POST, &self.path("/event", &[]), &[], Some(&request))
            .await
    }

    /// Send a reaction to every connected participant.
    pub async fn send_reaction(
        &self,
        request: SendVideoReactionRequest,
    ) -> Result<SendVideoReactionResponse> {
        let (token, query) = self.participant_auth("send_reaction")?;
        self.client
            .request_as_user(
                Method::POST,
                &self.path("/reaction", &[]),
                &query,
                Some(&request),
                &token,
            )
            .await
    }

    // live / recording / transcription

    /// Go live (`POST .../{type}/{id}/go_live`).
    pub async fn go_live(&self, request: GoLiveRequest) -> Result<GoLiveResponse> {
        self.client
            .request(
                Method::POST,
                &self.path("/go_live", &[]),
                &[],
                Some(&request),
            )
            .await
    }

    /// Stop being live (`POST .../{type}/{id}/stop_live`).
    pub async fn stop_live(&self, request: StopLiveRequest) -> Result<StopLiveResponse> {
        self.client
            .request(
                Method::POST,
                &self.path("/stop_live", &[]),
                &[],
                Some(&request),
            )
            .await
    }

    /// Start recording (`POST .../{type}/{id}/recordings/{recording_type}/start`).
    pub async fn start_recording(
        &self,
        recording_type: &str,
        request: StartRecordingRequest,
    ) -> Result<StartRecordingResponse> {
        let path = self.path(
            "/recordings/{recording_type}/start",
            &[("recording_type", recording_type)],
        );
        self.client
            .request(Method::POST, &path, &[], Some(&request))
            .await
    }

    /// Stop recording (`POST .../{type}/{id}/recordings/{recording_type}/stop`).
    pub async fn stop_recording(&self, recording_type: &str) -> Result<StopRecordingResponse> {
        let path = self.path(
            "/recordings/{recording_type}/stop",
            &[("recording_type", recording_type)],
        );
        self.client
            .request::<(), _>(Method::POST, &path, &[], None)
            .await
    }

    /// List completed recordings for the call.
    pub async fn list_recordings(&self) -> Result<ListRecordingsResponse> {
        self.client
            .request::<(), _>(Method::GET, &self.path("/recordings", &[]), &[], None)
            .await
    }

    /// Delete one completed recording artifact.
    pub async fn delete_recording(
        &self,
        session: &str,
        filename: &str,
    ) -> Result<DeleteRecordingResponse> {
        let path = self.path(
            "/{session}/recordings/{filename}",
            &[("session", session), ("filename", filename)],
        );
        self.client
            .request::<(), _>(Method::DELETE, &path, &[], None)
            .await
    }

    /// Start transcription (`POST .../{type}/{id}/start_transcription`).
    pub async fn start_transcription(
        &self,
        request: StartTranscriptionRequest,
    ) -> Result<StartTranscriptionResponse> {
        self.client
            .request(
                Method::POST,
                &self.path("/start_transcription", &[]),
                &[],
                Some(&request),
            )
            .await
    }

    /// Stop transcription (`POST .../{type}/{id}/stop_transcription`).
    pub async fn stop_transcription(&self) -> Result<StopTranscriptionResponse> {
        self.client
            .request::<(), _>(
                Method::POST,
                &self.path("/stop_transcription", &[]),
                &[],
                None,
            )
            .await
    }

    /// List completed transcriptions for the call.
    pub async fn list_transcriptions(&self) -> Result<ListTranscriptionsResponse> {
        self.client
            .request::<(), _>(Method::GET, &self.path("/transcriptions", &[]), &[], None)
            .await
    }

    /// Delete one transcription artifact.
    pub async fn delete_transcription(
        &self,
        session: &str,
        filename: &str,
    ) -> Result<DeleteTranscriptionResponse> {
        let path = self.path(
            "/{session}/transcriptions/{filename}",
            &[("session", session), ("filename", filename)],
        );
        self.client
            .request::<(), _>(Method::DELETE, &path, &[], None)
            .await
    }

    /// Start closed captions for the call.
    pub async fn start_closed_captions(
        &self,
        request: StartClosedCaptionsRequest,
    ) -> Result<StartClosedCaptionsResponse> {
        self.client
            .request(
                Method::POST,
                &self.path("/start_closed_captions", &[]),
                &[],
                Some(&request),
            )
            .await
    }

    /// Stop closed captions for the call.
    pub async fn stop_closed_captions(
        &self,
        request: StopClosedCaptionsRequest,
    ) -> Result<StopClosedCaptionsResponse> {
        self.client
            .request(
                Method::POST,
                &self.path("/stop_closed_captions", &[]),
                &[],
                Some(&request),
            )
            .await
    }

    /// Send an application-provided closed-caption segment.
    pub async fn send_closed_caption(
        &self,
        request: SendClosedCaptionRequest,
    ) -> Result<SendClosedCaptionResponse> {
        self.client
            .request(
                Method::POST,
                &self.path("/closed_captions", &[]),
                &[],
                Some(&request),
            )
            .await
    }

    /// Start frame-by-frame recording.
    pub async fn start_frame_recording(
        &self,
        request: StartFrameRecordingRequest,
    ) -> Result<StartFrameRecordingResponse> {
        self.client
            .request(
                Method::POST,
                &self.path("/start_frame_recording", &[]),
                &[],
                Some(&request),
            )
            .await
    }

    /// Stop frame-by-frame recording.
    pub async fn stop_frame_recording(&self) -> Result<StopFrameRecordingResponse> {
        self.client
            .request::<(), _>(
                Method::POST,
                &self.path("/stop_frame_recording", &[]),
                &[],
                None,
            )
            .await
    }

    // HLS / RTMP

    /// Start HLS broadcasting (`POST .../{type}/{id}/start_broadcasting`).
    pub async fn start_hls_broadcasting(&self) -> Result<StartHLSBroadcastingResponse> {
        self.client
            .request::<(), _>(
                Method::POST,
                &self.path("/start_broadcasting", &[]),
                &[],
                None,
            )
            .await
    }

    /// Stop HLS broadcasting (`POST .../{type}/{id}/stop_broadcasting`).
    pub async fn stop_hls_broadcasting(&self) -> Result<StopHLSBroadcastingResponse> {
        self.client
            .request::<(), _>(
                Method::POST,
                &self.path("/stop_broadcasting", &[]),
                &[],
                None,
            )
            .await
    }

    /// Start RTMP broadcasts (`POST .../{type}/{id}/rtmp_broadcasts`).
    pub async fn start_rtmp_broadcasts(
        &self,
        request: StartRtmpBroadcastsRequest,
    ) -> Result<StartRtmpBroadcastsResponse> {
        self.client
            .request(
                Method::POST,
                &self.path("/rtmp_broadcasts", &[]),
                &[],
                Some(&request),
            )
            .await
    }

    /// Stop all RTMP broadcasts (`POST .../{type}/{id}/rtmp_broadcasts/stop`).
    pub async fn stop_all_rtmp_broadcasts(&self) -> Result<StopAllRtmpBroadcastsResponse> {
        self.client
            .request::<(), _>(
                Method::POST,
                &self.path("/rtmp_broadcasts/stop", &[]),
                &[],
                None,
            )
            .await
    }

    /// Stop a named RTMP broadcast (`POST .../rtmp_broadcasts/{name}/stop`).
    pub async fn stop_rtmp_broadcast(&self, name: &str) -> Result<StopRtmpBroadcastsResponse> {
        let path = self.path("/rtmp_broadcasts/{name}/stop", &[("name", name)]);
        self.client
            .request::<(), _>(Method::POST, &path, &[], None)
            .await
    }

    // pin / permissions

    /// Pin a track for everyone (`POST .../{type}/{id}/pin`).
    pub async fn pin_for_everyone(&self, request: PinRequest) -> Result<PinResponse> {
        self.client
            .request(Method::POST, &self.path("/pin", &[]), &[], Some(&request))
            .await
    }

    /// Unpin a track for everyone (`POST .../{type}/{id}/unpin`).
    pub async fn unpin_for_everyone(&self, request: UnpinRequest) -> Result<UnpinResponse> {
        self.client
            .request(Method::POST, &self.path("/unpin", &[]), &[], Some(&request))
            .await
    }

    /// Grant/revoke user permissions (`POST .../{type}/{id}/user_permissions`).
    pub async fn update_user_permissions(
        &self,
        request: UpdateUserPermissionsRequest,
    ) -> Result<UpdateUserPermissionsResponse> {
        self.client
            .request(
                Method::POST,
                &self.path("/user_permissions", &[]),
                &[],
                Some(&request),
            )
            .await
    }

    /// Request permissions as a participant (`POST .../{type}/{id}/request_permission`).
    pub async fn request_permissions(
        &self,
        request: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse> {
        let (token, query) = self.participant_auth("request_permissions")?;
        self.client
            .request_as_user(
                Method::POST,
                &self.path("/request_permission", &[]),
                &query,
                Some(&request),
                &token,
            )
            .await
    }

    /// Grant call-scoped permissions to a user.
    pub async fn grant_permissions(
        &self,
        user_id: impl Into<String>,
        permissions: Vec<String>,
    ) -> Result<UpdateUserPermissionsResponse> {
        self.update_user_permissions(UpdateUserPermissionsRequest {
            user_id: user_id.into(),
            grant_permissions: permissions,
            revoke_permissions: Vec::new(),
        })
        .await
    }

    /// Revoke call-scoped permissions from a user.
    pub async fn revoke_permissions(
        &self,
        user_id: impl Into<String>,
        permissions: Vec<String>,
    ) -> Result<UpdateUserPermissionsResponse> {
        self.update_user_permissions(UpdateUserPermissionsRequest {
            user_id: user_id.into(),
            grant_permissions: Vec::new(),
            revoke_permissions: permissions,
        })
        .await
    }

    /// Submit user feedback for a call session.
    pub async fn submit_feedback(
        &self,
        request: CollectUserFeedbackRequest,
    ) -> Result<CollectUserFeedbackResponse> {
        let (token, query) = self.participant_auth("submit_feedback")?;
        self.client
            .request_as_user(
                Method::POST,
                &self.path("/feedback", &[]),
                &query,
                Some(&request),
                &token,
            )
            .await
    }

    /// Retrieve historical statistics for one call session.
    pub async fn get_call_stats(&self, session_id: &str) -> Result<GetCallStatsResponse> {
        let path = self.path("/stats/{session_id}", &[("session_id", session_id)]);
        self.client
            .request::<(), _>(Method::GET, &path, &[], None)
            .await
    }

    /// Retrieve the report for the latest or selected call session.
    pub async fn get_call_report(
        &self,
        request: GetCallReportRequest,
    ) -> Result<GetCallReportResponse> {
        let query = request
            .session_id
            .map(|session_id| vec![("session_id".to_owned(), session_id)])
            .unwrap_or_default();
        self.client
            .request::<(), _>(Method::GET, &self.path("/report", &[]), &query, None)
            .await
    }

    /// Retrieve map-oriented statistics for one call session.
    pub async fn get_call_stats_map(
        &self,
        session_id: &str,
        request: GetCallStatsMapRequest,
    ) -> Result<QueryCallStatsMapResponse> {
        let mut query = Vec::new();
        if let Some(value) = request.start_time.as_ref() {
            query.push(("start_time".to_owned(), timestamp_query(value)));
        }
        if let Some(value) = request.end_time.as_ref() {
            query.push(("end_time".to_owned(), timestamp_query(value)));
        }
        if let Some(value) = request.exclude_publishers {
            query.push(("exclude_publishers".to_owned(), value.to_string()));
        }
        if let Some(value) = request.exclude_subscribers {
            query.push(("exclude_subscribers".to_owned(), value.to_string()));
        }
        if let Some(value) = request.exclude_sfus {
            query.push(("exclude_sfus".to_owned(), value.to_string()));
        }
        let path = Client::build_path(
            "/api/v2/video/call_stats/{type}/{id}/{session_id}/map",
            &[
                ("type", &self.call_type),
                ("id", &self.call_id),
                ("session_id", session_id),
            ],
        );
        self.client
            .request::<(), _>(Method::GET, &path, &query, None)
            .await
    }

    // participant path (SFU WebRTC)

    /// Set the maximum reconnect duration. Zero keeps reconnecting indefinitely.
    pub fn set_disconnection_timeout(&self, timeout: Duration) {
        self.rtc.set_disconnection_timeout(timeout);
    }

    /// Update publishing preferences for the next join generation.
    ///
    /// Call this before [`Call::join`]. Updates after joining starts emit a
    /// warning and cannot affect the active join generation, but remain
    /// available after [`Call::leave`] for a later join on this handle.
    pub fn update_publish_options(&self, options: crate::rtc::ClientPublishOptions) {
        self.rtc.update_publish_options(options);
    }

    /// Join the call as an SFU participant.
    ///
    /// Mints a finite, call-CID-scoped user token internally from the server secret, runs the
    /// coordinator join, establishes the publisher/subscriber PeerConnections,
    /// and completes the SFU handshake. Illegal (typed error) if already
    /// `JOINING`/`JOINED`. Observe participants via [`Call::subscribe`].
    pub async fn join(&self, data: crate::rtc::JoinCallData) -> crate::rtc::RtcResult<()> {
        let source = crate::rtc::client::UserTokenSource::ServerMinted {
            client: self.client.clone(),
            user_id: data.user_id.clone(),
            call_cid: self.cid(),
            expiration: INTERNAL_RTC_TOKEN_LIFETIME,
        };
        self.rtc.join_with_token_source(source, data).await
    }

    /// Leave the call, closing the SFU connection and PeerConnections. Succeeds
    /// from any state, including `JOINING`.
    pub async fn leave(&self) -> crate::rtc::RtcResult<()> {
        self.rtc.leave("user requested leave").await
    }

    /// Subscribe to the typed SFU event stream (participant joined/left, tracks,
    /// errors). Subscribe before or after [`Call::join`].
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<crate::rtc::CallEvent> {
        self.rtc.subscribe()
    }

    /// Register a callback for typed call events.
    pub fn on<F>(&self, callback: F) -> tokio::task::AbortHandle
    where
        F: Fn(crate::rtc::CallEvent) + Send + 'static,
    {
        self.rtc.on(callback)
    }

    /// Remove a callback registered with [`Call::on`].
    pub fn off(&self, handler: &tokio::task::AbortHandle) {
        self.rtc.off(handler);
    }

    /// The current calling state (`Idle` / `Joining` / `Joined` / …).
    pub fn calling_state(&self) -> crate::rtc::CallingState {
        self.rtc.state()
    }

    /// The live SFU session id once joined.
    pub async fn session_id(&self) -> Option<String> {
        self.rtc.session_id().await
    }

    /// A snapshot of the participants currently in the call, including this
    /// session. Populated at join from the SFU call state and kept current as
    /// participants join and leave.
    pub fn participants(&self) -> Vec<crate::rtc::RemoteParticipant> {
        self.rtc.participants()
    }

    /// Return the latest participant, count, pin, grant, and session snapshot.
    pub fn call_state(&self) -> crate::rtc::CallStateSnapshot {
        self.rtc.call_state()
    }

    // publish / subscribe (SFU WebRTC)

    /// Publish a local audio track (Opus). The track keeps producing media (PCM
    /// pacing / samples) until [`Call::stop_publish`] or [`Call::leave`].
    ///
    /// Accepts a [`LocalAudioTrack`](crate::rtc::LocalAudioTrack) only; a
    /// [`RemoteTrack`](crate::rtc::RemoteTrack) will not compile here.
    ///
    /// ```compile_fail
    /// # async fn cannot_republish_remote_track(
    /// #     call: getstream::Call,
    /// #     remote: getstream::rtc::RemoteTrack,
    /// # ) {
    /// call.publish_audio(remote).await;
    /// # }
    /// ```
    pub async fn publish_audio(
        &self,
        track: crate::rtc::LocalAudioTrack,
    ) -> crate::rtc::RtcResult<()> {
        self.rtc.publish(crate::rtc::LocalTrack::Audio(track)).await
    }

    /// Publish a local video track (VP8).
    pub async fn publish_video(
        &self,
        track: crate::rtc::LocalVideoTrack,
    ) -> crate::rtc::RtcResult<()> {
        self.rtc
            .publish(crate::rtc::LocalTrack::Video {
                track,
                track_type: crate::rtc::proto::models::TrackType::Video,
            })
            .await
    }

    /// Publish a local track as screen-share (video codec, screen-share type).
    pub async fn publish_screen_share(
        &self,
        track: crate::rtc::LocalVideoTrack,
    ) -> crate::rtc::RtcResult<()> {
        self.rtc
            .publish(crate::rtc::LocalTrack::Video {
                track,
                track_type: crate::rtc::proto::models::TrackType::ScreenShare,
            })
            .await
    }

    /// Publish an Opus audio track associated with the active screen share.
    pub async fn publish_screen_share_audio(
        &self,
        track: crate::rtc::LocalAudioTrack,
    ) -> crate::rtc::RtcResult<()> {
        self.rtc
            .publish(crate::rtc::LocalTrack::ScreenShareAudio(track))
            .await
    }

    /// Temporarily mute a published track kind without destroying its sender.
    pub async fn mute_track(
        &self,
        track_type: crate::rtc::proto::models::TrackType,
    ) -> crate::rtc::RtcResult<()> {
        self.rtc.set_track_muted(track_type, true).await
    }

    /// Resume a temporarily muted published track kind.
    pub async fn unmute_track(
        &self,
        track_type: crate::rtc::proto::models::TrackType,
    ) -> crate::rtc::RtcResult<()> {
        self.rtc.set_track_muted(track_type, false).await
    }

    /// Enable SFU-side noise cancellation for the local participant.
    pub async fn start_noise_cancellation(&self) -> crate::rtc::RtcResult<()> {
        self.rtc.start_noise_cancellation().await
    }

    /// Disable SFU-side noise cancellation for the local participant.
    pub async fn stop_noise_cancellation(&self) -> crate::rtc::RtcResult<()> {
        self.rtc.stop_noise_cancellation().await
    }

    /// Stop publishing a previously published track. The publisher keeps its
    /// transceiver in the negotiated envelope and signals the stop to the SFU
    /// via `UpdateMuteStates` (no publisher renegotiation), matching
    /// `stream-video-js`.
    pub async fn stop_publish(&self, track: crate::rtc::LocalTrack) -> crate::rtc::RtcResult<()> {
        self.rtc.stop_publish(track).await
    }

    /// Set the subscription policy and (re)send `UpdateSubscriptions`. The SFU
    /// forwards no media until this is called; the default policy is audio-only.
    pub async fn update_subscriptions(
        &self,
        config: crate::rtc::SubscriptionConfig,
    ) -> crate::rtc::RtcResult<()> {
        self.rtc.update_subscriptions(config).await
    }

    /// Subscribe to an exact set of participant-session tracks.
    pub async fn update_subscription_targets(
        &self,
        targets: Vec<crate::rtc::SubscriptionTarget>,
    ) -> crate::rtc::RtcResult<()> {
        self.rtc.update_subscription_targets(targets).await
    }

    /// Enable or disable incoming video from every remote participant.
    pub async fn set_incoming_video_enabled(&self, enabled: bool) -> crate::rtc::RtcResult<()> {
        self.rtc.set_incoming_video_enabled(enabled).await
    }

    /// Register a callback invoked with each inbound
    /// [`RemoteTrack`](crate::rtc::RemoteTrack) once a subscription delivers it.
    pub fn on_track<F>(&self, cb: F)
    where
        F: Fn(crate::rtc::RemoteTrack) + Send + Sync + 'static,
    {
        self.rtc.on_track(cb);
    }

    /// Test-only handle to the RTC core (used by live signaling probes).
    #[cfg(test)]
    pub(crate) fn rtc_core(&self) -> &std::sync::Arc<crate::rtc::RtcCore> {
        &self.rtc
    }
}

fn timestamp_query(value: &Timestamp) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}
