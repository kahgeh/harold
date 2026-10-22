use std::sync::OnceLock;

/// Keeps a second, do-nothing dispatcher registered for the whole test run.
/// With only one scoped dispatcher registered, tracing-core takes a call
/// site's first interest from the hitting thread's own default, so a thread
/// that isn't capturing can cache the site as never wanted and a capturing
/// test then misses its line. Two registered dispatchers make that "sometimes".
pub(crate) fn keep_second_dispatcher() {
    static SECOND: OnceLock<tracing::Dispatch> = OnceLock::new();
    SECOND.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
}
