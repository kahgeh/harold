//! One-time reminders, under paused time: when each is typed, dropped or cancelled, and
//! that a reminder and a watch never share a name.

use std::sync::Arc;
use std::time::Duration;

use harold_api::tools::watch::v1::RemindRequest;
use harold_api::tools::watch::v1::remind_request::When;
use time::UtcOffset;
use tokio::sync::watch as signal;

use super::*;
use crate::agent::domain::EffectiveAgentState;
use crate::outbound::fake::CapturedLogs;
use crate::settings::WatchSettings;
use crate::telemetry::test_support as telemetry_test;
use crate::tools::watch::clock::{self, clock_ms, day_bounds, jump_wall_clock};
use crate::tools::watch::scheduler::tests::{Harness, build_on, harness_on, pass, settings};
use crate::tools::watch::scheduler::{RemindRefusal, Scheduler, StartRefusal, StopRefusal};
use crate::tools::watch::store::ReminderState;
use crate::tools::watch::test_support::{FakeRunner, ScriptedOffset, agent};
use crate::tools::watch::validate::{CheckedRemind, ReportTo, check_remind};

const CHIEF_PANE: &str = "%1";
const SELF_PANE: &str = "%2";
const MINUTE: Duration = Duration::from_secs(60);
const MINUTE_MS: i64 = 60_000;

fn offset(hours: i8) -> UtcOffset {
    UtcOffset::from_hms(hours, 0, 0).unwrap()
}

async fn harness_at(local: &Arc<ScriptedOffset>) -> Harness {
    harness_on(settings(), vec![], local).await
}

async fn harness_utc() -> (Harness, Arc<ScriptedOffset>) {
    let local = ScriptedOffset::at(UtcOffset::UTC);
    (harness_at(&local).await, local)
}

fn remind_in(h: &Harness, name: &str, minutes: u32, text: &str) -> CheckedRemind {
    CheckedRemind {
        name: name.into(),
        folder: h.folder.0.clone(),
        report_to: ReportTo::Chief,
        in_minutes: Some(minutes),
        due_local: None,
        cron: None,
        text: text.into(),
    }
}

/// A `--at` reminder, checked as the service checks it, against the harness's clock.
async fn remind_at(h: &Harness, name: &str, at: &str) -> CheckedRemind {
    let now = h.scheduler.local_now().await;
    check_remind(
        &RemindRequest {
            name: name.into(),
            folder: h.folder.0.to_string_lossy().into_owned(),
            when: Some(When::AtLocal(at.into())),
            report_to: "chief".into(),
            text: "x".into(),
        },
        &now,
    )
    .unwrap()
}

/// Moves the test clock to 00:01 local time at `offset` of the next day. Returns the
/// start of that local day.
async fn to_one_past_midnight(offset: UtcOffset) -> i64 {
    let (_, next_midnight) = day_bounds(clock_ms(), offset);
    pass(Duration::from_millis(
        (next_midnight + MINUTE_MS - clock_ms()) as u64,
    ))
    .await;
    next_midnight
}

async fn state_of(h: &Harness, name: &str) -> ReminderState {
    h.store.reminder(name).await.unwrap().unwrap().state
}

/// A second scheduler on the first one's store, as after a Harold restart.
fn restarted(h: &Harness, local: &Arc<ScriptedOffset>) -> (Arc<Scheduler>, signal::Sender<()>) {
    let runner = FakeRunner::new(vec![]);
    let (stop, shutdown) = signal::channel(());
    let scheduler = build_on(
        local,
        &h.store,
        &runner,
        &h.typing,
        &h.agents,
        &h.chief,
        &h.pods,
        settings(),
        shutdown,
    );
    (scheduler, stop)
}

/// Harold stops: every task ends, and the store keeps the reminders.
async fn shut_down(h: &Harness) {
    let _ = h.stop.send(());
    pass(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn typed_once_at_in_time() {
    let (h, _local) = harness_utc().await;
    let self_folder = h.folder.0.to_string_lossy().into_owned();
    h.agents
        .agents
        .lock()
        .unwrap()
        .push(agent(SELF_PANE, &self_folder, EffectiveAgentState::Idle));
    let before = clock_ms();
    let due = h
        .scheduler
        .remind(remind_in(
            &h,
            "release-check",
            45,
            "check the v0.75.0 release",
        ))
        .await
        .unwrap();
    assert_eq!(due - before, 45 * MINUTE_MS);
    let mut to_self = remind_in(&h, "own", 45, "look at the logs");
    to_self.report_to = ReportTo::SelfPane;
    h.scheduler.remind(to_self).await.unwrap();

    pass(44 * MINUTE + Duration::from_secs(59)).await;
    assert!(h.typing.typed().is_empty(), "{:?}", h.typing.typed());

    pass(Duration::from_secs(2)).await;
    let lines = h.typing.lines.lock().unwrap().clone();
    assert_eq!(
        lines,
        [
            (
                CHIEF_PANE.to_string(),
                "REMIND release-check: check the v0.75.0 release".to_string()
            ),
            (
                SELF_PANE.to_string(),
                "REMIND own: look at the logs".to_string()
            ),
        ]
    );

    pass(3 * 60 * MINUTE).await;
    assert_eq!(h.typing.typed().len(), 2, "each reminder is typed once");
    assert_eq!(state_of(&h, "release-check").await, ReminderState::Typed);
    assert_eq!(state_of(&h, "own").await, ReminderState::Typed);
    assert!(h.scheduler.list().await.unwrap().reminders.is_empty());
}

#[tokio::test(start_paused = true)]
async fn at_today_and_at_tomorrow() {
    let local = ScriptedOffset::at(offset(11));
    let h = harness_at(&local).await;
    let day_start = to_one_past_midnight(offset(11)).await;

    // 00:31 is still ahead today; 00:00 has passed, so it is tomorrow.
    let today = h
        .scheduler
        .remind(remind_at(&h, "today", "00:31").await)
        .await
        .unwrap();
    let tomorrow = h
        .scheduler
        .remind(remind_at(&h, "tomorrow", "00:00").await)
        .await
        .unwrap();
    assert_eq!(today, day_start + 31 * MINUTE_MS);
    assert_eq!(tomorrow, day_start + 24 * 60 * MINUTE_MS);
    assert_eq!(
        h.store.reminder("today").await.unwrap().unwrap().due_local,
        Some(clock::local_text(today, offset(11))[..16].to_owned())
    );

    pass(29 * MINUTE).await;
    assert!(h.typing.typed().is_empty());
    pass(2 * MINUTE).await;
    assert_eq!(h.typing.typed(), ["REMIND today: x"]);

    // From 00:32 to 23:58.
    pass(23 * 60 * MINUTE + 26 * MINUTE).await;
    assert_eq!(h.typing.typed(), ["REMIND today: x"], "not yet 00:00");
    pass(3 * MINUTE).await;
    assert_eq!(h.typing.typed(), ["REMIND today: x", "REMIND tomorrow: x"]);
}

#[tokio::test(start_paused = true)]
async fn waits_while_the_pane_is_busy_then_types() {
    let (h, _local) = harness_utc().await;
    h.scheduler
        .remind(remind_in(&h, "r", 10, "look"))
        .await
        .unwrap();
    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Working);

    pass(40 * MINUTE).await;
    assert!(h.typing.typed().is_empty());
    assert_eq!(state_of(&h, "r").await, ReminderState::Waiting);

    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Idle);
    pass(Duration::from_secs(6)).await;
    assert_eq!(h.typing.typed(), ["REMIND r: look"]);
    assert_eq!(state_of(&h, "r").await, ReminderState::Typed);
    pass(60 * MINUTE).await;
    assert_eq!(h.typing.typed().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn dropped_after_120_minutes() {
    let (h, local) = harness_utc().await;
    h.scheduler
        .remind(remind_in(&h, "r", 10, "look"))
        .await
        .unwrap();
    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Working);

    pass(10 * MINUTE + 119 * MINUTE).await;
    assert_eq!(state_of(&h, "r").await, ReminderState::Waiting);
    pass(2 * MINUTE).await;
    assert_eq!(state_of(&h, "r").await, ReminderState::Dropped);
    assert!(h.scheduler.list().await.unwrap().reminders.is_empty());

    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Idle);
    pass(10 * MINUTE).await;
    assert!(
        h.typing.typed().is_empty(),
        "a dropped reminder is never typed"
    );

    // A restart after the 120 minutes drops a waiting reminder at once.
    h.scheduler
        .remind(remind_in(&h, "late", 10, "look"))
        .await
        .unwrap();
    shut_down(&h).await;
    pass(10 * MINUTE + 121 * MINUTE).await;
    let (scheduler, _stop) = restarted(&h, &local);
    scheduler.resume().await;
    pass(Duration::from_secs(10)).await;
    assert_eq!(state_of(&h, "late").await, ReminderState::Dropped);
    assert!(h.typing.typed().is_empty());
}

#[tokio::test(start_paused = true)]
async fn typed_late_after_a_restart_inside_the_window_and_only_once() {
    let (h, local) = harness_utc().await;
    h.scheduler
        .remind(remind_in(&h, "r", 10, "look"))
        .await
        .unwrap();
    pass(5 * MINUTE).await;
    shut_down(&h).await;
    // Harold is down from 5 to 45 minutes: the reminder was due at 10.
    pass(40 * MINUTE).await;
    assert!(h.typing.typed().is_empty());

    let (scheduler, stop) = restarted(&h, &local);
    scheduler.resume().await;
    pass(Duration::from_secs(5)).await;
    assert_eq!(h.typing.typed(), ["REMIND r: look"]);
    assert_eq!(state_of(&h, "r").await, ReminderState::Typed);

    // A second restart types nothing more.
    let _ = stop.send(());
    pass(Duration::from_secs(1)).await;
    let (again, _stop) = restarted(&h, &local);
    again.resume().await;
    pass(30 * MINUTE).await;
    assert_eq!(h.typing.typed(), ["REMIND r: look"]);
}

#[tokio::test(start_paused = true)]
async fn stop_cancels_a_waiting_reminder_and_types_nothing() {
    let (h, _local) = harness_utc().await;
    h.scheduler
        .remind(remind_in(&h, "r", 10, "look"))
        .await
        .unwrap();
    pass(MINUTE).await;

    h.scheduler.stop("r").await.unwrap();
    assert_eq!(state_of(&h, "r").await, ReminderState::Cancelled);
    assert!(h.scheduler.list().await.unwrap().reminders.is_empty());
    pass(30 * MINUTE).await;
    assert!(h.typing.typed().is_empty());

    assert_eq!(
        h.scheduler.stop("r").await,
        Err(StopRefusal::UnknownWatch),
        "nothing waits under that name now"
    );
    // A cancelled reminder frees its name.
    h.scheduler
        .remind(remind_in(&h, "r", 5, "again"))
        .await
        .unwrap();
    pass(6 * MINUTE).await;
    assert_eq!(h.typing.typed(), ["REMIND r: again"]);
}

#[tokio::test(start_paused = true)]
async fn a_name_in_use_is_refused_both_ways() {
    let budget = WatchSettings {
        daily_budget_usd: Some(1.0),
        ..settings()
    };
    let local = ScriptedOffset::at(UtcOffset::UTC);
    let h = harness_on(budget, vec![], &local).await;

    // A running watch holds its name.
    h.start("w", 10, 120).await;
    assert_eq!(
        h.scheduler.remind(remind_in(&h, "w", 10, "x")).await,
        Err(RemindRefusal::NameInUse)
    );
    // A paused watch holds it too.
    h.scheduler
        .pause(crate::tools::watch::validate::CheckedPause {
            target: crate::tools::watch::validate::Target::Name("w".into()),
            for_minutes: None,
            caller: crate::tools::watch::validate::Caller::Cli,
        })
        .await
        .unwrap();
    assert_eq!(
        h.scheduler.remind(remind_in(&h, "w", 10, "x")).await,
        Err(RemindRefusal::NameInUse)
    );

    // A waiting reminder holds its name against a watch and another reminder.
    h.scheduler
        .remind(remind_in(&h, "r", 10, "x"))
        .await
        .unwrap();
    assert_eq!(
        h.scheduler.start(h.checked("r", 10, 120)).await,
        Err(StartRefusal::NameInUse)
    );
    assert_eq!(
        h.scheduler.remind(remind_in(&h, "r", 20, "y")).await,
        Err(RemindRefusal::NameInUse)
    );
    assert!(h.store.get("r").await.unwrap().is_none(), "no watch stored");

    // A stopped watch and a typed reminder free their names.
    h.scheduler.stop("w").await.unwrap();
    h.scheduler
        .remind(remind_in(&h, "w", 30, "x"))
        .await
        .unwrap();
    pass(11 * MINUTE).await;
    assert_eq!(state_of(&h, "r").await, ReminderState::Typed);
    h.start("r", 10, 120).await;
}

#[tokio::test(start_paused = true)]
async fn list_shows_each_waiting_reminder_with_its_time() {
    let (h, _local) = harness_utc().await;
    let now = clock_ms();
    h.scheduler
        .remind(remind_in(&h, "first", 10, "x"))
        .await
        .unwrap();
    let mut second = remind_in(&h, "second", 30, "y");
    second.report_to = ReportTo::SelfPane;
    h.scheduler.remind(second).await.unwrap();

    let listed = h.scheduler.list().await.unwrap().reminders;
    assert_eq!(
        listed,
        [
            ReminderInfo {
                name: "first".into(),
                report_to: "chief".into(),
                due_at_ms: now + 10 * MINUTE_MS,
                cron: None,
            },
            ReminderInfo {
                name: "second".into(),
                report_to: "self".into(),
                due_at_ms: now + 30 * MINUTE_MS,
                cron: None,
            },
        ]
    );

    pass(11 * MINUTE).await;
    let listed = h.scheduler.list().await.unwrap().reminders;
    assert_eq!(
        listed.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["second"]
    );
}

#[tokio::test(start_paused = true)]
async fn at_follows_a_changed_offset() {
    let local = ScriptedOffset::at(offset(11));
    let h = harness_at(&local).await;
    let day_start = to_one_past_midnight(offset(11)).await;
    let due = h
        .scheduler
        .remind(remind_at(&h, "r", "02:00").await)
        .await
        .unwrap();
    assert_eq!(due, day_start + 120 * MINUTE_MS);

    // One hour later the clock goes back an hour (+11:00 to +10:00): 01:01 reads 00:01.
    pass(60 * MINUTE).await;
    local.set(offset(10));
    let listed = h.scheduler.list().await.unwrap().reminders;
    assert_eq!(listed[0].due_at_ms, due + 60 * MINUTE_MS);

    // 02:00 at +10:00 is 119 minutes away now, not 59.
    pass(118 * MINUTE).await;
    assert!(h.typing.typed().is_empty(), "{:?}", h.typing.typed());
    pass(2 * MINUTE).await;
    assert_eq!(h.typing.typed(), ["REMIND r: x"]);
}

#[tokio::test(start_paused = true)]
async fn at_in_a_daylight_saving_gap_is_due_at_the_end_of_the_gap() {
    let local = ScriptedOffset::at(offset(10));
    let h = harness_at(&local).await;
    to_one_past_midnight(offset(10)).await;
    h.scheduler
        .remind(remind_at(&h, "gap", "02:30").await)
        .await
        .unwrap();

    // At 02:00 +10:00 the clock goes forward to 03:00 +11:00: 02:30 never comes.
    pass(119 * MINUTE).await;
    local.set(offset(11));
    assert!(h.typing.typed().is_empty());
    pass(2 * MINUTE).await;
    assert_eq!(h.typing.typed(), ["REMIND gap: x"]);
    assert_eq!(state_of(&h, "gap").await, ReminderState::Typed);
}

#[tokio::test(start_paused = true)]
async fn no_text_in_the_log_lines() {
    let logs = CapturedLogs::start();
    let (h, _local) = harness_utc().await;
    let secret = format!("SECRET-{}", uuid::Uuid::new_v4().simple());
    let name = |what: &str| format!("{what}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let (typed, dropped, cancelled) = (name("typed"), name("dropped"), name("cancelled"));

    h.scheduler
        .remind(remind_in(&h, &typed, 1, &secret))
        .await
        .unwrap();
    pass(2 * MINUTE).await;
    h.scheduler
        .remind(remind_in(&h, &cancelled, 10, &secret))
        .await
        .unwrap();
    h.scheduler.stop(&cancelled).await.unwrap();
    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Working);
    h.scheduler
        .remind(remind_in(&h, &dropped, 1, &secret))
        .await
        .unwrap();
    pass(125 * MINUTE).await;

    let text = logs.text();
    assert!(!text.contains(&secret), "{text}");
    let line = |message: &str, reminder: &str| -> String {
        text.lines()
            .find(|line| {
                line.contains(message) && line.contains(&format!("reminder=\"{reminder}\""))
            })
            .unwrap_or_else(|| panic!("no {message} for {reminder}:\n{text}"))
            .to_owned()
    };
    for reminder in [&typed, &dropped, &cancelled] {
        assert!(line("reminder set", reminder).contains("due_at="));
    }
    assert!(line("reminder typed", &typed).contains("late_s=0"));
    assert!(line("reminder ended", &dropped).contains("reason=\"dropped\""));
    assert!(line("reminder ended", &cancelled).contains("reason=\"cancelled\""));
}

#[test]
fn too_late_starts_at_exactly_120_minutes() {
    let due = 1_000_000;
    assert!(!too_late(due + 120 * MINUTE_MS - 1, due));
    assert!(too_late(due + 120 * MINUTE_MS, due));
    assert!(!too_late(due, due));
}

/// Stops Harold, lets the clock reach `at`, and restarts it.
async fn restart_at(h: &Harness, local: &Arc<ScriptedOffset>, at: i64) -> signal::Sender<()> {
    shut_down(h).await;
    pass(Duration::from_millis((at - clock_ms()) as u64)).await;
    let (scheduler, stop) = restarted(h, local);
    scheduler.resume().await;
    pass(Duration::from_secs(1)).await;
    stop
}

#[tokio::test(start_paused = true)]
async fn a_restart_at_exactly_120_minutes_drops_it_and_one_second_before_types_it() {
    let (h, local) = harness_utc().await;
    let due = h
        .scheduler
        .remind(remind_in(&h, "r", 10, "look"))
        .await
        .unwrap();
    let _stop = restart_at(&h, &local, due + 120 * MINUTE_MS).await;
    assert_eq!(state_of(&h, "r").await, ReminderState::Dropped);
    assert!(h.typing.typed().is_empty());

    let (h, local) = harness_utc().await;
    let due = h
        .scheduler
        .remind(remind_in(&h, "r", 10, "look"))
        .await
        .unwrap();
    let _stop = restart_at(&h, &local, due + 120 * MINUTE_MS - 1_000).await;
    assert_eq!(state_of(&h, "r").await, ReminderState::Typed);
    assert_eq!(h.typing.typed(), ["REMIND r: look"]);
}

#[tokio::test(start_paused = true)]
async fn a_sleep_while_the_pane_is_busy_past_the_window_drops_it() {
    let (h, _local) = harness_utc().await;
    h.scheduler
        .remind(remind_in(&h, "r", 10, "look"))
        .await
        .unwrap();
    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Working);
    // Half-way between two of the task's own wake-ups, so only the slot can see the jump.
    pass(11 * MINUTE + Duration::from_secs(30)).await;
    assert_eq!(state_of(&h, "r").await, ReminderState::Waiting);

    // The computer sleeps three hours: the wall clock jumps, tokio's clock does not.
    jump_wall_clock(3 * 60 * MINUTE_MS);
    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Idle);
    pass(Duration::from_secs(6)).await;

    assert!(h.typing.typed().is_empty(), "{:?}", h.typing.typed());
    assert_eq!(state_of(&h, "r").await, ReminderState::Dropped);
}

#[tokio::test(start_paused = true)]
async fn a_typing_call_in_progress_at_shutdown_ends_typed_and_is_not_typed_again() {
    let (h, local) = harness_utc().await;
    h.scheduler
        .remind(remind_in(&h, "r", 10, "look"))
        .await
        .unwrap();
    *h.typing.delay.lock().unwrap() = Duration::from_secs(10);
    // The typing call starts at 10 minutes and takes 10 seconds.
    pass(10 * MINUTE + Duration::from_secs(2)).await;
    assert!(h.typing.typed().is_empty());

    shut_down(&h).await;
    pass(Duration::from_secs(30)).await;
    assert_eq!(h.typing.typed(), ["REMIND r: look"]);
    assert_eq!(state_of(&h, "r").await, ReminderState::Typed);

    *h.typing.delay.lock().unwrap() = Duration::ZERO;
    let (scheduler, _stop) = restarted(&h, &local);
    scheduler.resume().await;
    pass(30 * MINUTE).await;
    assert_eq!(h.typing.typed(), ["REMIND r: look"], "typed once only");
}

#[tokio::test(start_paused = true)]
async fn the_reminders_counter_is_registered_for_each_outcome_and_goes_up_with_each() {
    let count = |outcome: &str| {
        telemetry_test::exact_counter_value("watch.reminders", &[("outcome", outcome)])
    };
    let (h, _local) = harness_utc().await;
    let mut before = Vec::new();
    for outcome in ["typed", "dropped", "cancelled"] {
        let value = count(outcome);
        assert!(value.is_some(), "{outcome} is registered");
        before.push(value.unwrap());
    }

    h.scheduler
        .remind(remind_in(&h, "typed", 1, "x"))
        .await
        .unwrap();
    pass(2 * MINUTE).await;
    h.scheduler
        .remind(remind_in(&h, "cancelled", 10, "x"))
        .await
        .unwrap();
    h.scheduler.stop("cancelled").await.unwrap();
    h.agents.set_state(CHIEF_PANE, EffectiveAgentState::Working);
    h.scheduler
        .remind(remind_in(&h, "dropped", 1, "x"))
        .await
        .unwrap();
    pass(125 * MINUTE).await;

    for (outcome, before) in ["typed", "dropped", "cancelled"].into_iter().zip(before) {
        assert!(count(outcome).unwrap() > before, "{outcome}");
    }
}

// ---------------------------------------------------------------------------
// Cron reminders
// ---------------------------------------------------------------------------

/// A `--cron` reminder, checked as the service checks it, against the harness's clock.
async fn remind_cron(h: &Harness, name: &str, cron: &str) -> CheckedRemind {
    let now = h.scheduler.local_now().await;
    check_remind(
        &RemindRequest {
            name: name.into(),
            folder: h.folder.0.to_string_lossy().into_owned(),
            when: Some(When::Cron(cron.into())),
            report_to: "chief".into(),
            text: "x".into(),
        },
        &now,
    )
    .unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_cron_reminder_types_once_at_the_next_match_then_ends() {
    let logs = CapturedLogs::start();
    let (h, _local) = harness_utc().await;
    let day_start = to_one_past_midnight(UtcOffset::UTC).await;
    let name = format!("digest-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);

    let due = h
        .scheduler
        .remind(remind_cron(&h, &name, "30 0 * * *").await)
        .await
        .unwrap();
    assert_eq!(due, day_start + 30 * MINUTE_MS, "00:30 today");
    let stored = h.store.reminder(&name).await.unwrap().unwrap();
    assert_eq!(stored.cron.as_deref(), Some("30 0 * * *"));
    assert_eq!(
        stored.due_local,
        Some(clock::local_text(due, UtcOffset::UTC)[..16].to_owned())
    );

    pass(28 * MINUTE).await;
    assert!(h.typing.typed().is_empty());
    pass(2 * MINUTE).await;
    assert_eq!(h.typing.typed(), [format!("REMIND {name}: x")]);
    assert_eq!(state_of(&h, &name).await, ReminderState::Typed);

    // One shot: the next day's 00:30 types nothing.
    pass(25 * 60 * MINUTE).await;
    assert_eq!(h.typing.typed().len(), 1);
    assert!(h.scheduler.list().await.unwrap().reminders.is_empty());
    let text = logs.text();
    let typed = text
        .lines()
        .find(|line| line.contains("reminder typed") && line.contains(&name))
        .unwrap_or_else(|| panic!("no reminder typed:\n{text}"));
    assert!(
        typed.contains("late_s=0"),
        "late_s from the matched minute: {typed}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_cron_reminder_past_120_minutes_is_dropped() {
    let logs = CapturedLogs::start();
    let (h, local) = harness_utc().await;
    let day_start = to_one_past_midnight(UtcOffset::UTC).await;
    let name = format!("late-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    h.scheduler
        .remind(remind_cron(&h, &name, "30 0 * * *").await)
        .await
        .unwrap();

    // Harold is down from 00:01 to 02:31: 121 minutes after the match.
    let _stop = restart_at(&h, &local, day_start + 151 * MINUTE_MS).await;
    pass(Duration::from_secs(10)).await;

    assert_eq!(state_of(&h, &name).await, ReminderState::Dropped);
    assert!(h.typing.typed().is_empty());
    let text = logs.text();
    assert!(
        text.lines().any(|line| line.contains("reminder ended")
            && line.contains(&name)
            && line.contains("reason=\"dropped\"")
            && line.contains("schedule=\"cron\"")),
        "{text}"
    );
}

#[tokio::test(start_paused = true)]
async fn each_reminder_form_logs_its_schedule_with_late_s_and_missed_0() {
    let logs = CapturedLogs::start();
    let (h, _local) = harness_utc().await;
    to_one_past_midnight(UtcOffset::UTC).await;
    let name = |what: &str| format!("{what}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let (by_in, by_at, by_cron) = (name("in"), name("at"), name("cron"));
    h.scheduler
        .remind(remind_in(&h, &by_in, 10, "x"))
        .await
        .unwrap();
    h.scheduler
        .remind(remind_at(&h, &by_at, "00:21").await)
        .await
        .unwrap();
    h.scheduler
        .remind(remind_cron(&h, &by_cron, "31 0 * * *").await)
        .await
        .unwrap();

    pass(35 * MINUTE).await;

    let text = logs.text();
    for (reminder, schedule) in [(&by_in, "in"), (&by_at, "at"), (&by_cron, "cron")] {
        let typed = text
            .lines()
            .find(|line| {
                line.contains("reminder typed")
                    && line.contains(&format!("reminder=\"{reminder}\""))
            })
            .unwrap_or_else(|| panic!("no reminder typed for {reminder}:\n{text}"));
        for field in [
            format!("schedule=\"{schedule}\""),
            " missed=0".into(),
            " late_s=0".into(),
        ] {
            assert!(typed.contains(&field), "{field}: {typed}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn list_shows_the_cron_of_a_cron_reminder() {
    let (h, _local) = harness_utc().await;
    let day_start = to_one_past_midnight(UtcOffset::UTC).await;
    h.scheduler
        .remind(remind_cron(&h, "digest", "30 8 * * *").await)
        .await
        .unwrap();
    h.scheduler
        .remind(remind_in(&h, "plain", 10, "x"))
        .await
        .unwrap();

    let listed = h.scheduler.list().await.unwrap().reminders;
    let by_name = |name: &str| listed.iter().find(|r| r.name == name).unwrap().clone();
    assert_eq!(by_name("digest").cron.as_deref(), Some("30 8 * * *"));
    assert_eq!(by_name("digest").due_at_ms, day_start + 510 * MINUTE_MS);
    assert_eq!(by_name("plain").cron, None);
}
