//! Fakes and helpers shared by the question tool's tests.

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harold_api::tools::questions::v1 as pb;

use super::schema::{Answer, Question};
use super::store::{
    Listing, QuestionStore, Readiness, Refusal, Shown, StartedBatch, StoreError, TickRefusal,
    TursoQuestionStore,
};
use crate::tools::{BoxFuture, ChiefPane, PaneTyping, Typing};

pub(super) const CHIEF_PANE: &str = "%9";

/// A folder under the system temp folder, removed when dropped.
pub(super) struct TempFolder(pub PathBuf);

impl TempFolder {
    pub(super) fn new() -> Self {
        Self(std::env::temp_dir().join(format!("harold-questions-{}", uuid::Uuid::new_v4())))
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) async fn open_store(folder: &TempFolder) -> Arc<TursoQuestionStore> {
    Arc::new(TursoQuestionStore::open(&folder.0).await.unwrap())
}

/// What the fake pane does with one line.
#[derive(Clone, Copy)]
pub(super) enum Step {
    Answer(Typing),
    /// Never returns, as typing still in flight inside `type_line`.
    Hang,
}

/// Records every `(pane, text)` it is asked to type, and answers from a
/// script; once the script runs out it answers `fallback`.
pub(super) struct FakeTyping {
    script: Mutex<VecDeque<Step>>,
    fallback: Mutex<Typing>,
    calls: Mutex<Vec<(String, String)>>,
    /// Runs once, during the first call, as a write landing while a line is typed.
    during_first: Mutex<Option<BoxFuture<'static, ()>>>,
}

impl FakeTyping {
    pub(super) fn new(script: Vec<Step>, fallback: Typing) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            fallback: Mutex::new(fallback),
            calls: Mutex::new(Vec::new()),
            during_first: Mutex::new(None),
        })
    }

    /// Runs `write` during the first call, before it answers.
    pub(super) fn during_first_call(&self, write: impl Future<Output = ()> + Send + 'static) {
        *self.during_first.lock().unwrap() = Some(Box::pin(write));
    }

    pub(super) fn typed_ok() -> Arc<Self> {
        Self::new(Vec::new(), Typing::Typed)
    }

    /// Changes what the pane answers once the script has run out.
    pub(super) fn set_fallback(&self, fallback: Typing) {
        *self.fallback.lock().unwrap() = fallback;
    }

    pub(super) fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }

    pub(super) fn texts(&self) -> Vec<String> {
        self.calls().into_iter().map(|(_, text)| text).collect()
    }
}

impl PaneTyping for FakeTyping {
    fn type_line<'a>(&'a self, pane_id: &'a str, text: &'a str) -> BoxFuture<'a, Typing> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((pane_id.to_string(), text.to_string()));
            let during = self.during_first.lock().unwrap().take();
            if let Some(write) = during {
                write.await;
            }
            let step = self.script.lock().unwrap().pop_front();
            match step {
                Some(Step::Answer(typing)) => typing,
                Some(Step::Hang) => std::future::pending().await,
                None => *self.fallback.lock().unwrap(),
            }
        })
    }
}

pub(super) struct FakeChief(Mutex<Option<String>>);

impl FakeChief {
    pub(super) fn at(pane: Option<&str>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(pane.map(str::to_string))))
    }

    pub(super) fn set(&self, pane: Option<&str>) {
        *self.0.lock().unwrap() = pane.map(str::to_string);
    }
}

impl ChiefPane for FakeChief {
    fn chief_pane(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }
}

/// Forwards to a real store; its writes fail while `writes` is set,
/// `batch_typed` fails while `marks` is set, and `release_batch` fails while
/// `releases` is set.
pub(super) struct FailingStore {
    pub inner: Arc<TursoQuestionStore>,
    pub writes: AtomicBool,
    pub marks: AtomicBool,
    pub releases: AtomicBool,
}

impl FailingStore {
    pub(super) fn new(inner: Arc<TursoQuestionStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            writes: AtomicBool::new(true),
            marks: AtomicBool::new(false),
            releases: AtomicBool::new(false),
        })
    }

    fn check(&self) -> Result<(), StoreError> {
        if self.writes.load(Ordering::SeqCst) {
            return Err(StoreError::Data("write failed".into()));
        }
        Ok(())
    }
}

impl QuestionStore for FailingStore {
    async fn put(&self, question: Question, now_ms: i64) -> Result<u32, StoreError> {
        self.check()?;
        self.inner.put(question, now_ms).await
    }

    async fn dismiss(
        &self,
        id: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<Result<u32, Refusal>, StoreError> {
        self.check()?;
        self.inner.dismiss(id, reason, now_ms).await
    }

    async fn reorder(&self, ids: &[String], now_ms: i64) -> Result<(), StoreError> {
        self.check()?;
        self.inner.reorder(ids, now_ms).await
    }

    async fn list(&self, now_ms: i64) -> Result<Listing, StoreError> {
        self.inner.list(now_ms).await
    }

    async fn answer_traced(
        &self,
        id: &str,
        version: u32,
        send_now: bool,
        trace_parent: Option<&str>,
        answer: impl FnOnce(&Question) -> Result<Answer, String> + Send,
        now_ms: i64,
    ) -> Result<Result<Result<(), String>, Refusal>, StoreError> {
        self.check()?;
        self.inner
            .answer_traced(id, version, send_now, trace_parent, answer, now_ms)
            .await
    }

    async fn defer(&self, id: &str, now_ms: i64) -> Result<Result<u32, Refusal>, StoreError> {
        self.check()?;
        self.inner.defer(id, now_ms).await
    }

    async fn tick(
        &self,
        id: &str,
        version: u32,
        step: u32,
        ticked: bool,
        now_ms: i64,
    ) -> Result<Result<(), TickRefusal>, StoreError> {
        self.check()?;
        self.inner.tick(id, version, step, ticked, now_ms).await
    }

    async fn save_picks(
        &self,
        id: &str,
        version: u32,
        picks: &[pb::Pick],
    ) -> Result<Result<Result<(), String>, Refusal>, StoreError> {
        self.check()?;
        self.inner.save_picks(id, version, picks).await
    }

    async fn show(&self, id: &str) -> Result<Option<Shown>, StoreError> {
        self.inner.show(id).await
    }

    async fn ready(&self, now_ms: i64, settle_ms: i64) -> Result<Readiness, StoreError> {
        self.inner.ready(now_ms, settle_ms).await
    }

    async fn start_batch(
        &self,
        answers: &[(String, u32)],
        text: &str,
        now_ms: i64,
    ) -> Result<Option<StartedBatch>, StoreError> {
        self.check()?;
        self.inner.start_batch(answers, text, now_ms).await
    }

    async fn started_batch(&self) -> Result<Option<StartedBatch>, StoreError> {
        self.inner.started_batch().await
    }

    async fn batch_typed(&self, batch_id: i64, now_ms: i64) -> Result<(), StoreError> {
        self.check()?;
        if self.marks.load(Ordering::SeqCst) {
            return Err(StoreError::Data("mark failed".into()));
        }
        self.inner.batch_typed(batch_id, now_ms).await
    }

    async fn release_batch(&self, batch_id: i64) -> Result<bool, StoreError> {
        self.check()?;
        if self.releases.load(Ordering::SeqCst) {
            return Err(StoreError::Data("release failed".into()));
        }
        self.inner.release_batch(batch_id).await
    }
}

/// What `MarkHook::batch_typed` does instead of marking.
#[derive(Clone, Copy)]
pub(super) enum MarkMode {
    /// Never returns, as a stop that lands before the mark is recorded.
    Hang,
    /// Panics, as the sender's own task failing after the line was typed.
    Panic,
}

/// Forwards to a real store, except `batch_typed`, which hangs or panics.
pub(super) struct MarkHook {
    pub inner: Arc<TursoQuestionStore>,
    pub mode: MarkMode,
}

impl MarkHook {
    pub(super) fn new(inner: Arc<TursoQuestionStore>, mode: MarkMode) -> Arc<Self> {
        Arc::new(Self { inner, mode })
    }
}

impl QuestionStore for MarkHook {
    async fn put(&self, question: Question, now_ms: i64) -> Result<u32, StoreError> {
        self.inner.put(question, now_ms).await
    }

    async fn dismiss(
        &self,
        id: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<Result<u32, Refusal>, StoreError> {
        self.inner.dismiss(id, reason, now_ms).await
    }

    async fn reorder(&self, ids: &[String], now_ms: i64) -> Result<(), StoreError> {
        self.inner.reorder(ids, now_ms).await
    }

    async fn list(&self, now_ms: i64) -> Result<Listing, StoreError> {
        self.inner.list(now_ms).await
    }

    async fn answer_traced(
        &self,
        id: &str,
        version: u32,
        send_now: bool,
        trace_parent: Option<&str>,
        answer: impl FnOnce(&Question) -> Result<Answer, String> + Send,
        now_ms: i64,
    ) -> Result<Result<Result<(), String>, Refusal>, StoreError> {
        self.inner
            .answer_traced(id, version, send_now, trace_parent, answer, now_ms)
            .await
    }

    async fn defer(&self, id: &str, now_ms: i64) -> Result<Result<u32, Refusal>, StoreError> {
        self.inner.defer(id, now_ms).await
    }

    async fn tick(
        &self,
        id: &str,
        version: u32,
        step: u32,
        ticked: bool,
        now_ms: i64,
    ) -> Result<Result<(), TickRefusal>, StoreError> {
        self.inner.tick(id, version, step, ticked, now_ms).await
    }

    async fn save_picks(
        &self,
        id: &str,
        version: u32,
        picks: &[pb::Pick],
    ) -> Result<Result<Result<(), String>, Refusal>, StoreError> {
        self.inner.save_picks(id, version, picks).await
    }

    async fn show(&self, id: &str) -> Result<Option<Shown>, StoreError> {
        self.inner.show(id).await
    }

    async fn ready(&self, now_ms: i64, settle_ms: i64) -> Result<Readiness, StoreError> {
        self.inner.ready(now_ms, settle_ms).await
    }

    async fn start_batch(
        &self,
        answers: &[(String, u32)],
        text: &str,
        now_ms: i64,
    ) -> Result<Option<StartedBatch>, StoreError> {
        self.inner.start_batch(answers, text, now_ms).await
    }

    async fn started_batch(&self) -> Result<Option<StartedBatch>, StoreError> {
        self.inner.started_batch().await
    }

    async fn batch_typed(&self, _batch_id: i64, _now_ms: i64) -> Result<(), StoreError> {
        match self.mode {
            MarkMode::Hang => std::future::pending().await,
            MarkMode::Panic => panic!("batch_typed panicked"),
        }
    }

    async fn release_batch(&self, batch_id: i64) -> Result<bool, StoreError> {
        self.inner.release_batch(batch_id).await
    }
}

/// Log lines written while the guard lives, on this thread.
#[derive(Clone, Default)]
pub(super) struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    pub(super) fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub(super) fn with(&self, message: &str) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter(|line| line.contains(message))
            .collect()
    }
}

/// Captures this thread's log lines; use it from a current-thread runtime so
/// spawned tasks log on the same thread.
pub(super) fn capture_logs() -> (Logs, tracing::subscriber::DefaultGuard) {
    crate::test_logs::keep_second_dispatcher();
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .finish();
    (logs, tracing::subscriber::set_default(subscriber))
}

/// The field names on one formatted log line, after its fixed message.
pub(super) fn field_names(line: &str, message: &str) -> Vec<String> {
    let (_, rest) = line
        .split_once(message)
        .unwrap_or_else(|| panic!("{message:?} not in {line:?}"));
    rest.split_whitespace()
        .map(|token| {
            token
                .split_once('=')
                .unwrap_or_else(|| panic!("{token:?} in {line:?} is not a field"))
                .0
                .to_string()
        })
        .collect()
}

pub(super) fn option(key: &str, label: &str) -> pb::DecisionOption {
    pb::DecisionOption {
        key: key.into(),
        label: label.into(),
        impact: format!("impact of {label}"),
        suggested: false,
        free: false,
    }
}

pub(super) fn free_option(key: &str) -> pb::DecisionOption {
    pb::DecisionOption {
        key: key.into(),
        label: "Something else".into(),
        impact: "Type your answer.".into(),
        suggested: false,
        free: true,
    }
}

pub(super) fn decision(ask: &str, options: Vec<pb::DecisionOption>) -> pb::Decision {
    pb::Decision {
        ask: ask.into(),
        options,
    }
}

/// A question with one decision, `Which?`, offering `a` (`A`) and a free `z`.
pub(super) fn question(id: &str, spec: &str, sha: &str) -> pb::Question {
    question_with(
        id,
        spec,
        sha,
        vec![decision("Which?", vec![option("a", "A"), free_option("z")])],
    )
}

pub(super) fn question_with(
    id: &str,
    spec: &str,
    sha: &str,
    decisions: Vec<pb::Decision>,
) -> pb::Question {
    pb::Question {
        id: id.into(),
        product: "harold".into(),
        spec: spec.into(),
        sha: sha.into(),
        link: "https://example.com/spec".into(),
        intro: "An intro.".into(),
        title: String::new(),
        steps: vec![],
        decisions,
    }
}

pub(super) fn pick(decision: u32, key: &str) -> pb::Pick {
    pb::Pick {
        decision,
        key: key.into(),
        text: String::new(),
    }
}

pub(super) fn free_pick(decision: u32, key: &str, text: &str) -> pb::Pick {
    pb::Pick {
        decision,
        key: key.into(),
        text: text.into(),
    }
}

/// Waits, in small steps of (possibly paused) time, until `done` holds.
pub(super) async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..2_000 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting until {what}");
}
