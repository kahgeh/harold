use std::path::PathBuf;
use std::sync::Arc;

use super::WebAppRoute;
use crate::outbound::{AppDelivery, AppRoute, NotPushedReason};
use crate::store::{
    AppPushSubscribed, AppReplyState, AppSender, HaroldStore, append_app_push_subscribed,
    append_app_reply,
};
use crate::web_app::fake_push::FakePushService;
use crate::web_app::push::{Pusher, VapidKey, vapid_key_path};

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
    let state = turso::Builder::new_local(
        directory
            .0
            .join("harold-state.db")
            .to_str()
            .unwrap(),
    )
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
