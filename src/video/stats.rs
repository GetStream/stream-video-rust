//! Application-level call statistics and reporting endpoints on [`VideoClient`].

use reqwest::Method;

use super::VideoClient;
use crate::error::Result;
use crate::models::{
    GetActiveCallsStatusResponse, GetDailyDigestRequest, GetDailyDigestResponse,
    QueryAggregateCallStatsRequest, QueryAggregateCallStatsResponse, QueryCallSessionStatsRequest,
    QueryCallSessionStatsResponse, QueryUserFeedbackRequest, QueryUserFeedbackResponse,
    ReportClientCallEventRequest, ReportClientEventResponse,
};

impl VideoClient {
    /// Get the status of all active calls with metrics and summary
    /// (`GET /api/v2/video/active_calls_status`).
    pub async fn get_active_calls_status(&self) -> Result<GetActiveCallsStatusResponse> {
        self.client
            .request::<(), _>(Method::GET, "/api/v2/video/active_calls_status", &[], None)
            .await
    }

    /// Query aggregate call stats reports (`POST /api/v2/video/stats`).
    pub async fn query_aggregate_call_stats(
        &self,
        request: QueryAggregateCallStatsRequest,
    ) -> Result<QueryAggregateCallStatsResponse> {
        self.client
            .request(Method::POST, "/api/v2/video/stats", &[], Some(&request))
            .await
    }

    /// Query per-session call stats with filter/sort/pagination
    /// (`POST /api/v2/video/call_stats`).
    pub async fn query_call_session_stats(
        &self,
        request: QueryCallSessionStatsRequest,
    ) -> Result<QueryCallSessionStatsResponse> {
        self.client
            .request(
                Method::POST,
                "/api/v2/video/call_stats",
                &[],
                Some(&request),
            )
            .await
    }

    /// Get the per-broadcast daily digest bundle for one UTC day
    /// (`GET /api/v2/video/stats/daily_digest`).
    pub async fn get_daily_digest(
        &self,
        request: GetDailyDigestRequest,
    ) -> Result<GetDailyDigestResponse> {
        let mut query: Vec<(String, String)> = Vec::new();
        if let Some(date) = request.date {
            query.push(("date".to_owned(), date));
        }
        if let Some(target_app_id) = request.target_app_id {
            query.push(("target_app_id".to_owned(), target_app_id));
        }
        self.client
            .request::<(), _>(
                Method::GET,
                "/api/v2/video/stats/daily_digest",
                &query,
                None,
            )
            .await
    }

    /// Query user feedback with filter/sort/pagination
    /// (`POST /api/v2/video/call/feedback`).
    pub async fn query_user_feedback(
        &self,
        request: QueryUserFeedbackRequest,
    ) -> Result<QueryUserFeedbackResponse> {
        let query = request
            .full
            .map(|full| vec![("full".to_owned(), full.to_string())])
            .unwrap_or_default();
        self.client
            .request(
                Method::POST,
                "/api/v2/video/call/feedback",
                &query,
                Some(&request),
            )
            .await
    }

    /// Report a batch of client-side telemetry events
    /// (`POST /api/v2/video/call_client_event`).
    pub async fn report_client_call_event(
        &self,
        request: ReportClientCallEventRequest,
    ) -> Result<ReportClientEventResponse> {
        self.client
            .request(
                Method::POST,
                "/api/v2/video/call_client_event",
                &[],
                Some(&request),
            )
            .await
    }
}
