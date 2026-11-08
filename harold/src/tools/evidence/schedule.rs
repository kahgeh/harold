//! The schedule of the evidence pass: once at the start, then each 24 hours.

use std::future::Future;
use std::time::Duration;

/// The time between two passes. A release install restarts Harold, so each release also
/// gets a pass at the start.
pub(super) const PERIOD: Duration = Duration::from_secs(24 * 60 * 60);

/// Runs `pass` at once and then after each `period`, until the shutdown signal comes or its
/// sender drops.
pub(super) async fn run_every<F, Fut>(
    period: Duration,
    mut shutdown: tokio::sync::watch::Receiver<()>,
    mut pass: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        pass().await;
        tokio::select! {
            _ = tokio::time::sleep(period) => {}
            _ = shutdown.changed() => return,
        }
    }
}
