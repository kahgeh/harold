use std::path::PathBuf;
use std::sync::Arc;

use super::WebAppRoute;
use crate::outbound::{AppDelivery, AppRoute, NotPushedReason};
use crate::store::{
    AppPushSubscribed, AppReplyState, AppSender, HaroldStore, append_app_push_subscribed,
    append_app_reply,
};
use crate::web_app::OpenPages;
use crate::web_app::fake_push::FakePushService;
use crate::web_app::push::{Pusher, VapidKey, vapid_key_path};
use std::time::Duration;

const LOGIN: &str = "chief@example.com";

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-route-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A store in `directory` holding `subscriptions`, and the web app's route over it.
async fn route_with(
    directory: &TestDirectory,
    subscriptions: &[AppPushSubscribed],
) -> (Arc<HaroldStore>, Arc<WebAppRoute>) {
    let store = Arc::new(HaroldStore::open(&directory.0).await.unwrap());
    for subscription in subscriptions {
        append_app_push_subscribed(&store, subscription)
            .await
            .unwrap();
    }
    store.project_unhandled_events(100).await.unwrap();
    let key = VapidKey::load_or_create(&vapid_key_path(&directory.0.join("harold.db")))
        .expect("create the VAPID key");
    let pusher = Arc::new(Pusher::new(key, LOGIN));
    let route = Arc::new(WebAppRoute::new(store.clone(), pusher));
    (store, route)
}

/// Calls `deliver` on a blocking thread, as the delivery loop does.
async fn deliver(route: &Arc<WebAppRoute>, event_id: &str, text: &str) -> AppDelivery {
    let route = route.clone();
    let event_id = event_id.to_owned();
    let text = text.to_owned();
    tokio::task::spawn_blocking(move || route.deliver(&event_id, &text))
        .await
        .expect("deliver thread")
        .expect("thread write")
}

#[tokio::test(flavor = "multi_thread")]
async fn deliver_adds_one_thread_row_and_pushes_the_alert_body() {
    let service = FakePushService::start(201).await;
    let directory = TestDirectory::new();
    let (store, route) = route_with(&directory, &[service.subscription(1)]).await;
    // Longer than an alert: the push carries its first 120 characters only.
    let text = "x".repeat(400);

    let delivery = deliver(&route, "event-1", &text).await;

    assert_eq!(delivery, AppDelivery::Pushed);
    let messages = store.app_messages_since(0).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].event_id, "event-1");
    assert_eq!(messages[0].sender, AppSender::Chief);
    assert_eq!(messages[0].text, text);
    assert!(messages[0].at_ms > 0);
    let requests = service.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/push/1");
    assert!(
        requests[0].body.len() < text.len(),
        "the push holds the alert body, not the whole message"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gone_subscription_is_deleted_and_the_message_is_not_pushed() {
    let service = FakePushService::start(410).await;
    let directory = TestDirectory::new();
    let (store, route) = route_with(&directory, &[service.subscription(1)]).await;

    let delivery = deliver(&route, "event-1", "All pods are green").await;

    assert_eq!(
        delivery,
        AppDelivery::NotPushed(NotPushedReason::NoneAccepted)
    );
    assert_eq!(service.requests().len(), 1);
    // Deleted by the AppPushUnsubscribed event the route stored.
    store.project_unhandled_events(100).await.unwrap();
    assert!(store.app_push_subscriptions().await.unwrap().is_empty());
    assert_eq!(store.app_messages_since(0).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn all_pushes_refused_is_not_pushed_none_accepted() {
    let service = FakePushService::start(500).await;
    let directory = TestDirectory::new();
    let (_store, route) = route_with(&directory, &[service.subscription(1)]).await;

    let delivery = deliver(&route, "event-1", "All pods are green").await;

    assert_eq!(
        delivery,
        AppDelivery::NotPushed(NotPushedReason::NoneAccepted)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_subscription_read_is_not_pushed_subscriptions_unread_and_warns() {
    const CANARY: &str = "APP-CANARY-9182";
    let directory = TestDirectory::new();
    let (store, route) = route_with(&directory, &[]).await;
    // Another connection to the state database takes the table away, so the read fails.
    let state = turso::Builder::new_local(directory.0.join("harold-state.db").to_str().unwrap())
        .build()
        .await
        .unwrap();
    state
        .connect()
        .unwrap()
        .execute("DROP TABLE app_push_subscriptions", ())
        .await
        .unwrap();
    assert!(store.app_push_subscriptions().await.is_err());
    let logs = crate::outbound::fake::CapturedLogs::start();

    // On this thread, so that the capture sees the route's lines.
    let delivery = tokio::task::block_in_place(|| route.deliver("event-1", CANARY)).unwrap();

    assert_eq!(
        delivery,
        AppDelivery::NotPushed(NotPushedReason::SubscriptionsUnread)
    );
    assert_eq!(store.app_messages_since(0).await.unwrap().len(), 1);
    let text = logs.text();
    assert!(text.contains("WARN"), "{text}");
    assert!(text.contains("app push subscriptions not read"), "{text}");
    assert!(!text.contains(CANARY), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn one_gone_subscription_is_deleted_and_the_other_push_counts() {
    let gone = FakePushService::start(410).await;
    let live = FakePushService::start(201).await;
    let directory = TestDirectory::new();
    let live_subscription = live.subscription(2);
    let (store, route) = route_with(
        &directory,
        &[gone.subscription(1), live_subscription.clone()],
    )
    .await;

    let delivery = deliver(&route, "event-1", "All pods are green").await;

    assert_eq!(delivery, AppDelivery::Pushed);
    assert_eq!(gone.requests().len(), 1);
    assert_eq!(live.requests().len(), 1);
    store.project_unhandled_events(100).await.unwrap();
    assert_eq!(
        store.app_push_subscriptions().await.unwrap(),
        [live_subscription]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_subscription_is_not_pushed_but_the_thread_keeps_the_message() {
    let directory = TestDirectory::new();
    let (store, route) = route_with(&directory, &[]).await;

    let delivery = deliver(&route, "event-1", "All pods are green").await;

    assert_eq!(
        delivery,
        AppDelivery::NotPushed(NotPushedReason::NoSubscriptions)
    );
    let messages = store.app_messages_since(0).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text, "All pods are green");
}

#[tokio::test(flavor = "multi_thread")]
async fn delivering_twice_keeps_one_row() {
    let service = FakePushService::start(201).await;
    let directory = TestDirectory::new();
    let (store, route) = route_with(&directory, &[service.subscription(1)]).await;

    deliver(&route, "event-1", "All pods are green").await;
    deliver(&route, "event-1", "All pods are green, again").await;

    let messages = store.app_messages_since(0).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text, "All pods are green");
}

#[tokio::test(flavor = "multi_thread")]
async fn set_reply_state_relabels_the_user_row() {
    let directory = TestDirectory::new();
    let (store, route) = route_with(&directory, &[]).await;
    append_app_reply(&store, "on it?").await.unwrap();
    store.project_unhandled_events(100).await.unwrap();
    let reply_id = store.app_messages_since(0).await.unwrap()[0]
        .event_id
        .clone();

    for state in [AppReplyState::WaitingChiefBusy, AppReplyState::Typed] {
        let route = route.clone();
        let event_id = reply_id.clone();
        tokio::task::spawn_blocking(move || route.set_reply_state(&event_id, state))
            .await
            .expect("set_reply_state thread")
            .expect("state write");

        let messages = store.app_messages_since(0).await.unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].sender, AppSender::User);
        assert_eq!(messages[0].text, "on it?");
        assert_eq!(messages[0].state, Some(state));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn settle_waiting_replies_types_the_waiting_rows() {
    let directory = TestDirectory::new();
    let (store, route) = route_with(&directory, &[]).await;
    append_app_reply(&store, "on it?").await.unwrap();
    store.project_unhandled_events(100).await.unwrap();
    let reply_id = store.app_messages_since(0).await.unwrap()[0]
        .event_id
        .clone();
    store
        .set_app_reply_state(&reply_id, AppReplyState::WaitingChiefBusy)
        .await
        .unwrap();

    let settled = tokio::task::spawn_blocking({
        let route = route.clone();
        move || route.settle_waiting_replies(i64::MAX)
    })
    .await
    .expect("settle thread")
    .expect("settle write");

    assert_eq!(settled, [reply_id]);
    let messages = store.app_messages_since(0).await.unwrap();
    assert_eq!(messages[0].state, Some(AppReplyState::Typed));
}

/// The route reading `pages`, with one subscription at `service`.
async fn route_over(
    directory: &TestDirectory,
    service: &FakePushService,
    pages: &Arc<OpenPages>,
) -> (Arc<HaroldStore>, Arc<WebAppRoute>) {
    let (store, route) = route_with(directory, &[service.subscription(1)]).await;
    let route = Arc::new(
        Arc::try_unwrap(route)
            .ok()
            .unwrap()
            .with_pages(pages.clone()),
    );
    (store, route)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_visible_page_shows_the_message_and_no_push_goes_out() {
    let service = FakePushService::start(201).await;
    let directory = TestDirectory::new();
    let pages = Arc::new(OpenPages::default());
    let (store, route) = route_over(&directory, &service, &pages).await;
    // Three streams: no other test reports three pages, so its record is this one's.
    let _open = [pages.open(), pages.open(), pages.open()];
    pages.report_seen();
    crate::telemetry::test_support::install();

    let delivery = deliver(&route, "event-1", "hello").await;

    assert_eq!(delivery, AppDelivery::Shown);
    assert_eq!(store.app_messages_since(0).await.unwrap().len(), 1);
    assert!(service.requests().is_empty());
    let skipped: Vec<_> = crate::telemetry::test_support::logs()
        .into_iter()
        .filter(|log| {
            let record = format!("{:?}", log.record);
            record.contains("app push skipped")
                && record.contains("page_visible")
                && record.contains("pages")
                && record.contains("Int(3)")
        })
        .collect();
    assert_eq!(
        skipped.len(),
        1,
        "one exported skip line with reason and pages"
    );
    assert!(!format!("{:?}", skipped[0].record).contains("hello"));
}

#[tokio::test(flavor = "multi_thread")]
async fn no_page_or_a_hidden_page_pushes_as_before() {
    let service = FakePushService::start(201).await;
    let directory = TestDirectory::new();
    let pages = Arc::new(OpenPages::default());
    let (_store, route) = route_over(&directory, &service, &pages).await;

    assert_eq!(deliver(&route, "event-1", "a").await, AppDelivery::Pushed);

    let page = pages.open();
    pages.report_hidden();
    assert_eq!(deliver(&route, "event-2", "b").await, AppDelivery::Pushed);
    assert_eq!(service.requests().len(), 2);

    drop(page);
    assert_eq!(deliver(&route, "event-3", "c").await, AppDelivery::Pushed);
}

/// A clock the test moves by hand.
struct TestClock(Arc<std::sync::Mutex<std::time::Instant>>);

impl TestClock {
    fn new() -> Self {
        Self(Arc::new(std::sync::Mutex::new(std::time::Instant::now())))
    }

    fn pages(&self) -> Arc<OpenPages> {
        let now = self.0.clone();
        Arc::new(OpenPages::with_clock(Arc::new(move || {
            *now.lock().unwrap()
        })))
    }

    fn pass(&self, seconds: u64) {
        *self.0.lock().unwrap() += Duration::from_secs(seconds);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_opened_after_the_hidden_report_waits_to_hear_from_the_page() {
    let service = FakePushService::start(201).await;
    let directory = TestDirectory::new();
    let clock = TestClock::new();
    let pages = clock.pages();
    let (_store, route) = route_over(&directory, &service, &pages).await;
    let _first = pages.open();
    pages.report_seen();
    pages.report_hidden();
    let _second = pages.open();

    assert_eq!(pages.visible(), 0);
    assert_eq!(deliver(&route, "event-1", "a").await, AppDelivery::Pushed);
    assert_eq!(service.requests().len(), 1);

    pages.report_seen();
    assert_eq!(
        pages.visible(),
        1,
        "only the stream opened after the report"
    );
    assert_eq!(deliver(&route, "event-2", "b").await, AppDelivery::Shown);
    assert_eq!(service.requests().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_stream_stops_holding_pushes_back_after_75_seconds() {
    let service = FakePushService::start(201).await;
    let directory = TestDirectory::new();
    let clock = TestClock::new();
    let pages = clock.pages();
    let (_store, route) = route_over(&directory, &service, &pages).await;
    let _stale = pages.open();
    pages.report_seen();

    clock.pass(75);
    assert_eq!(pages.visible(), 1, "75 s is still within the limit");
    clock.pass(1);
    assert_eq!(pages.visible(), 0);
    assert_eq!(deliver(&route, "event-1", "a").await, AppDelivery::Pushed);

    pages.report_seen();
    assert_eq!(pages.visible(), 1, "a beat brings it back");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_brief_stream_elsewhere_leaves_the_stale_stream_not_visible() {
    let clock = TestClock::new();
    let pages = clock.pages();
    // The locked phone's stream: reported hidden, but its connection stays open.
    let _stale = pages.open();
    pages.report_seen();
    pages.report_hidden();

    // A page opened briefly elsewhere is on screen while it lasts, the stale stream not.
    let brief = pages.open();
    pages.report_seen();
    assert_eq!(pages.visible(), 1);
    drop(brief);
    assert_eq!(
        pages.visible(),
        0,
        "the stale stream does not take its place"
    );
}
