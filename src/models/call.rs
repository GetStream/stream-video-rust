use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::shared::{CustomData, SortParamRequest, Timestamp};
use super::user::{UserRequest, UserResponse};

// Members

/// A call member to add/update (`MemberRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct MemberRequest {
    pub user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<CustomData>,
}

impl MemberRequest {
    /// Construct a member request from a user ID.
    pub fn new(user_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
            ..Default::default()
        }
    }
}

/// A call member as returned by the API (`MemberResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MemberResponse {
    pub user_id: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub custom: CustomData,
    pub user: UserResponse,
    pub role: Option<String>,
    pub deleted_at: Option<Timestamp>,
}

// Call core

/// Call creation/update payload (`CallRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct CallRequest {
    /// Server-side creator user ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<UserRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starts_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<MemberRequest>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<CustomData>,
    /// Per-call settings override (opaque; see Stream docs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings_override: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_cid: Option<String>,
}

/// A call as returned by the API (`CallResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CallResponse {
    /// `<type>:<id>` identifier.
    pub cid: String,
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub backstage: bool,
    pub recording: bool,
    pub transcribing: bool,
    pub translating: bool,
    pub captioning: bool,
    pub current_session_id: String,
    pub blocked_user_ids: Vec<String>,
    pub created_by: UserResponse,
    pub custom: CustomData,
    pub team: Option<String>,
    pub starts_at: Option<Timestamp>,
    pub ended_at: Option<Timestamp>,
    pub channel_cid: Option<String>,
    pub join_ahead_time_seconds: Option<i32>,
    pub routing_number: Option<String>,
    /// Effective call settings, with common agent-facing settings typed.
    pub settings: Option<CallSettingsResponse>,
    /// Current call-session state.
    pub session: Option<CallSessionResponse>,
    pub egress: Option<Value>,
    pub ingress: Option<Value>,
    pub thumbnails: Option<Value>,
}

/// Current coordinator-side call session state.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CallSessionResponse {
    pub anonymous_participant_count: i32,
    pub id: String,
    pub participants: Vec<CallParticipantResponse>,
    pub accepted_by: HashMap<String, Timestamp>,
    pub missed_by: HashMap<String, Timestamp>,
    pub rejected_by: HashMap<String, Timestamp>,
    pub participants_count_by_role: HashMap<String, i32>,
    pub started_at: Option<Timestamp>,
    pub ended_at: Option<Timestamp>,
    pub live_started_at: Option<Timestamp>,
    pub live_ended_at: Option<Timestamp>,
    pub timer_ends_at: Option<Timestamp>,
}

/// Target video dimensions and rate advertised by call settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TargetResolution {
    pub width: i32,
    pub height: i32,
    pub bitrate: Option<i32>,
}

/// Effective call audio settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AudioSettingsResponse {
    pub access_request_enabled: bool,
    pub default_device: String,
    pub hifi_audio_enabled: bool,
    pub mic_default_on: bool,
    pub opus_dtx_enabled: bool,
    pub redundant_coding_enabled: bool,
    pub speaker_default_on: bool,
    pub noise_cancellation: Option<Value>,
}

/// Effective call video settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VideoSettingsResponse {
    pub access_request_enabled: bool,
    pub camera_default_on: bool,
    pub camera_facing: String,
    pub enabled: bool,
    pub target_resolution: TargetResolution,
}

/// Effective call screen-sharing settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ScreensharingSettingsResponse {
    pub access_request_enabled: bool,
    pub enabled: bool,
    pub target_resolution: Option<TargetResolution>,
}

/// Effective call settings returned by the coordinator.
///
/// Audio, camera, screen share, session, and limits are first-class; newer
/// server setting groups remain accessible through [`Self::additional_fields`]
/// without making deserialization brittle across API additions.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CallSettingsResponse {
    pub audio: AudioSettingsResponse,
    pub video: VideoSettingsResponse,
    pub screensharing: ScreensharingSettingsResponse,
    pub session: SessionSettingsResponse,
    pub limits: LimitsSettingsResponse,
    #[serde(flatten)]
    pub additional_fields: CustomData,
}

/// Effective session timeout settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionSettingsResponse {
    pub inactivity_timeout_seconds: i32,
}

/// Effective participant and duration limits.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LimitsSettingsResponse {
    pub max_duration_seconds: Option<i32>,
    pub max_participants: Option<i32>,
    pub max_participants_exclude_owner: Option<bool>,
    pub max_participants_exclude_roles: Vec<String>,
}

// get / create / update / delete / end

/// `get_or_create` request (`GetOrCreateCallRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct GetOrCreateCallRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<CallRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members_limit: Option<i32>,
    /// Send a notification event to members.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify: Option<bool>,
    /// Send a ring event to members.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ring: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<bool>,
}

/// `get_or_create` response (`GetOrCreateCallResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GetOrCreateCallResponse {
    pub duration: String,
    pub created: bool,
    pub call: CallResponse,
    pub members: Vec<MemberResponse>,
    pub own_capabilities: Vec<String>,
}

/// `get` response (`GetCallResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GetCallResponse {
    pub duration: String,
    pub call: CallResponse,
    pub members: Vec<MemberResponse>,
    pub own_capabilities: Vec<String>,
}

/// Query params for `get` (`GetCallRequest`).
#[derive(Debug, Clone, Default)]
pub struct GetCallRequest {
    pub members_limit: Option<i32>,
    pub ring: Option<bool>,
    pub notify: Option<bool>,
    pub video: Option<bool>,
}

/// `update` request (`UpdateCallRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateCallRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starts_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<CustomData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings_override: Option<Value>,
}

/// `update` response (`UpdateCallResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UpdateCallResponse {
    pub duration: String,
    pub call: CallResponse,
    pub members: Vec<MemberResponse>,
    pub own_capabilities: Vec<String>,
}

/// `delete` request (`DeleteCallRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeleteCallRequest {
    /// Hard-delete the call and all related data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hard: Option<bool>,
}

/// `delete` response (`DeleteCallResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DeleteCallResponse {
    pub duration: String,
    pub call: CallResponse,
    pub task_id: Option<String>,
}

/// `end` response (`EndCallResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct EndCallResponse {
    pub duration: String,
}

// Members update / query

/// `update_call_members` request (`UpdateCallMembersRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateCallMembersRequest {
    /// Members to upsert.
    pub update_members: Vec<MemberRequest>,
    /// User IDs to remove.
    pub remove_members: Vec<String>,
}

/// `update_call_members` response (`UpdateCallMembersResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UpdateCallMembersResponse {
    pub duration: String,
    pub members: Vec<MemberResponse>,
}

/// `query_call_members` request (`QueryCallMembersRequest`). `id`/`type` are set
/// by the SDK from the call handle.
#[derive(Debug, Clone, Default, Serialize)]
pub struct QueryCallMembersRequest {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<SortParamRequest>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub filter_conditions: CustomData,
}

/// `query_call_members` response (`QueryCallMembersResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QueryCallMembersResponse {
    pub duration: String,
    pub members: Vec<MemberResponse>,
    pub next: Option<String>,
    pub prev: Option<String>,
}

/// `query_participants` request. `limit` is encoded as a query parameter.
#[derive(Debug, Clone, Default, Serialize)]
pub struct QueryCallParticipantsRequest {
    #[serde(skip)]
    pub limit: Option<i32>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub filter_conditions: CustomData,
}

/// A participant currently connected to a call.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CallParticipantResponse {
    pub joined_at: Timestamp,
    pub role: String,
    pub user_session_id: String,
    pub user: UserResponse,
}

/// `query_participants` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QueryCallParticipantsResponse {
    pub duration: String,
    pub total_participants: i32,
    pub members: Vec<MemberResponse>,
    pub membership: Option<MemberResponse>,
    pub own_capabilities: Vec<String>,
    pub participants: Vec<CallParticipantResponse>,
    pub call: CallResponse,
}

// Moderation: mute / kick / block / unblock

/// `mute_users` request (`MuteUsersRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct MuteUsersRequest {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub user_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshare: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshare_audio: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mute_all_users: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted_by_id: Option<String>,
}

/// `mute_users` response (`MuteUsersResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MuteUsersResponse {
    pub duration: String,
}

/// `kick_user` request (`KickUserRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct KickUserRequest {
    pub user_id: String,
    /// Also block the user from rejoining.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kicked_by_id: Option<String>,
}

/// `kick_user` response (`KickUserResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct KickUserResponse {
    pub duration: String,
}

/// `block_user` request (`BlockUserRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct BlockUserRequest {
    pub user_id: String,
}

/// `block_user` response (`BlockUserResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BlockUserResponse {
    pub duration: String,
}

/// `unblock_user` request (`UnblockUserRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct UnblockUserRequest {
    pub user_id: String,
}

/// `unblock_user` response (`UnblockUserResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UnblockUserResponse {
    pub duration: String,
}

/// `ring` request.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RingCallRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<bool>,
    pub members_ids: Vec<String>,
}

/// `ring` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RingCallResponse {
    pub duration: String,
    pub members_ids: Vec<String>,
}

/// `send_custom_event` request.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SendCallEventRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub custom: CustomData,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<UserRequest>,
}

/// `send_custom_event` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SendCallEventResponse {
    pub duration: String,
}

/// `send_reaction` request.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SendVideoReactionRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<CustomData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji_code: Option<String>,
    #[serde(rename = "type")]
    pub reaction_type: String,
}

/// A reaction returned by the Video API.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VideoReactionResponse {
    pub custom: Option<CustomData>,
    pub emoji_code: Option<String>,
    #[serde(rename = "type")]
    pub reaction_type: String,
    pub user: UserResponse,
}

/// `send_reaction` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SendVideoReactionResponse {
    pub duration: String,
    pub reaction: VideoReactionResponse,
}

// Live / recording / transcription

/// `go_live` request (`GoLiveRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct GoLiveRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_hls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_recording: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_transcription: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_closed_caption: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_storage_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcription_storage_name: Option<String>,
}

/// `go_live` response (`GoLiveResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GoLiveResponse {
    pub duration: String,
    pub call: CallResponse,
}

/// `stop_live` request (`StopLiveRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct StopLiveRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continue_hls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continue_recording: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continue_transcription: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continue_rtmp_broadcasts: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continue_closed_caption: Option<bool>,
}

/// `stop_live` response (`StopLiveResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StopLiveResponse {
    pub duration: String,
    pub call: CallResponse,
}

/// `start_recording` request (`StartRecordingRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct StartRecordingRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_external_storage: Option<String>,
}

/// `start_recording` response (`StartRecordingResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StartRecordingResponse {
    pub duration: String,
}

/// `stop_recording` response (`StopRecordingResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StopRecordingResponse {
    pub duration: String,
}

/// `delete_recording` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DeleteRecordingResponse {
    pub duration: String,
}

/// `start_transcription` request (`StartTranscriptionRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct StartTranscriptionRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_closed_captions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcription_external_storage: Option<String>,
}

/// `start_transcription` response (`StartTranscriptionResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StartTranscriptionResponse {
    pub duration: String,
}

/// `stop_transcription` response (`StopTranscriptionResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StopTranscriptionResponse {
    pub duration: String,
}

/// A completed call recording.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CallRecording {
    pub end_time: Timestamp,
    pub filename: String,
    pub recording_type: String,
    pub session_id: String,
    pub start_time: Timestamp,
    pub url: String,
}

/// `list_recordings` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ListRecordingsResponse {
    pub duration: String,
    pub recordings: Vec<CallRecording>,
}

/// A completed call transcription.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CallTranscription {
    pub end_time: Timestamp,
    pub filename: String,
    pub session_id: String,
    pub start_time: Timestamp,
    pub url: String,
}

/// `list_transcriptions` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ListTranscriptionsResponse {
    pub duration: String,
    pub transcriptions: Vec<CallTranscription>,
}

/// `delete_transcription` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DeleteTranscriptionResponse {
    pub duration: String,
}

/// Closed-caption speech segmentation overrides.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SpeechSegmentConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_speech_caption_ms: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub silence_duration_ms: Option<i32>,
}

/// `start_closed_captions` request.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StartClosedCaptionsRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_transcription: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_storage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speech_segment_config: Option<SpeechSegmentConfig>,
}

/// `start_closed_captions` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StartClosedCaptionsResponse {
    pub duration: String,
}

/// `stop_closed_captions` request.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StopClosedCaptionsRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_transcription: Option<bool>,
}

/// `stop_closed_captions` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StopClosedCaptionsResponse {
    pub duration: String,
}

/// Send an application-provided closed-caption segment to the call.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SendClosedCaptionRequest {
    /// Participant or external speaker identifier displayed with the caption.
    pub speaker_id: String,
    /// Caption text.
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_time: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub translated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<UserRequest>,
}

impl SendClosedCaptionRequest {
    /// Build a caption segment for `speaker_id`.
    pub fn new(speaker_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            speaker_id: speaker_id.into(),
            text: text.into(),
            ..Default::default()
        }
    }
}

/// `send_closed_caption` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SendClosedCaptionResponse {
    pub duration: String,
}

/// `start_frame_recording` request.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StartFrameRecordingRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_external_storage: Option<String>,
}

/// `start_frame_recording` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StartFrameRecordingResponse {
    pub duration: String,
}

/// `stop_frame_recording` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StopFrameRecordingResponse {
    pub duration: String,
}

// HLS / RTMP

/// `start_hls_broadcasting` response (`StartHLSBroadcastingResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StartHLSBroadcastingResponse {
    pub duration: String,
    /// URL of the HLS playlist.
    pub playlist_url: String,
}

/// `stop_hls_broadcasting` response (`StopHLSBroadcastingResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StopHLSBroadcastingResponse {
    pub duration: String,
}

/// A single RTMP broadcast target (`RTMPBroadcastRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct RtmpBroadcastRequest {
    /// Unique name for the broadcast within the call.
    pub name: String,
    /// RTMP server URL.
    pub stream_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<Value>,
}

/// `start_rtmp_broadcasts` request (`StartRTMPBroadcastsRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct StartRtmpBroadcastsRequest {
    pub broadcasts: Vec<RtmpBroadcastRequest>,
}

/// `start_rtmp_broadcasts` response (`StartRTMPBroadcastsResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StartRtmpBroadcastsResponse {
    pub duration: String,
}

/// `stop_all_rtmp_broadcasts` response (`StopAllRTMPBroadcastsResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StopAllRtmpBroadcastsResponse {
    pub duration: String,
}

/// `stop_rtmp_broadcast` response (`StopRTMPBroadcastsResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StopRtmpBroadcastsResponse {
    pub duration: String,
}

// Pin / permissions

/// `pin_for_everyone` request (`VideoPinRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct PinRequest {
    pub session_id: String,
    pub user_id: String,
}

/// `pin_for_everyone` response (`PinResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PinResponse {
    pub duration: String,
}

/// `unpin_for_everyone` request (`VideoUnpinRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct UnpinRequest {
    pub session_id: String,
    pub user_id: String,
}

/// `unpin_for_everyone` response (`UnpinResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UnpinResponse {
    pub duration: String,
}

/// `update_user_permissions` request (`UpdateUserPermissionsRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateUserPermissionsRequest {
    pub user_id: String,
    pub grant_permissions: Vec<String>,
    pub revoke_permissions: Vec<String>,
}

/// `update_user_permissions` response (`UpdateUserPermissionsResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UpdateUserPermissionsResponse {
    pub duration: String,
}

/// `request_permissions` request (`RequestPermissionRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct RequestPermissionRequest {
    pub permissions: Vec<String>,
}

/// `request_permissions` response (`RequestPermissionResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RequestPermissionResponse {
    pub duration: String,
}

/// `submit_feedback` request.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CollectUserFeedbackRequest {
    pub rating: i32,
    pub sdk: String,
    pub sdk_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_session_id: Option<String>,
    pub custom: CustomData,
}

/// `submit_feedback` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CollectUserFeedbackResponse {
    pub duration: String,
}

/// Optional session selector for `get_call_report`.
#[derive(Debug, Clone, Default)]
pub struct GetCallReportRequest {
    pub session_id: Option<String>,
}

/// `get_call_report` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GetCallReportResponse {
    pub duration: String,
    pub session_id: String,
    pub report: Value,
    pub video_reactions: Vec<Value>,
    pub chat_activity: Option<Value>,
    pub session: Option<Value>,
}

/// Historical call-session statistics.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GetCallStatsResponse {
    pub aggregated: Option<Value>,
    pub average_connection_time: Option<f64>,
    pub call_duration_seconds: f64,
    pub call_status: String,
    pub call_timeline: Option<Value>,
    pub duration: String,
    pub is_truncated_report: bool,
    pub jitter: Option<Value>,
    pub latency: Option<Value>,
    pub max_freezes_duration_seconds: f64,
    pub max_participants: i32,
    pub max_total_quality_limitation_duration_seconds: f64,
    pub participant_report: Vec<Value>,
    pub publishing_participants: i32,
    pub quality_score: f64,
    pub sfu_count: i32,
    pub sfus: Vec<Value>,
}

/// Query parameters for `get_call_stats_map`.
#[derive(Debug, Clone, Default)]
pub struct GetCallStatsMapRequest {
    pub start_time: Option<Timestamp>,
    pub end_time: Option<Timestamp>,
    pub exclude_publishers: Option<bool>,
    pub exclude_subscribers: Option<bool>,
    pub exclude_sfus: Option<bool>,
}

/// Map-oriented call-session statistics.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QueryCallStatsMapResponse {
    pub call_id: String,
    pub call_session_id: String,
    pub call_type: String,
    pub duration: String,
    pub counts: Value,
    pub call_ended_at: Option<Timestamp>,
    pub call_started_at: Option<Timestamp>,
    pub data_source: Option<String>,
    pub end_time: Option<Timestamp>,
    pub generated_at: Option<Timestamp>,
    pub start_time: Option<Timestamp>,
    pub publishers: Option<Value>,
    pub sfus: Option<Value>,
    pub subscribers: Option<Value>,
}

// Query calls

/// `query_calls` request (`QueryCallsRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct QueryCallsRequest {
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub filter_conditions: CustomData,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<SortParamRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
}

/// A call plus its members/capabilities (`CallStateResponseFields`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CallStateResponseFields {
    pub call: CallResponse,
    pub members: Vec<MemberResponse>,
    pub own_capabilities: Vec<String>,
}

/// `query_calls` response (`QueryCallsResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QueryCallsResponse {
    pub duration: String,
    pub calls: Vec<CallStateResponseFields>,
    pub next: Option<String>,
    pub prev: Option<String>,
}

// Call types

/// A call type (`CallTypeResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CallTypeResponse {
    pub name: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub grants: HashMap<String, Vec<String>>,
    pub settings: Option<Value>,
    pub notification_settings: Option<Value>,
    pub external_storage: Option<String>,
}

/// `list_call_types` response (`ListCallTypeResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ListCallTypeResponse {
    pub duration: String,
    pub call_types: HashMap<String, CallTypeResponse>,
}

/// `create_call_type` request (`CreateCallTypeRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct CreateCallTypeRequest {
    pub name: String,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub grants: HashMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_settings: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_storage: Option<String>,
}

/// `create` / `get` / `update` call-type response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CallTypeCrudResponse {
    pub duration: String,
    pub name: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub grants: HashMap<String, Vec<String>>,
    pub settings: Option<Value>,
    pub notification_settings: Option<Value>,
    pub external_storage: Option<String>,
}

/// `update_call_type` request (`UpdateCallTypeRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateCallTypeRequest {
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub grants: HashMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_settings: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_storage: Option<String>,
}

// Edges

/// A single SFU edge (`EdgeResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct EdgeResponse {
    pub id: String,
    pub latency_test_url: String,
    pub continent_code: String,
    pub country_iso_code: String,
    pub subdivision_iso_code: String,
    pub latitude: f64,
    pub longitude: f64,
    pub green: i32,
    pub yellow: i32,
    pub red: i32,
}

/// `get_edges` response (`GetEdgesResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GetEdgesResponse {
    pub duration: String,
    pub edges: Vec<EdgeResponse>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn query_participants_limit_is_query_only() {
        let value = serde_json::to_value(QueryCallParticipantsRequest {
            limit: Some(25),
            filter_conditions: CustomData::new(),
        })
        .expect("request should serialize");
        assert_eq!(value, json!({}));
    }

    #[test]
    fn ring_request_preserves_empty_member_list() {
        let value = serde_json::to_value(RingCallRequest {
            video: Some(true),
            members_ids: Vec::new(),
        })
        .expect("request should serialize");
        assert_eq!(value, json!({"video": true, "members_ids": []}));
    }

    #[test]
    fn reaction_uses_type_wire_name() {
        let value = serde_json::to_value(SendVideoReactionRequest {
            reaction_type: "raise-hand".to_owned(),
            emoji_code: Some("✋".to_owned()),
            custom: None,
        })
        .expect("request should serialize");
        assert_eq!(value, json!({"type": "raise-hand", "emoji_code": "✋"}));
    }

    #[test]
    fn closed_caption_options_match_wire_shape() {
        let value = serde_json::to_value(StartClosedCaptionsRequest {
            enable_transcription: Some(true),
            language: Some("en".to_owned()),
            speech_segment_config: Some(SpeechSegmentConfig {
                max_speech_caption_ms: Some(2_000),
                silence_duration_ms: Some(500),
            }),
            ..Default::default()
        })
        .expect("request should serialize");
        assert_eq!(
            value,
            json!({
                "enable_transcription": true,
                "language": "en",
                "speech_segment_config": {
                    "max_speech_caption_ms": 2000,
                    "silence_duration_ms": 500
                }
            })
        );
    }

    #[test]
    fn manual_closed_caption_matches_current_coordinator_shape() {
        let value = serde_json::to_value(SendClosedCaptionRequest {
            speaker_id: "speaker-1".to_owned(),
            text: "hello".to_owned(),
            language: Some("en".to_owned()),
            translated: Some(false),
            ..Default::default()
        })
        .expect("request should serialize");
        assert_eq!(
            value,
            json!({
                "speaker_id": "speaker-1",
                "text": "hello",
                "language": "en",
                "translated": false
            })
        );
    }

    #[test]
    fn call_response_exposes_typed_session_and_common_settings() {
        let response: CallResponse = serde_json::from_value(json!({
            "cid": "default:call-1",
            "settings": {
                "audio": { "mic_default_on": true },
                "video": {
                    "enabled": true,
                    "target_resolution": { "width": 1280, "height": 720, "bitrate": 1000000 }
                },
                "screensharing": { "enabled": true },
                "session": { "inactivity_timeout_seconds": 30 },
                "limits": { "max_participants": 100 },
                "encryption": { "mode": "available" }
            },
            "session": {
                "id": "session-1",
                "anonymous_participant_count": 2,
                "participants_count_by_role": { "user": 3 }
            }
        }))
        .expect("call response should deserialize");

        let settings = response.settings.expect("typed settings");
        assert!(settings.audio.mic_default_on);
        assert_eq!(settings.video.target_resolution.width, 1280);
        assert_eq!(settings.limits.max_participants, Some(100));
        assert!(settings.additional_fields.contains_key("encryption"));
        let session = response.session.expect("typed session");
        assert_eq!(session.id, "session-1");
        assert_eq!(session.participants_count_by_role["user"], 3);
    }
}
