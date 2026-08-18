use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Arbitrary custom-data map (`custom` fields across the API).
pub type CustomData = HashMap<String, Value>;

/// A timestamp as returned by the API. Kept as raw JSON because different
/// endpoints encode it as an RFC3339 string or a numeric (unix) value; this
/// avoids a datetime dependency while tolerating both shapes.
pub type Timestamp = Value;

/// A single sort parameter for query endpoints.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SortParamRequest {
    /// Sort direction: `1` ascending, `-1` descending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<i32>,
    /// Field name to sort by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

/// Generic `{ "duration": ... }` response used by endpoints with no payload.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Response {
    /// Server-reported request duration.
    pub duration: String,
}
