//! Join, leave, token refresh, and coordinator-event lifecycle.

use super::*;

impl RtcCore {
    /// Run the initial join for-loop. On success the connection is stored and
    /// the state becomes `Joined`.
    pub async fn join(self: &Arc<Self>, user_token: String, data: JoinCallData) -> Result<()> {
        self.join_with_token_source(UserTokenSource::Static(user_token), data)
            .await
    }

    pub(crate) async fn join_with_token_source(
        self: &Arc<Self>,
        token_source: UserTokenSource,
        data: JoinCallData,
    ) -> Result<()> {
        ensure_crypto_provider();
        let generation = self.begin_join()?;
        // A fresh unified session id for this join lifecycle; reused across
        // reconnects so the dashboard correlates the participant end-to-end.
        {
            let mut uid = self
                .unified_session_id
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *uid = uuid::Uuid::new_v4().to_string();
        }
        // Store the join data + token source for reconnects.
        {
            let mut jd = self.join_data.lock().unwrap_or_else(|e| e.into_inner());
            *jd = data.clone();
        }
        {
            let mut source = self.token_source.lock().unwrap_or_else(|e| e.into_inner());
            *source = Some(token_source);
        }
        let user_token = match self
            .while_generation(generation, self.reload_user_token())
            .await
            .and_then(|result| result)
        {
            Ok(token) => token,
            Err(error) => {
                self.set_state_if_current(generation, CallingState::Idle);
                return Err(error);
            }
        };
        if let Err(error) = self
            .while_generation(
                generation,
                self.connect_coordinator_events(generation, &user_token, &data.user_id),
            )
            .await
            .and_then(|result| result)
        {
            self.stop_coordinator_events().await;
            self.set_state_if_current(generation, CallingState::Idle);
            return Err(error);
        }
        let this = self.clone();
        let result = self
            .while_generation(generation, this.join_loop(user_token, data, generation))
            .await
            .and_then(|result| result);
        if result.is_err() {
            self.stop_coordinator_events().await;
            // Restore to a non-joining terminal state so a retry is allowed.
            if self.state() == CallingState::Joining {
                self.set_state_if_current(generation, CallingState::Idle);
            }
        }
        result
    }

    pub(super) async fn reload_user_token(&self) -> Result<String> {
        let _refresh = self.token_refresh.lock().await;
        let source = self
            .token_source
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| RtcError::IllegalState("user token source is not set".to_owned()))?;
        let user_id = self
            .join_data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .user_id
            .clone();
        let token = source.load_with_expiry_retry(&user_id).await?;
        *self.user_token.lock().unwrap_or_else(|e| e.into_inner()) = token.clone();
        Ok(token)
    }

    pub(super) fn current_user_token(&self) -> Result<String> {
        let token = self
            .user_token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let user_id = self
            .join_data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .user_id
            .clone();
        crate::token::validate_operational_token(&token, &user_id).map_err(RtcError::from)?;
        Ok(token)
    }

    pub(super) async fn refresh_expired_user_token(&self) -> Result<String> {
        let can_refresh = self
            .token_source
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(UserTokenSource::can_refresh);
        if !can_refresh {
            return Err(RtcError::TokenValidation(
                crate::error::TokenError::ExpiredByServer,
            ));
        }
        self.reload_user_token().await
    }

    pub(super) async fn refresh_before_full_reconnect(&self) -> Result<String> {
        let refresh = self
            .token_source
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(UserTokenSource::refreshes_before_full_reconnect);
        if refresh {
            self.reload_user_token().await
        } else {
            match self.current_user_token() {
                Ok(token) => Ok(token),
                Err(error) if error.is_token_expired() => self.refresh_expired_user_token().await,
                Err(error) => Err(error),
            }
        }
    }

    pub(super) async fn join_loop(
        self: Arc<Self>,
        mut user_token: String,
        data: JoinCallData,
        generation: u64,
    ) -> Result<()> {
        let max_retries = data.max_join_retries.max(1);
        let location = match &data.location {
            Some(loc) => loc.clone(),
            None => {
                self.while_generation(
                    generation,
                    coordinator::discover_location(self.client.http()),
                )
                .await?
            }
        };

        let mut migrating_from: Option<String> = None;
        let mut edge_failures: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        let mut last_err: Option<RtcError> = None;
        let mut expired_retry_used = false;

        for attempt in 0..max_retries {
            let request = JoinCallRequest {
                location: location.clone(),
                create: Some(data.create),
                data: data.data.clone(),
                ring: data.ring.then_some(true),
                notify: data.notify.then_some(true),
                video: data.video.then_some(true),
                migrating_from: migrating_from.clone(),
                migrating_from_list: {
                    let bad = self
                        .confirmed_bad_sfus
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    bad.clone()
                },
                ..Default::default()
            };

            let attempt_result = loop {
                let result = self
                    .clone()
                    .join_once(JoinOnceOptions {
                        user_token: &user_token,
                        request: &request,
                        attempt,
                        strategy: ReconnectStrategy::Fast,
                        reconnect_details: None,
                        generation,
                        session_id: None,
                        retain_old: false,
                    })
                    .await;
                let is_expired = result
                    .as_ref()
                    .err()
                    .is_some_and(|(error, _)| error.is_token_expired());
                if is_expired && !expired_retry_used {
                    user_token = self.refresh_expired_user_token().await?;
                    expired_retry_used = true;
                    continue;
                }
                break result;
            };

            match attempt_result {
                Ok(success) => {
                    if !self.is_generation_current(generation) {
                        return Err(join_cancelled());
                    }
                    tracing::info!(cid = %self.cid(), edge = %success.edge_name, "stream.rtc.joined");
                    *self.started.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                    if !self.set_state_if_current(generation, CallingState::Joined) {
                        return Err(join_cancelled());
                    }
                    return Ok(());
                }
                Err((err, edge_name)) => {
                    let unrecoverable = err.is_unrecoverable();
                    let is_join_code = err.is_join_error_code();
                    let failures = if let Some(edge) = &edge_name {
                        let count = edge_failures.entry(edge.clone()).or_insert(0);
                        *count += 1;
                        *count
                    } else {
                        1
                    };

                    let outcome = reconnect::evaluate_join_failure(
                        unrecoverable,
                        is_join_code,
                        failures,
                        attempt,
                        max_retries,
                    );
                    tracing::warn!(
                        cid = %self.cid(),
                        attempt,
                        edge = edge_name.as_deref().unwrap_or("<none>"),
                        error = %err,
                        ?outcome,
                        "stream.rtc.join.attempt_failed"
                    );

                    match outcome {
                        reconnect::JoinAttemptOutcome::Abort => return Err(err),
                        reconnect::JoinAttemptOutcome::Exhausted => {
                            last_err = Some(err);
                            break;
                        }
                        reconnect::JoinAttemptOutcome::Retry { delay, switch_sfu } => {
                            if switch_sfu && let Some(edge) = edge_name {
                                migrating_from = Some(edge.clone());
                                let mut bad = self
                                    .confirmed_bad_sfus
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner());
                                if !bad.contains(&edge) {
                                    bad.push(edge);
                                }
                            }
                            last_err = Some(err);
                            self.while_generation(generation, tokio::time::sleep(delay))
                                .await?;
                        }
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            RtcError::Coordinator("join failed with no recorded error".to_owned())
        }))
    }

    /// One join attempt: coordinator join → SFU WS handshake → JoinResponse.
    /// Returns the edge name on success, or `(error, edge_name)` on failure so
    /// the loop can attribute per-edge failures.
    pub(super) async fn join_once(
        self: Arc<Self>,
        options: JoinOnceOptions<'_>,
    ) -> std::result::Result<JoinSuccess, (RtcError, Option<String>)> {
        let JoinOnceOptions {
            user_token,
            request,
            attempt,
            strategy,
            reconnect_details,
            generation,
            session_id,
            retain_old,
        } = options;
        if !self.is_generation_current(generation) {
            return Err((join_cancelled(), None));
        }
        // 1. Coordinator join (user token) → SFU credentials.
        let query = self.user_request_query().ok_or_else(|| {
            (
                RtcError::IllegalState("coordinator connection is not available".to_owned()),
                None,
            )
        })?;
        let join = coordinator::join_call(
            &self.client,
            user_token,
            &self.call_type,
            &self.call_id,
            request,
            &query,
        )
        .await
        .map_err(|e| (e, None))?;

        let credentials = join.credentials;
        let own_capabilities = join.own_capabilities;
        let edge_name = credentials.server.edge_name.clone();
        {
            let mut so = self.stats_options.lock().unwrap_or_else(|e| e.into_inner());
            *so = join.stats_options.clone();
        }

        if credentials.server.ws_endpoint.is_empty() {
            return Err((RtcError::missing_credential("ws_endpoint"), Some(edge_name)));
        }

        let attach = self
            .clone()
            .establish(
                &credentials,
                attempt,
                strategy,
                reconnect_details,
                generation,
                session_id,
            )
            .await;

        match attach {
            Ok(connection) => {
                let mut guard = self.connection.lock().await;
                if !self.is_generation_current(generation) {
                    drop(guard);
                    connection.teardown().await;
                    return Err((join_cancelled(), Some(edge_name)));
                }
                // Swap in the new connection, tearing down any old one.
                let old = guard.replace(connection);
                drop(guard);
                let retained_connection = if retain_old {
                    if let Some(old) = &old {
                        old.reconnect_enabled.store(false, Ordering::SeqCst);
                    }
                    old
                } else {
                    if let Some(old) = old {
                        old.teardown().await;
                    }
                    None
                };
                // The fresh SFU has no subscriptions yet; forget what we last
                // sent so `restore_subscribed_tracks` re-sends them.
                self.active_subs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clear();
                *self
                    .own_capabilities
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = own_capabilities.into_iter().collect();
                Ok(JoinSuccess {
                    edge_name,
                    retained_connection,
                })
            }
            Err(e) => Err((e, Some(edge_name))),
        }
    }

    /// Establish the SFU WebSocket + PeerConnections and complete the join
    /// handshake, returning the live [`Connection`].
    pub(super) async fn establish(
        self: Arc<Self>,
        credentials: &Credentials,
        attempt: u32,
        strategy: ReconnectStrategy,
        reconnect_details: Option<ReconnectDetails>,
        generation: u64,
        session_id: Option<String>,
    ) -> Result<Connection> {
        if !self.is_generation_current(generation) {
            return Err(join_cancelled());
        }
        let data = self
            .join_data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let session_id = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let connection_epoch = self
            .next_connection_epoch
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);

        // Per-source telemetry tracers. `id` distinguishes the connection in
        // the `rtc_stats` records (`publisher` / `subscriber`); the signal
        // tracer is call-level (`None`), matching JS.
        let signal_tracer = Arc::new(Tracer::new(None));
        let publisher_tracer = Arc::new(Tracer::new(Some("publisher".to_owned())));
        let subscriber_tracer = Arc::new(Tracer::new(Some("subscriber".to_owned())));

        // PeerConnections wired with the credential ICE servers.
        let subscriber = peer::new_peer_connection(&credentials.ice_servers).await?;
        let publisher = peer::new_peer_connection(&credentials.ice_servers).await?;

        // Trace PC lifecycle (signaling / ICE gathering / ICE connection /
        // negotiation / data channel) — the `onicecandidate`, `ontrack`, and
        // `connectionstatechange` tags come from the dedicated handlers below.
        peer::trace_peer_events(&subscriber, subscriber_tracer.clone());
        peer::trace_peer_events(&publisher, publisher_tracer.clone());

        // Throwaway generic SDPs so the SFU can learn our codecs.
        let subscriber_sdp =
            peer::generic_sdp(webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection::Recvonly)
                .await?;
        let publisher_sdp =
            peer::generic_sdp(webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection::Sendonly)
                .await?;

        // Signal client (Twirp) for SendAnswer / IceTrickle, tracing each RPC
        // into the call-level tracer for the `rtc_stats` rollup.
        let signal = SignalClient::with_http(
            self.client.http().clone(),
            &credentials.server.url,
            credentials.token.clone(),
        )?
        .with_timeout(data.rpc_request_timeout)
        .with_max_response_body_bytes(self.client.max_response_body_bytes())
        .with_tracer(signal_tracer.clone());

        // Open the SFU WebSocket with the JS-style query string.
        let ws_url = build_sfu_ws_url(
            &credentials.server.ws_endpoint,
            &self.api_key,
            &data.user_id,
            &session_id,
            &self.cid(),
            attempt,
        )?;
        let (mut sender, mut receiver) =
            match sfu_ws::connect_with_limit(&ws_url, self.client.max_websocket_message_bytes())
                .await
            {
                Ok(pair) => pair,
                Err(e) => {
                    signal.trace("signal.close", json!(e.to_string()));
                    return Err(RtcError::WsConnection(
                        super::super::error::WsConnectionError::transport(e.to_string()),
                    ));
                }
            };
        signal.trace("signal.ws.open", json!(credentials.server.edge_name));

        // Build + send the JoinRequest. `fast_reconnect` is deprecated upstream;
        // the FAST intent travels in `reconnect_details.strategy` instead.
        let _ = strategy;
        let join_request = JoinRequest {
            token: credentials.token.clone(),
            session_id: session_id.clone(),
            unified_session_id: self.unified_session_id(),
            subscriber_sdp,
            publisher_sdp,
            client_details: Some(identity::client_details()),
            reconnect_details,
            preferred_publish_options: self.preferred_publish_options(generation)?,
            ..Default::default()
        };
        // Trace the join without the SFU token (never log secrets).
        signal.trace(
            "joinRequest",
            json!({ "session_id": session_id, "edge": credentials.server.edge_name }),
        );
        sender.send_join(join_request).await?;

        // Await the JoinResponse (or an Error) under the client deadline.
        let join_response = await_join_response(&mut receiver, data.join_response_timeout).await?;
        signal.trace(
            "joinResponse",
            json!({
                "session_id": session_id,
                "fast_reconnect_deadline_seconds": join_response.fast_reconnect_deadline_seconds,
            }),
        );
        tracing::debug!(
            publish_options = ?join_response.publish_options.iter().map(|o| (
                o.track_type,
                o.id,
                o.codec.as_ref().map(|c| (c.name.clone(), c.payload_type, c.clock_rate, c.fmtp.clone())),
                o.max_spatial_layers,
            )).collect::<Vec<_>>(),
            "stream.rtc.join.publish_options"
        );
        let publish_options = join_response.publish_options.clone();
        let fast_reconnect_deadline =
            Duration::from_secs(join_response.fast_reconnect_deadline_seconds.max(0) as u64);

        // Deliver inbound tracks: the subscriber PC fires `on_track` once a
        // subscription lands and RTP arrives. Registered before the event loop
        // so no `SubscriberOffer` is answered before the handler is in place.
        if !self.apply_join_call_state_if_current(
            generation,
            &session_id,
            &data.user_id,
            join_response.call_state,
        ) {
            let _ = subscriber.close().await;
            let _ = publisher.close().await;
            return Err(join_cancelled());
        }
        let reconnect_enabled = Arc::new(AtomicBool::new(true));
        register_on_track(
            &subscriber,
            self.clone(),
            generation,
            connection_epoch,
            reconnect_enabled.clone(),
            subscriber_tracer.clone(),
        );

        // Wire ICE trickle out to the SFU for both peers (also traces
        // `onicecandidate`).
        register_ice_trickle(
            &subscriber,
            signal.clone(),
            session_id.clone(),
            PeerType::Subscriber,
            subscriber_tracer.clone(),
        );
        register_ice_trickle(
            &publisher,
            signal.clone(),
            session_id.clone(),
            PeerType::PublisherUnspecified,
            publisher_tracer.clone(),
        );

        // Track PC health for reconnect decisions (also traces
        // `connectionstatechange`).
        let ws_healthy = Arc::new(AtomicBool::new(true));
        register_connection_state(
            &subscriber,
            self.clone(),
            generation,
            reconnect_enabled.clone(),
            "subscriber",
            subscriber_tracer.clone(),
        );
        register_connection_state(
            &publisher,
            self.clone(),
            generation,
            reconnect_enabled.clone(),
            "publisher",
            publisher_tracer.clone(),
        );

        let sfu_sender = Arc::new(TokioMutex::new(sender));
        let pending_ice = Arc::new(PendingIce::default());

        // Background telemetry reporter (periodic `SendStats`), cadence driven
        // by the cached coordinator `stats_options` (default ON if unset).
        let stats = Arc::new(StatsReporter::new(StatsReporterParts {
            signal: signal.clone(),
            publisher: publisher.clone(),
            subscriber: subscriber.clone(),
            signal_tracer,
            publisher_tracer,
            subscriber_tracer,
            session_id: session_id.clone(),
            unified_session_id: self.unified_session_id(),
            interval: stats::reporting_interval(&self.stats_options()),
        }));

        // Spawn the WS event loop + health-check ping loop + stats loop.
        let event_loop = self.spawn_runtime_task(event_loop(
            receiver,
            EventLoopContext {
                core: self.clone(),
                subscriber: subscriber.clone(),
                publisher: publisher.clone(),
                signal: signal.clone(),
                session_id: session_id.clone(),
                pending_ice: pending_ice.clone(),
                generation,
                ws_healthy: ws_healthy.clone(),
                reconnect_enabled: reconnect_enabled.clone(),
            },
        ));
        let ping_loop = self.spawn_runtime_task(ping_loop(
            self.clone(),
            sfu_sender.clone(),
            generation,
            ws_healthy.clone(),
            reconnect_enabled.clone(),
        ));
        let stats_loop = self.spawn_runtime_task(stats::run(stats.clone()));

        Ok(Connection {
            generation,
            epoch: connection_epoch,
            session_id,
            subscriber,
            publisher,
            signal,
            sfu_sender,
            credentials: credentials.clone(),
            fast_reconnect_deadline,
            pending_ice,
            publish_options,
            stats,
            ws_healthy,
            reconnect_enabled,
            signal_tasks: vec![event_loop, ping_loop],
            publisher_tasks: Vec::new(),
            stats_task: stats_loop,
        })
    }
}

impl RtcCore {
    /// Leave the call: send `leave`, close the PeerConnections and WebSocket,
    /// abort background tasks. Succeeds from any state, including `Joining`
    /// (JS: force to a leaving state rather than waiting for `JOINED`).
    pub async fn leave(&self, reason: impl Into<String>) -> Result<()> {
        self.leave_inner(reason.into()).await
    }

    pub(super) async fn leave_inner(&self, reason: String) -> Result<()> {
        let generation = self.cancel_generation();

        let connection = self.connection.lock().await.take();
        if let Some(connection) = connection {
            let session_id = connection.session_id.clone();
            // Record the leave reason so the final `SendStats` (drained by
            // `teardown`) carries the end-of-call event (JS `call.leaveReason`).
            connection.signal.trace("call.leaveReason", json!(reason));
            {
                let mut sender = connection.sfu_sender.lock().await;
                let _ = sender.send_leave(session_id, &reason).await;
                let _ = sender.close().await;
            }
            connection.teardown().await;
        }
        self.roster
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *self
            .call_state
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = CallStateCache::default();
        self.active_subs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.own_capabilities
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *self
            .reconnect_generation
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        self.set_state_if_current(generation, CallingState::Left);
        self.stop_coordinator_events().await;
        Ok(())
    }
}

impl RtcCore {
    pub(super) async fn connect_coordinator_events(
        self: &Arc<Self>,
        generation: u64,
        user_token: &str,
        user_id: &str,
    ) -> Result<()> {
        if !self.is_generation_current(generation) {
            return Err(join_cancelled());
        }
        let auth = WsAuthMessage::video(user_token, ConnectUserDetails::new(user_id));
        let coordinator_url = coordinator_ws::coordinator_ws_url(self.client.base_url())?;
        let (mut coordinator, mut events, connected) = coordinator_ws::connect_with_limit(
            coordinator_url.as_str(),
            &self.api_key,
            user_id,
            &auth,
            self.client.max_websocket_message_bytes(),
        )
        .await?;
        if !self.is_generation_current(generation) {
            return Err(join_cancelled());
        }
        if connected.connection_id.is_empty() {
            return Err(RtcError::Coordinator(
                "coordinator did not provide a connection id".to_owned(),
            ));
        }
        *self
            .coordinator_connection_id
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some((generation, connected.connection_id));

        let cid = self.cid();
        let local_user_id = user_id.to_owned();
        let sender = self.events_tx.clone();
        let event_core = self.clone();
        let event_task = self.spawn_runtime_task(async move {
            loop {
                if !event_core.is_generation_current(generation) {
                    break;
                }
                match events.recv().await {
                    Ok(Some(event))
                        if event.raw.get("call_cid").and_then(|value| value.as_str())
                            == Some(cid.as_str()) =>
                    {
                        event_core
                            .apply_permissions_updated(&event, &local_user_id)
                            .await;
                        let _ = sender.send(CallEvent::Coordinator(event));
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        event_core.clear_coordinator_connection(generation);
                        event_core.trigger_reconnect(
                            generation,
                            ReconnectStrategy::Rejoin,
                            "coordinator event stream closed".to_owned(),
                        );
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "stream.rtc.coordinator_events_closed");
                        event_core.clear_coordinator_connection(generation);
                        event_core.trigger_reconnect(
                            generation,
                            ReconnectStrategy::Rejoin,
                            "coordinator event stream closed".to_owned(),
                        );
                        break;
                    }
                }
            }
        });
        let health_core = self.clone();
        let health_task = self.spawn_runtime_task(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(20));
            interval.tick().await;
            loop {
                interval.tick().await;
                if !health_core.is_generation_current(generation) {
                    break;
                }
                if let Err(error) = coordinator.send_health_check().await {
                    tracing::warn!(%error, "stream.rtc.coordinator_health_failed");
                    health_core.clear_coordinator_connection(generation);
                    health_core.trigger_reconnect(
                        generation,
                        ReconnectStrategy::Rejoin,
                        "coordinator health check failed".to_owned(),
                    );
                    break;
                }
            }
        });
        if !self.is_generation_current(generation) {
            event_task.abort();
            health_task.abort();
            self.clear_coordinator_connection(generation);
            return Err(join_cancelled());
        }
        self.coordinator_tasks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .extend([event_task, health_task]);
        Ok(())
    }

    pub(super) async fn apply_permissions_updated(
        self: &Arc<Self>,
        event: &CoordinatorEvent,
        local_user_id: &str,
    ) {
        let Some(capabilities) = own_capabilities_from_event(event, local_user_id) else {
            return;
        };
        *self
            .own_capabilities
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = capabilities.clone();

        let revoked_tracks = self
            .media
            .lock()
            .await
            .active_tracks()
            .into_iter()
            .filter(|track| !capabilities.contains(required_publish_capability(track.track_type())))
            .collect::<Vec<_>>();
        for track in revoked_tracks {
            let track_type = track.track_type();
            if let Err(error) = self.stop_publish(track).await {
                tracing::warn!(
                    %error,
                    ?track_type,
                    "stream.rtc.permission_revocation_stop_failed"
                );
            }
        }
    }

    pub(super) fn clear_coordinator_connection(&self, generation: u64) {
        let mut connection = self
            .coordinator_connection_id
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if connection
            .as_ref()
            .is_some_and(|(connection_generation, _)| *connection_generation == generation)
        {
            *connection = None;
        }
    }

    pub(super) async fn ensure_coordinator_events(
        self: &Arc<Self>,
        generation: u64,
        user_token: &str,
    ) -> Result<()> {
        if self.user_request_query().is_some() {
            return Ok(());
        }
        let user_id = self
            .join_data
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .user_id
            .clone();
        self.stop_coordinator_tasks().await;
        self.connect_coordinator_events(generation, user_token, &user_id)
            .await
    }

    pub(super) async fn stop_coordinator_tasks(&self) {
        let tasks = self
            .coordinator_tasks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain(..)
            .collect();
        abort_tasks(tasks).await;
    }

    pub(super) async fn stop_coordinator_events(&self) {
        *self
            .coordinator_connection_id
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        self.user_token
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.stop_coordinator_tasks().await;
    }
}
