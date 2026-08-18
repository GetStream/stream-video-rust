//! The publish path: local tracks, publisher negotiation, mute state, and
//! publish quality. Subscriptions live in [`super::subscriptions_runtime`].

use super::*;

impl RtcCore {
    /// Publish a local track: add its send-only transceiver and renegotiate the
    /// publisher PC with the SFU (`SetPublisher`). Errors if not joined.
    pub async fn publish(self: &Arc<Self>, track: LocalTrack) -> Result<()> {
        let Some((publisher, signal, session_id, publish_options)) = self.publisher_handles().await
        else {
            return Err(RtcError::IllegalState("publish() before join()".to_owned()));
        };
        let capability = required_publish_capability(track.track_type());
        if !self
            .own_capabilities
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(capability)
        {
            return Err(RtcError::PermissionDenied { capability });
        }
        let mut media = self.media.lock().await;
        let track_id = track.track_id();
        let status = media
            .position(&track_id)
            .map(|position| media.publications[position].status);
        match status {
            Some(PublicationStatus::Published) => return Ok(()),
            Some(PublicationStatus::PendingStopMute) => {
                return Err(RtcError::IllegalState(format!(
                    "publish() while stop is pending for track {track_id}"
                )));
            }
            Some(
                PublicationStatus::PendingPublishNegotiation
                | PublicationStatus::PendingPublishMute,
            ) => {}
            None => {}
        }
        publisher::validate_publish_codecs(std::slice::from_ref(&track), &publish_options)?;
        let publish_option_id = if let Some(position) = media.position(&track_id) {
            media.publications[position].publish_option_id
        } else {
            let used_publish_options = media
                .publications
                .iter()
                .map(|publication| {
                    (
                        publication.publish_option_id,
                        publication.track.track_type() as i32,
                    )
                })
                .collect::<HashSet<_>>();
            publisher::assign_publish_option(&track, &publish_options, &used_publish_options)?
        };
        let mut publisher_rtcp_tasks = Vec::new();
        if status.is_none() {
            publisher_rtcp_tasks =
                match publisher::add_transceiver_for_track(&publisher, &track, &publish_options)
                    .await
                {
                    Ok(tasks) => tasks,
                    Err(error) => {
                        if let Err(rollback_error) =
                            remove_sender_for_track(&publisher, &track_id).await
                        {
                            return Err(media_rollback_error("publish", error, rollback_error));
                        }
                        return Err(error);
                    }
                };
            media.begin_publish(track.clone(), publish_option_id);
            if let Some(layers) = media
                .publish_quality
                .get(&(publish_option_id, track.track_type() as i32))
            {
                track.apply_video_layer_settings(layers);
            }
        }

        let current_status = media
            .position(&track_id)
            .map(|position| media.publications[position].status)
            .ok_or_else(|| {
                RtcError::IllegalState("publish transaction lost its track state".to_owned())
            })?;
        if current_status == PublicationStatus::PendingPublishNegotiation {
            let desired = media.active_tracks();
            if let Err(error) = publisher::negotiate_publish(
                &publisher,
                &signal,
                &session_id,
                &desired,
                &publish_options,
            )
            .await
            {
                abort_tasks(publisher_rtcp_tasks).await;
                let rollback = remove_sender_for_track(&publisher, &track_id).await;
                if let Err(rollback_error) = rollback {
                    return Err(media_rollback_error("publish", error, rollback_error));
                }
                media.remove(&track_id);
                return Err(error);
            }
            self.register_publisher_tasks(publisher_rtcp_tasks).await;
            media.set_status(&track_id, PublicationStatus::PendingPublishMute);
            self.flush_publisher_ice().await;
        }

        let user_id = self
            .join_data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .user_id
            .clone();
        self.roster_add_track(&user_id, &session_id, track.track_type() as i32, None);
        track.start_media();
        signal
            .update_mute_states(signal::UpdateMuteStatesRequest {
                session_id: session_id.clone(),
                mute_states: vec![signal::TrackMuteState {
                    track_type: track.track_type() as i32,
                    muted: false,
                }],
            })
            .await?;
        media.set_status(&track_id, PublicationStatus::Published);
        tracing::info!(cid = %self.cid(), "stream.rtc.published");
        Ok(())
    }

    pub(super) async fn register_publisher_tasks(&self, tasks: Vec<JoinHandle<()>>) {
        if tasks.is_empty() {
            return;
        }
        let mut connection = self.connection.lock().await;
        if let Some(connection) = connection.as_mut() {
            connection.publisher_tasks.extend(tasks);
        } else {
            drop(connection);
            abort_tasks(tasks).await;
        }
    }

    /// Stop publishing `track`: stop producing its media and signal mute to the
    /// SFU via `UpdateMuteStates`, matching JS `Call.stopPublish` (which stops the
    /// sender's track and notifies mute state — it never removes the sender).
    ///
    /// The publisher transceiver is deliberately left in place. The SFU rejects a
    /// `SetPublisher` whose track envelope no longer matches the one it first saw
    /// (`Invalid SetPublisher request; ... new track must have the same envelope as
    /// previous`), so removing the sole sender and renegotiating an empty envelope
    /// fails on the wire; muting the track type is the wire-correct way to stop.
    pub async fn stop_publish(self: &Arc<Self>, track: LocalTrack) -> Result<()> {
        let mut media = self.media.lock().await;
        let track_id = track.track_id();
        if media.position(&track_id).is_none() {
            return Ok(());
        }
        let Some((_publisher, signal, session_id, _publish_options)) =
            self.publisher_handles().await
        else {
            if let Some(removed) = media.remove(&track_id) {
                removed.stop();
            }
            return Ok(());
        };

        // Mark the stop in progress and stop producing media so RTP ceases before
        // we tell the SFU the track is muted. This state is retryable until the
        // mute RPC commits (see `finish_pending_stops` for the reconnect path).
        media.set_status(&track_id, PublicationStatus::PendingStopMute);
        track.stop();

        let track_type = track.track_type();
        let muted = !media
            .active_tracks()
            .iter()
            .any(|published| published.track_type() == track_type);
        signal
            .update_mute_states(signal::UpdateMuteStatesRequest {
                session_id: session_id.clone(),
                mute_states: vec![signal::TrackMuteState {
                    track_type: track_type as i32,
                    muted,
                }],
            })
            .await?;
        if muted {
            self.roster_remove_track(&session_id, track_type as i32);
        }
        if let Some(removed) = media.remove(&track_id) {
            removed.stop();
        }
        Ok(())
    }

    /// Temporarily mute or unmute every active publication of `track_type`.
    ///
    /// Unlike [`Self::stop_publish`], this keeps the local track and negotiated
    /// sender alive, so unmuting does not require a replacement track or a new
    /// publisher negotiation.
    pub async fn set_track_muted(
        self: &Arc<Self>,
        track_type: TrackType,
        muted: bool,
    ) -> Result<()> {
        if !muted {
            let capability = required_publish_capability(track_type);
            if !self
                .own_capabilities
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains(capability)
            {
                return Err(RtcError::PermissionDenied { capability });
            }
            let grants = self
                .call_state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .current_grants;
            if grants
                .as_ref()
                .is_some_and(|grants| !grants_allow(grants, track_type))
            {
                return Err(RtcError::PermissionDenied { capability });
            }
        }

        let tracks = self
            .media
            .lock()
            .await
            .active_tracks()
            .into_iter()
            .filter(|track| track.track_type() == track_type)
            .collect::<Vec<_>>();
        if tracks.is_empty() || tracks.iter().all(|track| track.is_muted() == muted) {
            return Ok(());
        }
        let Some((_publisher, signal, session_id, _publish_options)) =
            self.publisher_handles().await
        else {
            return Err(RtcError::IllegalState(
                "set_track_muted() before join()".to_owned(),
            ));
        };
        for track in &tracks {
            track.set_muted(muted);
        }
        if let Err(error) = signal
            .update_mute_states(signal::UpdateMuteStatesRequest {
                session_id: session_id.clone(),
                mute_states: vec![signal::TrackMuteState {
                    track_type: track_type as i32,
                    muted,
                }],
            })
            .await
        {
            for track in &tracks {
                track.set_muted(!muted);
            }
            return Err(error);
        }
        if muted {
            self.roster_remove_track(&session_id, track_type as i32);
        } else {
            let user_id = self
                .join_data
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .user_id
                .clone();
            self.roster_add_track(&user_id, &session_id, track_type as i32, None);
        }
        Ok(())
    }

    /// Enable SFU-side noise cancellation for this participant session.
    pub async fn start_noise_cancellation(&self) -> Result<()> {
        let Some((_publisher, signal, session_id, _publish_options)) =
            self.publisher_handles().await
        else {
            return Err(RtcError::IllegalState(
                "start_noise_cancellation() before join()".to_owned(),
            ));
        };
        signal
            .start_noise_cancellation(signal::StartNoiseCancellationRequest { session_id })
            .await?;
        Ok(())
    }

    /// Disable SFU-side noise cancellation for this participant session.
    pub async fn stop_noise_cancellation(&self) -> Result<()> {
        let Some((_publisher, signal, session_id, _publish_options)) =
            self.publisher_handles().await
        else {
            return Err(RtcError::IllegalState(
                "stop_noise_cancellation() before join()".to_owned(),
            ));
        };
        signal
            .stop_noise_cancellation(signal::StopNoiseCancellationRequest { session_id })
            .await?;
        Ok(())
    }

    /// Release any ICE candidates the SFU trickled for the publisher before its
    /// remote description (the `SetPublisher` answer) was applied.
    pub(super) async fn flush_publisher_ice(&self) {
        let handles = self
            .connection
            .lock()
            .await
            .as_ref()
            .map(|connection| (connection.publisher.clone(), connection.pending_ice.clone()));
        if let Some((publisher, pending_ice)) = handles {
            flush_candidates(&publisher, &pending_ice.publisher).await;
        }
    }

    /// Snapshot the live publisher PC, signal client, session id, and the SFU's
    /// publish-option table (codec + id the SFU validates published tracks against).
    pub(super) async fn publisher_handles(
        &self,
    ) -> Option<(
        Arc<RTCPeerConnection>,
        SignalClient,
        String,
        Vec<models::PublishOption>,
    )> {
        let generation = self.generation();
        let guard = self.connection.lock().await;
        let conn = guard.as_ref()?;
        if conn.generation != generation {
            return None;
        }
        Some((
            conn.publisher.clone(),
            conn.signal.clone(),
            conn.session_id.clone(),
            conn.publish_options.clone(),
        ))
    }

    pub(super) async fn apply_publish_options(
        &self,
        generation: u64,
        publish_options: Vec<models::PublishOption>,
    ) -> Result<()> {
        {
            let mut connection = self.connection.lock().await;
            if let Some(connection) = connection
                .as_mut()
                .filter(|connection| connection.generation == generation)
            {
                connection.publish_options = publish_options.clone();
            } else {
                return Ok(());
            }
        }
        let mut media = self.media.lock().await;
        let tracks = media.active_tracks();
        if tracks.is_empty() {
            return Ok(());
        }
        if let Err(error) = publisher::validate_publish_codecs(&tracks, &publish_options) {
            tracing::warn!(
                %error,
                "stream.rtc.publish_options_active_codec_unavailable"
            );
            return Ok(());
        }
        media.refresh_publish_options(&publish_options)?;
        media.configure_active_tracks(&publish_options)?;
        media.reapply_cached_publish_quality();
        let Some((publisher, signal, session_id, _)) = self.publisher_handles().await else {
            return Ok(());
        };
        publisher::negotiate_publish(&publisher, &signal, &session_id, &tracks, &publish_options)
            .await?;
        self.flush_publisher_ice().await;
        drop(media);
        Ok(())
    }

    pub(super) async fn restart_publisher_ice(&self) -> Result<()> {
        let media = self.media.lock().await;
        let tracks = media.active_tracks();
        let Some((publisher, signal, session_id, publish_options)) = self.publisher_handles().await
        else {
            return Ok(());
        };
        publisher::restart_ice(&publisher, &signal, &session_id, &tracks, &publish_options).await?;
        self.flush_publisher_ice().await;
        drop(media);
        Ok(())
    }

    pub(super) async fn apply_publish_quality(
        self: &Arc<Self>,
        event: &event::ChangePublishQuality,
    ) {
        let matched = self.media.lock().await.apply_publish_quality(event);
        tracing::debug!(matched, "stream.rtc.publish_quality_applied");
    }

    pub(super) fn complete_migration(&self, generation: u64) {
        if let Some(sender) = self.take_migration_waiter(generation) {
            let _ = sender.send(());
        }
    }
}
