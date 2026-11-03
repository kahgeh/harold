//! The one place where the telemetry tool calls Harold's exporter. The service
//! sends through the `Signals` trait, so a test gives it a fake.

use crate::telemetry;

/// What the service sends. Each attribute pair is a declared key and a value.
pub(crate) trait Signals: Send + Sync + 'static {
    /// Adds `n` to the counter `name`.
    fn counter_add(&self, name: &str, n: u64, attrs: &[(String, String)]);
    /// Sets the gauge `name` to `value`.
    fn gauge_set(&self, name: &str, value: f64, attrs: &[(String, String)]);
    /// Writes one `agent event` line for `name`.
    fn event(&self, name: &str, attrs: &[(String, String)]);
    /// Stops the export of `name` at once.
    fn forget(&self, name: &str);
    /// Counts one call of the tool on `harold.telemetry.calls`.
    fn call(&self, prefix: &str, kind: &str, outcome: &str);
}

/// The signals that go to Harold's exporter.
pub(crate) struct ExportedSignals;

impl Signals for ExportedSignals {
    fn counter_add(&self, name: &str, n: u64, attrs: &[(String, String)]) {
        telemetry::signal_counter_add(name, n, attrs);
    }

    fn gauge_set(&self, name: &str, value: f64, attrs: &[(String, String)]) {
        telemetry::signal_gauge_set(name, value, attrs);
    }

    fn event(&self, name: &str, attrs: &[(String, String)]) {
        telemetry::signal_event(name, attrs);
    }

    fn forget(&self, name: &str) {
        telemetry::signal_forget(name);
    }

    fn call(&self, prefix: &str, kind: &str, outcome: &str) {
        telemetry::count_telemetry_call(prefix, kind, outcome);
    }
}
