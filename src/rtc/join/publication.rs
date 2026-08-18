//! Local publication state and SFU publish-quality controls.

use std::collections::{HashMap, HashSet};

use super::super::error::Result;
use super::super::local_track::LocalTrack;
use super::super::proto::event;
use super::super::proto::models::{self, TrackType};
use super::super::publisher;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PublicationStatus {
    PendingPublishNegotiation,
    PendingPublishMute,
    Published,
    PendingStopMute,
}

#[derive(Clone)]
pub(super) struct LocalPublication {
    pub(super) track: LocalTrack,
    pub(super) status: PublicationStatus,
    pub(super) publish_option_id: i32,
}

#[derive(Default)]
pub(super) struct MediaState {
    pub(super) publications: Vec<LocalPublication>,
    pub(super) publish_quality: HashMap<(i32, i32), Vec<event::VideoLayerSetting>>,
}

impl MediaState {
    pub(super) fn position(&self, track_id: &str) -> Option<usize> {
        self.publications
            .iter()
            .position(|publication| publication.track.track_id() == track_id)
    }

    pub(super) fn active_tracks(&self) -> Vec<LocalTrack> {
        self.publications
            .iter()
            .filter(|publication| {
                matches!(
                    publication.status,
                    PublicationStatus::PendingPublishNegotiation
                        | PublicationStatus::PendingPublishMute
                        | PublicationStatus::Published
                )
            })
            .map(|publication| publication.track.clone())
            .collect()
    }

    pub(super) fn begin_publish(
        &mut self,
        track: LocalTrack,
        publish_option_id: i32,
    ) -> PublicationStatus {
        let track_id = track.track_id();
        if let Some(position) = self.position(&track_id) {
            return self.publications[position].status;
        }
        self.publications.push(LocalPublication {
            track,
            status: PublicationStatus::PendingPublishNegotiation,
            publish_option_id,
        });
        PublicationStatus::PendingPublishNegotiation
    }

    pub(super) fn set_status(&mut self, track_id: &str, status: PublicationStatus) {
        if let Some(position) = self.position(track_id) {
            self.publications[position].status = status;
        }
    }

    pub(super) fn remove(&mut self, track_id: &str) -> Option<LocalTrack> {
        let position = self.position(track_id)?;
        let publication = self.publications.remove(position);
        let key = (
            publication.publish_option_id,
            publication.track.track_type() as i32,
        );
        if !self.publications.iter().any(|remaining| {
            (
                remaining.publish_option_id,
                remaining.track.track_type() as i32,
            ) == key
        }) {
            self.publish_quality.remove(&key);
        }
        Some(publication.track)
    }

    pub(super) fn refresh_publish_options(
        &mut self,
        publish_options: &[models::PublishOption],
    ) -> Result<()> {
        let mut used_options = HashSet::new();
        for publication in &mut self.publications {
            publication.publish_option_id = publisher::assign_publish_option(
                &publication.track,
                publish_options,
                &used_options,
            )?;
            used_options.insert((
                publication.publish_option_id,
                publication.track.track_type() as i32,
            ));
        }
        self.publish_quality.retain(|key, _| {
            self.publications.iter().any(|publication| {
                *key == (
                    publication.publish_option_id,
                    publication.track.track_type() as i32,
                )
            })
        });
        Ok(())
    }

    pub(super) fn reapply_cached_publish_quality(&self) {
        for publication in &self.publications {
            let key = (
                publication.publish_option_id,
                publication.track.track_type() as i32,
            );
            if let Some(layers) = self.publish_quality.get(&key) {
                publication.track.apply_video_layer_settings(layers);
            }
        }
    }

    pub(super) fn configure_active_tracks(
        &self,
        publish_options: &[models::PublishOption],
    ) -> Result<()> {
        for publication in &self.publications {
            if !matches!(
                publication.status,
                PublicationStatus::PendingPublishNegotiation
                    | PublicationStatus::PendingPublishMute
                    | PublicationStatus::Published
            ) {
                continue;
            }
            let option = publisher::publish_option(&publication.track, publish_options)?;
            publication.track.configure_for_publish(option)?;
        }
        Ok(())
    }

    pub(super) fn apply_publish_quality(&mut self, event: &event::ChangePublishQuality) -> usize {
        let mut matched = 0;
        for sender in &event.video_senders {
            if TrackType::try_from(sender.track_type).is_err() {
                continue;
            }
            let mut sender_matched = false;
            for publication in &self.publications {
                if publication.publish_option_id == sender.publish_option_id
                    && publication.track.track_type() as i32 == sender.track_type
                {
                    publication.track.apply_video_layer_settings(&sender.layers);
                    matched += 1;
                    sender_matched = true;
                }
            }
            if sender_matched {
                let key = (sender.publish_option_id, sender.track_type);
                self.publish_quality.insert(key, sender.layers.clone());
            }
        }
        matched
    }
}
