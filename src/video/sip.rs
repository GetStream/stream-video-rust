//! SIP (telephony) coordinator REST endpoints on [`VideoClient`].

use reqwest::Method;

use super::VideoClient;
use crate::client::Client;
use crate::error::Result;
use crate::models::{
    CreateSipInboundRoutingRuleRequest, CreateSipInboundRoutingRuleResponse, CreateSipTrunkRequest,
    CreateSipTrunkResponse, DeleteSipInboundRoutingRuleResponse, DeleteSipTrunkResponse,
    ListSipInboundRoutingRuleResponse, ListSipTrunksResponse, UpdateSipInboundRoutingRuleRequest,
    UpdateSipInboundRoutingRuleResponse, UpdateSipTrunkRequest, UpdateSipTrunkResponse,
};

const TRUNKS: &str = "/api/v2/video/sip/inbound_trunks";
const TRUNK_BY_ID: &str = "/api/v2/video/sip/inbound_trunks/{id}";
const RULES: &str = "/api/v2/video/sip/inbound_routing_rules";
const RULE_BY_ID: &str = "/api/v2/video/sip/inbound_routing_rules/{id}";

impl VideoClient {
    // SIP inbound trunks

    /// List SIP inbound trunks (`GET /api/v2/video/sip/inbound_trunks`).
    pub async fn list_sip_trunks(&self) -> Result<ListSipTrunksResponse> {
        self.client
            .request::<(), _>(Method::GET, TRUNKS, &[], None)
            .await
    }

    /// Create a SIP inbound trunk (`POST /api/v2/video/sip/inbound_trunks`).
    pub async fn create_sip_trunk(
        &self,
        request: CreateSipTrunkRequest,
    ) -> Result<CreateSipTrunkResponse> {
        self.client
            .request(Method::POST, TRUNKS, &[], Some(&request))
            .await
    }

    /// Update a SIP inbound trunk (`PUT /api/v2/video/sip/inbound_trunks/{id}`).
    pub async fn update_sip_trunk(
        &self,
        id: &str,
        request: UpdateSipTrunkRequest,
    ) -> Result<UpdateSipTrunkResponse> {
        let path = Client::build_path(TRUNK_BY_ID, &[("id", id)]);
        self.client
            .request(Method::PUT, &path, &[], Some(&request))
            .await
    }

    /// Delete a SIP inbound trunk (`DELETE /api/v2/video/sip/inbound_trunks/{id}`).
    pub async fn delete_sip_trunk(&self, id: &str) -> Result<DeleteSipTrunkResponse> {
        let path = Client::build_path(TRUNK_BY_ID, &[("id", id)]);
        self.client
            .request::<(), _>(Method::DELETE, &path, &[], None)
            .await
    }

    // SIP inbound routing rules

    /// List SIP inbound routing rules
    /// (`GET /api/v2/video/sip/inbound_routing_rules`).
    pub async fn list_sip_inbound_routing_rules(
        &self,
    ) -> Result<ListSipInboundRoutingRuleResponse> {
        self.client
            .request::<(), _>(Method::GET, RULES, &[], None)
            .await
    }

    /// Create a SIP inbound routing rule
    /// (`POST /api/v2/video/sip/inbound_routing_rules`).
    pub async fn create_sip_inbound_routing_rule(
        &self,
        request: CreateSipInboundRoutingRuleRequest,
    ) -> Result<CreateSipInboundRoutingRuleResponse> {
        self.client
            .request(Method::POST, RULES, &[], Some(&request))
            .await
    }

    /// Update a SIP inbound routing rule
    /// (`PUT /api/v2/video/sip/inbound_routing_rules/{id}`).
    pub async fn update_sip_inbound_routing_rule(
        &self,
        id: &str,
        request: UpdateSipInboundRoutingRuleRequest,
    ) -> Result<UpdateSipInboundRoutingRuleResponse> {
        let path = Client::build_path(RULE_BY_ID, &[("id", id)]);
        self.client
            .request(Method::PUT, &path, &[], Some(&request))
            .await
    }

    /// Delete a SIP inbound routing rule
    /// (`DELETE /api/v2/video/sip/inbound_routing_rules/{id}`).
    pub async fn delete_sip_inbound_routing_rule(
        &self,
        id: &str,
    ) -> Result<DeleteSipInboundRoutingRuleResponse> {
        let path = Client::build_path(RULE_BY_ID, &[("id", id)]);
        self.client
            .request::<(), _>(Method::DELETE, &path, &[], None)
            .await
    }
}
