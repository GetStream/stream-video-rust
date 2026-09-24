//! SFU connection setup, callbacks, event dispatch, and health checks.

use super::*;

/// Build the SFU signaling WebSocket URL from `ws_endpoint`, appending the
/// informational query params JS attaches (`attempt`, `user_id`, `api_key`,
/// `user_session_id`, `cid`). Ported from JS `StreamSfuClient.createWebSocket`.
pub(super) fn build_sfu_ws_url(
    ws_endpoint: &str,
    api_key: &str,
    user_id: &str,
    session_id: &str,
    cid: &str,
    attempt: u32,
) -> Result<String> {
    let mut url =
        Url::parse(ws_endpoint).map_err(|e| RtcError::Url(format!("{ws_endpoint:?}: {e}")))?;
    url.query_pairs_mut()
        .append_pair("attempt", &attempt.to_string())
        .append_pair("user_id", user_id)
        .append_pair("api_key", api_key)
        .append_pair("user_session_id", session_id)
        .append_pair("cid", cid);
    Ok(url.into())
}

/// Await the `JoinResponse` (or an SFU `Error`) under `timeout`.
pub(super) async fn await_join_response(
    receiver: &mut SfuReceiver,
    timeout: Duration,
) -> Result<JoinResponse> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => {
                return Err(RtcError::Timeout(SfuTimeoutError::new("join response", timeout)));
            }
            event = receiver.recv() => {
                match event? {
                    Some(SfuEvent { event_payload: Some(payload) }) => match payload {
                        sfu_event::EventPayload::JoinResponse(resp) => return Ok(resp),
                        sfu_event::EventPayload::Error(err) => {
                            return Err(RtcError::Join(SfuJoinError::from_event(
                                err.error,
                                err.reconnect_strategy,
                            )));
                        }
                        // Ignore any other event that arrives before the JoinResponse.
                        _ => continue,
                    },
                    Some(_) => continue,
                    None => {
                        return Err(RtcError::WsConnection(
                            super::super::error::WsConnectionError::transport(
                                "socket closed before join response",
                            ),
                        ));
                    }
                }
            }
        }
    }
}

/// Register the subscriber `on_track` handler, delivering each inbound track to
/// the core's correlation + `on_track` callback path. Also traces `ontrack`
/// (`<kind>:<track_id> [stream:<id>]`) to match JS.
pub(super) fn register_on_track(
    pc: &Arc<RTCPeerConnection>,
    core: Arc<RtcCore>,
    generation: u64,
    connection_epoch: u64,
    reconnect_enabled: Arc<AtomicBool>,
    tracer: Arc<Tracer>,
) {
    // Weak: the handler is owned by the PeerConnection, so a strong reference
    // here would be a cycle that leaks the connection.
    let weak_pc = Arc::downgrade(pc);
    pc.on_track(Box::new(move |track, _receiver, _transceiver| {
        let core = core.clone();
        let tracer = tracer.clone();
        let weak_pc = weak_pc.clone();
        let reconnect_enabled = reconnect_enabled.clone();
        Box::pin(async move {
            if !core.is_generation_current(generation) || !reconnect_enabled.load(Ordering::SeqCst)
            {
                return;
            }
            tracer.trace(
                "ontrack",
                json!(format!(
                    "{}:{} stream:{}",
                    track.kind(),
                    track.id(),
                    track.stream_id()
                )),
            );
            core.handle_incoming_track(
                generation,
                connection_epoch,
                reconnect_enabled,
                track,
                weak_pc,
            )
            .await;
        })
    }));
}

/// Track PeerConnection health and trigger a reconnect on a failed transport.
/// Also traces `connectionstatechange` (JS) into the connection's tracer.
pub(super) fn register_connection_state(
    pc: &Arc<RTCPeerConnection>,
    core: Arc<RtcCore>,
    generation: u64,
    reconnect_enabled: Arc<AtomicBool>,
    label: &'static str,
    tracer: Arc<Tracer>,
) {
    let ever_connected = Arc::new(AtomicBool::new(false));
    pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
        let core = core.clone();
        let tracer = tracer.clone();
        let reconnect_enabled = reconnect_enabled.clone();
        let ever_connected = ever_connected.clone();
        Box::pin(async move {
            tracing::debug!(label, ?state, "stream.rtc.pc.state");
            tracer.trace("connectionstatechange", json!(state.to_string()));
            if state == RTCPeerConnectionState::Connected {
                ever_connected.store(true, Ordering::SeqCst);
                core.failure_limits
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .reset_ice();
            }
            if state == RTCPeerConnectionState::Failed && reconnect_enabled.load(Ordering::SeqCst) {
                let (pub_h, sub_h) = core.pc_health().await;
                core.trigger_reconnect(
                    generation,
                    strategy_after_signal_close(pub_h, sub_h),
                    if ever_connected.load(Ordering::SeqCst) {
                        format!("{label} pc failed")
                    } else {
                        reconnect::REASON_ICE_UNSUPPORTED.to_owned()
                    },
                );
            }
        })
    }));
}

/// The SFU WebSocket event loop: negotiate subscriber offers, add remote ICE
/// candidates, and fan out typed [`CallEvent`]s.
pub(super) async fn event_loop(mut receiver: SfuReceiver, context: EventLoopContext) {
    loop {
        let event = match receiver.recv().await {
            Ok(Some(event)) => event,
            Ok(None) => {
                context.ws_healthy.store(false, Ordering::SeqCst);
                // Socket closed. Reconnect unless we're intentionally leaving.
                context
                    .signal
                    .trace("signal.close", json!("sfu signal ws closed"));
                if context.core.is_generation_current(context.generation)
                    && context.reconnect_enabled.load(Ordering::SeqCst)
                {
                    let (pub_h, sub_h) = context.core.pc_health().await;
                    context.core.trigger_reconnect(
                        context.generation,
                        strategy_after_signal_close(pub_h, sub_h),
                        "sfu signal ws closed".to_owned(),
                    );
                }
                return;
            }
            Err(e) => {
                context.ws_healthy.store(false, Ordering::SeqCst);
                tracing::debug!(error = %e, "stream.rtc.ws.recv_error");
                context.signal.trace("signal.close", json!(e.to_string()));
                if context.core.is_generation_current(context.generation)
                    && context.reconnect_enabled.load(Ordering::SeqCst)
                {
                    context.core.trigger_reconnect(
                        context.generation,
                        ReconnectStrategy::Rejoin,
                        e.to_string(),
                    );
                }
                return;
            }
        };

        let Some(payload) = event.event_payload else {
            continue;
        };
        if let Err(e) = handle_event(&context, payload).await {
            tracing::warn!(error = %e, "stream.rtc.event.control_failed");
            context.ws_healthy.store(false, Ordering::SeqCst);
            if context.core.is_generation_current(context.generation)
                && context.reconnect_enabled.load(Ordering::SeqCst)
            {
                context.core.trigger_reconnect(
                    context.generation,
                    ReconnectStrategy::Rejoin,
                    format!("SFU control operation failed: {e}"),
                );
            }
            return;
        }
    }
}

pub(super) async fn handle_event(
    context: &EventLoopContext,
    payload: sfu_event::EventPayload,
) -> Result<()> {
    let core = &context.core;
    if !core.is_generation_current(context.generation) {
        return Ok(());
    }
    use sfu_event::EventPayload as E;
    // During a migration the old SFU still sends events. Its WebRTC commands
    // are not for the new connection.
    if !context.reconnect_enabled.load(Ordering::SeqCst)
        && matches!(
            payload,
            E::SubscriberOffer(_)
                | E::IceTrickle(_)
                | E::ChangePublishOptions(_)
                | E::ChangePublishQuality(_)
                | E::IceRestart(_)
        )
    {
        return Ok(());
    }
    match payload {
        E::SubscriberOffer(offer) => {
            negotiate_subscriber(
                &context.subscriber,
                &context.signal,
                &context.session_id,
                offer,
                &context.pending_ice,
            )
            .await?;
        }
        E::IceTrickle(trickle) => {
            context
                .pending_ice
                .add_remote(&context.subscriber, &context.publisher, trickle)
                .await?;
        }
        E::ConnectionQualityChanged(event) => {
            core.update_connection_quality(&event.connection_quality_updates);
            let _ = core.events_tx.send(CallEvent::ConnectionQualityChanged(
                event.connection_quality_updates,
            ));
        }
        E::ParticipantJoined(ev) => {
            if let Some(p) = ev.participant {
                core.upsert_participant(&p);
                core.recompute_subscriptions_for_generation(context.generation)
                    .await?;
                let _ = core.events_tx.send(CallEvent::ParticipantJoined(p));
            }
        }
        E::ParticipantLeft(ev) => {
            if let Some(p) = ev.participant {
                core.remove_participant(&p.session_id);
                core.recompute_subscriptions_for_generation(context.generation)
                    .await?;
                let _ = core.events_tx.send(CallEvent::ParticipantLeft(p));
            }
        }
        E::ParticipantUpdated(ev) => {
            if let Some(p) = ev.participant {
                core.upsert_participant(&p);
                core.recompute_subscriptions_for_generation(context.generation)
                    .await?;
                let _ = core.events_tx.send(CallEvent::ParticipantUpdated(p));
            }
        }
        E::TrackPublished(ev) => {
            core.add_published_track(
                &ev.user_id,
                &ev.session_id,
                ev.r#type,
                ev.participant.as_ref(),
            );
            core.recompute_subscriptions_for_generation(context.generation)
                .await?;
            let _ = core.events_tx.send(CallEvent::TrackPublished {
                user_id: ev.user_id,
                session_id: ev.session_id,
                track_type: ev.r#type,
            });
        }
        E::TrackUnpublished(ev) => {
            core.remove_published_track(&ev.session_id, ev.r#type);
            core.recompute_subscriptions_for_generation(context.generation)
                .await?;
            let _ = core.events_tx.send(CallEvent::TrackUnpublished {
                user_id: ev.user_id,
                session_id: ev.session_id,
                track_type: ev.r#type,
            });
        }
        E::DominantSpeakerChanged(ev) => {
            core.update_dominant_speaker(&ev.session_id);
            let _ = core.events_tx.send(CallEvent::DominantSpeakerChanged {
                user_id: ev.user_id,
                session_id: ev.session_id,
            });
        }
        E::AudioLevelChanged(ev) => {
            core.update_audio_levels(&ev.audio_levels);
            let _ = core
                .events_tx
                .send(CallEvent::AudioLevelChanged(ev.audio_levels));
        }
        E::HealthCheckResponse(event) => {
            if let Some(participant_count) = event.participant_count {
                core.update_participant_count(participant_count);
                let _ = core
                    .events_tx
                    .send(CallEvent::ParticipantCountChanged(participant_count));
            }
        }
        E::PinsUpdated(event) => {
            core.update_pins(event.pins.clone());
            let _ = core.events_tx.send(CallEvent::PinsUpdated(event.pins));
        }
        E::InboundStateNotification(event) => {
            core.update_inbound_state(&event.inbound_video_states);
            let _ = core
                .events_tx
                .send(CallEvent::InboundStateChanged(event.inbound_video_states));
        }
        E::ChangePublishOptions(event) => {
            core.apply_publish_options(context.generation, event.publish_options.clone())
                .await?;
            let _ = core.events_tx.send(CallEvent::PublishOptionsChanged {
                publish_options: event.publish_options,
                reason: event.reason,
            });
        }
        E::ChangePublishQuality(event) => {
            core.apply_publish_quality(&event).await;
            let _ = core.events_tx.send(CallEvent::PublishQualityChanged(event));
        }
        E::CallGrantsUpdated(event) => {
            core.update_call_grants(event.current_grants);
            if let Some(grants) = &event.current_grants {
                for track_type in [
                    TrackType::Audio,
                    TrackType::Video,
                    TrackType::ScreenShare,
                    TrackType::ScreenShareAudio,
                ] {
                    if !grants_allow(grants, track_type) {
                        core.set_track_muted(track_type, true).await?;
                    }
                }
            }
            let _ = core.events_tx.send(CallEvent::CallGrantsUpdated(event));
        }
        E::IceRestart(event) => {
            let peer_type =
                PeerType::try_from(event.peer_type).unwrap_or(PeerType::PublisherUnspecified);
            if peer_type == PeerType::PublisherUnspecified {
                core.restart_publisher_ice().await?;
            }
            let _ = core.events_tx.send(CallEvent::IceRestarted(peer_type));
        }
        E::Error(err) => {
            let join_err = SfuJoinError::from_event(err.error, err.reconnect_strategy);
            let strategy = ReconnectStrategy::from_proto(err.reconnect_strategy);
            let _ = core.events_tx.send(CallEvent::Error(join_err.clone()));
            if let Some(strategy) = strategy {
                core.trigger_reconnect(context.generation, strategy, join_err.message.clone());
            }
        }
        E::GoAway(_) => {
            core.trigger_reconnect(
                context.generation,
                ReconnectStrategy::Migrate,
                "go away".to_owned(),
            );
        }
        E::ParticipantMigrationComplete(_) => {
            core.complete_migration(context.generation);
        }
        E::CallEnded(_) => {
            let _ = core.events_tx.send(CallEvent::CallEnded);
        }
        E::PublisherAnswer(_) | E::JoinResponse(_) => {
            tracing::debug!("stream.rtc.unexpected_handshake_event");
        }
    }
    Ok(())
}

/// Health-check ping loop (JS 5s cadence) keeping the SFU session alive.
pub(super) async fn ping_loop(
    core: Arc<RtcCore>,
    sender: Arc<TokioMutex<SfuSender>>,
    generation: u64,
    ws_healthy: Arc<AtomicBool>,
    reconnect_enabled: Arc<AtomicBool>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let mut sender = sender.lock().await;
        if let Err(error) = sender.send_health_check().await {
            ws_healthy.store(false, Ordering::SeqCst);
            drop(sender);
            if core.is_generation_current(generation) && reconnect_enabled.load(Ordering::SeqCst) {
                core.trigger_reconnect(
                    generation,
                    ReconnectStrategy::Rejoin,
                    format!("health-check send failed: {error}"),
                );
            }
            return;
        }
    }
}
