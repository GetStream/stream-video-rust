//! Video coordinator REST: [`VideoClient`] and [`Call`].

mod call;

pub use call::Call;

use std::sync::Arc;

use reqwest::Method;

use crate::client::Client;
use crate::error::Result;
use crate::models::{
    CallTypeCrudResponse, CreateCallTypeRequest, GetEdgesResponse, ListCallTypeResponse,
    QueryCallsRequest, QueryCallsResponse, Response, UpdateCallTypeRequest,
};

/// Entry point for video endpoints. Obtain via [`crate::Stream::video`].
#[derive(Clone)]
pub struct VideoClient {
    client: Arc<Client>,
}

impl VideoClient {
    pub(crate) fn new(client: Arc<Client>) -> Self {
        Self { client }
    }

    /// A handle to a specific call. No request is made until a method is called.
    pub fn call(&self, call_type: impl Into<String>, call_id: impl Into<String>) -> Call {
        Call::new(self.client.clone(), call_type.into(), call_id.into())
    }

    /// Query calls with filter/sort/pagination (`POST /api/v2/video/calls`).
    pub async fn query_calls(&self, request: QueryCallsRequest) -> Result<QueryCallsResponse> {
        self.client
            .request(Method::POST, "/api/v2/video/calls", &[], Some(&request))
            .await
    }

    /// List available SFU edges (`GET /api/v2/video/edges`).
    pub async fn get_edges(&self) -> Result<GetEdgesResponse> {
        self.client
            .request::<(), _>(Method::GET, "/api/v2/video/edges", &[], None)
            .await
    }

    /// List all call types (`GET /api/v2/video/calltypes`).
    pub async fn list_call_types(&self) -> Result<ListCallTypeResponse> {
        self.client
            .request::<(), _>(Method::GET, "/api/v2/video/calltypes", &[], None)
            .await
    }

    /// Create a call type (`POST /api/v2/video/calltypes`).
    pub async fn create_call_type(
        &self,
        request: CreateCallTypeRequest,
    ) -> Result<CallTypeCrudResponse> {
        self.client
            .request(Method::POST, "/api/v2/video/calltypes", &[], Some(&request))
            .await
    }

    /// Get a call type by name (`GET /api/v2/video/calltypes/{name}`).
    pub async fn get_call_type(&self, name: &str) -> Result<CallTypeCrudResponse> {
        let path = Client::build_path("/api/v2/video/calltypes/{name}", &[("name", name)]);
        self.client
            .request::<(), _>(Method::GET, &path, &[], None)
            .await
    }

    /// Update a call type (`PUT /api/v2/video/calltypes/{name}`).
    pub async fn update_call_type(
        &self,
        name: &str,
        request: UpdateCallTypeRequest,
    ) -> Result<CallTypeCrudResponse> {
        let path = Client::build_path("/api/v2/video/calltypes/{name}", &[("name", name)]);
        self.client
            .request(Method::PUT, &path, &[], Some(&request))
            .await
    }

    /// Delete a call type (`DELETE /api/v2/video/calltypes/{name}`).
    pub async fn delete_call_type(&self, name: &str) -> Result<Response> {
        let path = Client::build_path("/api/v2/video/calltypes/{name}", &[("name", name)]);
        self.client
            .request::<(), _>(Method::DELETE, &path, &[], None)
            .await
    }
}
