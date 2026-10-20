use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use harold_api::tools::questions::v1 as pb;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use super::*;
use crate::tools::questions::schema::{self, Kind};
use crate::tools::questions::store::{
    Listing, QuestionStore, Refusal, StartedBatch, StoreError, TickRefusal, TursoQuestionStore,
};
use crate::tools::questions::test_support::*;
use crate::tools::{Typing, questions};

/// One running sender over `store`, with its fakes.
struct Running {
    typing: Arc<FakeTyping>,
    chief: Arc<FakeChief>,
    wake: Arc<Notify>,
    shutdown: watch::Sender<()>,
    task: JoinHandle<()>,
}

fn spawn_sender<S: QuestionStore>(store: &Arc<S>, typing: Arc<FakeTyping>) -> Running {
    spawn_with_chief(store, typing, FakeChief::at(Some(CHIEF_PANE)))
}

fn spawn_with_chief<S: QuestionStore>(
    store: &Arc<S>,
    typing: Arc<FakeTyping>,
    chief: Arc<FakeChief>,
) -> Running {
    let wake = Arc::new(Notify::new());
    let (shutdown, shutdown_rx) = watch::channel(());
    let task = tokio::spawn(run(
        Arc::clone(store),
        typing.clone() as Arc<dyn crate::tools::PaneTyping>,
        chief.clone() as Arc<dyn crate::tools::ChiefPane>,
        Arc::clone(&wake),
        shutdown_rx,
    ));
    Running {
        typing,
        chief,
        wake,
        shutdown,
        task,
    }
}

async fn put(store: &impl QuestionStore, question: pb::Question) {
    store
        .put(schema::from_proto(question).unwrap(), 0)
        .await
        .unwrap();
}

async fn answer(
    store: &impl QuestionStore,
    id: &str,
    version: u32,
    kind: Kind,
    picks: Vec<pb::Pick>,
    note: &str,
) {
    answer_with(store, id, version, kind, picks, note, false).await;
}

async fn answer_with(
    store: &impl QuestionStore,
    id: &str,
    version: u32,
    kind: Kind,
    picks: Vec<pb::Pick>,
    note: &str,
    send_now: bool,
) {
    let note = note.to_string();
    let result = store
        .answer(
            id,
            version,
            send_now,
            move |q| schema::resolve_picks(q, kind, &picks, &note),
            clock_ms(),
        )
        .await
        .unwrap();
    assert_eq!(result, Ok(Ok(())), "answer {id}");
}

async fn answer_a(store: &impl QuestionStore, id: &str) {
    answer(store, id, 1, Kind::Answer, vec![pick(0, "a")], "").await;
}

async fn answer_marked(store: &impl QuestionStore, id: &str) {
    answer_with(store, id, 1, Kind::Answer, vec![pick(0, "a")], "", true).await;
}

async fn defer(store: &impl QuestionStore, id: &str) {
    store.defer(id, 0).await.unwrap().unwrap();
}

/// The listed questions, in order, each with whether it is set aside.
async fn listed(store: &impl QuestionStore) -> Vec<(String, bool)> {
    store
        .list(0)
        .await
        .unwrap()
        .questions
        .into_iter()
        .map(|l| (l.question.id, l.deferred))
        .collect()
}

/// Waits until `n` lines were tried without letting paused time move, so the
/// time that passed since an earlier `Instant` is exactly what the sender slept.
async fn tried_without_waiting(sender: &Running, n: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while sender.typing.calls().len() < n {
        assert!(
            std::time::Instant::now() < deadline,
            "line {n} needed paused time to pass"
        );
        tokio::task::yield_now().await;
    }
}

/// Lets the sender run for `seconds` of (paused) time.
async fn pass(seconds: u64) {
    tokio::time::sleep(Duration::from_secs(seconds)).await;
}

async fn pass_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

async fn until_no_started_batch(store: &impl QuestionStore) {
    for _ in 0..2_000 {
        if store.started_batch().await.unwrap().is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the batch was never marked typed");
}

fn ready_with(answers: Vec<(&str, pb::Question, schema::Answer)>, later: &[&str]) -> Ready {
    Ready {
        answers: answers
            .into_iter()
            .map(|(id, q, a)| (id.to_string(), 1, schema::from_proto(q).unwrap(), a))
            .collect(),
        later: later.iter().map(|id| id.to_string()).collect(),
    }
}

/// Forwards to a real store, but puts `race` just before the first
/// `start_batch`, as a write landing between `ready` and `start_batch`.
struct RacingStore {
    inner: Arc<TursoQuestionStore>,
    race: std::sync::Mutex<Option<pb::Question>>,
}

impl QuestionStore for RacingStore {
    async fn put(&self, question: schema::Question, now_ms: i64) -> Result<u32, StoreError> {
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

    async fn answer(
        &self,
        id: &str,
        version: u32,
        send_now: bool,
        answer: impl FnOnce(&schema::Question) -> Result<schema::Answer, String> + Send,
        now_ms: i64,
    ) -> Result<Result<Result<(), String>, Refusal>, StoreError> {
        self.inner
            .answer(id, version, send_now, answer, now_ms)
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

    async fn ready(&self, now_ms: i64, settle_ms: i64) -> Result<Readiness, StoreError> {
        self.inner.ready(now_ms, settle_ms).await
    }

    async fn start_batch(
        &self,
        answers: &[(String, u32)],
        text: &str,
        now_ms: i64,
    ) -> Result<Option<StartedBatch>, StoreError> {
        let race = self.race.lock().unwrap().take();
        if let Some(question) = race {
            put(&*self.inner, question).await;
        }
        self.inner.start_batch(answers, text, now_ms).await
    }

    async fn started_batch(&self) -> Result<Option<StartedBatch>, StoreError> {
        self.inner.started_batch().await
    }

    async fn batch_typed(&self, batch_id: i64, now_ms: i64) -> Result<(), StoreError> {
        self.inner.batch_typed(batch_id, now_ms).await
    }

    async fn release_batch(&self, batch_id: i64) -> Result<bool, StoreError> {
        self.inner.release_batch(batch_id).await
    }
}

#[tokio::test(start_paused = true)]
async fn a_put_again_between_ready_and_start_types_nothing_and_looks_again() {
    let folder = TempFolder::new();
    let inner = open_store(&folder).await;
    put(&*inner, question("Q1", "s1", "abc")).await;
    put(&*inner, question("Q2", "s2", "abc")).await;
    answer_a(&*inner, "Q1").await;
    answer_a(&*inner, "Q2").await;
    let racing = Arc::new(RacingStore {
        inner: Arc::clone(&inner),
        race: std::sync::Mutex::new(Some(question("Q1", "s1", "new"))),
    });
    let sender = spawn_sender(&racing, FakeTyping::typed_ok());
    pass(30).await;
    assert!(racing.race.lock().unwrap().is_none(), "the race ran");
    // The stale v1 line was not typed; the sender looked again and typed what
    // is answered now: Q2 alone, with Q1 waiting again.
    let second = "ANSWERS 1 | Q2 s2 sha abc: Which? A";
    assert_eq!(sender.typing.texts(), vec![second.to_string()]);
    assert_eq!(inner.started_batch().await.unwrap(), None);
    assert_eq!(
        listed(&*inner).await,
        vec![("Q1".to_string(), false)],
        "Q1 waits again"
    );

    answer(
        &*inner,
        "Q1",
        2,
        Kind::FreeText,
        vec![free_pick(0, "z", "second")],
        "",
    )
    .await;
    sender.wake.notify_one();
    pass(30).await;
    let third = "ANSWERS 1 | Q1 s1 sha new: Which? \"second\"";
    assert_eq!(
        sender.typing.texts(),
        vec![second.to_string(), third.to_string()]
    );
    println!(
        "evidence: re-put of Q1 between ready and start_batch: no stale v1 line; typed {second:?}; after Q1 v2 was answered: {third:?}"
    );
}

#[test]
fn a_free_pick_on_an_early_decision_renders_in_place() {
    let question = schema::from_proto(pb::Question {
        id: "Q9".into(),
        product: "p".into(),
        spec: "s9".into(),
        sha: "abc".into(),
        link: "https://x".into(),
        intro: "i".into(),
        title: String::new(),
        steps: vec![],
        decisions: ["First?", "Second?"]
            .map(|ask| pb::Decision {
                ask: ask.into(),
                options: vec![pb::DecisionOption {
                    key: "a".into(),
                    label: "A".into(),
                    impact: "i".into(),
                    suggested: false,
                    free: false,
                }],
            })
            .to_vec(),
    })
    .unwrap();
    let picked = |ask: &str, label: &str, free| schema::Picked {
        ask: ask.into(),
        label: label.into(),
        free,
    };
    let answer = schema::Answer {
        ticked: 0,
        kind: Kind::FreeText,
        picks: vec![
            picked("First?", "my\nwords", true),
            picked("Second?", "A", false),
        ],
        note: String::new(),
    };
    assert_eq!(
        answer_part("Q9", &question, &answer),
        "Q9 s9 sha abc: First? \"my words\"; Second? A"
    );
}

#[test]
fn clean_strips_escape_sequences_tabs_and_returns() {
    assert_eq!(clean("a\x1b[2Jb\tc\rd\ne"), "abcd e");
    assert_eq!(clean("one\r\ntwo\nthree"), "one two three");
    assert_eq!(clean("x\x1b[1;31mred\x1b[0m\x07\u{7f}\u{85}y"), "xredy");
    assert_eq!(clean("lone \x1b escape"), "lone  escape");
    assert_eq!(clean("cut \x1b[12"), "cut ");
    assert_eq!(clean("a | b \"q\""), "a | b \"q\"");
    println!(
        "evidence: clean(\"a\\x1b[2Jb\\tc\\rd\\ne\") = {:?}",
        clean("a\x1b[2Jb\tc\rd\ne")
    );
}

#[tokio::test(start_paused = true)]
async fn three_answers_type_one_line_with_ids_specs_shas_and_picks() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(
        &*store,
        question_with(
            "Q25",
            "durable-server",
            "d84bf386a2a5",
            vec![
                decision(
                    "protocol",
                    vec![option("a", "libSQL's Hrana"), option("b", "Postgres wire")],
                ),
                decision("build", vec![option("y", "yes"), option("n", "no")]),
            ],
        ),
    )
    .await;
    put(
        &*store,
        question_with(
            "Q26",
            "voice-x",
            "1111aaaa",
            vec![decision(
                "size",
                vec![option("s", "small"), option("l", "large")],
            )],
        ),
    )
    .await;
    put(
        &*store,
        question_with(
            "Q27",
            "old-y",
            "2222bbbb",
            vec![decision(
                "keep",
                vec![option("k", "keep"), option("d", "drop")],
            )],
        ),
    )
    .await;
    let sender = spawn_sender(&store, FakeTyping::typed_ok());

    answer(
        &*store,
        "Q25",
        1,
        Kind::Answer,
        vec![pick(0, "a"), pick(1, "y")],
        "",
    )
    .await;
    sender.wake.notify_one();
    answer(&*store, "Q26", 1, Kind::Answer, vec![pick(0, "l")], "").await;
    sender.wake.notify_one();
    answer(&*store, "Q27", 1, Kind::Answer, vec![pick(0, "d")], "").await;
    sender.wake.notify_one();
    until("the batch is typed", || sender.typing.calls().len() == 1).await;
    until_no_started_batch(&*store).await;

    let expected = "ANSWERS 3 | Q25 durable-server sha d84bf386a2a5: protocol libSQL's Hrana; build yes | Q26 voice-x sha 1111aaaa: size large | Q27 old-y sha 2222bbbb: keep drop";
    assert_eq!(
        sender.typing.calls(),
        vec![(CHIEF_PANE.to_string(), expected.to_string())]
    );
    assert!(store.list(0).await.unwrap().questions.is_empty());
    pass(30).await;
    assert_eq!(sender.typing.calls().len(), 1);
    println!("evidence: typed into {CHIEF_PANE}: {expected}");
}

#[tokio::test(start_paused = true)]
async fn revise_reject_and_free_text_share_the_line_with_newlines_as_spaces() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q26", "voice-x", "1111")).await;
    put(&*store, question("Q27", "old-y", "2222")).await;
    put(&*store, question("Q28", "net-z", "3333")).await;
    answer(
        &*store,
        "Q26",
        1,
        Kind::Revise,
        vec![],
        "make the button\nbigger",
    )
    .await;
    answer(
        &*store,
        "Q27",
        1,
        Kind::Reject,
        vec![],
        "not\r\nneeded\x1b[2J\t!",
    )
    .await;
    answer(
        &*store,
        "Q28",
        1,
        Kind::FreeText,
        vec![free_pick(0, "z", "use\nHTTP/3\r")],
        "",
    )
    .await;
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    until("the batch is typed", || sender.typing.calls().len() == 1).await;

    let expected = "ANSWERS 3 | Q26 voice-x: revise \"make the button bigger\" | Q27 old-y: reject \"not needed!\" | Q28 net-z sha 3333: Which? \"use HTTP/3\"";
    assert_eq!(sender.typing.texts(), vec![expected.to_string()]);
    println!("evidence: {expected}");
}

#[test]
fn a_note_with_a_pipe_or_quote_is_typed_as_is() {
    let note = "a | b \"quoted\"";
    let revise = schema::Answer {
        ticked: 0,
        kind: Kind::Revise,
        picks: vec![],
        note: note.into(),
    };
    let plain = schema::Answer {
        ticked: 0,
        kind: Kind::Answer,
        picks: vec![schema::Picked {
            ask: "Which?".into(),
            label: "A".into(),
            free: false,
        }],
        note: String::new(),
    };
    let ready = ready_with(
        vec![
            ("Q1", question("Q1", "spec-1", "abc"), revise),
            ("Q2", question("Q2", "spec-2", "def"), plain),
        ],
        &[],
    );
    let line = batch_line(&ready);
    assert_eq!(
        line,
        "ANSWERS 2 | Q1 spec-1: revise \"a | b \"quoted\"\" | Q2 spec-2 sha def: Which? A"
    );
    println!("evidence: {line}");
}

#[tokio::test(start_paused = true)]
async fn a_deferred_question_is_listed_under_later_and_returns_to_waiting() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    put(&*store, question("Q2", "s2", "abc")).await;
    put(&*store, question("Q3", "s3", "abc")).await;
    defer(&*store, "Q2").await;
    answer_a(&*store, "Q1").await;
    defer(&*store, "Q3").await;
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    until("the batch is typed", || sender.typing.calls().len() == 1).await;
    until_no_started_batch(&*store).await;

    let expected = "ANSWERS 1 | Q1 s1 sha abc: Which? A | later: Q2, Q3";
    assert_eq!(sender.typing.texts(), vec![expected.to_string()]);
    let waiting: Vec<(String, bool)> = store
        .list(0)
        .await
        .unwrap()
        .questions
        .into_iter()
        .map(|l| (l.question.id, l.deferred))
        .collect();
    assert_eq!(
        waiting,
        vec![("Q2".to_string(), false), ("Q3".to_string(), false)]
    );
    println!("evidence: {expected}; afterwards waiting: {waiting:?}");
}

#[tokio::test(start_paused = true)]
async fn a_waiting_question_does_not_hold_the_batch_and_is_sent_when_answered() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    put(&*store, question("Q2", "s2", "abc")).await;
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    answer_a(&*store, "Q1").await;
    sender.wake.notify_one();
    pass_ms(2_900).await;
    assert!(sender.typing.calls().is_empty(), "inside the settle");
    pass_ms(200).await;
    assert_eq!(
        sender.typing.texts(),
        vec!["ANSWERS 1 | Q1 s1 sha abc: Which? A".to_string()],
        "Q2 does not hold the line"
    );
    until_no_started_batch(&*store).await;
    assert_eq!(listed(&*store).await, vec![("Q2".to_string(), false)]);

    answer_a(&*store, "Q2").await;
    sender.wake.notify_one();
    pass(4).await;
    let texts = sender.typing.texts();
    assert_eq!(
        texts,
        vec![
            "ANSWERS 1 | Q1 s1 sha abc: Which? A".to_string(),
            "ANSWERS 1 | Q2 s2 sha abc: Which? A".to_string(),
        ]
    );
    println!(
        "evidence: Q1 sent after its settle with Q2 waiting; Q2 sent by the same rule: {texts:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_question_added_after_two_answers_does_not_hold_the_batch() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    put(&*store, question("Q2", "s2", "abc")).await;
    answer_a(&*store, "Q1").await;
    answer_a(&*store, "Q2").await;
    put(&*store, question("Q3", "s3", "abc")).await;
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    pass(4).await;
    let expected = "ANSWERS 2 | Q1 s1 sha abc: Which? A | Q2 s2 sha abc: Which? A";
    assert_eq!(sender.typing.texts(), vec![expected.to_string()]);
    assert_eq!(listed(&*store).await, vec![("Q3".to_string(), false)]);
    println!("evidence: Q3 waiting did not hold {expected}");
}

#[tokio::test(start_paused = true)]
async fn a_busy_pane_waits_logs_once_and_types_when_idle() {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    answer_a(&*store, "Q1").await;
    let busy = Step::Answer(Typing::PaneBusy);
    let sender = spawn_sender(&store, FakeTyping::new(vec![busy; 3], Typing::Typed));
    until("the first try", || sender.typing.calls().len() == 1).await;
    // The first try waits out the settle; each try is then one attempt, and
    // between rounds the batch is released, not waited on.
    let started = tokio::time::Instant::now();
    assert_eq!(store.started_batch().await.unwrap(), None);
    until("the batch is typed", || sender.typing.calls().len() == 4).await;
    until_no_started_batch(&*store).await;
    let elapsed = started.elapsed();

    let texts = sender.typing.texts();
    assert!(texts.iter().all(|t| t == &texts[0]), "{texts:?}");
    assert!(
        elapsed >= RETRY * 3 && elapsed < RETRY * 4,
        "three retries 5 s apart, took {elapsed:?}"
    );
    let waiting = logs.with("answers batch waiting");
    assert_eq!(waiting.len(), 1, "{:#?}", logs.lines());
    assert_eq!(
        field_names(&waiting[0], "answers batch waiting"),
        ["tool", "reason"]
    );
    assert!(
        waiting[0].contains("reason=\"pane_busy\""),
        "{}",
        waiting[0]
    );
    let typed = logs.with("answers batch typed");
    assert_eq!(typed.len(), 1);
    assert!(typed[0].contains("reason=\"typed\""), "{}", typed[0]);
    println!(
        "evidence: 3 busy tries, one a round, then typed after {elapsed:?}; log: {}; log: {}",
        waiting[0], typed[0]
    );
}

#[tokio::test(start_paused = true)]
async fn no_chief_waits() {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    answer_a(&*store, "Q1").await;
    let sender = spawn_with_chief(&store, FakeTyping::typed_ok(), FakeChief::at(None));
    pass(30).await;
    assert!(sender.typing.calls().is_empty());
    let waiting = logs.with("answers batch waiting");
    assert_eq!(waiting.len(), 1, "{:#?}", logs.lines());
    assert!(waiting[0].contains("reason=\"no_chief\""), "{}", waiting[0]);

    sender.chief.set(Some(CHIEF_PANE));
    let before = tokio::time::Instant::now();
    until("the batch is typed", || sender.typing.calls().len() == 1).await;
    assert!(before.elapsed() < RETRY + Duration::from_secs(1));
    assert_eq!(sender.typing.calls()[0].0, CHIEF_PANE);
    println!(
        "evidence: no chief for 30 s, nothing typed, log: {}; typed once the chief ran",
        waiting[0]
    );
}

#[tokio::test(start_paused = true)]
async fn a_started_batch_is_typed_once_after_a_restart() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    answer_a(&*store, "Q1").await;
    let first = spawn_sender(&store, FakeTyping::new(vec![Step::Hang], Typing::Typed));
    until("the first sender is typing", || {
        first.typing.calls().len() == 1
    })
    .await;
    // Harold dies while the line is being typed: the batch was started, never marked typed.
    first.task.abort();
    let _ = first.task.await;
    let started = store.started_batch().await.unwrap().expect("batch started");
    drop(store);

    let store: Arc<TursoQuestionStore> = open_store(&folder).await;
    let second = spawn_sender(&store, FakeTyping::typed_ok());
    until("the restarted sender types", || {
        second.typing.calls().len() == 1
    })
    .await;
    until_no_started_batch(&*store).await;
    second.wake.notify_one();
    pass(30).await;
    assert_eq!(second.typing.texts(), vec![started.text.clone()]);
    println!(
        "evidence: started batch {} typed once after the restart: {}",
        started.id, started.text
    );
}

#[tokio::test(start_paused = true)]
async fn a_typed_batch_is_never_typed_again() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    answer_a(&*store, "Q1").await;
    let first = spawn_sender(&store, FakeTyping::typed_ok());
    until("the batch is typed", || first.typing.calls().len() == 1).await;
    until_no_started_batch(&*store).await;
    for _ in 0..3 {
        first.wake.notify_one();
        pass(10).await;
    }
    assert_eq!(first.typing.calls().len(), 1);
    first.shutdown.send(()).unwrap();
    first.task.await.unwrap();
    drop(store);

    let store: Arc<TursoQuestionStore> = open_store(&folder).await;
    let second = spawn_sender(&store, FakeTyping::typed_ok());
    second.wake.notify_one();
    pass(30).await;
    assert!(second.typing.calls().is_empty());
    println!("evidence: one line typed; 30 s of wakes and a restart typed nothing more");
}

#[tokio::test(start_paused = true)]
async fn enter_failed_counts_as_typed_and_logs_its_reason() {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    answer_a(&*store, "Q1").await;
    let sender = spawn_sender(
        &store,
        FakeTyping::new(vec![Step::Answer(Typing::EnterFailed)], Typing::Typed),
    );
    until("the batch is typed", || sender.typing.calls().len() == 1).await;
    until_no_started_batch(&*store).await;
    pass(30).await;
    assert_eq!(sender.typing.calls().len(), 1);
    let typed = logs.with("answers batch typed");
    assert_eq!(typed.len(), 1, "{:#?}", logs.lines());
    assert_eq!(
        field_names(&typed[0], "answers batch typed"),
        ["tool", "reason"]
    );
    assert!(typed[0].contains("reason=\"enter_failed\""), "{}", typed[0]);
    assert!(logs.with("answers batch waiting").is_empty());
    println!(
        "evidence: one line typed, batch marked typed, log: {}",
        typed[0]
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_stops_a_waiting_batch() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    answer_a(&*store, "Q1").await;

    // Waiting for a busy pane between tries: nothing was typed, so the batch was released.
    let busy = spawn_sender(&store, FakeTyping::new(vec![], Typing::PaneBusy));
    until("the first try", || busy.typing.calls().len() == 1).await;
    busy.shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), busy.task)
        .await
        .expect("the sender stops at once")
        .unwrap();
    assert_eq!(store.started_batch().await.unwrap(), None, "batch released");
    let Readiness::Ready(ready) = store.ready(i64::MAX / 2, 3_000).await.unwrap() else {
        panic!("Q1 is unsent again");
    };
    assert_eq!(ready.answers.len(), 1);

    // Inside a `type_line` that has not returned: the line may have reached the pane.
    let typing = spawn_sender(&store, FakeTyping::new(vec![Step::Hang], Typing::Typed));
    until("typing", || typing.typing.calls().len() == 1).await;
    typing.shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), typing.task)
        .await
        .expect("the sender stops inside type_line")
        .unwrap();
    let started = store
        .started_batch()
        .await
        .unwrap()
        .expect("the batch stays started for the restart");
    assert_eq!(typing.typing.texts(), vec![started.text.clone()]);
    println!(
        "evidence: shutdown stopped the sender within 1 s while waiting (batch released) and while typing (batch {} still started)",
        started.id
    );
}

#[tokio::test(start_paused = true)]
async fn a_failing_store_waits_with_store_failed_and_retries() {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let inner = open_store(&folder).await;
    put(&*inner, question("Q1", "s1", "abc")).await;
    answer_a(&*inner, "Q1").await;
    let failing = FailingStore::new(Arc::clone(&inner));
    let sender = spawn_sender(&failing, FakeTyping::typed_ok());
    pass(30).await;
    assert!(sender.typing.calls().is_empty());
    let waiting = logs.with("answers batch waiting");
    assert_eq!(waiting.len(), 1, "{:#?}", logs.lines());
    assert!(
        waiting[0].contains("reason=\"store_failed\""),
        "{}",
        waiting[0]
    );

    failing.writes.store(false, Ordering::SeqCst);
    until("the batch is typed", || sender.typing.calls().len() == 1).await;
    println!(
        "evidence: no batch could start for 30 s, log: {}; typed once the database recovered",
        waiting[0]
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_mark_is_retried_without_typing_again() {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let inner = open_store(&folder).await;
    put(&*inner, question("Q1", "s1", "abc")).await;
    answer_a(&*inner, "Q1").await;
    let failing = FailingStore::new(Arc::clone(&inner));
    failing.writes.store(false, Ordering::SeqCst);
    failing.marks.store(true, Ordering::SeqCst);
    let sender = spawn_sender(&failing, FakeTyping::typed_ok());
    until("the batch is typed", || sender.typing.calls().len() == 1).await;
    pass(30).await;
    assert!(inner.started_batch().await.unwrap().is_some());

    failing.marks.store(false, Ordering::SeqCst);
    until_no_started_batch(&*inner).await;
    pass(30).await;
    assert_eq!(sender.typing.calls().len(), 1);
    let waiting = logs.with("answers batch waiting");
    assert_eq!(waiting.len(), 1, "{:#?}", logs.lines());
    assert!(waiting[0].contains("reason=\"store_failed\""));
    assert_eq!(logs.with("answers batch typed").len(), 1);
    println!("evidence: the mark failed for 30 s; it was retried and the line was typed once");
}

struct NoFolder;
impl crate::tools::PaneFolder for NoFolder {
    fn folder_of(&self, _pane_id: &str) -> Option<std::path::PathBuf> {
        None
    }
}

struct NoProducts;
impl crate::tools::Products for NoProducts {
    fn products(&self) -> Result<Vec<(String, std::path::PathBuf)>, &'static str> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn start_creates_questions_db_and_serves() {
    use prost::Message;
    use tonic::codegen::Service;

    let folder = TempFolder::new();
    let data_folder = folder.0.join("tools").join(questions::NAME);
    let (shutdown, shutdown_rx) = watch::channel(());
    let handle = questions::start(crate::tools::ToolContext {
        data_folder: data_folder.clone(),
        typing: Some(FakeTyping::typed_ok()),
        chief_pane: FakeChief::at(Some(CHIEF_PANE)),
        pane_folder: Arc::new(NoFolder),
        products: Arc::new(NoProducts),
        shutdown: shutdown_rx,
    })
    .await
    .unwrap_or_else(|e| panic!("start failed: {}", e.reason));
    assert!(data_folder.join("questions.db").is_file());

    // One ListQuestions call through the routes the host adds to its server.
    let mut router = handle.routes.into_axum_router();
    let message = pb::ListQuestionsRequest {}.encode_to_vec();
    let mut framed = vec![0u8];
    framed.extend_from_slice(&(message.len() as u32).to_be_bytes());
    framed.extend_from_slice(&message);
    let request = tonic::codegen::http::Request::builder()
        .method("POST")
        .uri("/harold.tools.questions.v1.QuestionService/ListQuestions")
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(axum::body::Body::from(framed))
        .unwrap();
    let response = router.call(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let listing = pb::ListQuestionsResponse::decode(&body[5..]).unwrap();
    assert_eq!(listing.waiting, 0);

    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle.task)
        .await
        .expect("the sender stops on shutdown")
        .unwrap();
    println!(
        "evidence: start created {} and served ListQuestions (waiting = {})",
        data_folder.join("questions.db").display(),
        listing.waiting
    );
}

#[tokio::test]
async fn start_on_a_folder_it_cannot_open_fails_with_store_open_failed() {
    let folder = TempFolder::new();
    // `tools` is a file, so `<folder>/tools/questions` cannot be made.
    std::fs::create_dir_all(&folder.0).unwrap();
    std::fs::write(folder.0.join("tools"), "not a folder").unwrap();
    let (_shutdown, shutdown_rx) = watch::channel(());
    let started = questions::start(crate::tools::ToolContext {
        data_folder: folder.0.join("tools").join(questions::NAME),
        typing: Some(FakeTyping::typed_ok()),
        chief_pane: FakeChief::at(Some(CHIEF_PANE)),
        pane_folder: Arc::new(NoFolder),
        products: Arc::new(NoProducts),
        shutdown: shutdown_rx,
    })
    .await;
    let Err(error) = started else {
        panic!("start succeeded under a file");
    };
    assert_eq!(error.reason, "store_open_failed");
}

/// The reviewer's scenario: Q1 is answered while the chief's pane is busy, and
/// 20 s later Q2 is put (or put and set aside), then the pane goes idle. The
/// next retry types one line for Q1 alone; Q2 does not hold it. Returns the
/// lines typed and the busy tries.
async fn a_question_added_while_a_batch_waits(defer_q2: bool) -> (Vec<String>, Vec<String>) {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    let sender = spawn_sender(&store, FakeTyping::new(vec![], Typing::PaneBusy));

    // t = 0: the user answers Q1, the only question; the pane is busy.
    answer_a(&*store, "Q1").await;
    sender.wake.notify_one();
    until("the batch is tried", || !sender.typing.calls().is_empty()).await;

    // t = 20 s: the chief puts Q2.
    pass(20).await;
    put(&*store, question("Q2", "s2", "abc")).await;
    if defer_q2 {
        defer(&*store, "Q2").await;
    }
    sender.wake.notify_one();
    pass(1).await;
    assert_eq!(
        store.started_batch().await.unwrap(),
        None,
        "no batch held open"
    );

    // The pane goes idle; the next retry types the line, Q2 or not.
    sender.typing.set_fallback(Typing::Typed);
    let tries = sender.typing.calls().len();
    until("the batch is typed", || {
        sender.typing.calls().len() == tries + 1
    })
    .await;
    until_no_started_batch(&*store).await;
    pass(30).await;
    assert_eq!(sender.typing.calls().len(), tries + 1, "one line, once");
    let typed = logs.with("answers batch typed");
    assert_eq!(typed.len(), 1, "the chief wakes once: {:#?}", logs.lines());
    let waiting = logs.with("answers batch waiting");
    assert_eq!(waiting.len(), 1, "{:#?}", logs.lines());
    assert!(
        waiting[0].contains("reason=\"pane_busy\""),
        "{}",
        waiting[0]
    );
    // Q2 waits, not set aside: the line left it, or returned it.
    assert_eq!(listed(&*store).await, vec![("Q2".to_string(), false)]);
    let texts = sender.typing.texts();
    (texts[tries..].to_vec(), texts[..tries].to_vec())
}

#[tokio::test(start_paused = true)]
async fn a_question_added_while_a_batch_waits_does_not_hold_it() {
    let (typed, tried) = a_question_added_while_a_batch_waits(false).await;
    let expected = "ANSWERS 1 | Q1 s1 sha abc: Which? A";
    assert_eq!(typed, vec![expected.to_string()]);
    println!(
        "evidence: {} busy tries of {:?}; Q2 put meanwhile; the idle pane got one line: {}",
        tried.len(),
        tried[0],
        typed[0]
    );
}

#[tokio::test(start_paused = true)]
async fn a_question_added_and_set_aside_while_a_batch_waits_is_listed_under_later() {
    let (typed, tried) = a_question_added_while_a_batch_waits(true).await;
    let expected = "ANSWERS 1 | Q1 s1 sha abc: Which? A | later: Q2";
    assert_eq!(typed, vec![expected.to_string()]);
    println!(
        "evidence: {} busy tries; Q2 set aside meanwhile; the idle pane got one line: {}",
        tried.len(),
        typed[0]
    );
}

#[tokio::test(start_paused = true)]
async fn a_batch_in_flight_at_shutdown_is_typed_again_once_after_a_restart() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    answer_a(&*store, "Q1").await;
    let first = spawn_sender(&store, FakeTyping::new(vec![Step::Hang], Typing::Typed));
    until("the first sender is typing", || {
        first.typing.calls().len() == 1
    })
    .await;
    // Harold stops while the line is being typed: it may have reached the pane.
    first.shutdown.send(()).unwrap();
    first.task.await.unwrap();
    let started = store
        .started_batch()
        .await
        .unwrap()
        .expect("the batch stays started");
    drop(store);

    let store: Arc<TursoQuestionStore> = open_store(&folder).await;
    let second = spawn_sender(&store, FakeTyping::typed_ok());
    until("the restarted sender types", || {
        second.typing.calls().len() == 1
    })
    .await;
    until_no_started_batch(&*store).await;
    second.shutdown.send(()).unwrap();
    second.task.await.unwrap();
    drop(store);

    let store: Arc<TursoQuestionStore> = open_store(&folder).await;
    let third = spawn_sender(&store, FakeTyping::typed_ok());
    third.wake.notify_one();
    pass(30).await;
    assert_eq!(second.typing.texts(), vec![started.text.clone()]);
    assert!(third.typing.calls().is_empty(), "typed once, not again");
    println!(
        "evidence: batch {} in flight at shutdown, typed once after the restart: {}; a second restart typed nothing",
        started.id, started.text
    );
}

/// The sender typed the line and Harold stopped before the mark was recorded:
/// the batch stays started, and the next start types it once more.
#[tokio::test(start_paused = true)]
async fn a_stop_after_typed_and_before_the_mark_is_typed_once_more_on_restart() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    answer_a(&*store, "Q1").await;
    let hanging = MarkHook::new(Arc::clone(&store), MarkMode::Hang);
    let first = spawn_sender(&hanging, FakeTyping::typed_ok());
    until("the first sender has typed", || {
        first.typing.calls().len() == 1
    })
    .await;
    // The mark never returns, so the stop lands between typing and the mark.
    first.shutdown.send(()).unwrap();
    first.task.await.unwrap();
    let started = store
        .started_batch()
        .await
        .unwrap()
        .expect("the batch stays started");
    drop(hanging);
    drop(store);

    let store: Arc<TursoQuestionStore> = open_store(&folder).await;
    let second = spawn_sender(&store, FakeTyping::typed_ok());
    until("the restarted sender types", || {
        second.typing.calls().len() == 1
    })
    .await;
    until_no_started_batch(&*store).await;
    second.shutdown.send(()).unwrap();
    second.task.await.unwrap();
    drop(store);

    let store: Arc<TursoQuestionStore> = open_store(&folder).await;
    let third = spawn_sender(&store, FakeTyping::typed_ok());
    third.wake.notify_one();
    pass(30).await;
    assert!(
        third.typing.calls().is_empty(),
        "typed once more, not again"
    );
    let lines: Vec<String> = first
        .typing
        .texts()
        .into_iter()
        .chain(second.typing.texts())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], lines[1]);
    assert_eq!(lines[0], started.text);
    println!(
        "evidence: batch {} typed, stopped before the mark, typed once more after the restart, nothing on a third start; {} lines: {:?}",
        started.id,
        lines.len(),
        lines
    );
}

/// The sender's own task panics after the line was typed, before the mark.
#[tokio::test(start_paused = true)]
async fn a_sender_panic_after_typed_ends_the_task_and_the_restart_types_once_more() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    answer_a(&*store, "Q1").await;
    let panicking = MarkHook::new(Arc::clone(&store), MarkMode::Panic);
    let first = spawn_sender(&panicking, FakeTyping::typed_ok());
    let error = first
        .task
        .await
        .expect_err("the sender task ends in a panic");
    assert!(error.is_panic(), "the task panicked: {error:?}");
    let started = store
        .started_batch()
        .await
        .unwrap()
        .expect("the batch stays started");
    drop(panicking);
    drop(store);

    let store: Arc<TursoQuestionStore> = open_store(&folder).await;
    let second = spawn_sender(&store, FakeTyping::typed_ok());
    until("the restarted sender types", || {
        second.typing.calls().len() == 1
    })
    .await;
    until_no_started_batch(&*store).await;
    second.shutdown.send(()).unwrap();
    second.task.await.unwrap();
    drop(store);

    let store: Arc<TursoQuestionStore> = open_store(&folder).await;
    let third = spawn_sender(&store, FakeTyping::typed_ok());
    third.wake.notify_one();
    pass(30).await;
    assert!(
        third.typing.calls().is_empty(),
        "typed once more, not again"
    );
    let lines: Vec<String> = first
        .typing
        .texts()
        .into_iter()
        .chain(second.typing.texts())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], lines[1]);
    assert_eq!(lines[0], started.text);
    println!(
        "evidence: batch {} typed, sender task panicked before the mark, typed once more after the restart, nothing on a third start; {} lines: {:?}",
        started.id,
        lines.len(),
        lines
    );
}

/// The reviewer's restart scenario: B1 may have reached the chief's pane when
/// Harold stopped. After the restart the pane is busy and Q2 is put; B1 must be
/// kept, not released (or Q1 would come again inside `ANSWERS 2`), and typed
/// once when the pane is idle, with Q2 still waiting.
#[tokio::test(start_paused = true)]
async fn a_restart_with_a_busy_pane_keeps_the_leftover_batch_and_types_it_once() {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    answer_a(&*store, "Q1").await;
    let first = spawn_sender(&store, FakeTyping::new(vec![Step::Hang], Typing::Typed));
    until("the first sender is typing", || {
        first.typing.calls().len() == 1
    })
    .await;
    // Harold dies while B1 is being typed: started, never marked typed.
    first.task.abort();
    let _ = first.task.await;
    let b1 = store.started_batch().await.unwrap().expect("B1 started");
    drop(store);

    // The restart: the chief is busy reading B1, and puts Q2 meanwhile.
    let store: Arc<TursoQuestionStore> = open_store(&folder).await;
    let second = spawn_sender(&store, FakeTyping::new(vec![], Typing::PaneBusy));
    until("the restarted sender tries B1", || {
        !second.typing.calls().is_empty()
    })
    .await;
    put(&*store, question("Q2", "s2", "abc")).await;
    second.wake.notify_one();
    pass(30).await;
    let tries = second.typing.calls().len();
    assert!(tries >= 2, "tried again while busy: {tries}");
    assert_eq!(
        store.started_batch().await.unwrap(),
        Some(b1.clone()),
        "B1 is kept, not released"
    );
    assert!(
        second.typing.texts().iter().all(|t| t == &b1.text),
        "{:?}",
        second.typing.texts()
    );
    assert!(
        logs.lines().iter().all(|l| !l.contains("ANSWERS 2")),
        "{:#?}",
        logs.lines()
    );

    // The pane goes idle: B1 is typed once, though Q2 waits.
    second.typing.set_fallback(Typing::Typed);
    until("B1 is typed", || second.typing.calls().len() == tries + 1).await;
    until_no_started_batch(&*store).await;
    second.wake.notify_one();
    pass(30).await;
    let texts = second.typing.texts();
    assert_eq!(texts.len(), tries + 1, "one line, once: {texts:?}");
    assert!(texts.iter().all(|t| t == &b1.text), "{texts:?}");
    assert!(
        texts.iter().all(|t| !t.starts_with("ANSWERS 2")),
        "{texts:?}"
    );
    assert_eq!(logs.with("answers batch typed").len(), 1);
    let waiting = logs.with("answers batch waiting");
    assert_eq!(waiting.len(), 1, "{:#?}", logs.lines());
    assert!(
        waiting[0].contains("reason=\"pane_busy\""),
        "{}",
        waiting[0]
    );
    let listing = store.list(0).await.unwrap();
    let q2 = listing
        .questions
        .iter()
        .find(|l| l.question.id == "Q2")
        .expect("Q2 listed");
    assert!(!q2.deferred, "Q2 still waiting");
    assert_eq!(
        store.ready(i64::MAX / 2, 3_000).await.unwrap(),
        Readiness::Nothing,
        "nothing is unsent"
    );
    println!(
        "evidence: after a restart B1 ({}) was kept through {tries} busy tries while Q2 waited, never released; then typed once: {}; Q2 still waiting",
        b1.id, texts[tries]
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_release_is_retried_before_the_batch_is_built_again() {
    let (logs, _guard) = capture_logs();
    let folder = TempFolder::new();
    let inner = open_store(&folder).await;
    put(&*inner, question("Q1", "s1", "abc")).await;
    answer_a(&*inner, "Q1").await;
    let failing = FailingStore::new(Arc::clone(&inner));
    failing.writes.store(false, Ordering::SeqCst);
    failing.releases.store(true, Ordering::SeqCst);
    let sender = spawn_sender(&failing, FakeTyping::new(vec![], Typing::PaneBusy));
    until("the first try", || sender.typing.calls().len() == 1).await;
    // The failed release is logged in the same round, after its `pane_busy`.
    pass(1).await;
    assert_eq!(
        logs.with("reason=\"store_failed\"").len(),
        1,
        "{:#?}",
        logs.lines()
    );
    pass(29).await;
    // The unreleased batch is neither typed again nor left for a question to skip.
    assert_eq!(sender.typing.calls().len(), 1);
    assert!(inner.started_batch().await.unwrap().is_some());

    failing.releases.store(false, Ordering::SeqCst);
    until_no_started_batch(&*inner).await;
    sender.typing.set_fallback(Typing::Typed);
    until("the batch is typed", || {
        logs.with("answers batch typed").len() == 1
    })
    .await;
    // One line per change of reason: the busy pane, then the failed release.
    let waiting = logs.with("answers batch waiting");
    assert!(waiting.len() >= 2, "{:#?}", logs.lines());
    assert!(
        waiting[0].contains("reason=\"pane_busy\""),
        "{}",
        waiting[0]
    );
    assert!(
        waiting[1].contains("reason=\"store_failed\""),
        "{}",
        waiting[1]
    );
    assert!(
        logs.with("reason=\"store_failed\"").len() == 1,
        "store_failed logged once for 30 s of failed releases: {:#?}",
        logs.lines()
    );
    assert!(
        waiting.windows(2).all(|w| w[0] != w[1]),
        "no reason logged twice in a row: {waiting:#?}"
    );
    println!(
        "evidence: the release failed for 30 s with no retyping; once it succeeded the batch was built again and typed; logs: {}; {}",
        waiting[0], waiting[1]
    );
}

#[tokio::test(start_paused = true)]
async fn an_idle_sender_looks_again_without_a_wake() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    pass(1).await;
    // A write whose call was cancelled after it committed sends no wake.
    answer_a(&*store, "Q1").await;
    pass(IDLE_RECHECK.as_secs() - 2).await;
    assert!(sender.typing.calls().is_empty());
    pass(2).await;
    until("the batch is typed", || sender.typing.calls().len() == 1).await;
    println!(
        "evidence: an answer stored with no wake was typed on the {IDLE_RECHECK:?} re-check: {:?}",
        sender.typing.texts()
    );
}

#[tokio::test(start_paused = true)]
async fn three_waiting_two_answered_types_one_line_after_the_settle() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    for n in 25..=28 {
        put(&*store, question(&format!("Q{n}"), &format!("s{n}"), "abc")).await;
    }
    defer(&*store, "Q28").await;
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    pass(1).await;

    // t = 0: Q25; t = 1 s: Q26. The line is due 3 s after Q26, at t = 4 s.
    answer_a(&*store, "Q25").await;
    sender.wake.notify_one();
    pass(1).await;
    answer_a(&*store, "Q26").await;
    sender.wake.notify_one();
    pass_ms(2_900).await;
    assert!(sender.typing.calls().is_empty(), "inside the settle");
    pass_ms(200).await;
    let expected = "ANSWERS 2 | Q25 s25 sha abc: Which? A | Q26 s26 sha abc: Which? A | later: Q28";
    assert_eq!(sender.typing.texts(), vec![expected.to_string()]);
    println!("evidence: {:?}", sender.typing.texts());
    until_no_started_batch(&*store).await;
    // Q27 keeps waiting, and Q28 stays set aside while Q27 waits.
    assert_eq!(
        listed(&*store).await,
        vec![("Q27".to_string(), false), ("Q28".to_string(), true)]
    );
    pass(30).await;
    assert_eq!(sender.typing.calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn answers_one_second_apart_type_one_line_and_five_seconds_apart_two() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    for n in 1..=4 {
        put(&*store, question(&format!("Q{n}"), &format!("s{n}"), "abc")).await;
    }
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    answer_a(&*store, "Q1").await;
    sender.wake.notify_one();
    pass(1).await;
    answer_a(&*store, "Q2").await;
    sender.wake.notify_one();
    pass(10).await;
    assert_eq!(
        sender.typing.texts(),
        vec!["ANSWERS 2 | Q1 s1 sha abc: Which? A | Q2 s2 sha abc: Which? A".to_string()]
    );

    answer_a(&*store, "Q3").await;
    sender.wake.notify_one();
    pass(5).await;
    answer_a(&*store, "Q4").await;
    sender.wake.notify_one();
    pass(10).await;
    let texts = sender.typing.texts();
    assert_eq!(texts.len(), 3, "{texts:?}");
    assert_eq!(texts[1], "ANSWERS 1 | Q3 s3 sha abc: Which? A");
    assert_eq!(texts[2], "ANSWERS 1 | Q4 s4 sha abc: Which? A");
    println!("evidence: 1 s apart one line, 5 s apart two: {texts:?}");
}

#[tokio::test(start_paused = true)]
async fn nothing_answered_types_nothing() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    put(&*store, question("Q2", "s2", "abc")).await;
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    pass(60).await;
    assert!(sender.typing.calls().is_empty());
    assert_eq!(store.started_batch().await.unwrap(), None);
}

#[tokio::test(start_paused = true)]
async fn a_marked_answer_is_typed_at_once() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    put(&*store, question("Q2", "s2", "abc")).await;
    put(&*store, question("Q3", "s3", "abc")).await;
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    pass(1).await;

    // A marked answer goes at once, though Q2 and Q3 wait.
    let marked_at = tokio::time::Instant::now();
    answer_marked(&*store, "Q1").await;
    sender.wake.notify_one();
    tried_without_waiting(&sender, 1).await;
    let elapsed = marked_at.elapsed();
    assert_eq!(elapsed, Duration::ZERO);
    assert_eq!(
        sender.typing.texts(),
        vec!["ANSWERS 1 | Q1 s1 sha abc: Which? A".to_string()]
    );
    println!(
        "evidence: marked answer typed {elapsed:?} after it was given: {:?}",
        sender.typing.texts()
    );

    // Unmarked at t = 0, marked at t = 1 s: one line with both, at t = 1 s.
    until_no_started_batch(&*store).await;
    answer_a(&*store, "Q2").await;
    sender.wake.notify_one();
    pass(1).await;
    assert_eq!(sender.typing.calls().len(), 1, "Q2 is settling");
    let marked_at = tokio::time::Instant::now();
    answer_marked(&*store, "Q3").await;
    sender.wake.notify_one();
    tried_without_waiting(&sender, 2).await;
    assert_eq!(marked_at.elapsed(), Duration::ZERO);
    assert_eq!(
        sender.typing.texts()[1],
        "ANSWERS 2 | Q2 s2 sha abc: Which? A | Q3 s3 sha abc: Which? A"
    );
    println!(
        "evidence: unmarked then marked: {:?}",
        sender.typing.texts()[1]
    );
}

/// An answer given while a line is typed: `marked` or not, it is not in that
/// line, is not lost, and goes in the next line once.
async fn an_answer_while_a_line_is_typed(marked: bool) -> (Vec<String>, Duration) {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    put(&*store, question("Q2", "s2", "abc")).await;
    if marked {
        answer_marked(&*store, "Q1").await;
    } else {
        answer_a(&*store, "Q1").await;
    }
    let typing = FakeTyping::typed_ok();
    let writer = Arc::clone(&store);
    typing.during_first_call(async move {
        if marked {
            answer_marked(&*writer, "Q2").await;
        } else {
            answer_a(&*writer, "Q2").await;
        }
    });
    let sender = spawn_sender(&store, typing);
    let waited = if marked {
        tried_without_waiting(&sender, 1).await;
        let first_at = tokio::time::Instant::now();
        tried_without_waiting(&sender, 2).await;
        first_at.elapsed()
    } else {
        until("the first line is tried", || {
            sender.typing.calls().len() == 1
        })
        .await;
        let first_at = tokio::time::Instant::now();
        pass_ms(2_800).await;
        assert_eq!(sender.typing.calls().len(), 1, "inside the second settle");
        pass_ms(400).await;
        assert_eq!(sender.typing.calls().len(), 2);
        first_at.elapsed()
    };
    pass(30).await;
    assert_eq!(sender.typing.calls().len(), 2, "never sent twice");
    (sender.typing.texts(), waited)
}

#[tokio::test(start_paused = true)]
async fn an_answer_while_a_line_is_typed_goes_in_the_next_line_once() {
    // The first line is due after the settle; the unmarked answer given
    // during it is due a settle after that.
    let (texts, waited) = an_answer_while_a_line_is_typed(false).await;
    assert_eq!(
        texts,
        vec![
            "ANSWERS 1 | Q1 s1 sha abc: Which? A".to_string(),
            "ANSWERS 1 | Q2 s2 sha abc: Which? A".to_string(),
        ]
    );
    println!(
        "evidence: unmarked answer during a line goes next, once: {texts:?} ({waited:?} after the first)"
    );

    // A marked one goes at once after the first is typed.
    let (texts, waited) = an_answer_while_a_line_is_typed(true).await;
    assert_eq!(
        texts,
        vec![
            "ANSWERS 1 | Q1 s1 sha abc: Which? A".to_string(),
            "ANSWERS 1 | Q2 s2 sha abc: Which? A".to_string(),
        ]
    );
    assert_eq!(waited, Duration::ZERO);
    println!(
        "evidence: marked answer during a line goes next at once, once: {texts:?} ({waited:?} after the first)"
    );
}

#[tokio::test(start_paused = true)]
async fn a_deferred_question_stays_set_aside_while_another_waits() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    for n in 1..=3 {
        put(&*store, question(&format!("Q{n}"), &format!("s{n}"), "abc")).await;
    }
    defer(&*store, "Q3").await;
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    answer_a(&*store, "Q1").await;
    sender.wake.notify_one();
    pass(5).await;
    assert_eq!(
        sender.typing.texts(),
        vec!["ANSWERS 1 | Q1 s1 sha abc: Which? A | later: Q3".to_string()]
    );
    until_no_started_batch(&*store).await;
    assert_eq!(
        listed(&*store).await,
        vec![("Q2".to_string(), false), ("Q3".to_string(), true)],
        "Q3 stays set aside while Q2 waits"
    );

    answer_a(&*store, "Q2").await;
    sender.wake.notify_one();
    pass(5).await;
    assert_eq!(
        sender.typing.texts()[1],
        "ANSWERS 1 | Q2 s2 sha abc: Which? A | later: Q3"
    );
    until_no_started_batch(&*store).await;
    assert_eq!(listed(&*store).await, vec![("Q3".to_string(), false)]);
}

#[tokio::test(start_paused = true)]
async fn after_a_restart_old_unsent_answers_go_at_once_and_fresh_ones_wait_one_settle() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    answer_a(&*store, "Q1").await;
    drop(store);
    // Harold was down for a minute.
    pass(60).await;
    let store: Arc<TursoQuestionStore> = open_store(&folder).await;
    let restarted_at = tokio::time::Instant::now();
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    tried_without_waiting(&sender, 1).await;
    assert_eq!(
        restarted_at.elapsed(),
        Duration::ZERO,
        "old answers go at once"
    );
    until_no_started_batch(&*store).await;

    // An answer given just before the restart waits out what is left of its settle.
    put(&*store, question("Q2", "s2", "abc")).await;
    answer_a(&*store, "Q2").await;
    sender.shutdown.send(()).unwrap();
    let _ = sender.task.await;
    drop(store);
    pass(1).await;
    let store: Arc<TursoQuestionStore> = open_store(&folder).await;
    let restarted_at = tokio::time::Instant::now();
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    pass_ms(1_900).await;
    assert!(
        sender.typing.calls().is_empty(),
        "two seconds of the settle are left"
    );
    pass_ms(200).await;
    assert_eq!(sender.typing.calls().len(), 1);
    assert!(restarted_at.elapsed() < Duration::from_secs(3));
}

#[tokio::test(start_paused = true)]
async fn after_a_restart_a_marked_answer_goes_at_once() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    answer_marked(&*store, "Q1").await;
    drop(store);
    let store: Arc<TursoQuestionStore> = open_store(&folder).await;
    let restarted_at = tokio::time::Instant::now();
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    tried_without_waiting(&sender, 1).await;
    assert_eq!(restarted_at.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn a_wake_from_a_new_question_does_not_restart_the_settle() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    pass(1).await;
    let answered_at = tokio::time::Instant::now();
    answer_a(&*store, "Q1").await;
    sender.wake.notify_one();
    pass(2).await;
    put(&*store, question("Q2", "s2", "abc")).await;
    sender.wake.notify_one();
    until("the line is tried", || sender.typing.calls().len() == 1).await;
    let elapsed = answered_at.elapsed();
    assert!(
        elapsed >= SETTLE && elapsed < SETTLE + Duration::from_millis(100),
        "the line is due 3 s after the answer, was {elapsed:?}"
    );
    println!(
        "evidence: answer at 0, put at 2 s with a wake, line typed {elapsed:?} after the answer"
    );
}

#[tokio::test(start_paused = true)]
async fn a_busy_pane_retry_adds_no_second_settle() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    let busy = Step::Answer(Typing::PaneBusy);
    let sender = spawn_sender(&store, FakeTyping::new(vec![busy; 2], Typing::Typed));
    pass(1).await;
    let answered_at = tokio::time::Instant::now();
    answer_a(&*store, "Q1").await;
    sender.wake.notify_one();
    until("the line is typed", || sender.typing.calls().len() == 3).await;
    let elapsed = answered_at.elapsed();
    // Tries at 3 s, 8 s and 13 s: the settle once, then 5 s apart.
    let due = SETTLE + RETRY * 2;
    assert!(
        elapsed >= due && elapsed < due + Duration::from_millis(100),
        "expected about {due:?}, was {elapsed:?}"
    );
    println!(
        "evidence: 2 busy tries then typed {elapsed:?} after the answer (settle {SETTLE:?} once, retry {RETRY:?} twice)"
    );
}

#[tokio::test(start_paused = true)]
async fn deferring_every_waiting_question_returns_them_without_a_line() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    put(&*store, question("Q2", "s2", "abc")).await;
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    defer(&*store, "Q1").await;
    sender.wake.notify_one();
    assert_eq!(
        listed(&*store).await,
        vec![("Q2".to_string(), false), ("Q1".to_string(), true)]
    );
    defer(&*store, "Q2").await;
    sender.wake.notify_one();
    pass(60).await;
    let listing = listed(&*store).await;
    assert_eq!(
        listing,
        vec![("Q1".to_string(), false), ("Q2".to_string(), false)]
    );
    assert!(sender.typing.calls().is_empty(), "no line");
    println!("evidence: every waiting question set aside; no line typed; listed {listing:?}");
}

#[tokio::test(start_paused = true)]
async fn deferring_the_last_waiting_question_with_an_unsent_answer_returns_it_after_the_line() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    put(&*store, question("Q2", "s2", "abc")).await;
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    answer_a(&*store, "Q1").await;
    defer(&*store, "Q2").await;
    sender.wake.notify_one();
    pass_ms(1_000).await;
    assert_eq!(
        listed(&*store).await,
        vec![("Q2".to_string(), true)],
        "set aside until the line is typed"
    );
    pass(5).await;
    let texts = sender.typing.texts();
    assert_eq!(
        texts,
        vec!["ANSWERS 1 | Q1 s1 sha abc: Which? A | later: Q2".to_string()]
    );
    until_no_started_batch(&*store).await;
    let listing = listed(&*store).await;
    assert_eq!(listing, vec![("Q2".to_string(), false)]);
    println!(
        "evidence: unsent answer, last waiting question set aside: line {texts:?}, then listed {listing:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_answer_stamped_in_the_future_is_typed_within_one_settle() {
    let folder = TempFolder::new();
    let store = open_store(&folder).await;
    put(&*store, question("Q1", "s1", "abc")).await;
    let result = store
        .answer(
            "Q1",
            1,
            false,
            |q| schema::resolve_picks(q, Kind::Answer, &[pick(0, "a")], ""),
            clock_ms() + 10_000,
        )
        .await
        .unwrap();
    assert_eq!(result, Ok(Ok(())));
    let started = tokio::time::Instant::now();
    let sender = spawn_sender(&store, FakeTyping::typed_ok());
    pass(3).await;
    assert_eq!(sender.typing.calls().len(), 1, "typed within one settle");
    println!(
        "evidence: answer stamped 10 s ahead of the clock, typed within {:?} of the sender starting: {:?}",
        started.elapsed(),
        sender.typing.texts()
    );
}
