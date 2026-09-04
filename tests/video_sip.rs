//! Live integration tests for the SIP (telephony) server REST surface.
//!
//! Run with credentials present (repo `.env`): `cargo test`. Without credentials
//! the tests print a SKIP line and pass without touching the API. Each test
//! creates unique resources and cleans them up on success, failure, and timeout.

mod common;

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use getstream::Stream;
use getstream::models::{
    CreateSipInboundRoutingRuleRequest, CreateSipTrunkRequest, SipCallerConfigsRequest,
    SipDirectRoutingRuleCallConfigsRequest, UpdateSipTrunkRequest,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// A unique, plausible E.164 number derived from a fresh UUID.
fn unique_number() -> String {
    let digits = uuid::Uuid::new_v4().as_u128().to_string();
    format!("+1{}", &digits[..10])
}

/// Full trunk lifecycle: create → list (present) → update → delete → list (gone).
#[tokio::test]
async fn sip_trunk_crud_lifecycle() {
    let Some(client) = common::client_or_skip() else {
        return;
    };

    let name = common::unique_id("rust-it-sip-trunk");
    let number = unique_number();

    let created = client
        .video()
        .create_sip_trunk(CreateSipTrunkRequest {
            password: Some("s3cr3t-pass".to_owned()),
            ..CreateSipTrunkRequest::new(&name, [number.clone()])
        })
        .await
        .expect("create_sip_trunk failed");
    let trunk_id = created
        .sip_trunk
        .expect("create response missing sip_trunk")
        .id;
    assert!(!trunk_id.is_empty(), "created trunk has empty id");

    let outcome = tokio::time::timeout(
        TEST_TIMEOUT,
        exercise_trunk(&client, &trunk_id, &name, &number),
    )
    .await;

    // Best-effort cleanup regardless of how the assertions above resolved.
    let _ = client.video().delete_sip_trunk(&trunk_id).await;

    outcome
        .expect("sip trunk lifecycle timed out")
        .expect("sip trunk lifecycle assertions failed");
}

async fn exercise_trunk(client: &Stream, trunk_id: &str, name: &str, number: &str) -> Result<()> {
    let video = client.video();

    let listed = video.list_sip_trunks().await.context("list_sip_trunks")?;
    let found = listed
        .sip_trunks
        .iter()
        .find(|t| t.id == trunk_id)
        .context("created trunk not present in list")?;
    ensure!(found.name == name, "listed trunk name mismatch");
    ensure!(
        found.numbers.iter().any(|n| n == number),
        "listed trunk missing its number"
    );

    let updated_name = format!("{name}-updated");
    let updated = video
        .update_sip_trunk(
            trunk_id,
            UpdateSipTrunkRequest {
                name: updated_name.clone(),
                numbers: vec![number.to_owned()],
                ..Default::default()
            },
        )
        .await
        .context("update_sip_trunk")?;
    ensure!(
        updated
            .sip_trunk
            .map(|t| t.name == updated_name)
            .unwrap_or(false),
        "update did not return the new trunk name"
    );

    video
        .delete_sip_trunk(trunk_id)
        .await
        .context("delete_sip_trunk")?;

    let after = video
        .list_sip_trunks()
        .await
        .context("list_sip_trunks after delete")?;
    ensure!(
        after.sip_trunks.iter().all(|t| t.id != trunk_id),
        "deleted trunk still present in list"
    );

    Ok(())
}

/// Full routing-rule lifecycle against a real trunk, cleaning up both resources.
#[tokio::test]
async fn sip_inbound_routing_rule_crud_lifecycle() {
    let Some(client) = common::client_or_skip() else {
        return;
    };

    let trunk_name = common::unique_id("rust-it-sip-rule-trunk");
    let created = client
        .video()
        .create_sip_trunk(CreateSipTrunkRequest::new(&trunk_name, [unique_number()]))
        .await
        .expect("create_sip_trunk failed");
    let trunk_id = created
        .sip_trunk
        .expect("create response missing sip_trunk")
        .id;

    let outcome =
        tokio::time::timeout(TEST_TIMEOUT, exercise_routing_rule(&client, &trunk_id)).await;

    let _ = client.video().delete_sip_trunk(&trunk_id).await;

    outcome
        .expect("sip routing rule lifecycle timed out")
        .expect("sip routing rule lifecycle assertions failed");
}

async fn exercise_routing_rule(client: &Stream, trunk_id: &str) -> Result<()> {
    let video = client.video();
    let rule_name = common::unique_id("rust-it-sip-rule");

    let created = video
        .create_sip_inbound_routing_rule(CreateSipInboundRoutingRuleRequest {
            name: rule_name.clone(),
            trunk_ids: vec![trunk_id.to_owned()],
            caller_configs: SipCallerConfigsRequest {
                id: "{{caller_number}}".to_owned(),
                custom_data: None,
            },
            direct_routing_configs: Some(SipDirectRoutingRuleCallConfigsRequest {
                call_id: "{{caller_number}}".to_owned(),
                call_type: "default".to_owned(),
            }),
            ..Default::default()
        })
        .await
        .context("create_sip_inbound_routing_rule")?;
    let rule_id = created.id;
    ensure!(!rule_id.is_empty(), "created rule has empty id");

    let listed = video
        .list_sip_inbound_routing_rules()
        .await
        .context("list_sip_inbound_routing_rules")?;
    let found = listed
        .sip_inbound_routing_rules
        .iter()
        .find(|r| r.id == rule_id)
        .context("created rule not present in list")?;
    ensure!(
        found.trunk_ids.iter().any(|id| id == trunk_id),
        "listed rule missing its trunk id"
    );

    video
        .delete_sip_inbound_routing_rule(&rule_id)
        .await
        .context("delete_sip_inbound_routing_rule")?;

    let after = video
        .list_sip_inbound_routing_rules()
        .await
        .context("list after delete")?;
    ensure!(
        after
            .sip_inbound_routing_rules
            .iter()
            .all(|r| r.id != rule_id),
        "deleted rule still present in list"
    );

    Ok(())
}
