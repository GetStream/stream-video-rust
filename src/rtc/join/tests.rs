//! Unit and local-live coverage for the call lifecycle facade.

use super::*;
use crate::client::ClientConfig;
use crate::rtc::{
    ClientPublishOptions, LocalAudioTrack, LocalVideoTrack, LocalVideoTrackConfig,
    PreferredVideoCodec, publish_options::H264_FMTP,
};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

fn test_core() -> Arc<RtcCore> {
    test_core_with_config(ClientConfig::default())
}

fn test_core_with_config(config: ClientConfig) -> Arc<RtcCore> {
    let client = Arc::new(
        Client::new("test-key".to_owned(), "test-secret".to_owned(), config).expect("test client"),
    );
    RtcCore::new(client, "default".to_owned(), "test-call".to_owned())
}

fn prepare_joined_core(core: &Arc<RtcCore>, user_id: &str) -> u64 {
    let generation = core.begin_join().expect("test generation");
    *core
        .join_data
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = JoinCallData::new(user_id);
    let token = crate::token::create_user_token(
        b"test-secret",
        user_id,
        &crate::token::TokenOptions::default(),
    )
    .expect("test user token");
    *core
        .user_token
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = token;
    assert!(core.set_state_if_current(generation, CallingState::Joined));
    generation
}

fn refresh_server() -> (
    String,
    std::sync::mpsc::Receiver<String>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind refresh server");
    listener
        .set_nonblocking(true)
        .expect("set refresh server nonblocking");
    let address = listener.local_addr().expect("refresh server address");
    let (request_tx, request_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .expect("set refresh stream blocking");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .expect("set refresh read timeout");
                    let mut request = [0_u8; 4096];
                    let read = stream.read(&mut request).expect("read refresh request");
                    let request = String::from_utf8_lossy(&request[..read]).into_owned();
                    request_tx.send(request).expect("record refresh request");
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                        )
                        .expect("write refresh response");
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "refresh server received no request"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept refresh request: {error}"),
            }
        }
    });
    (format!("http://{address}"), request_rx, server)
}

async fn wait_for(timeout: Duration, mut predicate: impl FnMut() -> bool, description: &str) {
    tokio::time::timeout(timeout, async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {description}"));
}

/// A local SFU WebSocket. It sends each received request to the channel. The
/// channel closes when the client socket closes. With `answer_join`, it answers
/// the `JoinRequest`.
async fn fake_sfu(
    answer_join: bool,
) -> (
    Credentials,
    tokio::sync::mpsc::UnboundedReceiver<event::SfuRequest>,
) {
    use futures_util::{SinkExt, StreamExt};
    use prost::Message as _;
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake SFU");
    let address = listener.local_addr().expect("fake SFU address");
    let (requests, received) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept SFU client");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("SFU WebSocket handshake");
        while let Some(Ok(message)) = socket.next().await {
            let Message::Binary(bytes) = message else {
                continue;
            };
            let request = event::SfuRequest::decode(bytes).expect("SFU request");
            if answer_join
                && matches!(
                    request.request_payload,
                    Some(event::sfu_request::RequestPayload::JoinRequest(_))
                )
            {
                let response = SfuEvent {
                    event_payload: Some(sfu_event::EventPayload::JoinResponse(
                        JoinResponse::default(),
                    )),
                };
                socket
                    .send(Message::Binary(response.encode_to_vec().into()))
                    .await
                    .expect("send join response");
            }
            let _ = requests.send(request);
        }
    });
    let credentials = Credentials {
        server: coordinator::SfuServer {
            edge_name: "fake-edge".to_owned(),
            url: "http://127.0.0.1:9/twirp".to_owned(),
            ws_endpoint: format!("ws://{address}/ws"),
        },
        token: "sfu-token".to_owned(),
        ice_servers: Vec::new(),
    };
    (credentials, received)
}

/// A local coordinator WebSocket that sends `connection.ok`. It returns the
/// REST base URL and a task that ends when the client socket closes.
async fn fake_coordinator() -> (String, tokio::task::JoinHandle<()>) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake coordinator");
    let address = listener.local_addr().expect("fake coordinator address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept coordinator client");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("coordinator WebSocket handshake");
        socket
            .next()
            .await
            .expect("auth frame")
            .expect("valid auth frame");
        socket
            .send(Message::Text(
                json!({ "type": "connection.ok", "connection_id": "connection-1" })
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send connection.ok");
        while let Some(Ok(_)) = socket.next().await {}
    });
    (format!("http://{address}"), server)
}

/// Every request the fake SFU received until the client socket closed.
async fn requests_until_close(
    mut received: tokio::sync::mpsc::UnboundedReceiver<event::SfuRequest>,
) -> Vec<event::SfuRequest> {
    tokio::time::timeout(Duration::from_secs(2), async move {
        let mut requests = Vec::new();
        while let Some(request) = received.recv().await {
            requests.push(request);
        }
        requests
    })
    .await
    .expect("SFU socket closed")
}

fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

async fn establish_fake(
    core: &Arc<RtcCore>,
    generation: u64,
) -> (
    Connection,
    tokio::sync::mpsc::UnboundedReceiver<event::SfuRequest>,
) {
    let (credentials, sfu) = fake_sfu(true).await;
    let connection = core
        .clone()
        .establish(
            &credentials,
            0,
            ReconnectStrategy::Fast,
            None,
            generation,
            None,
        )
        .await
        .expect("establish against fake SFU");
    (connection, sfu)
}

/// The event loop context of `connection` after a migration detached it.
fn detached_context(core: &Arc<RtcCore>, connection: &Connection) -> EventLoopContext {
    connection.reconnect_enabled.store(false, Ordering::SeqCst);
    EventLoopContext {
        core: core.clone(),
        subscriber: connection.subscriber.clone(),
        publisher: connection.publisher.clone(),
        signal: connection.signal.clone(),
        session_id: connection.session_id.clone(),
        pending_ice: connection.pending_ice.clone(),
        generation: connection.generation,
        ws_healthy: connection.ws_healthy.clone(),
        reconnect_enabled: connection.reconnect_enabled.clone(),
    }
}

fn preferred_codec(core: &RtcCore, generation: u64) -> Option<models::Codec> {
    core.preferred_publish_options(generation)
        .expect("current generation")
        .into_iter()
        .next()
        .and_then(|option| option.codec)
}

#[test]
fn pre_join_h264_preference_builds_canonical_join_option() {
    let core = test_core();
    core.update_publish_options(ClientPublishOptions {
        preferred_codec: Some(PreferredVideoCodec::H264),
    });
    let generation = core.begin_join().expect("join generation");

    let codec = preferred_codec(&core, generation).expect("H264 preference");
    assert_eq!(codec.name, "H264");
    assert_eq!(codec.fmtp, H264_FMTP);
}

#[test]
fn update_after_join_starts_has_no_effect() {
    let core = test_core();
    core.update_publish_options(ClientPublishOptions {
        preferred_codec: Some(PreferredVideoCodec::H264),
    });
    let generation = core.begin_join().expect("join generation");
    assert!(core.set_state_if_current(generation, CallingState::Joined));

    core.update_publish_options(ClientPublishOptions {
        preferred_codec: Some(PreferredVideoCodec::Vp9),
    });

    let codec = preferred_codec(&core, generation).expect("active H264 preference");
    assert_eq!(codec.name, "H264");

    let leave_generation = core.cancel_generation();
    assert!(core.set_state_if_current(leave_generation, CallingState::Left));
    let next_generation = core.begin_join().expect("next join generation");
    let codec = preferred_codec(&core, next_generation).expect("future VP9 preference");
    assert_eq!(codec.name, "VP9");
}

#[tokio::test]
async fn publish_preference_persists_across_leave_and_rejoin() {
    let core = test_core();
    core.update_publish_options(ClientPublishOptions {
        preferred_codec: Some(PreferredVideoCodec::H264),
    });
    core.begin_join().expect("first generation");
    core.leave("test leave").await.expect("leave");
    let second = core.begin_join().expect("second generation");

    let codec = preferred_codec(&core, second).expect("retained H264 preference");
    assert_eq!(codec.name, "H264");
    assert_eq!(codec.fmtp, H264_FMTP);

    core.leave("test reset").await.expect("second leave");
    core.update_publish_options(ClientPublishOptions::default());
    let third = core.begin_join().expect("third generation");
    assert!(
        core.preferred_publish_options(third)
            .expect("current")
            .is_empty()
    );
}

#[tokio::test]
async fn leave_cancels_join_generation_and_allows_later_join() {
    let core = test_core();
    let first = core.begin_join().expect("first generation");
    assert_eq!(core.lifecycle_snapshot(), (CallingState::Joining, first));

    let waiter_core = core.clone();
    let waiter = tokio::spawn(async move {
        waiter_core
            .while_generation(first, std::future::pending::<()>())
            .await
    });
    tokio::task::yield_now().await;

    core.leave("test cancellation").await.expect("leave");
    assert!(waiter.await.expect("waiter task").is_err());
    assert_eq!(core.state(), CallingState::Left);

    let second = core.begin_join().expect("second generation");
    assert_ne!(first, second);
    assert_eq!(core.lifecycle_snapshot(), (CallingState::Joining, second));
}

#[tokio::test]
async fn leave_tears_down_the_stored_connection() {
    let core = test_core();
    let generation = prepare_joined_core(&core, "alice");
    let (connection, sfu) = establish_fake(&core, generation).await;
    let (subscriber, publisher) = (connection.subscriber.clone(), connection.publisher.clone());
    *core.connection.lock().await = Some(connection);

    core.leave("test leave").await.expect("leave");

    let requests = requests_until_close(sfu).await;
    assert!(requests.iter().any(|request| matches!(
        request.request_payload,
        Some(event::sfu_request::RequestPayload::LeaveCallRequest(_))
    )));
    assert_eq!(
        subscriber.connection_state(),
        RTCPeerConnectionState::Closed
    );
    assert_eq!(publisher.connection_state(), RTCPeerConnectionState::Closed);
    let (active, spawned, completed) = core.runtime_task_snapshot();
    assert_eq!(active, 0);
    assert_eq!(spawned, completed);
}

#[tokio::test]
async fn leave_closes_a_connection_owned_by_a_cancelled_join() {
    let core = test_core();
    let generation = prepare_joined_core(&core, "alice");
    let baseline = alive_tasks();
    let (connection, sfu) = establish_fake(&core, generation).await;
    let owner_core = core.clone();
    let owner = tokio::spawn(async move {
        owner_core
            .while_generation(generation, async move {
                let _connection = connection;
                std::future::pending::<()>().await;
            })
            .await
    });
    tokio::task::yield_now().await;

    core.leave("cancel join").await.expect("leave");

    assert!(owner.await.expect("owner task").is_err());
    requests_until_close(sfu).await;
    wait_for(
        Duration::from_secs(2),
        || alive_tasks() == baseline,
        "cancelled connection cleanup",
    )
    .await;
}

#[tokio::test]
async fn generation_change_closes_the_coordinator_socket() {
    let (base_url, coordinator) = fake_coordinator().await;
    let core = test_core_with_config(ClientConfig {
        base_url,
        ..ClientConfig::default()
    });
    let generation = prepare_joined_core(&core, "alice");
    let token = core.current_user_token().expect("user token");
    core.connect_coordinator_events(generation, &token, "alice")
        .await
        .expect("coordinator events");

    core.cancel_generation();

    tokio::time::timeout(Duration::from_secs(2), coordinator)
        .await
        .expect("coordinator socket closed")
        .expect("fake coordinator task");
}

#[tokio::test]
async fn generation_change_stops_the_connection_tasks() {
    let core = test_core();
    let generation = prepare_joined_core(&core, "alice");
    let (connection, _sfu) = establish_fake(&core, generation).await;
    assert!(core.runtime_task_snapshot().0 > 0);

    core.cancel_generation();

    wait_for(
        Duration::from_secs(2),
        || core.runtime_task_snapshot().0 == 0,
        "connection tasks stop",
    )
    .await;
    drop(connection);
}

#[tokio::test]
async fn failed_establish_leaves_no_background_tasks() {
    let core = test_core();
    let generation = prepare_joined_core(&core, "alice");
    core.join_data
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .join_response_timeout = Duration::from_millis(50);
    let baseline = alive_tasks();
    let (credentials, sfu) = fake_sfu(false).await;

    let result = core
        .clone()
        .establish(
            &credentials,
            0,
            ReconnectStrategy::Fast,
            None,
            generation,
            None,
        )
        .await;

    assert!(matches!(result, Err(RtcError::Timeout(_))));
    requests_until_close(sfu).await;
    wait_for(
        Duration::from_secs(2),
        || alive_tasks() == baseline,
        "failed establish cleanup",
    )
    .await;
}

#[tokio::test]
async fn leave_during_establish_leaves_no_background_tasks() {
    let core = test_core();
    let generation = prepare_joined_core(&core, "alice");
    let baseline = alive_tasks();
    let (credentials, mut sfu) = fake_sfu(false).await;
    let owner_core = core.clone();
    let owner = tokio::spawn(async move {
        owner_core
            .while_generation(
                generation,
                owner_core.clone().establish(
                    &credentials,
                    0,
                    ReconnectStrategy::Fast,
                    None,
                    generation,
                    None,
                ),
            )
            .await
            .map(|_| ())
    });
    let join_request = tokio::time::timeout(Duration::from_secs(2), sfu.recv())
        .await
        .expect("join request")
        .expect("SFU socket open");
    assert!(matches!(
        join_request.request_payload,
        Some(event::sfu_request::RequestPayload::JoinRequest(_))
    ));

    core.leave("cancel establish").await.expect("leave");

    assert!(owner.await.expect("owner task").is_err());
    requests_until_close(sfu).await;
    wait_for(
        Duration::from_secs(2),
        || alive_tasks() == baseline,
        "cancelled establish cleanup",
    )
    .await;
}

#[tokio::test]
async fn leave_that_overlaps_a_new_join_keeps_the_new_join_state() {
    let core = test_core();
    prepare_joined_core(&core, "alice");
    core.leave("first leave").await.expect("first leave");
    let connection_slot = core.connection.lock().await;
    let cancelled = core.generation();
    let leave_core = core.clone();
    let leave = tokio::spawn(async move { leave_core.leave("second leave").await });
    wait_for(
        Duration::from_secs(1),
        || core.generation() != cancelled,
        "second leave cancels its generation",
    )
    .await;

    let second = prepare_joined_core(&core, "alice");
    *core
        .coordinator_connection_id
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some((second, "connection-2".to_owned()));
    assert!(core.apply_join_call_state_if_current(second, "session-2", "alice", None));
    assert!(core.claim_reconnect(second));
    drop(connection_slot);
    leave.await.expect("leave task").expect("second leave");

    assert_eq!(core.state(), CallingState::Joined);
    assert!(core.user_auth().is_some());
    assert!(
        core.participants
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key("session-2")
    );
    assert_eq!(core.active_reconnect_generation(), Some(second));
}

#[tokio::test]
async fn stale_coordinator_stop_keeps_the_current_coordinator() {
    let (base_url, coordinator) = fake_coordinator().await;
    let core = test_core_with_config(ClientConfig {
        base_url,
        ..ClientConfig::default()
    });
    let first = prepare_joined_core(&core, "alice");
    core.leave("cancel first join").await.expect("leave");
    let second = prepare_joined_core(&core, "alice");
    let token = core.current_user_token().expect("user token");
    core.connect_coordinator_events(second, &token, "alice")
        .await
        .expect("coordinator events");

    core.stop_coordinator_events(first).await;

    assert!(core.user_auth().is_some());
    assert!(!coordinator.is_finished());
    core.leave("cleanup").await.expect("cleanup leave");
    tokio::time::timeout(Duration::from_secs(2), coordinator)
        .await
        .expect("coordinator socket closed")
        .expect("fake coordinator task");
}

#[tokio::test]
async fn detached_connection_ignores_publish_options_from_its_sfu() {
    let core = test_core();
    let generation = prepare_joined_core(&core, "alice");
    let (old, _old_sfu) = establish_fake(&core, generation).await;
    let (current, _sfu) = establish_fake(&core, generation).await;
    *core.connection.lock().await = Some(current);
    let context = detached_context(&core, &old);

    connection::handle_event(
        &context,
        sfu_event::EventPayload::ChangePublishOptions(event::ChangePublishOptions {
            publish_options: vec![models::PublishOption {
                id: 99,
                ..Default::default()
            }],
            reason: "old SFU".to_owned(),
        }),
    )
    .await
    .expect("handle event");

    let connection = core.connection.lock().await;
    assert!(
        connection
            .as_ref()
            .expect("current connection")
            .publish_options
            .is_empty()
    );
}

#[tokio::test]
async fn detached_connection_still_completes_the_migration() {
    let core = test_core();
    let generation = prepare_joined_core(&core, "alice");
    let (old, _old_sfu) = establish_fake(&core, generation).await;
    let context = detached_context(&core, &old);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    core.install_migration_waiter(generation, sender)
        .expect("migration waiter");

    connection::handle_event(
        &context,
        sfu_event::EventPayload::ParticipantMigrationComplete(
            event::ParticipantMigrationComplete {},
        ),
    )
    .await
    .expect("handle event");

    receiver.await.expect("migration complete");
}

#[tokio::test]
async fn forced_strategy_failures_reach_timeout_and_refresh_over_http() {
    for strategy in [
        ReconnectStrategy::Fast,
        ReconnectStrategy::Rejoin,
        ReconnectStrategy::Migrate,
    ] {
        let (base_url, request_rx, server) = refresh_server();
        let core = test_core_with_config(ClientConfig {
            base_url,
            request_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(1),
            ..ClientConfig::default()
        });
        let generation = prepare_joined_core(&core, "alice");
        core.set_disconnection_timeout(Duration::from_millis(10));
        let probe = Arc::new(ReconnectProbe::default());
        probe.fail_once(strategy, ReconnectFaultPoint::BeforeAttempt);
        core.install_reconnect_probe(probe.clone());

        core.trigger_reconnect(generation, strategy, "forced failure".to_owned());
        wait_for(
            Duration::from_secs(2),
            || {
                core.state() == CallingState::ReconnectingFailed
                    && core.active_reconnect_generation().is_none()
                    && core.runtime_task_snapshot().0 == 0
            },
            "reconnecting-failed transition",
        )
        .await;

        assert_eq!(
            *probe
                .attempts
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            vec![strategy]
        );
        let request = request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("call-data refresh request");
        assert!(
            request.starts_with("GET /api/v2/video/call/default/test-call?"),
            "unexpected refresh request: {request}"
        );
        server.join().expect("refresh server");
        let (active, spawned, completed) = core.runtime_task_snapshot();
        assert_eq!(active, 0);
        assert_eq!(spawned, completed);
    }
}

#[tokio::test]
async fn leave_cancels_reconnect_task_before_next_generation() {
    let core = test_core();
    let generation = prepare_joined_core(&core, "alice");
    let probe = Arc::new(ReconnectProbe::default());
    probe.fail_once(ReconnectStrategy::Fast, ReconnectFaultPoint::BeforeAttempt);
    core.install_reconnect_probe(probe.clone());

    core.trigger_reconnect(
        generation,
        ReconnectStrategy::Fast,
        "forced stale generation".to_owned(),
    );
    wait_for(
        Duration::from_secs(1),
        || {
            !probe
                .attempts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty()
        },
        "forced reconnect attempt",
    )
    .await;
    assert_eq!(core.runtime_task_snapshot().0, 1);

    core.leave("cancel reconnect").await.expect("leave");
    let next_generation = core.begin_join().expect("next generation");
    assert!(core.set_state_if_current(next_generation, CallingState::Joined));
    wait_for(
        Duration::from_secs(1),
        || core.runtime_task_snapshot().0 == 0,
        "reconnect task cleanup",
    )
    .await;

    let (active, spawned, completed) = core.runtime_task_snapshot();
    assert_eq!(active, 0);
    assert_eq!(spawned, completed);
    assert_eq!(core.state(), CallingState::Joined);
    assert_eq!(core.active_reconnect_generation(), None);
}

#[test]
fn state_events_arrive_in_the_order_of_the_state_changes() {
    let core = test_core();
    let generation = core.begin_join().expect("join generation");
    let mut events = core.subscribe();
    let rounds = 20_000;
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let workers = [CallingState::Joined, CallingState::Reconnecting].map(|state| {
        let core = core.clone();
        let barrier = barrier.clone();
        thread::spawn(move || {
            for _ in 0..rounds {
                barrier.wait();
                core.set_state_if_current(generation, state);
                barrier.wait();
            }
        })
    });

    for round in 0..rounds {
        barrier.wait();
        barrier.wait();
        let mut last = None;
        while let Ok(event) = events.try_recv() {
            if let CallEvent::CallingStateChanged(state) = event {
                last = Some(state);
            }
        }
        assert_eq!(last, Some(core.state()), "round {round}");
    }
    for worker in workers {
        worker.join().expect("state worker");
    }
}

#[test]
fn setting_the_same_state_again_sends_no_event() {
    let core = test_core();
    let generation = core.begin_join().expect("join generation");
    let mut events = core.subscribe();

    assert!(core.set_state_if_current(generation, CallingState::Reconnecting));
    assert!(core.set_state_if_current(generation, CallingState::Reconnecting));

    assert!(matches!(
        events.try_recv(),
        Ok(CallEvent::CallingStateChanged(CallingState::Reconnecting))
    ));
    assert!(events.try_recv().is_err());
}

#[test]
fn join_start_sends_joining() {
    let core = test_core();
    let mut events = core.subscribe();

    core.begin_join().expect("join generation");

    assert!(matches!(
        events.try_recv(),
        Ok(CallEvent::CallingStateChanged(CallingState::Joining))
    ));
}

#[tokio::test]
async fn state_during_leave_matches_the_last_state_event() {
    let core = test_core();
    let mut events = core.subscribe();
    core.begin_join().expect("join generation");
    let connection_slot = core.connection.lock().await;
    let generation = core.generation();
    let leave_core = core.clone();
    let leave = tokio::spawn(async move { leave_core.leave("leave during join").await });
    wait_for(
        Duration::from_secs(1),
        || core.generation() != generation,
        "leave cancels the join",
    )
    .await;

    let mut last = None;
    while let Ok(event) = events.try_recv() {
        if let CallEvent::CallingStateChanged(state) = event {
            last = Some(state);
        }
    }
    assert_eq!(last, Some(core.state()));
    drop(connection_slot);
    leave.await.expect("leave task").expect("leave");
}

#[tokio::test]
async fn late_reconnect_task_does_not_count_toward_the_next_join() {
    let core = test_core();
    let first = prepare_joined_core(&core, "alice");
    core.trigger_reconnect(
        first,
        ReconnectStrategy::Fast,
        reconnect::REASON_ICE_UNSUPPORTED.to_owned(),
    );
    core.leave("leave before the reconnect task runs")
        .await
        .expect("leave");
    let second = prepare_joined_core(&core, "alice");
    wait_for(
        Duration::from_secs(1),
        || core.runtime_task_snapshot().0 == 0,
        "late reconnect task ends",
    )
    .await;

    core.trigger_reconnect(
        second,
        ReconnectStrategy::Fast,
        reconnect::REASON_ICE_UNSUPPORTED.to_owned(),
    );
    wait_for(
        Duration::from_secs(1),
        || core.state() != CallingState::Joined,
        "second reconnect starts",
    )
    .await;

    assert_eq!(core.state(), CallingState::Reconnecting);
    core.leave("cleanup").await.expect("cleanup leave");
}

#[tokio::test]
async fn token_load_that_ends_after_a_new_join_keeps_the_new_token() {
    let core = test_core();
    let generation = prepare_joined_core(&core, "alice");
    let old_token = core
        .user_token
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let weak_core = Arc::downgrade(&core);
    let provider = move || {
        let weak_core = weak_core.clone();
        let old_token = old_token.clone();
        async move {
            // A new join starts in the same poll in which this load ends.
            if let Some(core) = weak_core.upgrade() {
                core.cancel_generation();
                *core
                    .user_token
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = "new-join-token".to_owned();
            }
            Ok(old_token)
        }
    };
    *core
        .token_source
        .lock()
        .unwrap_or_else(|error| error.into_inner()) =
        Some(UserTokenSource::Provider(Arc::new(provider)));

    let result = core.reload_user_token(generation).await;

    assert!(result.is_err());
    assert_eq!(
        *core
            .user_token
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
        "new-join-token"
    );
}

#[tokio::test]
async fn work_that_ends_after_its_generation_changed_is_cancelled() {
    let core = test_core();
    let generation = core.begin_join().expect("join generation");
    let work_core = core.clone();

    let result = core
        .while_generation(generation, async move {
            work_core.cancel_generation();
        })
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn work_for_a_stale_generation_never_runs() {
    let core = test_core();
    let generation = core.begin_join().expect("join generation");
    core.cancel_generation();
    let ran = Arc::new(AtomicBool::new(false));
    let work_ran = ran.clone();

    let result = core
        .while_generation(generation, async move {
            work_ran.store(true, Ordering::SeqCst);
        })
        .await;

    assert!(result.is_err());
    assert!(!ran.load(Ordering::SeqCst));
}

#[test]
fn concurrent_reconnect_claims_have_one_winner() {
    let core = test_core();
    let generation = core.begin_join().expect("join generation");
    let rounds = 5_000;
    let claimers = 4;
    let barrier = Arc::new(std::sync::Barrier::new(claimers + 1));
    let wins = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let workers: Vec<_> = (0..claimers)
        .map(|_| {
            let core = core.clone();
            let barrier = barrier.clone();
            let wins = wins.clone();
            thread::spawn(move || {
                for _ in 0..rounds {
                    barrier.wait();
                    if core.claim_reconnect(generation) {
                        wins.fetch_add(1, Ordering::SeqCst);
                    }
                    barrier.wait();
                }
            })
        })
        .collect();

    for round in 0..rounds {
        barrier.wait();
        barrier.wait();
        assert_eq!(wins.swap(0, Ordering::SeqCst), 1, "round {round}");
        core.release_reconnect(generation);
    }
    for worker in workers {
        worker.join().expect("claim worker");
    }
}

#[test]
fn second_migration_waiter_for_a_generation_is_rejected() {
    let core = test_core();
    let generation = core.begin_join().expect("join generation");
    let (first, _first_receiver) = tokio::sync::oneshot::channel();
    let (second, _second_receiver) = tokio::sync::oneshot::channel();
    core.install_migration_waiter(generation, first)
        .expect("first migration waiter");

    assert!(matches!(
        core.install_migration_waiter(generation, second),
        Err(RtcError::IllegalState(_))
    ));
}

#[tokio::test]
async fn migration_waiter_of_a_new_generation_replaces_the_old_one() {
    let core = test_core();
    let first = core.begin_join().expect("first generation");
    let (old_sender, old_receiver) = tokio::sync::oneshot::channel();
    core.install_migration_waiter(first, old_sender)
        .expect("old migration waiter");
    core.leave("next generation").await.expect("leave");
    let second = core.begin_join().expect("second generation");
    let (sender, mut receiver) = tokio::sync::oneshot::channel();

    core.install_migration_waiter(second, sender)
        .expect("new migration waiter");

    assert!(old_receiver.await.is_err());
    core.complete_migration(second);
    assert_eq!(receiver.try_recv(), Ok(()));
}

#[test]
fn user_query_needs_the_coordinator_connection_of_the_current_generation() {
    let core = test_core();
    let generation = prepare_joined_core(&core, "alice");
    *core
        .coordinator_connection_id
        .lock()
        .unwrap_or_else(|error| error.into_inner()) =
        Some((generation.wrapping_sub(1), "old-connection".to_owned()));

    assert!(core.user_request_query().is_none());
    assert!(core.user_auth().is_none());
}

#[test]
fn user_query_needs_a_connection_id_and_a_user_id() {
    let core = test_core();
    let generation = prepare_joined_core(&core, "alice");
    let set_connection_id = |id: &str| {
        *core
            .coordinator_connection_id
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some((generation, id.to_owned()));
    };

    set_connection_id("");
    assert!(core.user_request_query().is_none());

    set_connection_id("connection-1");
    core.join_data
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .user_id
        .clear();
    assert!(core.user_request_query().is_none());
}

#[test]
fn stale_reconnect_completion_does_not_release_the_current_generation() {
    let core = test_core();
    let first = core.begin_join().expect("first generation");
    assert!(core.claim_reconnect(first));

    let leave_generation = core.cancel_generation();
    core.set_state_if_current(leave_generation, CallingState::Left);
    let second = core.begin_join().expect("second generation");
    assert!(core.claim_reconnect(second));

    core.release_reconnect(first);
    assert_eq!(core.active_reconnect_generation(), Some(second));
}

#[test]
fn stale_migration_event_preserves_the_current_waiter() {
    let core = test_core();
    let generation = core.begin_join().expect("generation");
    let (sender, mut receiver) = tokio::sync::oneshot::channel();
    core.install_migration_waiter(generation, sender)
        .expect("current migration waiter");

    assert!(
        core.take_migration_waiter(generation.wrapping_sub(1))
            .is_none()
    );
    core.complete_migration(generation.wrapping_sub(1));
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    core.complete_migration(generation);
    assert_eq!(receiver.try_recv(), Ok(()));
}

#[tokio::test]
async fn stale_remote_track_drop_does_not_change_new_generation_subscriptions() {
    let core = test_core();
    let first = prepare_joined_core(&core, "alice");
    core.leave("next generation").await.expect("leave");
    let second = core.begin_join().expect("second generation");
    assert!(core.set_state_if_current(second, CallingState::Joined));

    core.clone()
        .on_remote_track_dropped(
            first,
            0,
            TrackKey::new("remote-session".to_owned(), TrackType::Audio),
        )
        .await;

    assert!(
        core.manual_unsub
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty()
    );
}

#[test]
fn participant_refresh_replaces_published_track_state() {
    let core = test_core();
    let mut participant = models::Participant {
        user_id: "alice".to_owned(),
        session_id: "session-a".to_owned(),
        published_tracks: vec![TrackType::Audio as i32, TrackType::Video as i32],
        ..Default::default()
    };
    core.upsert_participant(&participant);
    participant.published_tracks = vec![TrackType::Audio as i32];
    core.upsert_participant(&participant);

    let participants = core.participants.lock().unwrap_or_else(|e| e.into_inner());
    let entry = participants.get("session-a").expect("participant");
    assert_eq!(entry.published.len(), 1);
    assert!(entry.published.contains(&(TrackType::Audio as i32)));
}

#[test]
fn call_state_snapshot_combines_join_state_and_incremental_sfu_updates() {
    let core = test_core();
    let generation = core.begin_join().expect("join generation");
    assert!(core.apply_join_call_state_if_current(
        generation,
        "local-session",
        "local-user",
        Some(models::CallState {
            participants: vec![models::Participant {
                user_id: "alice".to_owned(),
                session_id: "session-a".to_owned(),
                published_tracks: vec![TrackType::Video as i32],
                ..Default::default()
            }],
            participant_count: Some(models::ParticipantCount {
                total: 2,
                anonymous: 0,
            }),
            pins: vec![models::Pin {
                user_id: "alice".to_owned(),
                session_id: "session-a".to_owned(),
            }],
            e2ee_enabled: true,
            ..Default::default()
        })
    ));
    core.update_connection_quality(&[event::ConnectionQualityInfo {
        user_id: "alice".to_owned(),
        session_id: "session-a".to_owned(),
        connection_quality: models::ConnectionQuality::Excellent as i32,
    }]);
    core.update_inbound_state(&[event::InboundVideoState {
        user_id: "alice".to_owned(),
        session_id: "session-a".to_owned(),
        track_type: TrackType::Video as i32,
        paused: true,
    }]);
    core.update_call_grants(Some(models::CallGrants {
        can_publish_audio: true,
        can_publish_video: false,
        can_screenshare: false,
    }));

    let state = core.call_state();
    assert_eq!(state.participant_count.total, 2);
    assert!(state.e2ee_enabled);
    assert_eq!(state.pins[0].session_id, "session-a");
    let alice = state
        .participants
        .iter()
        .find(|participant| participant.session_id == "session-a")
        .expect("alice snapshot");
    assert_eq!(
        alice.connection_quality,
        models::ConnectionQuality::Excellent
    );
    assert_eq!(alice.paused_tracks, vec![TrackType::Video]);
    assert!(state.current_grants.expect("grants").can_publish_audio);
}

#[test]
fn mute_state_builder_deduplicates_track_types() {
    let first = LocalTrack::Audio(LocalAudioTrack::opus().expect("first audio track"));
    let second = LocalTrack::Audio(LocalAudioTrack::opus().expect("second audio track"));
    second.set_muted(true);
    let states = current_mute_states_for_tracks(&[first, second]);

    assert_eq!(states.len(), 1);
    assert_eq!(states[0].track_type, TrackType::Audio as i32);
    assert!(!states[0].muted);
}

#[test]
fn publish_quality_routes_by_option_id_and_track_type() {
    let config = LocalVideoTrackConfig::default().server_managed();
    let first = LocalTrack::Video {
        track: LocalVideoTrack::h264_with_config(config).expect("first layered track"),
        track_type: TrackType::Video,
    };
    let second = LocalTrack::Video {
        track: LocalVideoTrack::h264_with_config(config).expect("second layered track"),
        track_type: TrackType::Video,
    };
    let option = models::PublishOption {
        track_type: TrackType::Video as i32,
        codec: Some(models::Codec {
            name: "H264".to_owned(),
            ..Default::default()
        }),
        bitrate: 1_200_000,
        fps: 30,
        max_spatial_layers: 3,
        video_dimension: Some(models::VideoDimension {
            width: 1280,
            height: 720,
        }),
        ..Default::default()
    };
    first.configure_for_publish(&option).expect("first plan");
    second.configure_for_publish(&option).expect("second plan");
    let mut media = MediaState::default();
    media.begin_publish(first.clone(), 10);
    media.begin_publish(second.clone(), 11);

    let matched = media.apply_publish_quality(&event::ChangePublishQuality {
        video_senders: vec![event::VideoSender {
            track_type: TrackType::Video as i32,
            publish_option_id: 11,
            layers: vec![event::VideoLayerSetting {
                name: "h".to_owned(),
                active: false,
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    });

    assert_eq!(matched, 1);
    assert_eq!(first.is_video_layer_paused("h"), Some(false));
    assert_eq!(second.is_video_layer_paused("h"), Some(true));
    assert_eq!(media.publish_quality.len(), 1);
}

#[test]
fn publish_quality_routes_to_vp9_without_mutating_an_h264_publication() {
    let h264_track =
        LocalVideoTrack::h264_with_config(LocalVideoTrackConfig::default().server_managed())
            .expect("layered H264");
    let vp9_track = LocalVideoTrack::vp9_svc().expect("VP9 SVC");
    let h264 = LocalTrack::Video {
        track: h264_track,
        track_type: TrackType::Video,
    };
    let vp9 = LocalTrack::Video {
        track: vp9_track.clone(),
        track_type: TrackType::Video,
    };
    let option = |id, codec: &str| models::PublishOption {
        id,
        track_type: TrackType::Video as i32,
        codec: Some(models::Codec {
            name: codec.to_owned(),
            ..Default::default()
        }),
        bitrate: 1_200_000,
        fps: 30,
        max_spatial_layers: 3,
        max_temporal_layers: 3,
        video_dimension: Some(models::VideoDimension {
            width: 1280,
            height: 720,
        }),
        ..Default::default()
    };
    h264.configure_for_publish(&option(10, "H264"))
        .expect("H264 plan");
    vp9.configure_for_publish(&option(11, "VP9"))
        .expect("VP9 plan");
    let h264_before = h264.video_layer_control_state("q").expect("H264 low layer");
    let mut media = MediaState::default();
    media.begin_publish(h264.clone(), 10);
    media.begin_publish(vp9.clone(), 11);

    let matched = media.apply_publish_quality(&event::ChangePublishQuality {
        video_senders: vec![event::VideoSender {
            track_type: TrackType::Video as i32,
            publish_option_id: 11,
            layers: vec![event::VideoLayerSetting {
                name: "q".to_owned(),
                active: true,
                max_bitrate: 400_000,
                max_framerate: 15,
                scale_resolution_down_by: 4.0,
                scalability_mode: "L1T2".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    });

    assert_eq!(matched, 1);
    assert_eq!(h264.video_layer_control_state("q"), Some(h264_before));
    assert_eq!(vp9_track.svc_mode(), Some((1, 2)));
    assert_eq!(
        vp9.video_layer_control_state("q"),
        Some((false, 400, 15, 4.0))
    );
    assert!(
        media
            .publish_quality
            .contains_key(&(11, TrackType::Video as i32))
    );
    assert!(
        !media
            .publish_quality
            .contains_key(&(10, TrackType::Video as i32))
    );
}

#[test]
fn refreshed_vp9_publish_option_restores_cached_svc_quality() {
    let vp9_track = LocalVideoTrack::vp9_svc().expect("VP9 SVC");
    let video = LocalTrack::Video {
        track: vp9_track.clone(),
        track_type: TrackType::Video,
    };
    let option = models::PublishOption {
        id: 51,
        track_type: TrackType::Video as i32,
        codec: Some(models::Codec {
            name: "VP9".to_owned(),
            ..Default::default()
        }),
        bitrate: 1_200_000,
        fps: 30,
        max_spatial_layers: 3,
        max_temporal_layers: 3,
        video_dimension: Some(models::VideoDimension {
            width: 1280,
            height: 720,
        }),
        ..Default::default()
    };
    video
        .configure_for_publish(&option)
        .expect("initial VP9 topology");
    let mut media = MediaState::default();
    media.begin_publish(video.clone(), option.id);
    assert_eq!(
        media.apply_publish_quality(&event::ChangePublishQuality {
            video_senders: vec![event::VideoSender {
                track_type: TrackType::Video as i32,
                publish_option_id: option.id,
                layers: vec![event::VideoLayerSetting {
                    name: "q".to_owned(),
                    active: true,
                    max_bitrate: 400_000,
                    max_framerate: 15,
                    scale_resolution_down_by: 4.0,
                    scalability_mode: "L1T3".to_owned(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }),
        1
    );
    let controlled = video
        .video_layer_control_state("q")
        .expect("controlled VP9 encoding");
    assert_eq!(vp9_track.svc_mode(), Some((1, 3)));

    media
        .refresh_publish_options(std::slice::from_ref(&option))
        .expect("VP9 option refresh");
    media
        .configure_active_tracks(std::slice::from_ref(&option))
        .expect("refresh baseline topology");
    assert_eq!(
        vp9_track.svc_mode(),
        Some((3, 3)),
        "publish-option refresh first restores the server baseline"
    );
    media.reapply_cached_publish_quality();

    assert_eq!(vp9_track.svc_mode(), Some((1, 3)));
    assert_eq!(video.video_layer_control_state("q"), Some(controlled));
    assert!(
        media
            .publish_quality
            .contains_key(&(option.id, TrackType::Video as i32))
    );
}

#[test]
fn refreshed_publish_options_preserve_pair_scoped_quality() {
    let audio = LocalTrack::Audio(LocalAudioTrack::opus().expect("audio track"));
    let video = LocalTrack::Video {
        track: LocalVideoTrack::h264_with_config(LocalVideoTrackConfig::default().server_managed())
            .expect("layered video"),
        track_type: TrackType::Video,
    };
    let options = vec![
        models::PublishOption {
            id: 41,
            track_type: TrackType::Audio as i32,
            codec: Some(models::Codec {
                name: "opus".to_owned(),
                ..Default::default()
            }),
            ..Default::default()
        },
        models::PublishOption {
            id: 41,
            track_type: TrackType::Video as i32,
            codec: Some(models::Codec {
                name: "H264".to_owned(),
                ..Default::default()
            }),
            bitrate: 1_200_000,
            fps: 30,
            max_spatial_layers: 3,
            video_dimension: Some(models::VideoDimension {
                width: 1280,
                height: 720,
            }),
            ..Default::default()
        },
    ];
    video
        .configure_for_publish(&options[1])
        .expect("initial video topology");
    let mut media = MediaState::default();
    media.begin_publish(audio, 41);
    media.begin_publish(video.clone(), 41);
    assert_eq!(
        media.apply_publish_quality(&event::ChangePublishQuality {
            video_senders: vec![event::VideoSender {
                track_type: TrackType::Video as i32,
                publish_option_id: 41,
                layers: vec![event::VideoLayerSetting {
                    name: "h".to_owned(),
                    active: false,
                    max_bitrate: 450_000,
                    max_framerate: 15,
                    scale_resolution_down_by: 3.0,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }),
        1
    );
    let controlled = video
        .video_layer_control_state("h")
        .expect("controlled middle layer");

    media
        .refresh_publish_options(&options)
        .expect("pair-scoped option refresh");
    media
        .configure_active_tracks(&options)
        .expect("refresh baseline topology");
    media.reapply_cached_publish_quality();

    assert_eq!(media.publications[0].publish_option_id, 41);
    assert_eq!(media.publications[1].publish_option_id, 41);
    assert!(
        media
            .publish_quality
            .contains_key(&(41, TrackType::Video as i32))
    );
    assert!(
        !media
            .publish_quality
            .contains_key(&(41, TrackType::Audio as i32))
    );
    assert_eq!(video.video_layer_control_state("h"), Some(controlled));
}

#[test]
fn permission_update_only_applies_to_the_local_user() {
    let event = CoordinatorEvent {
        event_type: "call.permissions_updated".to_owned(),
        raw: json!({
            "user": { "id": "alice" },
            "own_capabilities": ["join-call", "send-video"]
        }),
    };

    let capabilities =
        own_capabilities_from_event(&event, "alice").expect("local permission update");
    assert!(capabilities.contains("join-call"));
    assert!(capabilities.contains("send-video"));
    assert!(!capabilities.contains("send-audio"));
    assert!(own_capabilities_from_event(&event, "bob").is_none());
}

#[tokio::test]
async fn permission_revocation_stops_an_active_track() {
    let core = test_core();
    core.own_capabilities
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert("send-audio".to_owned());
    let audio = LocalAudioTrack::opus().expect("audio track");
    let local_track = LocalTrack::Audio(audio.clone());
    let track_id = local_track.track_id();
    {
        let mut media = core.media.lock().await;
        media.begin_publish(local_track, 7);
        media.set_status(&track_id, PublicationStatus::Published);
    }
    let event = CoordinatorEvent {
        event_type: "call.permissions_updated".to_owned(),
        raw: json!({
            "user": { "id": "alice" },
            "own_capabilities": ["join-call"]
        }),
    };

    core.apply_permissions_updated(&event, "alice").await;

    assert!(core.media.lock().await.publications.is_empty());
    assert!(
        audio
            .write_pcm(crate::rtc::PcmFrame::mono(vec![0; 960], 48_000))
            .await
            .is_err(),
        "revoked audio track should be stopped"
    );
}

#[tokio::test]
async fn concurrent_duplicate_publication_has_one_state_entry() {
    let core = test_core();
    let track = LocalTrack::Audio(LocalAudioTrack::opus().expect("audio track"));
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let core = core.clone();
        let track = track.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            core.media.lock().await.begin_publish(track, 7)
        }));
    }
    barrier.wait().await;
    for task in tasks {
        assert_eq!(
            task.await.expect("publication task"),
            PublicationStatus::PendingPublishNegotiation
        );
    }

    let media = core.media.lock().await;
    assert_eq!(media.publications.len(), 1);
    assert_eq!(media.publications[0].track.track_id(), track.track_id());
}

#[test]
fn stop_state_is_retryable_until_mute_sync_commits() {
    let track = LocalTrack::Audio(LocalAudioTrack::opus().expect("audio track"));
    let track_id = track.track_id();
    let mut media = MediaState::default();
    media.begin_publish(track, 7);
    media.set_status(&track_id, PublicationStatus::Published);

    // A stop marks the track `PendingStopMute`: it leaves the active set (so it
    // is not re-announced on reconnect) but stays in the publication list until the mute
    // RPC commits, so a failed mute can be retried without losing the track.
    media.set_status(&track_id, PublicationStatus::PendingStopMute);
    assert!(media.active_tracks().is_empty());
    assert!(media.position(&track_id).is_some());

    assert!(media.remove(&track_id).is_some());
    assert!(media.position(&track_id).is_none());
}

#[test]
fn sfu_ws_url_appends_query_params() {
    let url = build_sfu_ws_url(
        "wss://sfu.example.com/ws",
        "key123",
        "alice",
        "sess-1",
        "default:call-1",
        2,
    )
    .expect("url");
    assert!(url.starts_with("wss://sfu.example.com/ws?"));
    assert!(url.contains("attempt=2"));
    assert!(url.contains("user_id=alice"));
    assert!(url.contains("api_key=key123"));
    assert!(url.contains("user_session_id=sess-1"));
    assert!(url.contains("cid=default%3Acall-1"));
}

/// Live probe resolving the carry-over Twirp body-format + auth-header flag:
/// a media-less join never triggers `SendAnswer`/`IceTrickle`, so we fire one
/// real `IceTrickle` after joining and assert the SFU accepts our framing
/// (`application/protobuf` + `Authorization: Bearer <sfu_token>`). Skips
/// cleanly without credentials.
#[tokio::test]
async fn live_twirp_ice_trickle_framing_accepted() {
    let _ = dotenvy::dotenv();
    let key = std::env::var("STREAM_API_KEY").unwrap_or_default();
    let secret = std::env::var("STREAM_API_SECRET").unwrap_or_default();
    if key.is_empty() || secret.is_empty() {
        eprintln!("SKIP: STREAM creds absent; skipping live twirp framing probe");
        return;
    }

    let stream = crate::Stream::new(key, secret).expect("client");
    let user = format!("rust-twirp-{}", uuid::Uuid::new_v4().simple());
    let call_id = format!("rust-twirp-call-{}", uuid::Uuid::new_v4().simple());
    let call = stream.video().call("default", &call_id);

    call.get_or_create(crate::models::GetOrCreateCallRequest {
        data: Some(crate::models::CallRequest {
            created_by_id: Some(user.clone()),
            ..Default::default()
        }),
        ..Default::default()
    })
    .await
    .expect("get_or_create failed");

    call.join(JoinCallData::create(&user))
        .await
        .expect("join failed");
    let probe = call.rtc_core().twirp_probe().await;
    let _ = call.leave().await;
    let _ = call
        .delete(crate::models::DeleteCallRequest { hard: Some(true) })
        .await;

    match probe {
        // 200 + empty error, or an application-level Signal error: either way
        // the SFU parsed our protobuf body and accepted the auth header.
        Ok(()) => {}
        Err(RtcError::Signal { code, message, .. }) => {
            eprintln!("twirp framing accepted; SFU app-level response code={code} msg={message:?}");
        }
        Err(RtcError::Twirp(e)) => {
            panic!("SFU rejected Twirp framing (body format or auth header): {e:?}")
        }
        Err(RtcError::Transport(e)) => panic!("twirp transport error: {e}"),
        Err(RtcError::Decode(e)) => panic!("twirp response not protobuf-decodable: {e}"),
        Err(other) => panic!("unexpected twirp probe error: {other}"),
    }
}

/// Live proof that reconnect surfaces media-restoration failures.
///
/// A reconnect that fails while restoring the publisher must surface the error
/// instead of silently reporting `Joined`. This joins a live call, publishes
/// audio, injects a one-shot failure at the REJOIN published-restore hook, forces
/// a REJOIN, and proves (1) the restore hook was reached and the injected failure
/// propagated out of the attempt, and (2) the driver retried and recovered to
/// `Joined` rather than leaving a failed attempt marked joined.
#[tokio::test]
async fn live_forced_media_restore_failure_is_surfaced_and_recovers() {
    let _ = dotenvy::dotenv();
    let key = std::env::var("STREAM_API_KEY").unwrap_or_default();
    let secret = std::env::var("STREAM_API_SECRET").unwrap_or_default();
    if key.is_empty() || secret.is_empty() {
        eprintln!("SKIP: STREAM creds absent; skipping live media-restore failure probe");
        return;
    }

    let stream = crate::Stream::new(key, secret).expect("client");
    let user = format!("rust-restore-{}", uuid::Uuid::new_v4().simple());
    let call_id = format!("rust-restore-call-{}", uuid::Uuid::new_v4().simple());
    stream
        .upsert_users([crate::models::UserRequest::new(&user)])
        .await
        .expect("upsert_users");
    let admin = stream.video().call("default", &call_id);
    admin
        .get_or_create(crate::models::GetOrCreateCallRequest {
            data: Some(crate::models::CallRequest {
                created_by_id: Some(user.clone()),
                members: Some(vec![crate::models::MemberRequest::new(&user)]),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("get_or_create");

    let call = stream.video().call("default", &call_id);
    let outcome = tokio::time::timeout(Duration::from_secs(120), async {
        call.join(JoinCallData::new(&user)).await.expect("join");
        let audio = LocalAudioTrack::opus().expect("opus track");
        call.publish_audio(audio.clone()).await.expect("publish");
        let feeder = {
            let audio = audio.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(20));
                loop {
                    interval.tick().await;
                    let frame = crate::rtc::PcmFrame::mono(vec![0_i16; 960], 48_000);
                    if audio.write_pcm(frame).await.is_err() {
                        return;
                    }
                }
            })
        };

        let core = call.rtc_core().clone();
        wait_for(
            Duration::from_secs(30),
            || core.state() == CallingState::Joined,
            "initial joined",
        )
        .await;
        let generation = core.lifecycle_snapshot().1;

        let probe = Arc::new(ReconnectProbe::default());
        probe.fail_once(
            ReconnectStrategy::Rejoin,
            ReconnectFaultPoint::AfterPublishedRestore,
        );
        core.install_reconnect_probe(probe.clone());
        core.trigger_reconnect(
            generation,
            ReconnectStrategy::Rejoin,
            "forced media restore failure".to_owned(),
        );

        let reached = |probe: &Arc<ReconnectProbe>| {
            probe
                .restores
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .any(|(strategy, point)| {
                    *strategy == ReconnectStrategy::Rejoin
                        && *point == ReconnectFaultPoint::AfterPublishedRestore
                })
        };
        wait_for(
            Duration::from_secs(45),
            || reached(&probe),
            "forced restore fault reached",
        )
        .await;
        wait_for(
            Duration::from_secs(45),
            || {
                matches!(
                    core.state(),
                    CallingState::Joined | CallingState::ReconnectingFailed
                )
            },
            "reconnect settled",
        )
        .await;

        feeder.abort();
        let restores = probe
            .restores
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        (restores, core.state())
    })
    .await;

    let _ = call.leave().await;
    let _ = admin
        .delete(crate::models::DeleteCallRequest { hard: Some(true) })
        .await;

    let (restores, final_state) = outcome.expect("media-restore test timed out (120s guard)");
    eprintln!("RESTORE FAILURE: restores={restores:?} final_state={final_state:?}");
    assert!(
        restores.iter().any(|(strategy, point)| {
            *strategy == ReconnectStrategy::Rejoin
                && *point == ReconnectFaultPoint::AfterPublishedRestore
        }),
        "forced REJOIN published-restore failure was never reached/surfaced"
    );
    assert_eq!(
        final_state,
        CallingState::Joined,
        "reconnect did not recover after a surfaced media-restoration failure"
    );
}
