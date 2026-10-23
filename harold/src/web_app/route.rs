//! The web app as the delivery loop's app route: a chief's message is added to the
//! app's thread and pushed to every phone that turned alerts on.

use std::sync::Arc;

use time::OffsetDateTime;
use tracing::{info, warn};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use super::OpenPages;
use super::push::{PushOutcome, Pusher, alert_body};
use crate::outbound::{AppDelivery, AppRoute, NotPushedReason};
use crate::store::{AppPushUnsubscribed, AppReplyState, HaroldStore, append_app_push_unsubscribed};
use crate::telemetry;

pub(crate) struct WebAppRoute {
    store: Arc<HaroldStore>,
    pusher: Arc<Pusher>,
    /// The daemon's runtime. The delivery loop calls the route from `spawn_blocking`, so
    /// store calls run on it with `block_on`.
    runtime: tokio::runtime::Handle,
    /// The open page streams: a push waits while one is on screen.
    pages: Arc<OpenPages>,
}

impl WebAppRoute {
    /// Call inside the daemon's runtime: the route keeps a handle to it.
    pub(crate) fn new(store: Arc<HaroldStore>, pusher: Arc<Pusher>) -> Self {
        Self {
            store,
            pusher,
            runtime: tokio::runtime::Handle::current(),
            pages: Arc::default(),
        }
    }

    /// This route reading `pages`, the ones the app's server counts.
    pub(crate) fn with_pages(mut self, pages: Arc<OpenPages>) -> Self {
        self.pages = pages;
        self
    }
}

/// The current time in milliseconds since the Unix epoch.
pub(super) fn now_ms() -> i64 {
    i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX)
}

impl AppRoute for WebAppRoute {
    /// Blocking: call from `spawn_blocking`. The pushes are blocking requests and run
    /// outside `block_on`.
    fn deliver(&self, event_id: &str, text: &str) -> Result<AppDelivery, String> {
        // The row records the span it is stored in, which is the turn end's delivery or the
        // away message, so the page can join the trace when it draws the row.
        let trace = telemetry::traceparent_of(&tracing::Span::current().context());
        // The error names the store's failure only, never the text.
        self.runtime
            .block_on(self.store.add_chief_app_message_traced(
                event_id,
                now_ms(),
                text,
                trace.as_deref(),
            ))
            .map_err(|error| format!("app thread write failed: {error}"))?;

        // A page on screen shows the message; every subscription is skipped, as Safari and the
        // home-screen app hold separate ones on one phone.
        let pages = self.pages.visible();
        if pages > 0 {
            info!(reason = "page_visible", pages, "app push skipped");
            return Ok(AppDelivery::Shown);
        }

        // A listing that fails pushes nothing: the message goes to the away channel instead.
        let subscriptions = match self.runtime.block_on(self.store.app_push_subscriptions()) {
            Ok(subscriptions) => subscriptions,
            Err(error) => {
                warn!(error = %error, "app push subscriptions not read");
                return Ok(AppDelivery::NotPushed(NotPushedReason::SubscriptionsUnread));
            }
        };
        if subscriptions.is_empty() {
            return Ok(AppDelivery::NotPushed(NotPushedReason::NoSubscriptions));
        }
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
            AppDelivery::NotPushed(NotPushedReason::NoneAccepted)
        })
    }

    /// Blocking: call from `spawn_blocking`.
    fn set_reply_state(&self, event_id: &str, state: AppReplyState) -> Result<(), String> {
        self.runtime
            .block_on(self.store.set_app_reply_state(event_id, state))
            .map_err(|error| format!("app reply state write failed: {error}"))
    }

    /// Blocking: call from `spawn_blocking`.
    fn settle_waiting_replies(&self, before_ms: i64) -> Result<Vec<String>, String> {
        self.runtime
            .block_on(self.store.settle_waiting_app_replies(before_ms))
            .map_err(|error| format!("app reply settle write failed: {error}"))
    }
}

#[cfg(test)]
#[path = "route_tests.rs"]
mod tests;
