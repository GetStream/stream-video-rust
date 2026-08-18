use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::shared::{CustomData, SortParamRequest, Timestamp};

/// A user to upsert (`UserRequest`). Only `id` is required.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UserRequest {
    /// Unique user ID.
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Global role for the user (e.g. `admin`, `user`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invisible: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Teams the user belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teams: Option<Vec<String>>,
    /// Custom user data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<CustomData>,
    /// Per-team roles.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teams_role: Option<HashMap<String, String>>,
}

impl UserRequest {
    /// Construct a bare user request from an ID.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Default::default()
        }
    }
}

/// A user as returned by the API (`UserResponse` / `FullUserResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UserResponse {
    pub id: String,
    pub role: String,
    pub banned: bool,
    pub online: bool,
    pub invisible: bool,
    pub language: String,
    pub shadow_banned: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub blocked_user_ids: Vec<String>,
    pub teams: Vec<String>,
    pub custom: CustomData,
    pub name: Option<String>,
    pub image: Option<String>,
    pub last_active: Option<Timestamp>,
    pub deactivated_at: Option<Timestamp>,
    pub deleted_at: Option<Timestamp>,
    pub teams_role: Option<HashMap<String, String>>,
}

/// Bulk upsert request (`POST /api/v2/users`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateUsersRequest {
    /// Users to create/update, keyed by user ID.
    pub users: HashMap<String, UserRequest>,
}

/// Response for upsert / partial update (`UpdateUsersResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UpdateUsersResponse {
    pub duration: String,
    pub membership_deletion_task_id: String,
    /// Upserted users keyed by user ID.
    pub users: HashMap<String, UserResponse>,
}

/// Query-users payload, JSON-encoded into the `payload` query param.
#[derive(Debug, Clone, Default, Serialize)]
pub struct QueryUsersPayload {
    /// MongoDB-style filter conditions.
    pub filter_conditions: CustomData,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<Vec<SortParamRequest>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// Response for `query_users` (`QueryUsersResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QueryUsersResponse {
    pub duration: String,
    pub users: Vec<UserResponse>,
}

/// A single partial-update entry (`UpdateUserPartialRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateUserPartialRequest {
    /// User ID to update.
    pub id: String,
    /// Fields to set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set: Option<CustomData>,
    /// Field paths to unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unset: Option<Vec<String>>,
}

/// Bulk partial-update request (`PATCH /api/v2/users`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateUsersPartialRequest {
    pub users: Vec<UpdateUserPartialRequest>,
}

/// Delete-users request (`POST /api/v2/users/delete`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeleteUsersRequest {
    /// IDs of users to delete.
    pub user_ids: Vec<String>,
    /// Calls delete mode: `soft` | `hard` | null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calls: Option<String>,
    /// Messages delete mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<String>,
    /// Conversations delete mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversations: Option<String>,
    /// Whether to delete user-uploaded files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<bool>,
    /// User delete mode: `soft` | `pruning` | `hard`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_call_owner_id: Option<String>,
}

/// Response for `delete_users` (`DeleteUsersResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DeleteUsersResponse {
    pub duration: String,
    /// Async task ID for the deletion.
    pub task_id: String,
}
