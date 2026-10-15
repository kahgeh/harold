use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use super::*;
use crate::agent::domain::EffectiveAgentState;
use crate::agent::snapshot::AgentSnapshotHub;
use crate::outbound::fake::CapturedLogs;
use crate::timed_prompts::tests::{CHIEF, Fake, Key, snapshot, wait_until};

const DAY: &str = "2026-10-14";
const SLOT_IDLE_FIRING: &str = "harold alert: slot-idle firing";

/// The fake's chief, idle in pane `%2` with an empty input box.
fn idle_chief() -> Fake {
    Fake::at(DAY, "09:00:00")
}

fn typed(line: &str) -> Vec<Key> {
    vec![Key::Text("%2".into(), line.into()), Key::Enter("%2".into())]
}

fn set_chief_state(fake: &Fake, state: EffectiveAgentState) {
    fake.set(|fake| fake.view.panes[0].state = state);
}

/// Serves the alert address on a free local port with `fake` as the chief's pane.
async fn serve_fake(
    fake: &Fake,
    chief_folder: Option<&str>,
) -> (SocketAddr, watch::Sender<()>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let handle = serve_on(
        listener,
        chief_folder.map(str::to_string),
        fake.clone(),
        shutdown_rx,
    );
    (addr, shutdown_tx, handle)
}

async fn post(addr: SocketAddr, path: &str, body: &'static [u8]) -> u16 {
    reqwest::Client::new()
        .post(format!("http://{addr}{path}"))
        .body(body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// Runs `deliver` on its own task with `fake` as the chief's pane.
fn deliver_to(
    fake: &Fake,
    chief_folder: Option<&str>,
) -> (mpsc::Sender<String>, watch::Sender<()>, JoinHandle<()>) {
    let (queue_tx, queue_rx) = mpsc::channel(QUEUE_CAPACITY);
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let handle = tokio::spawn(deliver(
        fake.clone(),
        chief_folder.map(str::to_string),
        queue_rx,
        shutdown_rx,
    ));
    (queue_tx, shutdown_tx, handle)
}

// --- the line ---

#[test]
fn known_alerts_make_the_exact_line() {
    assert_eq!(
        alert_line(br#"{"alert":"slot-idle","state":"firing"}"#),
        Ok("harold alert: slot-idle firing".into())
    );
    assert_eq!(
        alert_line(br#"{"alert":"wrong-agent","state":"cleared"}"#),
        Ok("harold alert: wrong-agent cleared".into())
    );
    for name in ALERT_NAMES {
        for state in ["firing", "cleared"] {
            let body = format!(r#"{{"alert":"{name}","state":"{state}"}}"#);
            assert_eq!(
                alert_line(body.as_bytes()),
                Ok(format!("harold alert: {name} {state}"))
            );
        }
    }
}

#[test]
fn the_four_alerts_are_the_ones_in_the_spec() {
    assert_eq!(
        ALERT_NAMES,
        [
            "phone-request-unavailable",
            "phone-request-slow",
            "wrong-agent",
            "slot-idle"
        ]
    );
}

#[test]
fn anything_else_is_refused() {
    for body in [
        &br#"{"alert":"monitor-trouble","state":"firing"}"#[..],
        br#"{"alert":"slot-idle","state":"on"}"#,
        br#"{"alert":"Slot-Idle","state":"firing"}"#,
        br#"{"alert":"slot-idle"}"#,
        br#"{"alert":"slot-idle","state":1}"#,
        br#"["slot-idle","firing"]"#,
        b"not json",
        b"",
    ] {
        assert_eq!(alert_line(body), Err(()));
    }
}

// --- the address ---

#[tokio::test]
async fn a_post_types_exactly_the_line_into_the_chief_pane() {
    let logs = CapturedLogs::start();
    let fake = idle_chief();
    let (addr, shutdown_tx, handle) = serve_fake(&fake, Some(CHIEF)).await;

    let status = post(addr, "/alert", br#"{"alert":"slot-idle","state":"firing"}"#).await;

    assert_eq!(status, 204);
    wait_until("the alert is typed", || fake.keys().len() == 2).await;
    assert_eq!(fake.keys(), typed(SLOT_IDLE_FIRING));
    let text = logs.text();
    assert!(text.contains("alert typed"), "{text}");
    assert!(text.contains("alert=\"slot-idle\""), "{text}");
    assert!(text.contains("state=\"firing\""), "{text}");

    drop(shutdown_tx);
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("the address and the delivery stop on shutdown")
        .unwrap();
}

#[tokio::test]
async fn a_bad_post_gets_400_and_types_nothing() {
    let logs = CapturedLogs::start();
    let fake = idle_chief();
    let (addr, _shutdown_tx, _handle) = serve_fake(&fake, Some(CHIEF)).await;

    for (path, body) in [
        (
            "/alert",
            &br#"{"alert":"monitor-trouble","state":"firing"}"#[..],
        ),
        ("/alert", br#"{"alert":"slot-idle","state":"on"}"#),
        ("/alert", b"not json"),
        ("/alert", b""),
        ("/other", br#"{"alert":"slot-idle","state":"firing"}"#),
    ] {
        assert_eq!(post(addr, path, body).await, 400, "{path} {body:?}");
    }
    let get = reqwest::get(format!("http://{addr}/alert")).await.unwrap();
    assert_eq!(get.status().as_u16(), 400);

    // A good post after the bad ones is the only line typed.
    let status = post(addr, "/alert", br#"{"alert":"slot-idle","state":"firing"}"#).await;
    assert_eq!(status, 204);
    wait_until("the good alert is typed", || fake.keys().len() == 2).await;
    assert_eq!(fake.keys(), typed(SLOT_IDLE_FIRING));

    let text = logs.text();
    assert_eq!(
        text.matches("alert ignored (unknown alert)").count(),
        6,
        "{text}"
    );
    // No part of a refused body reaches the log.
    assert!(!text.contains("monitor-trouble"), "{text}");
    assert!(!text.contains("not json"), "{text}");
}

#[tokio::test]
async fn a_full_queue_still_answers_204_and_logs_the_drop() {
    let logs = CapturedLogs::start();
    let (queue_tx, mut queue_rx) = mpsc::channel(1);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(queue_tx)).await });

    let first = post(addr, "/alert", br#"{"alert":"slot-idle","state":"firing"}"#).await;
    let second = post(
        addr,
        "/alert",
        br#"{"alert":"wrong-agent","state":"firing"}"#,
    )
    .await;

    assert_eq!((first, second), (204, 204));
    assert_eq!(queue_rx.recv().await.as_deref(), Some(SLOT_IDLE_FIRING));
    assert!(queue_rx.try_recv().is_err());
    let text = logs.text();
    assert!(text.contains("alert not delivered (queue_full)"), "{text}");
    assert!(text.contains("alert=\"wrong-agent\""), "{text}");
}

#[tokio::test]
async fn an_address_already_in_use_is_an_error() {
    let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = taken.local_addr().unwrap();
    let (_shutdown_tx, shutdown_rx) = watch::channel(());
    let ports = TmuxPorts::new(
        AgentSnapshotHub::new(snapshot(None, Vec::new())),
        Vec::new(),
    );

    let started = start(addr, Some(CHIEF.into()), ports, shutdown_rx).await;

    assert!(started.is_err());
}

// --- delivering ---

#[tokio::test]
async fn with_no_chief_the_alert_is_dropped() {
    let logs = CapturedLogs::start();
    // No chief's folder in the settings.
    let fake = idle_chief();
    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, None);
    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    wait_until("the drop is logged", || {
        logs.text().contains("alert not delivered (no_chief)")
    })
    .await;

    // A folder with no agent in it.
    let empty = idle_chief();
    empty.set(|fake| fake.view.panes.clear());
    let (queue, _shutdown_tx, _handle) = deliver_to(&empty, Some(CHIEF));
    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    wait_until("the second drop is logged", || {
        logs.text()
            .matches("alert not delivered (no_chief)")
            .count()
            == 2
    })
    .await;

    assert!(fake.keys().is_empty());
    assert!(empty.keys().is_empty());
    assert!(logs.text().contains("reason=\"no_chief\""));
}

#[tokio::test(start_paused = true)]
async fn a_chief_busy_for_ten_minutes_drops_the_alert() {
    let logs = CapturedLogs::start();
    let fake = idle_chief();
    set_chief_state(&fake, EffectiveAgentState::Busy);
    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, Some(CHIEF));

    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(9 * 60 + 55)).await;
    assert!(fake.keys().is_empty());
    assert!(!logs.text().contains("alert not delivered"));

    tokio::time::sleep(Duration::from_secs(10)).await;
    let text = logs.text();
    assert!(text.contains("alert not delivered (agent_busy)"), "{text}");
    assert!(text.contains("reason=\"agent_busy\""), "{text}");

    // The dropped line is not typed once the chief is idle; the next one is.
    set_chief_state(&fake, EffectiveAgentState::Idle);
    queue
        .send("harold alert: wrong-agent cleared".into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(fake.keys(), typed("harold alert: wrong-agent cleared"));
}

#[tokio::test(start_paused = true)]
async fn a_chief_busy_for_a_while_gets_the_alert_when_it_is_idle() {
    let fake = idle_chief();
    set_chief_state(&fake, EffectiveAgentState::Busy);
    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, Some(CHIEF));

    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3 * 60)).await;
    assert!(fake.keys().is_empty());
    set_chief_state(&fake, EffectiveAgentState::Idle);
    tokio::time::sleep(Duration::from_secs(6)).await;

    assert_eq!(fake.keys(), typed(SLOT_IDLE_FIRING));
}

#[tokio::test(start_paused = true)]
async fn a_failed_enter_is_logged_and_never_typed_again() {
    let logs = CapturedLogs::start();
    let fake = idle_chief();
    fake.set(|fake| fake.fail_enter = true);
    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, Some(CHIEF));

    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(15 * 60)).await;

    assert_eq!(
        fake.keys(),
        vec![Key::Text("%2".into(), SLOT_IDLE_FIRING.into())]
    );
    let text = logs.text();
    assert!(
        text.contains("alert not delivered (enter_failed)"),
        "{text}"
    );
    assert!(text.contains("reason=\"enter_failed\""), "{text}");
}

#[tokio::test(start_paused = true)]
async fn the_next_alert_waits_until_the_pane_has_been_seen_busy() {
    let fake = idle_chief();
    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, Some(CHIEF));

    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    queue
        .send("harold alert: wrong-agent firing".into())
        .await
        .unwrap();
    // The snapshot still says idle just after typing, so the second line waits.
    tokio::time::sleep(Duration::from_secs(20)).await;
    assert_eq!(fake.keys(), typed(SLOT_IDLE_FIRING));

    set_chief_state(&fake, EffectiveAgentState::Busy);
    tokio::time::sleep(Duration::from_secs(6)).await;
    set_chief_state(&fake, EffectiveAgentState::Idle);
    tokio::time::sleep(Duration::from_secs(6)).await;

    let mut both = typed(SLOT_IDLE_FIRING);
    both.extend(typed("harold alert: wrong-agent firing"));
    assert_eq!(fake.keys(), both);
}

#[tokio::test(start_paused = true)]
async fn shutdown_abandons_an_alert_waiting_for_the_chief() {
    let fake = idle_chief();
    set_chief_state(&fake, EffectiveAgentState::Busy);
    let (queue, shutdown_tx, handle) = deliver_to(&fake, Some(CHIEF));
    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(30)).await;

    drop(shutdown_tx);
    let start = tokio::time::Instant::now();
    handle.await.unwrap();

    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(fake.keys().is_empty());
}
