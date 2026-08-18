//! User management endpoints (common surface, `/api/v2/users`).

use reqwest::Method;

use crate::Stream;
use crate::error::Result;
use crate::models::{
    DeleteUsersRequest, DeleteUsersResponse, QueryUsersPayload, QueryUsersResponse,
    UpdateUserPartialRequest, UpdateUsersPartialRequest, UpdateUsersRequest, UpdateUsersResponse,
    UserRequest,
};

impl Stream {
    /// Create or update users in bulk (`POST /api/v2/users`).
    ///
    /// Mirrors getstream-go `UpdateUsers` (the upsert endpoint). Users are keyed
    /// by their `id`.
    pub async fn upsert_users(
        &self,
        users: impl IntoIterator<Item = UserRequest>,
    ) -> Result<UpdateUsersResponse> {
        let map = users.into_iter().map(|u| (u.id.clone(), u)).collect();
        let body = UpdateUsersRequest { users: map };
        self.client()
            .request(Method::POST, "/api/v2/users", &[], Some(&body))
            .await
    }

    /// Query users with filter/sort/pagination (`GET /api/v2/users`).
    ///
    /// The payload is JSON-encoded into the `payload` query param, matching the
    /// Go SDK.
    pub async fn query_users(&self, payload: QueryUsersPayload) -> Result<QueryUsersResponse> {
        let encoded = serde_json::to_string(&payload)?;
        let query = [("payload".to_string(), encoded)];
        self.client()
            .request::<(), _>(Method::GET, "/api/v2/users", &query, None)
            .await
    }

    /// Partially update users (`PATCH /api/v2/users`).
    pub async fn update_users_partial(
        &self,
        users: impl IntoIterator<Item = UpdateUserPartialRequest>,
    ) -> Result<UpdateUsersResponse> {
        let body = UpdateUsersPartialRequest {
            users: users.into_iter().collect(),
        };
        self.client()
            .request(Method::PATCH, "/api/v2/users", &[], Some(&body))
            .await
    }

    /// Delete users (`POST /api/v2/users/delete`).
    pub async fn delete_users(&self, request: DeleteUsersRequest) -> Result<DeleteUsersResponse> {
        self.client()
            .request(Method::POST, "/api/v2/users/delete", &[], Some(&request))
            .await
    }
}
