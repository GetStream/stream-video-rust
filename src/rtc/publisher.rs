//! Publisher-side SDP negotiation (`SetPublisher`).
//!
//! The publisher PeerConnection is always the offerer (JS / videosdk): we add a
//! send-only transceiver per local track, create an offer, hand the offer plus a
//! [`TrackInfo`] per m-line to the SFU via the `SetPublisher` Twirp RPC, and
//! apply the SFU's answer. One renegotiation covers every currently-published
//! track, so republish-after-reconnect re-adds all tracks and negotiates once.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::task::JoinHandle;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::signaling_state::RTCSignalingState;

use super::error::{NegotiationError, Result, RtcError};
use super::local_track::LocalTrack;
use super::proto::models::{PublishOption, TrackInfo, TrackType};
use super::proto::signal::SetPublisherRequest;
use super::signal::SignalClient;

/// Renegotiate the publisher PeerConnection with the SFU for `tracks`.
///
/// Assumes each track's send-only transceiver has already been added to
/// `publisher`. Creates the offer, sends `SetPublisher`, and applies the answer.
/// `publish_options` (from the join response) supplies the codec + option id the
/// SFU validates each announced track against.
pub(crate) async fn negotiate_publish(
    publisher: &Arc<RTCPeerConnection>,
    signal: &SignalClient,
    session_id: &str,
    tracks: &[LocalTrack],
    publish_options: &[PublishOption],
) -> Result<()> {
    validate_publish_codecs(tracks, publish_options)?;
    let offer = publisher.create_offer(None).await.map_err(neg)?;
    publisher
        .set_local_description(offer.clone())
        .await
        .map_err(neg)?;

    let result = async {
        let track_infos = build_track_infos(publisher, tracks, publish_options).await?;
        let resp = signal
            .set_publisher(SetPublisherRequest {
                sdp: offer.sdp,
                session_id: session_id.to_owned(),
                tracks: track_infos,
            })
            .await?;

        let answer = RTCSessionDescription::answer(resp.sdp).map_err(neg)?;
        publisher.set_remote_description(answer).await.map_err(neg)
    }
    .await;
    if let Err(error) = result {
        if publisher.signaling_state() == RTCSignalingState::HaveLocalOffer {
            let mut rollback = RTCSessionDescription::default();
            rollback.sdp_type = RTCSdpType::Rollback;
            if let Err(rollback_error) = publisher.set_local_description(rollback).await {
                return Err(RtcError::Negotiation(NegotiationError(format!(
                    "{error}; signaling rollback failed: {rollback_error}"
                ))));
            }
        }
        return Err(error);
    }
    Ok(())
}

/// Restart publisher ICE and renegotiate the active track set.
pub(crate) async fn restart_ice(
    publisher: &Arc<RTCPeerConnection>,
    signal: &SignalClient,
    session_id: &str,
    tracks: &[LocalTrack],
    publish_options: &[PublishOption],
) -> Result<()> {
    if tracks.is_empty() {
        return Ok(());
    }
    publisher.restart_ice().await.map_err(neg)?;
    negotiate_publish(publisher, signal, session_id, tracks, publish_options).await
}

fn neg(e: impl std::fmt::Display) -> RtcError {
    RtcError::Negotiation(NegotiationError(e.to_string()))
}

/// The codec name portion of an RTP MIME type (`video/VP9` -> `vp9`), matched
/// case-insensitively against the SFU publish option's `codec.name`.
fn codec_subtype(mime_type: &str) -> String {
    mime_type
        .rsplit('/')
        .next()
        .unwrap_or(mime_type)
        .to_lowercase()
}

pub(crate) fn publish_option<'a>(
    track: &LocalTrack,
    publish_options: &'a [PublishOption],
) -> Result<&'a PublishOption> {
    let track_type = track.track_type();
    let requested = codec_subtype(&track.mime_type());
    if let Some(assigned_id) = track.publish_option_id()
        && let Some(option) = publish_options.iter().find(|option| {
            option.id == assigned_id
                && option.track_type == track_type as i32
                && option
                    .codec
                    .as_ref()
                    .is_some_and(|codec| codec.name.eq_ignore_ascii_case(&requested))
        })
    {
        return Ok(option);
    }
    if let Some(option) = publish_options.iter().find(|option| {
        option.track_type == track_type as i32
            && option
                .codec
                .as_ref()
                .is_some_and(|codec| codec.name.eq_ignore_ascii_case(&requested))
    }) {
        return Ok(option);
    }

    let available = publish_options
        .iter()
        .filter(|option| option.track_type == track_type as i32)
        .filter_map(|option| option.codec.as_ref().map(|codec| codec.name.as_str()))
        .collect::<Vec<_>>();
    Err(RtcError::Media(format!(
        "sfu did not advertise {requested} for {track_type:?}; available codecs: {}",
        available.join(", ")
    )))
}

pub(crate) fn assign_publish_option(
    track: &LocalTrack,
    publish_options: &[PublishOption],
    used_options: &HashSet<(i32, i32)>,
) -> Result<i32> {
    let track_type = track.track_type();
    let requested = codec_subtype(&track.mime_type());
    let option = publish_options
        .iter()
        .find(|option| {
            !used_options.contains(&(option.id, option.track_type))
                && option.track_type == track_type as i32
                && option
                    .codec
                    .as_ref()
                    .is_some_and(|codec| codec.name.eq_ignore_ascii_case(&requested))
        })
        .ok_or_else(|| {
            RtcError::Media(format!(
                "sfu did not advertise an unused {requested} publish option for {track_type:?}"
            ))
        })?;
    track.set_publish_option_id(option.id);
    Ok(option.id)
}

/// Attach a logical publication to one m-line. Layered video adds its remaining
/// RID tracks to the first sender before negotiation, matching videosdk's
/// `AddSimulcastTracks` flow.
pub(crate) async fn add_transceiver_for_track(
    publisher: &Arc<RTCPeerConnection>,
    track: &LocalTrack,
    publish_options: &[PublishOption],
) -> Result<Vec<JoinHandle<()>>> {
    let option = publish_option(track, publish_options)?;
    track.configure_for_publish(option)?;
    let physical_tracks = track.webrtc_tracks();
    let rids = physical_tracks
        .iter()
        .filter_map(|track| track.rid().map(str::to_owned))
        .collect::<Vec<_>>();
    let mut tracks = physical_tracks.into_iter();
    let first = tracks
        .next()
        .ok_or_else(|| RtcError::Media("local publication has no physical encodings".to_owned()))?;
    let transceiver = publisher
        .add_transceiver_from_track(first, Some(super::join::send_only()))
        .await
        .map_err(RtcError::from)?;
    let sender = transceiver.sender().await;
    for encoding in tracks {
        sender
            .add_encoding(encoding)
            .await
            .map_err(RtcError::from)?;
    }
    let mut tasks = Vec::with_capacity(rids.len().max(1));
    if rids.is_empty() {
        tasks.push(spawn_rtcp_reader(sender, None, track.clone()));
    } else {
        for rid in rids {
            tasks.push(spawn_rtcp_reader(sender.clone(), Some(rid), track.clone()));
        }
    }
    Ok(tasks)
}

fn spawn_rtcp_reader(
    sender: Arc<webrtc::rtp_transceiver::rtp_sender::RTCRtpSender>,
    rid: Option<String>,
    track: LocalTrack,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let result = match rid.as_deref() {
                Some(rid) => {
                    let mut buffer = vec![0_u8; 1_500];
                    sender.read_simulcast(&mut buffer, rid).await
                }
                None => sender.read_rtcp().await,
            };
            let Ok((packets, _attributes)) = result else {
                break;
            };
            let keyframe_requested = packets.iter().any(|packet| {
                packet
                    .as_any()
                    .is::<webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication>()
                    || packet
                        .as_any()
                        .is::<webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest>()
            });
            if keyframe_requested {
                track.force_video_keyframe();
            }
        }
    })
}

pub(crate) fn validate_publish_codecs(
    tracks: &[LocalTrack],
    publish_options: &[PublishOption],
) -> Result<()> {
    for track in tracks {
        publish_option(track, publish_options)?;
    }
    Ok(())
}

/// Build one [`TrackInfo`] per publisher m-line that carries one of `tracks`,
/// pairing the negotiated `mid` with the track's declared [`TrackType`] and the
/// matching publish option's codec + id (the SFU rejects a `SetPublisher` whose
/// video track omits these).
pub(crate) async fn build_track_infos(
    publisher: &Arc<RTCPeerConnection>,
    tracks: &[LocalTrack],
    publish_options: &[PublishOption],
) -> Result<Vec<TrackInfo>> {
    let by_id: HashMap<String, &LocalTrack> = tracks.iter().map(|t| (t.track_id(), t)).collect();

    let mut infos = Vec::new();
    for transceiver in publisher.get_transceivers().await {
        let sender = transceiver.sender().await;
        let Some(bound) = sender.track().await else {
            continue;
        };
        let Some(local) = by_id.get(bound.id()) else {
            continue;
        };
        let track_type = local.track_type();
        // Never announce a fallback codec: the payload produced by the local
        // track would not match that option, and applying the answer would fail.
        let option = publish_option(local, publish_options)?;

        let mut info = TrackInfo {
            track_id: bound.id().to_owned(),
            track_type: track_type as i32,
            mid: transceiver.mid().map(|m| m.to_string()).unwrap_or_default(),
            dtx: false,
            stereo: false,
            red: false,
            muted: false,
            ..Default::default()
        };
        info.codec = option.codec.clone();
        info.publish_option_id = option.id;
        // Video/screen-share need at least one layer; a single-encoding track
        // declares one HIGH layer (the SFU treats an empty layer list as invalid).
        if matches!(track_type, TrackType::Video | TrackType::ScreenShare) {
            info.layers = local.planned_layers_for_publish(option)?;
        }
        infos.push(info);
    }
    Ok(infos)
}

#[cfg(test)]
mod tests {
    use super::super::local_track::{LocalVideoTrack, LocalVideoTrackConfig};
    use super::super::peer;
    use super::super::proto::models::{Codec, VideoDimension};
    use super::*;

    fn video_option(name: &str) -> PublishOption {
        PublishOption {
            id: 7,
            track_type: TrackType::Video as i32,
            codec: Some(Codec {
                name: name.to_owned(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn publish_codec_validation_accepts_an_exact_case_insensitive_match() {
        let track: LocalTrack = LocalVideoTrack::h264().expect("H264 track").into();
        validate_publish_codecs(&[track], &[video_option("H264")])
            .expect("matching H264 publish option");
    }

    #[test]
    fn publish_codec_validation_rejects_a_fallback_codec() {
        let track: LocalTrack = LocalVideoTrack::h264().expect("H264 track").into();
        let error = validate_publish_codecs(&[track], &[video_option("VP9")])
            .expect_err("VP9 cannot carry an H264 bitstream");
        assert!(
            matches!(error, RtcError::Media(message) if message.contains("available codecs: VP9"))
        );
    }

    #[test]
    fn duplicate_codec_publications_receive_distinct_server_option_ids() {
        let first: LocalTrack = LocalVideoTrack::h264().expect("first H264").into();
        let second: LocalTrack = LocalVideoTrack::h264().expect("second H264").into();
        let options = [
            video_option("H264"),
            PublishOption {
                id: 8,
                ..video_option("H264")
            },
        ];
        let mut used = HashSet::new();
        let first_id = assign_publish_option(&first, &options, &used).expect("first option");
        used.insert((first_id, TrackType::Video as i32));
        let second_id = assign_publish_option(&second, &options, &used).expect("second option");
        assert_eq!((first_id, second_id), (7, 8));
        assert_eq!(
            publish_option(&first, &options).expect("assigned first").id,
            7
        );
        assert_eq!(
            publish_option(&second, &options)
                .expect("assigned second")
                .id,
            8
        );
    }

    #[tokio::test]
    async fn layered_track_uses_one_mline_and_metadata_planning_is_read_only() {
        let track =
            LocalVideoTrack::h264_with_config(LocalVideoTrackConfig::default().server_managed())
                .expect("layered H264");
        let local = LocalTrack::Video {
            track,
            track_type: TrackType::Video,
        };
        let option = PublishOption {
            id: 73,
            track_type: TrackType::Video as i32,
            codec: Some(Codec {
                name: "H264".to_owned(),
                ..Default::default()
            }),
            bitrate: 1_200_000,
            fps: 30,
            max_spatial_layers: 3,
            video_dimension: Some(VideoDimension {
                width: 1280,
                height: 720,
            }),
            ..Default::default()
        };
        let pc = peer::new_peer_connection(&[])
            .await
            .expect("peer connection");
        let tasks = add_transceiver_for_track(&pc, &local, std::slice::from_ref(&option))
            .await
            .expect("attach simulcast tracks");
        let offer = pc.create_offer(None).await.expect("create offer");
        assert_eq!(offer.sdp.matches("m=video").count(), 1);
        assert!(offer.sdp.contains("a=rid:q send"));
        assert!(offer.sdp.contains("a=rid:h send"));
        assert!(offer.sdp.contains("a=rid:f send"));
        assert!(offer.sdp.contains("a=simulcast:send q;h;f"));
        pc.set_local_description(offer).await.expect("set offer");
        local.apply_video_layer_settings(&[super::super::proto::event::VideoLayerSetting {
            name: "h".to_owned(),
            active: false,
            max_bitrate: 450_000,
            max_framerate: 15,
            scale_resolution_down_by: 3.0,
            ..Default::default()
        }]);
        let controlled = local
            .video_layer_control_state("h")
            .expect("middle layer controls");
        let infos = build_track_infos(&pc, std::slice::from_ref(&local), &[option])
            .await
            .expect("track info");
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].publish_option_id, 73);
        assert_eq!(
            infos[0]
                .layers
                .iter()
                .map(|layer| layer.rid.as_str())
                .collect::<Vec<_>>(),
            ["q", "h", "f"]
        );
        assert_eq!(
            local.video_layer_control_state("h"),
            Some(controlled),
            "TrackInfo/reconnect metadata planning must not reset SFU controls"
        );
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
        let _ = pc.close().await;
    }

    #[tokio::test]
    async fn vp9_svc_publish_option_matrix_uses_one_encoding_and_truthful_track_info() {
        let cases =
            (1..=3).flat_map(|spatial| (1..=3).map(move |temporal| (spatial, temporal, false)));
        let cases = cases.chain(std::iter::once((3, 3, true)));

        for (spatial, temporal, use_single_layer) in cases {
            let track = LocalVideoTrack::vp9_svc().expect("VP9 SVC");
            let local = LocalTrack::Video {
                track: track.clone(),
                track_type: TrackType::Video,
            };
            let option = PublishOption {
                id: 74,
                track_type: TrackType::Video as i32,
                codec: Some(Codec {
                    name: "VP9".to_owned(),
                    ..Default::default()
                }),
                bitrate: 1_200_000,
                fps: 30,
                max_spatial_layers: spatial,
                max_temporal_layers: temporal,
                video_dimension: Some(VideoDimension {
                    width: 1280,
                    height: 720,
                }),
                use_single_layer,
                ..Default::default()
            };
            let pc = peer::new_peer_connection(&[])
                .await
                .expect("peer connection");
            let tasks = add_transceiver_for_track(&pc, &local, std::slice::from_ref(&option))
                .await
                .expect("attach VP9 SVC track");

            let offer = pc.create_offer(None).await.expect("create offer");
            assert_eq!(offer.sdp.matches("m=video").count(), 1);
            // webrtc-rs omits RID SDP for a lone sender encoding, even though
            // the bound TrackLocal retains `q`; no simulcast line means one SSRC.
            assert!(!offer.sdp.contains("a=rid:q send"));
            assert!(!offer.sdp.contains("a=rid:h send"));
            assert!(!offer.sdp.contains("a=rid:f send"));
            assert!(!offer.sdp.contains("a=simulcast:send"));
            pc.set_local_description(offer).await.expect("set offer");

            let infos = build_track_infos(&pc, std::slice::from_ref(&local), &[option])
                .await
                .expect("track info");
            assert_eq!(infos.len(), 1);
            assert_eq!(infos[0].publish_option_id, 74);
            assert_eq!(track.webrtc_tracks().len(), 1);
            assert_eq!(track.webrtc_tracks()[0].rid(), Some("q"));
            assert_eq!(
                local.video_layer_control_state("q"),
                Some((false, 1_200, 30, 1.0)),
                "the physical SVC encoding must use the full-resolution layer"
            );
            assert_eq!(
                track.svc_mode(),
                Some((
                    if use_single_layer { 1 } else { spatial as u8 },
                    temporal as u8
                ))
            );

            let expected_rids = ["q", "h", "f"]
                .into_iter()
                .take(spatial as usize)
                .collect::<Vec<_>>();
            assert_eq!(
                infos[0]
                    .layers
                    .iter()
                    .map(|layer| layer.rid.as_str())
                    .collect::<Vec<_>>(),
                expected_rids
            );
            assert_eq!(
                infos[0]
                    .layers
                    .iter()
                    .map(|layer| layer.video_dimension.expect("layer dimension").width)
                    .collect::<Vec<_>>(),
                match spatial {
                    1 => vec![1280],
                    2 => vec![640, 1280],
                    _ => vec![320, 640, 1280],
                }
            );
            assert_eq!(
                infos[0]
                    .layers
                    .iter()
                    .map(|layer| layer.bitrate)
                    .collect::<Vec<_>>(),
                match spatial {
                    1 => vec![1_200_000],
                    2 => vec![600_000, 1_200_000],
                    _ => vec![300_000, 600_000, 1_200_000],
                }
            );
            assert!(infos[0].layers.iter().all(|layer| layer.fps == 30));

            for task in tasks {
                task.abort();
                let _ = task.await;
            }
            let _ = pc.close().await;
        }
    }
}
