//! SIP (telephony) request/response models.
//!
//! Field names track the getstream-go JSON tags so a later OpenAPI codegen pass
//! can replace these transparently. Response types derive `Default` +
//! `#[serde(default)]` so partial payloads deserialize cleanly.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::shared::{CustomData, Timestamp};

// SIP trunks

/// `create_sip_trunk` request (`CreateSIPTrunkRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct CreateSipTrunkRequest {
    /// Name of the SIP trunk.
    pub name: String,
    /// Phone numbers associated with this SIP trunk.
    pub numbers: Vec<String>,
    /// Optional password for SIP trunk authentication.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Optional list of allowed IPv4/IPv6 addresses or CIDR blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_ips: Option<Vec<String>>,
}

impl CreateSipTrunkRequest {
    /// Build a create request for a named trunk with its phone numbers.
    pub fn new(name: impl Into<String>, numbers: impl IntoIterator<Item = String>) -> Self {
        Self {
            name: name.into(),
            numbers: numbers.into_iter().collect(),
            ..Default::default()
        }
    }
}

/// `update_sip_trunk` request (`UpdateSIPTrunkRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateSipTrunkRequest {
    /// Name of the SIP trunk.
    pub name: String,
    /// Phone numbers associated with this SIP trunk.
    pub numbers: Vec<String>,
    /// Optional password for SIP trunk authentication.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Optional list of allowed IPv4/IPv6 addresses or CIDR blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_ips: Option<Vec<String>>,
}

/// A SIP trunk (`SIPTrunkResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SipTrunkResponse {
    pub id: String,
    pub name: String,
    /// Password for SIP trunk authentication.
    pub password: String,
    /// Username for SIP trunk authentication.
    pub username: String,
    /// The URI for the SIP trunk.
    pub uri: String,
    pub numbers: Vec<String>,
    pub allowed_ips: Vec<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// `create_sip_trunk` response (`CreateSIPTrunkResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CreateSipTrunkResponse {
    pub duration: String,
    pub sip_trunk: Option<SipTrunkResponse>,
}

/// `update_sip_trunk` response (`UpdateSIPTrunkResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UpdateSipTrunkResponse {
    pub duration: String,
    pub sip_trunk: Option<SipTrunkResponse>,
}

/// `delete_sip_trunk` response (`DeleteSIPTrunkResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DeleteSipTrunkResponse {
    pub duration: String,
}

/// `list_sip_trunks` response (`ListSIPTrunksResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ListSipTrunksResponse {
    pub duration: String,
    pub sip_trunks: Vec<SipTrunkResponse>,
}

// SIP inbound routing rules

/// SIP caller settings (`SIPCallerConfigsRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct SipCallerConfigsRequest {
    /// Unique identifier for the caller (handlebars template).
    pub id: String,
    /// Custom data associated with the caller (values are handlebars templates).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_data: Option<CustomData>,
}

/// SIP call settings (`SIPCallConfigsRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct SipCallConfigsRequest {
    /// Custom data associated with the call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_data: Option<CustomData>,
}

/// Direct routing rule call settings (`SIPDirectRoutingRuleCallConfigsRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct SipDirectRoutingRuleCallConfigsRequest {
    /// ID of the call (handlebars template).
    pub call_id: String,
    /// Type of the call.
    pub call_type: String,
}

/// PIN protection settings (`SIPPinProtectionConfigsRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct SipPinProtectionConfigsRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_pin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_pin_digits: Option<i32>,
}

/// PIN routing rule call settings (`SIPInboundRoutingRulePinConfigsRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct SipInboundRoutingRulePinConfigsRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_webhook_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin_failed_attempt_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin_hangup_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin_success_prompt: Option<String>,
}

/// `create_sip_inbound_routing_rule` request (`CreateSIPInboundRoutingRuleRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct CreateSipInboundRoutingRuleRequest {
    pub name: String,
    pub trunk_ids: Vec<String>,
    pub caller_configs: SipCallerConfigsRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub called_numbers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_numbers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_configs: Option<SipCallConfigsRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direct_routing_configs: Option<SipDirectRoutingRuleCallConfigsRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin_protection_configs: Option<SipPinProtectionConfigsRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin_routing_configs: Option<SipInboundRoutingRulePinConfigsRequest>,
}

/// `update_sip_inbound_routing_rule` request (`UpdateSIPInboundRoutingRuleRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateSipInboundRoutingRuleRequest {
    pub name: String,
    pub trunk_ids: Vec<String>,
    pub caller_configs: SipCallerConfigsRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub called_numbers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_numbers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_configs: Option<SipCallConfigsRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direct_routing_configs: Option<SipDirectRoutingRuleCallConfigsRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin_protection_configs: Option<SipPinProtectionConfigsRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin_routing_configs: Option<SipInboundRoutingRulePinConfigsRequest>,
}

/// SIP call settings response (`SIPCallConfigsResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SipCallConfigsResponse {
    pub custom_data: CustomData,
}

/// SIP caller settings response (`SIPCallerConfigsResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SipCallerConfigsResponse {
    pub id: String,
    pub custom_data: CustomData,
}

/// Direct routing rule call settings response
/// (`SIPDirectRoutingRuleCallConfigsResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SipDirectRoutingRuleCallConfigsResponse {
    pub call_id: String,
    pub call_type: String,
}

/// PIN protection settings response (`SIPPinProtectionConfigsResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SipPinProtectionConfigsResponse {
    pub enabled: bool,
    pub default_pin: Option<String>,
    pub max_attempts: Option<i32>,
    pub required_pin_digits: Option<i32>,
}

/// PIN routing rule call settings response
/// (`SIPInboundRoutingRulePinConfigsResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SipInboundRoutingRulePinConfigsResponse {
    pub custom_webhook_url: Option<String>,
    pub pin_failed_attempt_prompt: Option<String>,
    pub pin_hangup_prompt: Option<String>,
    pub pin_prompt: Option<String>,
    pub pin_success_prompt: Option<String>,
}

/// A SIP inbound routing rule (`SIPInboundRoutingRuleResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SipInboundRoutingRuleResponse {
    pub id: String,
    pub name: String,
    pub called_numbers: Vec<String>,
    pub trunk_ids: Vec<String>,
    pub caller_numbers: Vec<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub call_configs: Option<SipCallConfigsResponse>,
    pub caller_configs: Option<SipCallerConfigsResponse>,
    pub direct_routing_configs: Option<SipDirectRoutingRuleCallConfigsResponse>,
    pub pin_protection_configs: Option<SipPinProtectionConfigsResponse>,
    pub pin_routing_configs: Option<SipInboundRoutingRulePinConfigsResponse>,
}

/// `create_sip_inbound_routing_rule` response (`SIPInboundRoutingRuleResponse`
/// envelope, returned directly for create).
///
/// The create endpoint returns the rule fields inline alongside `duration`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CreateSipInboundRoutingRuleResponse {
    pub duration: String,
    pub id: String,
    pub name: String,
    pub called_numbers: Vec<String>,
    pub trunk_ids: Vec<String>,
    pub caller_numbers: Vec<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub call_configs: Option<SipCallConfigsResponse>,
    pub caller_configs: Option<SipCallerConfigsResponse>,
    pub direct_routing_configs: Option<SipDirectRoutingRuleCallConfigsResponse>,
    pub pin_protection_configs: Option<SipPinProtectionConfigsResponse>,
    pub pin_routing_configs: Option<SipInboundRoutingRulePinConfigsResponse>,
}

/// `update_sip_inbound_routing_rule` response (`UpdateSIPInboundRoutingRuleResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UpdateSipInboundRoutingRuleResponse {
    pub duration: String,
    pub sip_inbound_routing_rule: Option<SipInboundRoutingRuleResponse>,
}

/// `delete_sip_inbound_routing_rule` response (`DeleteSIPInboundRoutingRuleResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DeleteSipInboundRoutingRuleResponse {
    pub duration: String,
}

/// `list_sip_inbound_routing_rules` response (`ListSIPInboundRoutingRuleResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ListSipInboundRoutingRuleResponse {
    pub duration: String,
    pub sip_inbound_routing_rules: Vec<SipInboundRoutingRuleResponse>,
}

// SIP auth / resolve

/// `resolve_sip_auth` request (`ResolveSipAuthRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ResolveSipAuthRequest {
    pub sip_caller_number: String,
    pub sip_trunk_number: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ip: Option<String>,
}

/// `resolve_sip_auth` response (`ResolveSipAuthResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ResolveSipAuthResponse {
    /// Authentication result: `password`, `accept`, or `no_trunk_found`.
    pub auth_result: String,
    pub duration: String,
    pub password: Option<String>,
    pub trunk_id: Option<String>,
    pub username: Option<String>,
}

/// SIP digest challenge authentication data (`SIPChallengeRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct SipChallengeRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub charset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cnonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opaque: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub realm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub userhash: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub domain: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub qop: Vec<String>,
}

/// `resolve_sip_inbound` request (`ResolveSipInboundRequest`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ResolveSipInboundRequest {
    pub sip_caller_number: String,
    pub sip_trunk_number: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trunk_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub challenge: Option<SipChallengeRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sip_headers: Option<HashMap<String, String>>,
}

/// Credentials for SIP inbound call authentication (`SipInboundCredentials`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SipInboundCredentials {
    pub api_key: String,
    pub call_id: String,
    pub call_type: String,
    pub token: String,
    pub user_id: String,
    pub call_custom_data: CustomData,
    pub user_custom_data: CustomData,
}

/// `resolve_sip_inbound` response (`ResolveSipInboundResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ResolveSipInboundResponse {
    pub duration: String,
    pub credentials: SipInboundCredentials,
    pub sip_routing_rule: Option<SipInboundRoutingRuleResponse>,
    pub sip_trunk: Option<SipTrunkResponse>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn create_trunk_omits_absent_optionals_and_keeps_required_fields() {
        let value = serde_json::to_value(CreateSipTrunkRequest::new(
            "primary",
            ["+15551230000".to_owned()],
        ))
        .expect("request should serialize");
        assert_eq!(
            value,
            json!({ "name": "primary", "numbers": ["+15551230000"] })
        );
    }

    #[test]
    fn create_routing_rule_serializes_required_and_nested_configs() {
        let request = CreateSipInboundRoutingRuleRequest {
            name: "rule-1".to_owned(),
            trunk_ids: vec!["trunk-1".to_owned()],
            caller_configs: SipCallerConfigsRequest {
                id: "{{caller_number}}".to_owned(),
                custom_data: None,
            },
            direct_routing_configs: Some(SipDirectRoutingRuleCallConfigsRequest {
                call_id: "support".to_owned(),
                call_type: "default".to_owned(),
            }),
            ..Default::default()
        };
        let value = serde_json::to_value(&request).expect("request should serialize");
        assert_eq!(
            value,
            json!({
                "name": "rule-1",
                "trunk_ids": ["trunk-1"],
                "caller_configs": { "id": "{{caller_number}}" },
                "direct_routing_configs": { "call_id": "support", "call_type": "default" }
            })
        );
    }

    #[test]
    fn trunk_response_deserializes_partial_payload() {
        let response: CreateSipTrunkResponse = serde_json::from_value(json!({
            "duration": "1.2ms",
            "sip_trunk": {
                "id": "trunk-1",
                "name": "primary",
                "numbers": ["+15551230000"]
            }
        }))
        .expect("response should deserialize");
        let trunk = response.sip_trunk.expect("trunk present");
        assert_eq!(trunk.id, "trunk-1");
        assert_eq!(trunk.numbers, vec!["+15551230000".to_owned()]);
        assert!(trunk.allowed_ips.is_empty());
    }
}
