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
    let (handle, _queue) = serve_on(
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
) -> (Poster, watch::Sender<()>, JoinHandle<()>) {
    let (queue_tx, queue_rx) = mpsc::channel(QUEUE_CAPACITY);
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let handle = tokio::spawn(deliver(
        fake.clone(),
        chief_folder.map(str::to_string),
        queue_rx,
        shutdown_rx,
    ));
    (Poster(queue_tx), shutdown_tx, handle)
}

/// Queues lines for `deliver` as the address does, stamped with the time of the post.
struct Poster(mpsc::Sender<Queued>);

impl Poster {
    async fn send(&self, line: String) -> Result<(), mpsc::error::SendError<Queued>> {
        self.0.send((line, Instant::now())).await
    }
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
fn the_five_alerts_are_the_ones_in_the_specs() {
    assert_eq!(
        ALERT_NAMES,
        [
            "phone-request-unavailable",
            "phone-request-slow",
            "wrong-agent",
            "slot-idle",
            "local-model-failing"
        ]
    );
    // `local-model-slow` is a dashboard card, not an alert.
    assert!(alert_line(br#"{"alert":"local-model-slow","state":"firing"}"#).is_err());
    assert_eq!(
        alert_line(br#"{"alert":"local-model-failing","state":"firing"}"#),
        Ok("harold alert: local-model-failing firing".into())
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
        br#"{"alert":"delivery-given-up","state":"firing"}"#,
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
        (
            "/alert",
            &br#"{"alert":"delivery-given-up","state":"firing"}"#[..],
        ),
        (
            "/alert",
            &br#"{"alert":"store-failing","state":"firing"}"#[..],
        ),
        (
            "/alert",
            &br#"{"alert":"store-failing","state":"cleared"}"#[..],
        ),
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
        9,
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
    let before_check = Instant::now();
    let (line, posted_at) = queue_rx.recv().await.unwrap();
    assert_eq!(line, SLOT_IDLE_FIRING);
    // Stamped when it was posted, which was just now.
    assert!(posted_at <= before_check);
    assert!(before_check - posted_at < Duration::from_secs(5));
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

/// Runs `deliver` on its own task, fed by Harold's own queue, which drops a line when full.
fn deliver_from_queue(fake: &Fake) -> (AlertQueue, watch::Sender<()>, JoinHandle<()>) {
    let (queue, queue_rx) = AlertQueue::new();
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let handle = tokio::spawn(deliver(
        fake.clone(),
        Some(CHIEF.into()),
        queue_rx,
        shutdown_rx,
    ));
    (queue, shutdown_tx, handle)
}

/// Shows the chief busy and then idle again, so a pane held after typing is let go.
async fn busy_then_idle(fake: &Fake) {
    set_chief_state(fake, EffectiveAgentState::Busy);
    tokio::time::sleep(Duration::from_secs(6)).await;
    set_chief_state(fake, EffectiveAgentState::Idle);
    tokio::time::sleep(Duration::from_secs(6)).await;
}

fn texts(fake: &Fake) -> Vec<String> {
    fake.keys()
        .into_iter()
        .filter_map(|key| match key {
            Key::Text(_, text) => Some(text),
            Key::Enter(_) => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn three_firings_in_a_row_type_one_line() {
    let fake = idle_chief();
    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, Some(CHIEF));

    for _ in 0..3 {
        queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    busy_then_idle(&fake).await;
    tokio::time::sleep(Duration::from_secs(2 * 60)).await;

    assert_eq!(fake.keys(), typed(SLOT_IDLE_FIRING));
}

#[tokio::test(start_paused = true)]
async fn a_chief_busy_for_thirty_minutes_gets_the_alert_once_when_idle() {
    let logs = CapturedLogs::start();
    let fake = idle_chief();
    set_chief_state(&fake, EffectiveAgentState::Busy);
    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, Some(CHIEF));

    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(30 * 60)).await;
    assert!(fake.keys().is_empty());
    let text = logs.text();
    assert!(!text.contains("alert not delivered"), "{text}");
    assert_eq!(
        text.matches("alert waiting (agent_busy)").count(),
        1,
        "{text}"
    );
    assert!(text.contains("reason=\"agent_busy\""), "{text}");

    set_chief_state(&fake, EffectiveAgentState::Idle);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(fake.keys(), typed(SLOT_IDLE_FIRING));
    tokio::time::sleep(Duration::from_secs(5 * 60)).await;
    assert_eq!(fake.keys(), typed(SLOT_IDLE_FIRING));
    assert!(logs.text().contains("alert typed"));
}

#[tokio::test(start_paused = true)]
async fn a_changed_refusal_reason_logs_waiting_only_once() {
    let logs = CapturedLogs::start();
    let fake = idle_chief();
    set_chief_state(&fake, EffectiveAgentState::Busy);
    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, Some(CHIEF));

    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(10 * 60)).await;
    set_chief_state(&fake, EffectiveAgentState::Unknown);
    tokio::time::sleep(Duration::from_secs(10 * 60)).await;

    assert!(fake.keys().is_empty());
    let text = logs.text();
    assert_eq!(text.matches("alert waiting (").count(), 1, "{text}");
}

#[tokio::test(start_paused = true)]
async fn firing_cleared_firing_type_three_lines() {
    let fake = idle_chief();
    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, Some(CHIEF));

    for line in [SLOT_IDLE_FIRING, SLOT_IDLE_CLEARED, SLOT_IDLE_FIRING] {
        queue.send(line.into()).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        busy_then_idle(&fake).await;
    }

    assert_eq!(
        texts(&fake),
        [SLOT_IDLE_FIRING, SLOT_IDLE_CLEARED, SLOT_IDLE_FIRING]
    );
}

#[tokio::test(start_paused = true)]
async fn a_cleared_never_fired_types_nothing() {
    let fake = idle_chief();
    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, Some(CHIEF));
    queue.send(SLOT_IDLE_CLEARED.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(fake.keys().is_empty());

    // A firing and its cleared, both while the chief is busy.
    set_chief_state(&fake, EffectiveAgentState::Busy);
    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(60)).await;
    queue.send(SLOT_IDLE_CLEARED.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(60)).await;
    set_chief_state(&fake, EffectiveAgentState::Idle);
    tokio::time::sleep(Duration::from_secs(60)).await;

    assert!(fake.keys().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_firing_with_no_chief_is_typed_once_a_chief_starts() {
    let logs = CapturedLogs::start();
    let fake = idle_chief();
    let chief = fake.agents().panes;
    fake.set(|fake| fake.view.panes.clear());
    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, Some(CHIEF));

    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(20 * 60)).await;
    assert!(fake.keys().is_empty());
    let text = logs.text();
    assert_eq!(
        text.matches("alert waiting (no_chief)").count(),
        1,
        "{text}"
    );
    assert!(text.contains("reason=\"no_chief\""), "{text}");

    fake.set(|fake| fake.view.panes = chief);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(fake.keys(), typed(SLOT_IDLE_FIRING));
}

#[tokio::test(start_paused = true)]
async fn with_no_chief_folder_the_alert_waits() {
    let logs = CapturedLogs::start();
    let fake = idle_chief();
    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, None);

    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(20 * 60)).await;

    assert!(fake.keys().is_empty());
    let text = logs.text();
    assert_eq!(
        text.matches("alert waiting (no_chief)").count(),
        1,
        "{text}"
    );
    assert!(!text.contains("alert not delivered"), "{text}");
}

#[tokio::test(start_paused = true)]
async fn delivery_given_up_is_typed_each_time_and_merged_while_busy() {
    let fake = idle_chief();
    let (queue, _shutdown_tx, _handle) = deliver_from_queue(&fake);
    let line = "harold alert: delivery-given-up firing";

    queue.firing(DELIVERY_GIVEN_UP);
    tokio::time::sleep(Duration::from_secs(1)).await;
    queue.firing(DELIVERY_GIVEN_UP);
    tokio::time::sleep(Duration::from_secs(70)).await;
    assert_eq!(texts(&fake), [line, line]);

    set_chief_state(&fake, EffectiveAgentState::Busy);
    tokio::time::sleep(Duration::from_secs(6)).await;
    queue.firing(DELIVERY_GIVEN_UP);
    queue.firing(DELIVERY_GIVEN_UP);
    tokio::time::sleep(Duration::from_secs(60)).await;
    set_chief_state(&fake, EffectiveAgentState::Idle);
    tokio::time::sleep(Duration::from_secs(2 * 60)).await;

    assert_eq!(texts(&fake), [line, line, line]);
}

#[tokio::test(start_paused = true)]
async fn store_failing_then_cleared_type_two_lines() {
    let fake = idle_chief();
    let (queue, _shutdown_tx, _handle) = deliver_from_queue(&fake);

    queue.firing(STORE_FAILING);
    tokio::time::sleep(Duration::from_secs(1)).await;
    queue.cleared(STORE_FAILING);
    tokio::time::sleep(Duration::from_secs(70)).await;

    assert_eq!(
        texts(&fake),
        [
            "harold alert: store-failing firing",
            "harold alert: store-failing cleared"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_hundred_repeats_while_busy_do_not_fill_the_queue() {
    let logs = CapturedLogs::start();
    let fake = idle_chief();
    set_chief_state(&fake, EffectiveAgentState::Busy);
    let (queue, _shutdown_tx, _handle) = deliver_from_queue(&fake);

    for _ in 0..100 {
        queue.firing("slot-idle");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let text = logs.text();
    assert!(!text.contains("queue_full"), "{text}");

    set_chief_state(&fake, EffectiveAgentState::Idle);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(fake.keys(), typed(SLOT_IDLE_FIRING));
}

#[tokio::test(start_paused = true)]
async fn a_new_deliver_starts_with_nothing_told() {
    let fake = idle_chief();
    let (queue, shutdown_tx, handle) = deliver_to(&fake, Some(CHIEF));
    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    drop(shutdown_tx);
    handle.await.unwrap();

    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, Some(CHIEF));
    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(70)).await;

    assert_eq!(texts(&fake), [SLOT_IDLE_FIRING, SLOT_IDLE_FIRING]);
}

#[tokio::test(start_paused = true)]
async fn waiting_lines_for_different_alerts_are_typed_oldest_first() {
    let fake = idle_chief();
    set_chief_state(&fake, EffectiveAgentState::Busy);
    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, Some(CHIEF));

    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    queue.send(WRONG_AGENT_FIRING.into()).await.unwrap();
    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(10)).await;
    set_chief_state(&fake, EffectiveAgentState::Idle);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(fake.keys(), typed(SLOT_IDLE_FIRING));
    busy_then_idle(&fake).await;
    // The repeated slot-idle line was held back, not queued behind wrong-agent.
    busy_then_idle(&fake).await;

    let mut both = typed(SLOT_IDLE_FIRING);
    both.extend(typed(WRONG_AGENT_FIRING));
    assert_eq!(fake.keys(), both);
}

#[tokio::test(start_paused = true)]
async fn a_failed_enter_is_not_typed_again_by_a_repeat() {
    let fake = idle_chief();
    fake.set(|fake| fake.fail_enter = true);
    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, Some(CHIEF));

    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2 * 60)).await;
    fake.set(|fake| fake.fail_enter = false);
    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2 * 60)).await;

    assert_eq!(
        fake.keys(),
        vec![Key::Text("%2".into(), SLOT_IDLE_FIRING.into())]
    );
}

#[tokio::test(start_paused = true)]
async fn a_line_waiting_when_the_queue_closes_is_still_typed() {
    let fake = idle_chief();
    set_chief_state(&fake, EffectiveAgentState::Busy);
    let (queue, _shutdown_tx, handle) = deliver_to(&fake, Some(CHIEF));

    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();
    drop(queue);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(!handle.is_finished());
    set_chief_state(&fake, EffectiveAgentState::Idle);
    tokio::time::sleep(Duration::from_secs(6)).await;

    assert_eq!(fake.keys(), typed(SLOT_IDLE_FIRING));
    // With nothing left to type and no more lines coming, the delivery ends.
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("the delivery ends")
        .unwrap();
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

// --- one writer at a time ---

#[tokio::test]
async fn an_alert_and_a_timed_prompt_at_once_type_two_whole_lines() {
    let fake = idle_chief();
    let view = fake.agents();

    // The fake yields after typing, so without the pane lock the keys would interleave.
    let (alert, prompt) = tokio::join!(
        type_when_ready(&fake, Writer::Alert, CHIEF, SLOT_IDLE_FIRING, &view, &[]),
        type_when_ready(&fake, Writer::TimedPrompt, CHIEF, "digest", &view, &[]),
    );

    assert_eq!(fake.keys(), typed(SLOT_IDLE_FIRING));
    assert!(matches!(alert, Attempt::Typed { .. }));
    // The second saw, on a fresh view inside the lock, the pane the first just typed into.
    assert!(matches!(prompt, Attempt::Wait(Reason::AgentBusy)));

    // Once the pane has been seen busy and is idle again, the second types its whole line.
    set_chief_state(&fake, EffectiveAgentState::Busy);
    let busy = type_when_ready(
        &fake,
        Writer::TimedPrompt,
        CHIEF,
        "digest",
        &fake.agents(),
        &[],
    )
    .await;
    assert!(matches!(busy, Attempt::Wait(Reason::AgentBusy)));
    set_chief_state(&fake, EffectiveAgentState::Idle);
    let prompt = type_when_ready(
        &fake,
        Writer::TimedPrompt,
        CHIEF,
        "digest",
        &fake.agents(),
        &[],
    )
    .await;

    assert!(matches!(prompt, Attempt::Typed { .. }));
    let mut both = typed(SLOT_IDLE_FIRING);
    both.extend(typed("digest"));
    assert_eq!(fake.keys(), both);
}

#[tokio::test(start_paused = true)]
async fn an_alert_posted_as_a_timed_prompt_is_typed_waits_and_follows_it_whole() {
    let fake = idle_chief();
    let (queue, _shutdown_tx, _handle) = deliver_to(&fake, Some(CHIEF));
    queue.send(SLOT_IDLE_FIRING.into()).await.unwrap();

    // The timed prompt takes the pane first; the delivery task runs while it types.
    let prompt = type_when_ready(
        &fake,
        Writer::TimedPrompt,
        CHIEF,
        "digest",
        &fake.agents(),
        &[],
    )
    .await;
    assert!(matches!(prompt, Attempt::Typed { .. }));
    tokio::time::sleep(Duration::from_secs(20)).await;
    // The snapshot still reads idle, and the pane counts as busy for the alert.
    assert_eq!(fake.keys(), typed("digest"));

    set_chief_state(&fake, EffectiveAgentState::Busy);
    tokio::time::sleep(Duration::from_secs(6)).await;
    set_chief_state(&fake, EffectiveAgentState::Idle);
    tokio::time::sleep(Duration::from_secs(6)).await;

    let mut both = typed("digest");
    both.extend(typed(SLOT_IDLE_FIRING));
    assert_eq!(fake.keys(), both);
}

#[tokio::test(start_paused = true)]
async fn a_pane_another_writer_typed_into_is_free_again_after_a_minute() {
    let fake = idle_chief();
    let view = fake.agents();
    let alert = type_when_ready(&fake, Writer::Alert, CHIEF, SLOT_IDLE_FIRING, &view, &[]).await;
    assert!(matches!(alert, Attempt::Typed { .. }));

    tokio::time::sleep(Duration::from_secs(59)).await;
    let early = type_when_ready(&fake, Writer::TimedPrompt, CHIEF, "digest", &view, &[]).await;
    assert!(matches!(early, Attempt::Wait(Reason::AgentBusy)));
    tokio::time::sleep(Duration::from_secs(2)).await;
    let late = type_when_ready(&fake, Writer::TimedPrompt, CHIEF, "digest", &view, &[]).await;

    assert!(matches!(late, Attempt::Typed { .. }));
}

#[tokio::test]
async fn a_phone_reply_and_an_alert_at_once_type_two_whole_lines() {
    let fake = idle_chief();
    let lock = fake.pane_locks().for_pane("%2");
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();

    // The relay types its line, and takes a while before Enter.
    let relay = tokio::task::spawn_blocking({
        let fake = fake.clone();
        move || {
            crate::inbound::tmux::relay_holding_lock(&lock, || {
                fake.set(|state| state.keys.push(Key::Text("%2".into(), "📱 reply".into())));
                started_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(100));
                fake.set(|state| state.keys.push(Key::Enter("%2".into())));
                Ok(())
            })
        }
    });
    started_rx.await.unwrap();
    let alert = type_when_ready(
        &fake,
        Writer::Alert,
        CHIEF,
        SLOT_IDLE_FIRING,
        &fake.agents(),
        &[],
    )
    .await;
    relay.await.unwrap().unwrap();

    // The alert waited for the relay, and then counted the pane as busy.
    assert_eq!(fake.keys(), typed("📱 reply"));
    assert!(matches!(alert, Attempt::Wait(Reason::AgentBusy)));

    // Once the pane has been seen busy and is idle again, the alert types its whole line.
    set_chief_state(&fake, EffectiveAgentState::Busy);
    let view = fake.agents();
    let busy = type_when_ready(&fake, Writer::Alert, CHIEF, SLOT_IDLE_FIRING, &view, &[]).await;
    assert!(matches!(busy, Attempt::Wait(Reason::AgentBusy)));
    set_chief_state(&fake, EffectiveAgentState::Idle);
    let view = fake.agents();
    let alert = type_when_ready(&fake, Writer::Alert, CHIEF, SLOT_IDLE_FIRING, &view, &[]).await;
    assert!(matches!(alert, Attempt::Typed { .. }));
    let mut both = typed("📱 reply");
    both.extend(typed(SLOT_IDLE_FIRING));
    assert_eq!(fake.keys(), both);
}

#[tokio::test]
async fn harolds_own_line_is_typed_into_the_chief_pane() {
    let fake = idle_chief();
    let (queue, queue_rx) = AlertQueue::new();
    let (_shutdown_tx, shutdown_rx) = watch::channel(());
    let _handle = tokio::spawn(deliver(
        fake.clone(),
        Some(CHIEF.into()),
        queue_rx,
        shutdown_rx,
    ));

    queue.firing(DELIVERY_GIVEN_UP);

    wait_until("the line is typed", || fake.keys().len() == 2).await;
    assert_eq!(fake.keys(), typed("harold alert: delivery-given-up firing"));
}

#[tokio::test]
async fn a_full_queue_drops_harolds_own_line_and_logs_it() {
    let logs = CapturedLogs::start();
    let (queue, mut queue_rx) = AlertQueue::new();
    for _ in 0..QUEUE_CAPACITY {
        queue.firing(DELIVERY_GIVEN_UP);
    }

    queue.firing(DELIVERY_GIVEN_UP);

    let text = logs.text();
    assert_eq!(
        text.matches("alert not delivered (queue_full)").count(),
        1,
        "{text}"
    );
    assert!(text.contains("alert=\"delivery-given-up\""), "{text}");
    let (line, _) = queue_rx.recv().await.unwrap();
    assert_eq!(line, "harold alert: delivery-given-up firing");
}

#[test]
fn harolds_own_alerts_are_not_ones_hyperdx_may_post() {
    assert!(!ALERT_NAMES.contains(&DELIVERY_GIVEN_UP));
    assert!(!ALERT_NAMES.contains(&STORE_FAILING));
}

#[test]
fn cleared_queues_the_cleared_line() {
    let (queue, mut queue_rx) = AlertQueue::new();

    queue.cleared(STORE_FAILING);

    let (line, _) = queue_rx.try_recv().unwrap();
    assert_eq!(line, "harold alert: store-failing cleared");
    assert!(queue_rx.try_recv().is_err());
}

// --- what the chief was told ---

const SLOT_IDLE_CLEARED: &str = "harold alert: slot-idle cleared";
const WRONG_AGENT_FIRING: &str = "harold alert: wrong-agent firing";

#[test]
fn a_repeated_firing_is_held_back() {
    let mut desk = Desk::default();
    assert_eq!(desk.take(SLOT_IDLE_FIRING.into()), Ok(()));
    desk.told_oldest();

    assert_eq!(desk.take(SLOT_IDLE_FIRING.into()), Err(HeldBack::Repeat));
    assert_eq!(desk.take(SLOT_IDLE_FIRING.into()), Err(HeldBack::Repeat));
    assert_eq!(desk.oldest(), None);
}

#[test]
fn a_repeat_while_the_line_waits_changes_nothing() {
    let mut desk = Desk::default();
    assert_eq!(desk.take(SLOT_IDLE_FIRING.into()), Ok(()));

    assert_eq!(desk.take(SLOT_IDLE_FIRING.into()), Err(HeldBack::Repeat));
    assert_eq!(desk.oldest(), Some(SLOT_IDLE_FIRING));
}

#[test]
fn firing_cleared_firing_all_wait_in_turn() {
    let mut desk = Desk::default();
    for line in [SLOT_IDLE_FIRING, SLOT_IDLE_CLEARED, SLOT_IDLE_FIRING] {
        assert_eq!(desk.take(line.into()), Ok(()), "{line}");
        assert_eq!(desk.oldest(), Some(line));
        desk.told_oldest();
    }
    assert_eq!(desk.oldest(), None);
}

#[test]
fn a_cleared_never_fired_is_held_back() {
    let mut desk = Desk::default();
    assert_eq!(
        desk.take(SLOT_IDLE_CLEARED.into()),
        Err(HeldBack::NeverFired)
    );
    assert_eq!(desk.oldest(), None);

    // A cleared overtaking its firing before it was typed cancels both.
    let mut desk = Desk::default();
    assert_eq!(desk.take(SLOT_IDLE_FIRING.into()), Ok(()));
    assert_eq!(
        desk.take(SLOT_IDLE_CLEARED.into()),
        Err(HeldBack::NeverFired)
    );
    assert_eq!(desk.oldest(), None);

    // A second cleared after a typed one is a repeat.
    let mut desk = Desk::default();
    for line in [SLOT_IDLE_FIRING, SLOT_IDLE_CLEARED] {
        desk.take(line.into()).unwrap();
        desk.told_oldest();
    }
    assert_eq!(desk.take(SLOT_IDLE_CLEARED.into()), Err(HeldBack::Repeat));
    assert_eq!(desk.oldest(), None);
}

#[test]
fn a_firing_overtaking_a_waiting_cleared_cancels_both() {
    let mut desk = Desk::default();
    desk.take(SLOT_IDLE_FIRING.into()).unwrap();
    desk.told_oldest();
    assert_eq!(desk.take(SLOT_IDLE_CLEARED.into()), Ok(()));

    assert_eq!(desk.take(SLOT_IDLE_FIRING.into()), Err(HeldBack::Repeat));
    assert_eq!(desk.oldest(), None);
    // The chief still believes firing.
    assert_eq!(desk.take(SLOT_IDLE_FIRING.into()), Err(HeldBack::Repeat));
}

#[test]
fn delivery_given_up_waits_each_time_and_merges_while_waiting() {
    let line = "harold alert: delivery-given-up firing";
    let mut desk = Desk::default();
    assert_eq!(desk.take(line.into()), Ok(()));
    assert_eq!(desk.take(line.into()), Err(HeldBack::Repeat));
    desk.told_oldest();
    assert_eq!(desk.oldest(), None);

    assert_eq!(desk.take(line.into()), Ok(()));
    assert_eq!(desk.oldest(), Some(line));
}

#[test]
fn store_failing_is_typed_on_each_change() {
    let mut desk = Desk::default();
    for line in [
        "harold alert: store-failing firing",
        "harold alert: store-failing cleared",
    ] {
        assert_eq!(desk.take(line.into()), Ok(()), "{line}");
        desk.told_oldest();
    }
}

#[test]
fn a_repeat_does_not_move_a_waiting_line_back() {
    let mut desk = Desk::default();
    desk.take(SLOT_IDLE_FIRING.into()).unwrap();
    desk.take(WRONG_AGENT_FIRING.into()).unwrap();

    assert_eq!(desk.take(SLOT_IDLE_FIRING.into()), Err(HeldBack::Repeat));
    assert_eq!(desk.oldest(), Some(SLOT_IDLE_FIRING));
    desk.told_oldest();
    assert_eq!(desk.oldest(), Some(WRONG_AGENT_FIRING));
}

#[test]
fn first_refusal_is_true_once_per_line() {
    let mut desk = Desk::default();
    desk.take(SLOT_IDLE_FIRING.into()).unwrap();
    assert!(desk.first_refusal());
    assert!(!desk.first_refusal());
    desk.told_oldest();

    desk.take(WRONG_AGENT_FIRING.into()).unwrap();
    assert!(desk.first_refusal());
}

#[test]
fn held_back_reasons_have_the_spec_names() {
    assert_eq!(HeldBack::Repeat.as_str(), "repeat");
    assert_eq!(HeldBack::NeverFired.as_str(), "never_fired");
}
