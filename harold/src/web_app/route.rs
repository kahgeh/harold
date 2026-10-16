//! The web app as the delivery loop's app route: a chief's message is added to the
//! app's thread and pushed to every phone that turned alerts on.

use std::sync::Arc;

use time::OffsetDateTime;

use super::push::{PushOutcome, Pusher, alert_body};
use crate::outbound::{AppDelivery, AppRoute};
use crate::store::{AppPushUnsubscribed, AppReplyState, HaroldStore, append_app_push_unsubscribed};

pub(crate) struct WebAppRoute {
    store: Arc<HaroldStore>,
    pusher: Arc<Pusher>,
    /// The daemon's runtime. The delivery loop calls the route from `spawn_blocking`, so
    /// store calls run on it with `block_on`.
    runtime: tokio::runtime::Handle,
}

impl WebAppRoute {
    /// Call inside the daemon's runtime: the route keeps a handle to it.
    pub(crate) fn new(store: Arc<HaroldStore>, pusher: Arc<Pusher>) -> Self {
        Self {
            store,
            pusher,
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

/// The current time in milliseconds since the Unix epoch.
fn now_ms() -> i64 {
    i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX)
}

impl AppRoute for WebAppRoute {
    /// Blocking: call from `spawn_blocking`. The pushes are blocking requests and run
    /// outside `block_on`.
    fn deliver(&self, event_id: &str, text: &str) -> Result<AppDelivery, String> {
        // The error names the store's failure only, never the text.
        self.runtime
            .block_on(self.store.add_chief_app_message(event_id, now_ms(), text))
            .map_err(|error| format!("app thread write failed: {error}"))?;

        // A listing that fails pushes nothing: the message goes to iMessage instead.
        let subscriptions = self
            .runtime
            .block_on(self.store.app_push_subscriptions())
            .unwrap_or_default();
        let body = alert_body(text);
        let mut pushed = false;
        let mut gone = Vec::new();
        for subscription in subscriptions {
            match self.pusher.push(&subscription, &body) {
                PushOutcome::Accepted => pushed = true,
                PushOutcome::Gone => gone.push(subscription.endpoint),
                PushOutcome::Refused | PushOutcome::Unreachable => {}
            }
        }
        for endpoint in gone {
            // Not stored, the subscription stays, and the next push to it deletes it then.
            // The message is not tried again for it: its pushes have been made.
            let _ = self.runtime.block_on(append_app_push_unsubscribed(
                &self.store,
                &AppPushUnsubscribed { endpoint },
            ));
        }
        Ok(if pushed {
            AppDelivery::Pushed
        } else {
            AppDelivery::NotPushed
        })
    }

    /// Blocking: call from `spawn_blocking`.
    fn set_reply_state(&self, event_id: &str, state: AppReplyState) -> Result<(), String> {
        self.runtime
            .block_on(self.store.set_app_reply_state(event_id, state))
            .map_err(|error| format!("app reply state write failed: {error}"))
    }
}

#[cfg(test)]
#[path = "route_tests.rs"]
mod tests;
