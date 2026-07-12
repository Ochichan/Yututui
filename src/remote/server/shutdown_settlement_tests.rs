use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::BufReader;
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::session_socket_tests::{
    connect, hello, read_json_line, start_stalled_command_server, test_endpoint, write_json,
};
use super::*;
use crate::remote::proto::{
    ClientFrame, ClientOp, HelloAck, RemoteCommand, RemoteRequest, RemoteResponse, ServerFrame,
    Topic,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const INJECTED_WIRE_DELAY: Duration = Duration::from_millis(125);

async fn take_accepted_reply(
    replies: &Arc<Mutex<Vec<oneshot::Sender<RemoteResponse>>>>,
) -> oneshot::Sender<RemoteResponse> {
    timeout(TEST_TIMEOUT, async {
        loop {
            if let Some(reply) = replies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop()
            {
                return reply;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("remote request must reach the owner before shutdown")
}

fn delayed_hub() -> Arc<RemoteSessionHub> {
    test_hub_with(SessionTuning {
        wire_write_delay: INJECTED_WIRE_DELAY,
        ..SessionTuning::default()
    })
}

async fn await_structural_barrier(hub: &Arc<RemoteSessionHub>, expected_active: usize) {
    let barrier = tokio::spawn({
        let hub = Arc::clone(hub);
        async move { hub.wait_for_wire_settlements().await }
    });
    tokio::time::sleep(Duration::from_millis(75)).await;
    assert!(
        !barrier.is_finished(),
        "the old 50ms grace must not retire a token before delayed flush"
    );
    assert_eq!(hub.active_wire_settlements(), expected_active);
    assert!(
        timeout(TEST_TIMEOUT, barrier)
            .await
            .expect("wire barrier stays bounded")
            .expect("wire barrier task must not panic")
    );
    assert_eq!(hub.active_wire_settlements(), 0);
}

#[tokio::test]
async fn owner_signal_barrier_flushes_a_delayed_one_shot_reply_before_hub_abort() {
    let hub = delayed_hub();
    let (endpoint, held_replies) = start_stalled_command_server("sd-os", Arc::clone(&hub));
    let conn = connect(&endpoint).await;
    let (read_half, mut write_half) = tokio::io::split(conn);
    let mut reader = BufReader::new(read_half);
    write_json(
        &mut write_half,
        &RemoteRequest {
            version: PROTOCOL_VERSION,
            token: "secret".to_owned(),
            request_id: Some("shutdown-one-shot-settlement".to_owned()),
            command: RemoteCommand::TogglePause,
        },
    )
    .await;

    let reply = take_accepted_reply(&held_replies).await;
    assert_eq!(hub.active_wire_settlements(), 1);
    hub.quiesce_owner_admission();
    reply
        .send(RemoteResponse::err("shutting_down"))
        .expect("accepted one-shot request still owns its response receiver");
    await_structural_barrier(&hub, 1).await;
    hub.shutdown_all();

    let response: RemoteResponse = read_json_line(&mut reader).await;
    assert_eq!(response.reason.as_deref(), Some("shutting_down"));
}

#[tokio::test]
async fn owner_signal_barrier_flushes_a_delayed_session_reply_before_goodbye() {
    let hub = delayed_hub();
    let (endpoint, held_replies) = start_stalled_command_server("sd-sess", Arc::clone(&hub));
    let conn = connect(&endpoint).await;
    let (read_half, mut write_half) = tokio::io::split(conn);
    let mut reader = BufReader::new(read_half);
    write_json(
        &mut write_half,
        &hello(PROTOCOL_VERSION, PROTOCOL_VERSION, "secret"),
    )
    .await;
    let ack: HelloAck = read_json_line(&mut reader).await;
    assert!(ack.ok);

    write_json(
        &mut write_half,
        &ClientFrame {
            id: 77,
            request_id: Some("shutdown-session-settlement".to_owned()),
            page_id: None,
            op: ClientOp::Command(RemoteCommand::TogglePause),
        },
    )
    .await;
    let reply = take_accepted_reply(&held_replies).await;
    assert_eq!(hub.active_wire_settlements(), 1);
    hub.quiesce_owner_admission();
    reply
        .send(RemoteResponse::err("shutting_down"))
        .expect("accepted session request still owns its response receiver");
    await_structural_barrier(&hub, 1).await;
    hub.shutdown_all();

    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Reply { id, resp } => {
            assert_eq!(id, 77);
            assert_eq!(resp.reason.as_deref(), Some("shutting_down"));
        }
        other => panic!("accepted request reply must precede shutdown goodbye, got {other:?}"),
    }
}

#[tokio::test]
async fn owner_signal_barrier_flushes_a_delayed_subscribe_rejection_before_goodbye() {
    let hub = delayed_hub();
    let endpoint = test_endpoint("sd-sub");
    let listener = bind(&endpoint).expect("bind shutdown-subscribe test endpoint");
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(serve(
        listener,
        Arc::from("secret"),
        Arc::new(move |event| event_tx.send(event).is_ok()),
        Arc::clone(&hub),
    ));
    let conn = connect(&endpoint).await;
    let (read_half, mut write_half) = tokio::io::split(conn);
    let mut reader = BufReader::new(read_half);
    write_json(
        &mut write_half,
        &hello(PROTOCOL_VERSION, PROTOCOL_VERSION, "secret"),
    )
    .await;
    let ack: HelloAck = read_json_line(&mut reader).await;
    assert!(ack.ok);

    write_json(
        &mut write_half,
        &ClientFrame {
            id: 91,
            request_id: None,
            page_id: Some("shutdown-page".to_owned()),
            op: ClientOp::Subscribe {
                topics: vec![Topic::System],
            },
        },
    )
    .await;
    let event = timeout(TEST_TIMEOUT, event_rx.recv())
        .await
        .expect("subscribe must reach the owner before shutdown")
        .expect("remote event lane remains open");
    let RemoteEvent::SessionSubscribe {
        session,
        frame_id,
        page_id,
        topics: _,
        settlement,
    } = event
    else {
        panic!("expected accepted subscribe event");
    };
    assert_eq!(hub.active_wire_settlements(), 1);
    hub.quiesce_owner_admission();
    assert!(
        crate::remote::publish::Publisher::new(Arc::clone(&hub)).reject_subscribe_for_shutdown(
            &session,
            page_id.as_deref(),
            frame_id,
            settlement,
        )
    );
    await_structural_barrier(&hub, 1).await;
    hub.shutdown_all();

    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Reply { id, resp } => {
            assert_eq!(id, 91);
            assert_eq!(resp.reason.as_deref(), Some("shutting_down"));
        }
        other => panic!("accepted subscribe reply must precede shutdown goodbye, got {other:?}"),
    }
}

#[tokio::test]
async fn joined_retry_keeps_its_own_wire_token_after_the_first_peer_disconnects() {
    let hub = delayed_hub();
    let (endpoint, held_replies) = start_stalled_command_server("sd-retry", Arc::clone(&hub));
    let request = || RemoteRequest {
        version: PROTOCOL_VERSION,
        token: "secret".to_owned(),
        request_id: Some("joined-shutdown-retry".to_owned()),
        command: RemoteCommand::TogglePause,
    };

    let first = connect(&endpoint).await;
    let (first_read, mut first_write) = tokio::io::split(first);
    write_json(&mut first_write, &request()).await;
    let owner_reply = take_accepted_reply(&held_replies).await;
    assert_eq!(hub.active_wire_settlements(), 1);
    drop(first_read);
    drop(first_write);

    let second = connect(&endpoint).await;
    let (second_read, mut second_write) = tokio::io::split(second);
    let mut second_reader = BufReader::new(second_read);
    write_json(&mut second_write, &request()).await;
    timeout(TEST_TIMEOUT, async {
        while hub.active_wire_settlements() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the joined retry must own a distinct wire lifecycle token");

    hub.quiesce_owner_admission();
    owner_reply
        .send(RemoteResponse::err("shutting_down"))
        .expect("the deduped owner execution remains live");
    await_structural_barrier(&hub, 2).await;
    hub.shutdown_all();

    let response: RemoteResponse = read_json_line(&mut second_reader).await;
    assert_eq!(response.reason.as_deref(), Some("shutting_down"));
}
