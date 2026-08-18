//! Reconnect execution and media restoration for an active call.

use super::*;

impl RtcCore {
    /// Re-publish every local track on the fresh publisher PC after a
    /// REJOIN/MIGRATE. The tracks are the same `Arc`-backed handles, so the pacer
    /// keeps running and simply binds to the new PeerConnection.
    pub(super) async fn restore_published_tracks(&self) -> Result<()> {
        let mut media = self.media.lock().await;
        let capabilities = self
            .own_capabilities
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        media.publications.retain(|publication| {
            let allowed =
                capabilities.contains(required_publish_capability(publication.track.track_type()));
            if !allowed {
                publication.track.stop();
            }
            allowed
        });
        if media.publications.is_empty() {
            return Ok(());
        }
        let tracks = media.active_tracks();
        let Some((publisher, signal, session_id, publish_options)) = self.publisher_handles().await
        else {
            return Err(RtcError::IllegalState(
                "restore published tracks without a connection".to_owned(),
            ));
        };
        media.refresh_publish_options(&publish_options)?;
        if !tracks.is_empty() {
            let mut publisher_rtcp_tasks = Vec::new();
            for track in &tracks {
                match publisher::add_transceiver_for_track(&publisher, track, &publish_options)
                    .await
                {
                    Ok(tasks) => publisher_rtcp_tasks.extend(tasks),
                    Err(error) => {
                        abort_tasks(publisher_rtcp_tasks).await;
                        return Err(error);
                    }
                }
            }
            media.reapply_cached_publish_quality();
            if let Err(error) = publisher::negotiate_publish(
                &publisher,
                &signal,
                &session_id,
                &tracks,
                &publish_options,
            )
            .await
            {
                abort_tasks(publisher_rtcp_tasks).await;
                return Err(error);
            }
            self.register_publisher_tasks(publisher_rtcp_tasks).await;
            self.flush_publisher_ice().await;
            self.sync_published_state(&signal, &session_id, &tracks)
                .await?;
            for publication in &mut media.publications {
                if matches!(
                    publication.status,
                    PublicationStatus::PendingPublishNegotiation
                        | PublicationStatus::PendingPublishMute
                        | PublicationStatus::Published
                ) {
                    publication.status = PublicationStatus::Published;
                }
            }
        }
        self.finish_pending_stops(&mut media, &signal, &session_id)
            .await?;
        tracing::info!(count = tracks.len(), "stream.rtc.restore_published_tracks");
        Ok(())
    }

    pub(super) async fn sync_published_state(
        &self,
        signal: &SignalClient,
        session_id: &str,
        tracks: &[LocalTrack],
    ) -> Result<()> {
        signal
            .update_mute_states(signal::UpdateMuteStatesRequest {
                session_id: session_id.to_owned(),
                mute_states: current_mute_states_for_tracks(tracks),
            })
            .await?;
        let user_id = self
            .join_data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .user_id
            .clone();
        for track in tracks {
            if track.is_muted() {
                self.roster_remove_track(session_id, track.track_type() as i32);
            } else {
                self.roster_add_track(&user_id, session_id, track.track_type() as i32, None);
            }
            track.start_media();
        }
        Ok(())
    }

    pub(super) async fn finish_pending_stops(
        &self,
        media: &mut MediaState,
        signal: &SignalClient,
        session_id: &str,
    ) -> Result<()> {
        let pending: Vec<(String, TrackType)> = media
            .publications
            .iter()
            .filter(|publication| matches!(publication.status, PublicationStatus::PendingStopMute))
            .map(|publication| (publication.track.track_id(), publication.track.track_type()))
            .collect();
        let mut synced_types = HashSet::new();
        for (_, track_type) in &pending {
            if !synced_types.insert(*track_type as i32) {
                continue;
            }
            let muted = !media
                .active_tracks()
                .iter()
                .any(|track| track.track_type() == *track_type);
            signal
                .update_mute_states(signal::UpdateMuteStatesRequest {
                    session_id: session_id.to_owned(),
                    mute_states: vec![signal::TrackMuteState {
                        track_type: *track_type as i32,
                        muted,
                    }],
                })
                .await?;
            if muted {
                self.roster_remove_track(session_id, *track_type as i32);
            }
        }
        for (track_id, _) in pending {
            if let Some(track) = media.remove(&track_id) {
                track.stop();
            }
        }
        Ok(())
    }

    /// Re-send `UpdateSubscriptions` for the desired tracks after a
    /// REJOIN/MIGRATE (the connection swap cleared the last-sent set).
    pub(super) async fn restore_subscribed_tracks(&self) -> Result<()> {
        self.recompute_subscriptions().await
    }

    /// Trigger a reconnect if one is not already running and we are not leaving.
    /// Dedups so `goAway`/dispatcher errors never start a parallel reconnect
    /// (JS `withoutConcurrency`).
    pub(super) fn trigger_reconnect(
        self: &Arc<Self>,
        generation: u64,
        strategy: ReconnectStrategy,
        reason: String,
    ) {
        if !self.is_generation_current(generation) {
            return;
        }
        if strategy == ReconnectStrategy::Disconnect {
            let this = self.clone();
            std::mem::drop(self.spawn_runtime_task(async move {
                if this.is_generation_current(generation) {
                    let _ = this.leave("SFU instructed to disconnect").await;
                }
            }));
            return;
        }
        // Do not start a reconnect while a join for-loop is running or another
        // reconnect is in flight.
        if matches!(
            self.state(),
            CallingState::Joining
                | CallingState::Reconnecting
                | CallingState::Migrating
                | CallingState::ReconnectingFailed
        ) {
            return;
        }
        if !self.claim_reconnect(generation) {
            return;
        }
        let this = self.clone();
        let claim = ReconnectClaim {
            core: this.clone(),
            generation,
        };
        let task = self.spawn_runtime_task(async move {
            let _claim = claim;
            this.clone()
                .run_reconnect(generation, strategy, reason)
                .await;
        });
        std::mem::drop(task);
    }

    /// The reconnect state machine loop (JS `Call.reconnect`). Honors the rejoin
    /// rate limiter, ICE / negotiation caps, the disconnection timeout, and the
    /// restore hooks. Bounded: it stops when `JOINED`, `RECONNECTING_FAILED`, or
    /// `LEFT`, so it can never spin.
    pub(super) async fn run_reconnect(
        self: Arc<Self>,
        generation: u64,
        mut strategy: ReconnectStrategy,
        reason: String,
    ) {
        let disconnection_timeout = *self
            .disconnection_timeout
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let start = Instant::now();
        let mut attempt = 0;
        let mut was_migrating = strategy == ReconnectStrategy::Migrate;
        self.reconnect_edge_failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.confirmed_bad_sfus
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();

        self.set_state_if_current(
            generation,
            if was_migrating {
                CallingState::Migrating
            } else {
                CallingState::Reconnecting
            },
        );

        if reason == reconnect::REASON_ICE_UNSUPPORTED {
            let tripped = self
                .caps
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record_ice_never_connected();
            if tripped {
                let _ = self.leave(reconnect::REASON_ICE_UNSUPPORTED).await;
                return;
            }
        }

        loop {
            if !self.is_generation_current(generation) {
                return;
            }

            // Disconnection timeout: stop reconnecting and mark failed.
            if reconnect::disconnection_timed_out(start.elapsed(), disconnection_timeout) {
                // Refresh call data (JS `get()` after RECONNECTING_FAILED).
                match self
                    .while_generation(generation, self.refresh_call_data())
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "stream.rtc.reconnect.refresh_call_failed");
                    }
                    Err(_) => return,
                }
                self.set_state_if_current(generation, CallingState::ReconnectingFailed);
                return;
            }

            // Rate limit only REJOIN/MIGRATE.
            if strategy.is_rate_limited() {
                let now_ms = elapsed_ms();
                let allowed = self
                    .rate_limiter
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .try_register(now_ms);
                if !allowed {
                    let _ = self.leave(reconnect::REASON_REJOIN_LIMIT).await;
                    return;
                }
                self.reconnect_attempts.fetch_add(1, Ordering::SeqCst);
            }

            let outcome = match self
                .while_generation(
                    generation,
                    self.clone().reconnect_once(generation, strategy, &reason),
                )
                .await
            {
                Ok(outcome) => outcome,
                Err(_) => return,
            };
            match outcome {
                Ok(()) => {
                    self.caps
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .reset_negotiation();
                    self.set_state_if_current(generation, CallingState::Joined);
                    return;
                }
                Err(err) => {
                    if err.is_unrecoverable() {
                        match self
                            .while_generation(generation, self.refresh_call_data())
                            .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => {
                                tracing::warn!(%error, "stream.rtc.reconnect.refresh_call_failed");
                            }
                            Err(_) => return,
                        }
                        self.set_state_if_current(generation, CallingState::ReconnectingFailed);
                        return;
                    }
                    if matches!(err, RtcError::Negotiation(_)) {
                        let tripped = self
                            .caps
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .record_negotiation_failure();
                        if tripped {
                            let _ = self.leave(reconnect::REASON_NEGOTIATION_FAILURES).await;
                            return;
                        }
                    }
                    tracing::warn!(cid = %self.cid(), attempt, error = %err, "stream.rtc.reconnect.attempt_failed");
                    if self
                        .while_generation(
                            generation,
                            tokio::time::sleep(reconnect::retry_interval(attempt)),
                        )
                        .await
                        .is_err()
                    {
                        return;
                    }

                    // Escalate FAST → REJOIN per JS.
                    let (pub_healthy, sub_healthy) = self.pc_health().await;
                    strategy = escalate_strategy(
                        start.elapsed(),
                        self.fast_reconnect_deadline().await,
                        was_migrating,
                        attempt,
                        pub_healthy,
                        sub_healthy,
                    );
                    attempt = attempt.saturating_add(1);
                    was_migrating = strategy == ReconnectStrategy::Migrate;
                    self.set_state_if_current(
                        generation,
                        if was_migrating {
                            CallingState::Migrating
                        } else {
                            CallingState::Reconnecting
                        },
                    );
                }
            }
        }
    }

    /// Execute a single reconnect attempt for `strategy`.
    pub(super) async fn reconnect_once(
        self: Arc<Self>,
        generation: u64,
        strategy: ReconnectStrategy,
        reason: &str,
    ) -> Result<()> {
        self.observe_reconnect(strategy, ReconnectFaultPoint::BeforeAttempt)?;
        match strategy {
            ReconnectStrategy::Fast => self.reconnect_fast(generation, reason).await,
            ReconnectStrategy::Rejoin => self.reconnect_rejoin(generation, reason).await,
            ReconnectStrategy::Migrate => self.reconnect_migrate(generation, reason).await,
            ReconnectStrategy::Disconnect => Err(RtcError::IllegalState(
                "disconnect strategy must leave the call".to_owned(),
            )),
        }
    }

    pub(super) async fn reconnect_fast(
        self: Arc<Self>,
        generation: u64,
        reason: &str,
    ) -> Result<()> {
        let ws_healthy = self
            .connection
            .lock()
            .await
            .as_ref()
            .is_some_and(|connection| connection.ws_healthy.load(Ordering::SeqCst));
        if !ws_healthy {
            self.reopen_fast_signal(generation, reason).await?;
        }

        let mut media = self.media.lock().await;
        let tracks = media.active_tracks();
        let Some((publisher, signal, session_id, publish_options)) = self.publisher_handles().await
        else {
            return Err(RtcError::IllegalState(
                "fast reconnect without a connection".to_owned(),
            ));
        };
        if !tracks.is_empty() {
            publisher::restart_ice(&publisher, &signal, &session_id, &tracks, &publish_options)
                .await?;
            self.flush_publisher_ice().await;
            self.sync_published_state(&signal, &session_id, &tracks)
                .await?;
            for publication in &mut media.publications {
                if matches!(
                    publication.status,
                    PublicationStatus::PendingPublishNegotiation
                        | PublicationStatus::PendingPublishMute
                        | PublicationStatus::Published
                ) {
                    publication.status = PublicationStatus::Published;
                }
            }
        }
        self.finish_pending_stops(&mut media, &signal, &session_id)
            .await?;
        drop(media);
        self.observe_reconnect(
            ReconnectStrategy::Fast,
            ReconnectFaultPoint::AfterPublishedRestore,
        )?;
        self.restore_subscribed_tracks().await?;
        self.observe_reconnect(
            ReconnectStrategy::Fast,
            ReconnectFaultPoint::AfterSubscribedRestore,
        )?;
        self.refresh_call_data().await
    }

    pub(super) async fn reconnect_rejoin(
        self: Arc<Self>,
        generation: u64,
        reason: &str,
    ) -> Result<()> {
        let data = self
            .join_data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let (previous_session_id, from_sfu_id) = {
            let guard = self.connection.lock().await;
            match guard.as_ref() {
                Some(connection) => (
                    connection.session_id.clone(),
                    connection.credentials.server.edge_name.clone(),
                ),
                None => {
                    return Err(RtcError::IllegalState(
                        "rejoin without a connection".to_owned(),
                    ));
                }
            }
        };

        let confirmed_bad_sfus = self
            .confirmed_bad_sfus
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let request = JoinCallRequest {
            location: data
                .location
                .clone()
                .unwrap_or_else(|| coordinator::FALLBACK_LOCATION.to_owned()),
            create: Some(data.create),
            migrating_from: confirmed_bad_sfus.last().cloned(),
            migrating_from_list: confirmed_bad_sfus,
            ..Default::default()
        };

        let reconnect_details = Some(
            self.reconnect_details(
                ReconnectStrategy::Rejoin,
                reason,
                from_sfu_id,
                previous_session_id,
            )
            .await?,
        );

        let mut token = self.refresh_before_full_reconnect().await?;
        self.ensure_coordinator_events(generation, &token).await?;
        let reconnect_attempt = self.reconnect_attempts.load(Ordering::SeqCst);
        let mut result = self
            .clone()
            .join_once(JoinOnceOptions {
                user_token: &token,
                request: &request,
                attempt: reconnect_attempt,
                strategy: ReconnectStrategy::Rejoin,
                reconnect_details: reconnect_details.clone(),
                generation,
                session_id: None,
                retain_old: false,
            })
            .await;
        if result
            .as_ref()
            .err()
            .is_some_and(|(error, _)| error.is_token_expired())
        {
            token = self.refresh_expired_user_token().await?;
            result = self
                .clone()
                .join_once(JoinOnceOptions {
                    user_token: &token,
                    request: &request,
                    attempt: reconnect_attempt,
                    strategy: ReconnectStrategy::Rejoin,
                    reconnect_details,
                    generation,
                    session_id: None,
                    retain_old: false,
                })
                .await;
        }
        match result {
            Ok(_) => {}
            Err((error, edge)) => {
                if let Some(edge) = edge {
                    self.record_reconnect_edge_failure(&edge, error.is_join_error_code());
                }
                return Err(error);
            }
        }
        self.restore_published_tracks().await?;
        self.observe_reconnect(
            ReconnectStrategy::Rejoin,
            ReconnectFaultPoint::AfterPublishedRestore,
        )?;
        self.restore_subscribed_tracks().await?;
        self.observe_reconnect(
            ReconnectStrategy::Rejoin,
            ReconnectFaultPoint::AfterSubscribedRestore,
        )
    }

    pub(super) fn record_reconnect_edge_failure(&self, edge: &str, force_switch: bool) {
        let failures = {
            let mut counts = self
                .reconnect_edge_failures
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let count = counts.entry(edge.to_owned()).or_insert(0);
            *count = count.saturating_add(1);
            if force_switch {
                *count = (*count).max(2);
            }
            *count
        };
        if failures >= 2 {
            let mut bad = self
                .confirmed_bad_sfus
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !bad.iter().any(|known| known == edge) {
                bad.push(edge.to_owned());
            }
        }
    }

    pub(super) async fn reconnect_migrate(
        self: Arc<Self>,
        generation: u64,
        reason: &str,
    ) -> Result<()> {
        let data = self
            .join_data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut token = self.refresh_before_full_reconnect().await?;
        self.ensure_coordinator_events(generation, &token).await?;
        let (previous_session_id, migrating_from, old_reconnect_enabled) = {
            let guard = self.connection.lock().await;
            let connection = guard.as_ref().ok_or_else(|| {
                RtcError::IllegalState("cannot migrate without a connection".to_owned())
            })?;
            (
                connection.session_id.clone(),
                connection.credentials.server.edge_name.clone(),
                connection.reconnect_enabled.clone(),
            )
        };
        old_reconnect_enabled.store(false, Ordering::SeqCst);

        let (migration_tx, migration_rx) = tokio::sync::oneshot::channel();
        if let Err(error) = self.install_migration_waiter(generation, migration_tx) {
            old_reconnect_enabled.store(true, Ordering::SeqCst);
            return Err(error);
        }

        let request = JoinCallRequest {
            location: data
                .location
                .clone()
                .unwrap_or_else(|| coordinator::FALLBACK_LOCATION.to_owned()),
            create: Some(data.create),
            migrating_from: Some(migrating_from.clone()),
            migrating_from_list: vec![migrating_from.clone()],
            ..Default::default()
        };
        let reconnect_details = Some(
            self.reconnect_details(
                ReconnectStrategy::Migrate,
                reason,
                migrating_from,
                String::new(),
            )
            .await?,
        );
        let reconnect_attempt = self.reconnect_attempts.load(Ordering::SeqCst);
        let mut result = self
            .clone()
            .join_once(JoinOnceOptions {
                user_token: &token,
                request: &request,
                attempt: reconnect_attempt,
                strategy: ReconnectStrategy::Migrate,
                reconnect_details: reconnect_details.clone(),
                generation,
                session_id: Some(previous_session_id.clone()),
                retain_old: true,
            })
            .await;
        if result
            .as_ref()
            .err()
            .is_some_and(|(error, _)| error.is_token_expired())
        {
            token = match self.refresh_expired_user_token().await {
                Ok(token) => token,
                Err(error) => {
                    drop(self.take_migration_waiter(generation));
                    old_reconnect_enabled.store(true, Ordering::SeqCst);
                    return Err(error);
                }
            };
            result = self
                .clone()
                .join_once(JoinOnceOptions {
                    user_token: &token,
                    request: &request,
                    attempt: reconnect_attempt,
                    strategy: ReconnectStrategy::Migrate,
                    reconnect_details,
                    generation,
                    session_id: Some(previous_session_id),
                    retain_old: true,
                })
                .await;
        }
        let success = match result {
            Ok(success) => success,
            Err((error, _edge)) => {
                drop(self.take_migration_waiter(generation));
                old_reconnect_enabled.store(true, Ordering::SeqCst);
                return Err(error);
            }
        };

        let restore_result = async {
            self.restore_published_tracks().await?;
            self.observe_reconnect(
                ReconnectStrategy::Migrate,
                ReconnectFaultPoint::AfterPublishedRestore,
            )?;
            self.restore_subscribed_tracks().await?;
            self.observe_reconnect(
                ReconnectStrategy::Migrate,
                ReconnectFaultPoint::AfterSubscribedRestore,
            )?;
            self.while_generation(
                generation,
                tokio::time::timeout(MIGRATION_COMPLETE_TIMEOUT, migration_rx),
            )
            .await?
            .map_err(|_| {
                RtcError::Timeout(SfuTimeoutError::new(
                    "participant migration complete",
                    MIGRATION_COMPLETE_TIMEOUT,
                ))
            })?
            .map_err(|_| RtcError::Closed("migration waiter closed".to_owned()))
        }
        .await;

        drop(self.take_migration_waiter(generation));
        if let Some(old) = success.retained_connection {
            old.teardown().await;
        }
        restore_result
    }

    pub(super) async fn reconnect_details(
        &self,
        strategy: ReconnectStrategy,
        reason: &str,
        from_sfu_id: String,
        previous_session_id: String,
    ) -> Result<ReconnectDetails> {
        let announced_tracks = {
            let media = self.media.lock().await;
            let tracks = media.active_tracks();
            let publisher = self.connection.lock().await.as_ref().map(|connection| {
                (
                    connection.publisher.clone(),
                    connection.publish_options.clone(),
                )
            });
            match publisher {
                Some((publisher, publish_options)) => {
                    publisher::build_track_infos(&publisher, &tracks, &publish_options).await?
                }
                None => Vec::new(),
            }
        };
        let subscriptions = self
            .active_subs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Ok(ReconnectDetails {
            strategy: strategy.as_proto(),
            announced_tracks,
            subscriptions,
            reconnect_attempt: self.reconnect_attempts.load(Ordering::SeqCst),
            from_sfu_id,
            previous_session_id,
            reason: reason.to_owned(),
        })
    }

    pub(super) async fn reopen_fast_signal(
        self: &Arc<Self>,
        generation: u64,
        reason: &str,
    ) -> Result<()> {
        let data = self
            .join_data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let (
            credentials,
            session_id,
            subscriber,
            publisher,
            signal,
            pending_ice,
            sfu_sender,
            ws_healthy,
            reconnect_enabled,
        ) = {
            let guard = self.connection.lock().await;
            let connection = guard.as_ref().ok_or_else(|| {
                RtcError::IllegalState("fast reconnect without a connection".to_owned())
            })?;
            (
                connection.credentials.clone(),
                connection.session_id.clone(),
                connection.subscriber.clone(),
                connection.publisher.clone(),
                connection.signal.clone(),
                connection.pending_ice.clone(),
                connection.sfu_sender.clone(),
                connection.ws_healthy.clone(),
                connection.reconnect_enabled.clone(),
            )
        };
        let reconnect_details = self
            .reconnect_details(
                ReconnectStrategy::Fast,
                reason,
                credentials.server.edge_name.clone(),
                String::new(),
            )
            .await?;
        let subscriber_sdp = peer::generic_sdp(RTCRtpTransceiverDirection::Recvonly).await?;
        let publisher_sdp = peer::generic_sdp(RTCRtpTransceiverDirection::Sendonly).await?;
        let ws_url = build_sfu_ws_url(
            &credentials.server.ws_endpoint,
            &self.api_key,
            &data.user_id,
            &session_id,
            &self.cid(),
            self.reconnect_attempts.load(Ordering::SeqCst),
        )?;
        let (mut next_sender, mut receiver) =
            sfu_ws::connect_with_limit(&ws_url, self.client.max_websocket_message_bytes()).await?;
        let mut join_request = JoinRequest {
            token: credentials.token,
            session_id: session_id.clone(),
            unified_session_id: self.unified_session_id(),
            subscriber_sdp,
            publisher_sdp,
            client_details: Some(identity::client_details()),
            reconnect_details: Some(reconnect_details),
            preferred_publish_options: self.preferred_publish_options(generation)?,
            ..Default::default()
        };
        mark_fast_reconnect(&mut join_request);
        next_sender.send_join(join_request).await?;
        let join_response = await_join_response(&mut receiver, data.join_response_timeout).await?;
        if !join_response.reconnected {
            return Err(RtcError::Coordinator(
                "SFU did not retain fast-reconnect state".to_owned(),
            ));
        }
        if !self.apply_join_call_state_if_current(
            generation,
            &session_id,
            &data.user_id,
            join_response.call_state,
        ) {
            let _ = next_sender.close().await;
            return Err(join_cancelled());
        }

        let old_tasks = {
            let mut guard = self.connection.lock().await;
            let connection = guard.as_mut().ok_or_else(|| {
                RtcError::IllegalState("fast reconnect connection disappeared".to_owned())
            })?;
            if connection.generation != generation
                || connection.session_id != session_id
                || !self.is_generation_current(generation)
            {
                return Err(join_cancelled());
            }
            connection.signal_tasks.drain(..).collect()
        };
        abort_tasks(old_tasks).await;
        {
            let mut sender = sfu_sender.lock().await;
            *sender = next_sender;
        }
        ws_healthy.store(true, Ordering::SeqCst);
        reconnect_enabled.store(true, Ordering::SeqCst);
        let event_task = self.spawn_runtime_task(event_loop(
            receiver,
            EventLoopContext {
                core: self.clone(),
                subscriber,
                publisher,
                signal,
                session_id: session_id.clone(),
                pending_ice,
                generation,
                ws_healthy: ws_healthy.clone(),
                reconnect_enabled: reconnect_enabled.clone(),
            },
        ));
        let ping_task = self.spawn_runtime_task(ping_loop(
            self.clone(),
            sfu_sender,
            generation,
            ws_healthy,
            reconnect_enabled,
        ));
        let mut guard = self.connection.lock().await;
        let connection = guard.as_mut().ok_or_else(|| {
            RtcError::IllegalState("fast reconnect connection disappeared".to_owned())
        })?;
        if connection.generation != generation
            || connection.session_id != session_id
            || !self.is_generation_current(generation)
        {
            event_task.abort();
            ping_task.abort();
            return Err(join_cancelled());
        }
        connection.fast_reconnect_deadline =
            Duration::from_secs(join_response.fast_reconnect_deadline_seconds.max(0) as u64);
        connection.publish_options = join_response.publish_options;
        connection.signal_tasks.push(event_task);
        connection.signal_tasks.push(ping_task);
        Ok(())
    }

    pub(super) async fn refresh_call_data(&self) -> Result<()> {
        // JS calls call.get() after RECONNECTING_FAILED to refresh state.
        let path = Client::build_path(
            "/api/v2/video/call/{type}/{id}",
            &[("type", &self.call_type), ("id", &self.call_id)],
        );
        let token = self.current_user_token()?;
        let _: serde_json::Value = self
            .client
            .request_as_user(reqwest::Method::GET, &path, &[], None::<&()>, &token)
            .await
            .map_err(RtcError::from)?;
        Ok(())
    }

    pub(super) async fn pc_health(&self) -> (bool, bool) {
        let generation = self.generation();
        let guard = self.connection.lock().await;
        match guard.as_ref() {
            Some(conn) if conn.generation == generation => (
                is_pc_healthy(conn.publisher.connection_state()),
                is_pc_healthy(conn.subscriber.connection_state()),
            ),
            Some(_) | None => (false, false),
        }
    }

    pub(super) async fn fast_reconnect_deadline(&self) -> Duration {
        let generation = self.generation();
        self.connection
            .lock()
            .await
            .as_ref()
            .filter(|connection| connection.generation == generation)
            .map(|connection| connection.fast_reconnect_deadline)
            .unwrap_or_default()
    }

    /// Fire a single live Twirp `IceTrickle` against the connected SFU. Used by
    /// the integration test to validate the HTTP Twirp body format + auth header
    /// (which a media-less join never exercises). A transport / `Twirp` / decode
    /// error means the SFU rejected our framing; `Ok(())` or an application-level
    /// `Signal` error both mean the framing was accepted.
    #[cfg(test)]
    pub(crate) async fn twirp_probe(&self) -> Result<()> {
        let guard = self.connection.lock().await;
        let conn = guard
            .as_ref()
            .ok_or_else(|| RtcError::IllegalState("twirp_probe: not connected".to_owned()))?;
        let candidate = r#"{"candidate":"candidate:1 1 udp 2130706431 127.0.0.1 9 typ host","sdpMid":"0","sdpMLineIndex":0}"#.to_owned();
        conn.signal
            .ice_trickle(models::IceTrickle {
                peer_type: PeerType::Subscriber as i32,
                ice_candidate: candidate,
                session_id: conn.session_id.clone(),
            })
            .await
            .map(|_| ())
    }

    /// Snapshot the live stats reporter's observations (live telemetry test):
    /// the successful-send count, the last SFU error, and the distinct
    /// `rtc_stats` tags sent so far. `None` if not connected.
    #[cfg(test)]
    pub(crate) async fn stats_observations(&self) -> Option<super::stats::TestObservations> {
        self.connection
            .lock()
            .await
            .as_ref()
            .map(|c| c.stats.observations())
    }

    /// Deterministically drive one `SendStats` on the live connection (live
    /// telemetry test), independent of the periodic cadence. Returns the RPC
    /// result so the test can surface any SFU error verbatim; `None` if not
    /// connected. The connection lock is released before the network round-trip.
    #[cfg(test)]
    pub(crate) async fn force_stats_report(&self) -> Option<Result<()>> {
        let reporter = self
            .connection
            .lock()
            .await
            .as_ref()
            .map(|c| c.stats.clone())?;
        Some(reporter.report_once().await)
    }
}
