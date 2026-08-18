//! Subscription negotiation, inbound tracks, and participant media state.

use super::*;

impl RtcCore {
    /// Register the callback fired for every inbound [`RemoteTrack`]. Persists
    /// across reconnects; set it before subscribing to observe the first track.
    pub fn on_track<F>(&self, cb: F)
    where
        F: Fn(RemoteTrack) + Send + Sync + 'static,
    {
        *self.on_track_cb.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(cb));
    }

    /// Set the subscription policy and (re)send `UpdateSubscriptions`. Activates
    /// the reactive subscriber: subscriptions are recomputed on every roster
    /// change from here on.
    pub async fn update_subscriptions(&self, config: SubscriptionConfig) -> Result<()> {
        *self.sub_config.lock().unwrap_or_else(|e| e.into_inner()) = config;
        *self
            .manual_subscriptions
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        self.subs_active.store(true, Ordering::SeqCst);
        self.recompute_subscriptions().await
    }

    /// Replace the desired subscriptions with an exact per-session track list.
    ///
    /// Calling [`Self::update_subscriptions`] switches back to the coarse
    /// all-participant policy. Passing an empty list unsubscribes from all media.
    pub async fn update_subscription_targets(
        &self,
        targets: Vec<SubscriptionTarget>,
    ) -> Result<()> {
        *self
            .manual_subscriptions
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(targets);
        self.subs_active.store(true, Ordering::SeqCst);
        self.recompute_subscriptions().await
    }

    /// Enable or disable incoming video for every remote participant.
    pub async fn set_incoming_video_enabled(&self, enabled: bool) -> Result<()> {
        let config = {
            let mut config = self.sub_config.lock().unwrap_or_else(|e| e.into_inner());
            config.video = enabled;
            config.video_dimension = None;
            *config
        };
        *self
            .manual_subscriptions
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        self.subs_active.store(true, Ordering::SeqCst);
        self.recompute_subscriptions().await?;
        tracing::debug!(enabled = config.video, "stream.rtc.incoming_video_updated");
        Ok(())
    }

    /// Rebuild the desired subscription list from the roster + policy and send it
    /// to the SFU if it changed since the last send on this connection.
    pub(super) async fn recompute_subscriptions(&self) -> Result<()> {
        self.recompute_subscriptions_for_generation(self.generation())
            .await
    }

    pub(super) async fn recompute_subscriptions_for_generation(
        &self,
        generation: u64,
    ) -> Result<()> {
        if !self.subs_active.load(Ordering::SeqCst) {
            return Ok(());
        }
        if !self.is_generation_current(generation) {
            return Ok(());
        }
        let (session_id, signal) = {
            let guard = self.connection.lock().await;
            match guard.as_ref() {
                Some(c) if c.generation == generation => (c.session_id.clone(), c.signal.clone()),
                None => return Ok(()),
                Some(_) => return Ok(()),
            }
        };

        let config = *self.sub_config.lock().unwrap_or_else(|e| e.into_inner());
        let targets = self
            .manual_subscriptions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let manual = self
            .manual_unsub
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let mut tracks: Vec<signal::TrackSubscriptionDetails> = Vec::new();
        {
            let roster = self.roster.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(targets) = targets {
                for target in targets {
                    let Some(entry) = roster.get(&target.session_id) else {
                        continue;
                    };
                    if entry.session_id == session_id
                        || !entry.published.contains(&(target.track_type as i32))
                        || manual
                            .contains(&TrackKey::new(entry.session_id.clone(), target.track_type))
                    {
                        continue;
                    }
                    tracks.push(signal::TrackSubscriptionDetails {
                        user_id: entry.user_id.clone(),
                        session_id: entry.session_id.clone(),
                        track_type: target.track_type as i32,
                        dimension: target.dimension.and_then(|(width, height)| {
                            is_video_type(target.track_type)
                                .then_some(models::VideoDimension { width, height })
                        }),
                    });
                }
            } else {
                for entry in roster.values() {
                    if entry.session_id == session_id {
                        continue;
                    }
                    for &tt_i in &entry.published {
                        let Ok(track_type) = TrackType::try_from(tt_i) else {
                            continue;
                        };
                        if !config.matches(track_type)
                            || manual.contains(&TrackKey::new(entry.session_id.clone(), track_type))
                        {
                            continue;
                        }
                        let dimension = if is_video_type(track_type) {
                            config
                                .video_dimension
                                .map(|(width, height)| models::VideoDimension { width, height })
                        } else {
                            None
                        };
                        tracks.push(signal::TrackSubscriptionDetails {
                            user_id: entry.user_id.clone(),
                            session_id: entry.session_id.clone(),
                            track_type: tt_i,
                            dimension,
                        });
                    }
                }
            }
        }
        tracks.sort_by(|a, b| {
            (a.session_id.as_str(), a.track_type).cmp(&(b.session_id.as_str(), b.track_type))
        });
        tracks.dedup_by(|left, right| {
            left.session_id == right.session_id && left.track_type == right.track_type
        });

        // Skip an identical resend (roster churn that doesn't change the set).
        if *self.active_subs.lock().unwrap_or_else(|e| e.into_inner()) == tracks {
            return Ok(());
        }

        tracing::debug!(
            tracks = ?tracks.iter().map(|t| (t.user_id.clone(), t.track_type)).collect::<Vec<_>>(),
            "stream.rtc.update_subscriptions"
        );
        signal
            .update_subscriptions(signal::UpdateSubscriptionsRequest {
                session_id: session_id.clone(),
                tracks: tracks.clone(),
            })
            .await?;
        let connection = self.connection.lock().await;
        if connection.as_ref().is_some_and(|current| {
            current.generation == generation && current.session_id == session_id
        }) {
            *self.active_subs.lock().unwrap_or_else(|e| e.into_inner()) = tracks;
        }
        Ok(())
    }

    /// Correlate an inbound track to a participant, build a [`RemoteTrack`], and
    /// deliver it to the `on_track` callback. Called by the subscriber PC, which
    /// passes itself as `subscriber` so the track can send RTCP keyframe
    /// requests.
    pub(super) async fn handle_incoming_track(
        self: Arc<Self>,
        generation: u64,
        connection_epoch: u64,
        reconnect_enabled: Arc<AtomicBool>,
        track: Arc<TrackRemote>,
        subscriber: Weak<RTCPeerConnection>,
    ) {
        if !self.is_generation_current(generation) || !reconnect_enabled.load(Ordering::SeqCst) {
            return;
        }
        let stream_id = track.stream_id();
        let (prefix, msid_type) = parse_msid(&stream_id);
        let track_type = msid_type
            .and_then(|n| TrackType::try_from(n).ok())
            .unwrap_or_else(|| match track.kind() {
                RTPCodecType::Audio => TrackType::Audio,
                _ => TrackType::Video,
            });

        let cb = self
            .on_track_cb
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(cb) = cb else {
            tracing::debug!(%stream_id, "stream.rtc.on_track.no_callback");
            return;
        };

        let participant = self.lookup_participant(&prefix);
        tracing::debug!(
            %stream_id,
            ?track_type,
            user_id = %participant.user_id,
            session_id = %participant.session_id,
            "stream.rtc.on_track.delivered"
        );
        if !self.is_generation_current(generation) || !reconnect_enabled.load(Ordering::SeqCst) {
            return;
        }
        let key = TrackKey::new(participant.session_id.clone(), track_type);
        let weak = Arc::downgrade(&self);
        let on_drop = Box::new(move || {
            if let Some(core) = weak.upgrade() {
                let task_core = core.clone();
                std::mem::drop(core.spawn_runtime_task(async move {
                    task_core
                        .on_remote_track_dropped(generation, connection_epoch, key)
                        .await;
                }));
            }
        });
        let remote = RemoteTrack::new(track, participant, track_type, subscriber, on_drop);
        cb(remote);
    }

    /// The publisher dropped their inbound track handle → unsubscribe from it.
    pub(super) async fn on_remote_track_dropped(
        self: Arc<Self>,
        generation: u64,
        connection_epoch: u64,
        key: TrackKey,
    ) {
        {
            let connection = self.connection.lock().await;
            if !connection.as_ref().is_some_and(|current| {
                current.generation == generation && current.epoch == connection_epoch
            }) {
                return;
            }
            self.manual_unsub
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key);
        }
        if !self
            .is_connection_current(generation, connection_epoch)
            .await
        {
            return;
        }
        if let Err(e) = self.recompute_subscriptions().await {
            tracing::debug!(error = %e, "stream.rtc.unsubscribe_on_drop_failed");
        }
    }
}
