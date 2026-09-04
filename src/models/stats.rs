//! Advanced call statistics and reporting request/response models.
//!
//! Field names track the getstream-go JSON tags. Deeply nested analytics
//! payloads are kept as [`serde_json::Value`] to stay robust across server-side
//! schema additions, matching the existing stats/report types in
//! [`super::call`]. Response types derive `Default` + `#[serde(default)]`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::shared::{CustomData, SortParamRequest, Timestamp};

// Active calls status

/// Aggregate counts for the current active-calls snapshot (`ActiveCallsSummary`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ActiveCallsSummary {
    pub active_calls: i32,
    pub active_publishers: i32,
    pub active_subscribers: i32,
    pub participants: i32,
}

/// `get_active_calls_status` response (`GetActiveCallsStatusResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GetActiveCallsStatusResponse {
    pub duration: String,
    pub start_time: Timestamp,
    pub end_time: Timestamp,
    /// Detailed join/publisher/subscriber metrics (opaque, schema-versioned).
    pub metrics: Option<Value>,
    pub summary: Option<ActiveCallsSummary>,
}

// Aggregate call stats

/// `query_aggregate_call_stats` request (`QueryAggregateCallStatsRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct QueryAggregateCallStatsRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_types: Option<Vec<String>>,
}

/// `query_aggregate_call_stats` response (`QueryAggregateCallStatsResponse`).
///
/// Each report is an opaque, schema-versioned analytics bundle.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QueryAggregateCallStatsResponse {
    pub duration: String,
    pub call_duration_report: Option<Value>,
    pub call_participant_count_report: Option<Value>,
    pub calls_per_day_report: Option<Value>,
    pub network_metrics_report: Option<Value>,
    pub quality_score_report: Option<Value>,
    pub sdk_usage_report: Option<Value>,
    pub user_feedback_report: Option<Value>,
}

// Call session stats

/// `query_call_session_stats` request (`QueryCallSessionStatsRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct QueryCallSessionStatsRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<SortParamRequest>,
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub filter_conditions: CustomData,
}

/// `query_call_session_stats` response (`QueryCallSessionStatsResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QueryCallSessionStatsResponse {
    pub duration: String,
    /// Per-session stat summaries (opaque, schema-versioned).
    pub call_stats: Vec<Value>,
    pub next: Option<String>,
    pub prev: Option<String>,
}

// Session participant stats (call-scoped)

/// Query params for `get_call_session_participant_stats_details`.
#[derive(Debug, Clone, Default)]
pub struct GetCallSessionParticipantStatsDetailsRequest {
    pub since: Option<String>,
    pub until: Option<String>,
    pub max_points: Option<i32>,
}

/// `get_call_session_participant_stats_details` response
/// (`GetCallSessionParticipantStatsDetailsResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GetCallSessionParticipantStatsDetailsResponse {
    pub duration: String,
    pub call_id: String,
    pub call_session_id: String,
    pub call_type: String,
    pub user_id: String,
    pub user_session_id: String,
    pub publisher: Option<Value>,
    pub subscriber: Option<Value>,
    pub timeframe: Option<Value>,
    pub user: Option<Value>,
}

/// Query params for `query_call_session_participant_stats`.
#[derive(Debug, Clone, Default)]
pub struct QueryCallSessionParticipantStatsRequest {
    pub limit: Option<i32>,
    pub prev: Option<String>,
    pub next: Option<String>,
    pub sort: Vec<SortParamRequest>,
    pub filter_conditions: CustomData,
}

/// `query_call_session_participant_stats` response
/// (`QueryCallSessionParticipantStatsResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QueryCallSessionParticipantStatsResponse {
    pub duration: String,
    pub call_id: String,
    pub call_session_id: String,
    pub call_type: String,
    /// Per-participant stat summaries (opaque, schema-versioned).
    pub participants: Vec<Value>,
    pub counts: Value,
    pub call_started_at: Option<Timestamp>,
    pub call_ended_at: Option<Timestamp>,
    pub next: Option<String>,
    pub prev: Option<String>,
    pub tmp_data_source: Option<String>,
    pub call_events: Vec<Value>,
}

/// Query params for `get_call_session_participant_stats_timeline`.
#[derive(Debug, Clone, Default)]
pub struct GetCallSessionParticipantStatsTimelineRequest {
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub severity: Vec<String>,
}

/// `get_call_session_participant_stats_timeline` response
/// (`QueryCallSessionParticipantStatsTimelineResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QueryCallSessionParticipantStatsTimelineResponse {
    pub duration: String,
    pub call_id: String,
    pub call_session_id: String,
    pub call_type: String,
    pub user_id: String,
    pub user_session_id: String,
    /// Timeline events (opaque, schema-versioned).
    pub events: Vec<Value>,
}

// Participant session metrics / sessions (call-scoped)

/// Query params for `get_call_participant_session_metrics`.
#[derive(Debug, Clone, Default)]
pub struct GetCallParticipantSessionMetricsRequest {
    pub since: Option<Timestamp>,
    pub until: Option<Timestamp>,
}

/// `get_call_participant_session_metrics` response
/// (`GetCallParticipantSessionMetricsResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GetCallParticipantSessionMetricsResponse {
    pub duration: String,
    pub is_publisher: Option<bool>,
    pub is_subscriber: Option<bool>,
    pub joined_at: Option<Timestamp>,
    pub publisher_type: Option<String>,
    pub user_id: Option<String>,
    pub user_session_id: Option<String>,
    /// Per-track publish metrics (opaque, schema-versioned).
    pub published_tracks: Vec<Value>,
    pub client: Option<Value>,
}

/// Query params for `query_call_participant_sessions`.
#[derive(Debug, Clone, Default)]
pub struct QueryCallParticipantSessionsRequest {
    pub limit: Option<i32>,
    pub prev: Option<String>,
    pub next: Option<String>,
    pub filter_conditions: CustomData,
}

/// `query_call_participant_sessions` response
/// (`QueryCallParticipantSessionsResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QueryCallParticipantSessionsResponse {
    pub duration: i64,
    pub call_id: String,
    pub call_session_id: String,
    pub call_type: String,
    pub total_participant_duration: i64,
    pub total_participant_sessions: i64,
    /// Per-participant-session details (opaque, schema-versioned).
    pub participants_sessions: Vec<Value>,
    pub next: Option<String>,
    pub prev: Option<String>,
    pub session: Option<super::call::CallSessionResponse>,
}

// Daily digest

/// Query params for `get_daily_digest`.
#[derive(Debug, Clone, Default)]
pub struct GetDailyDigestRequest {
    pub date: Option<String>,
    pub target_app_id: Option<String>,
}

/// `get_daily_digest` response (`GetDailyDigestResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GetDailyDigestResponse {
    pub duration: String,
    pub date: String,
    /// Readiness status: `ready`, `pending`, `failed`, `future_date`, `expired`.
    pub status: String,
    pub generated_at: Option<String>,
    pub retry_after: Option<i32>,
    pub revision: Option<i32>,
    pub schema_version: Option<String>,
    pub digest_kinds: Vec<String>,
    /// Per-broadcast digests (opaque, present only when `status` is `ready`).
    pub broadcasts: Vec<Value>,
    /// Per-call-session summaries (opaque, present only when `status` is `ready`).
    pub call_sessions: Vec<Value>,
    pub broadcast_rollup: Option<Value>,
}

// User feedback

/// `query_user_feedback` request (`QueryUserFeedbackRequest`).
///
/// `full` is sent as a query parameter; the remaining fields form the JSON body.
#[derive(Debug, Clone, Default, Serialize)]
pub struct QueryUserFeedbackRequest {
    /// Return full feedback records. Sent as a query parameter.
    #[serde(skip)]
    pub full: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<SortParamRequest>,
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub filter_conditions: CustomData,
}

/// A single user feedback record (`UserFeedbackResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UserFeedbackResponse {
    pub cid: String,
    pub rating: i32,
    pub reason: String,
    pub sdk: String,
    pub sdk_version: String,
    pub session_id: String,
    pub user_id: String,
    pub platform: Value,
    pub custom: CustomData,
}

/// `query_user_feedback` response (`QueryUserFeedbackResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QueryUserFeedbackResponse {
    pub duration: String,
    pub user_feedback: Vec<UserFeedbackResponse>,
    pub next: Option<String>,
    pub prev: Option<String>,
}

// Client call events

/// A single client-side telemetry event (`ClientEvent`).
///
/// Every field is optional; which fields are required depends on the event's
/// `stage`/`event_type`. See the serverside API reference for the per-stage
/// requirements.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ClientEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub call_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator_connect_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join_attempt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_time: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_count_attempt: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_failure_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_connection: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ice_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sfu_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub was_previously_connected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previously_connected_timestamp: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera_permission_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub microphone_permission_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_share_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdk_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<Timestamp>,
}

/// `report_client_call_event` request (`ReportClientCallEventRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReportClientCallEventRequest {
    /// Client-side events to report (1–100 per request).
    pub events: Vec<ClientEvent>,
}

/// `report_client_call_event` response (`ReportClientEventResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReportClientEventResponse {
    pub duration: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn user_feedback_request_excludes_full_from_body() {
        let value = serde_json::to_value(QueryUserFeedbackRequest {
            full: Some(true),
            limit: Some(10),
            ..Default::default()
        })
        .expect("request should serialize");
        assert_eq!(value, json!({ "limit": 10 }));
    }

    #[test]
    fn client_event_uses_type_wire_name_and_omits_absent_fields() {
        let value = serde_json::to_value(ClientEvent {
            stage: Some("JoinInitiated".to_owned()),
            call_type: Some("default".to_owned()),
            id: Some("call-1".to_owned()),
            ..Default::default()
        })
        .expect("event should serialize");
        assert_eq!(
            value,
            json!({ "stage": "JoinInitiated", "type": "default", "id": "call-1" })
        );
    }

    #[test]
    fn aggregate_stats_request_omits_absent_optionals() {
        let value = serde_json::to_value(QueryAggregateCallStatsRequest {
            report_types: Some(vec!["call_duration_report".to_owned()]),
            ..Default::default()
        })
        .expect("request should serialize");
        assert_eq!(value, json!({ "report_types": ["call_duration_report"] }));
    }

    #[test]
    fn participant_sessions_response_tolerates_integer_duration() {
        let response: QueryCallParticipantSessionsResponse = serde_json::from_value(json!({
            "duration": 12,
            "call_id": "call-1",
            "total_participant_sessions": 3
        }))
        .expect("response should deserialize");
        assert_eq!(response.duration, 12);
        assert_eq!(response.total_participant_sessions, 3);
    }
}
