//! The call answerer: the arguments and input of a run, what the facts hold and leave out,
//! the reading of a run's output, each way a run fails, and the real Claude Code process
//! standing in as a script.

use super::*;
use crate::agent::domain::EffectiveAgentState;
use crate::outbound::fake::CapturedLogs;
use crate::pod_stats::{SlotCounts, SpecFacts, SpecState};
use crate::telemetry::test_support;
use harold_api::tools::questions::v1::{Decision, DecisionOption, Pick, Question, Step};
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

struct FakeSources {
    agents: Vec<AgentFact>,
    pods: PodSnapshot,
    todos: Option<Vec<Todo>>,
    queue: Option<Vec<QueueNote>>,
    /// How long the question tool and the backlog tool each take.
    slow: Duration,
}

impl FactSources for FakeSources {
    fn agents(&self) -> Vec<AgentFact> {
        self.agents.clone()
    }

    fn pods(&self) -> PodSnapshot {
        self.pods.clone()
    }

    fn todos(&self) -> BoxFuture<'_, Option<Vec<Todo>>> {
        Box::pin(async move {
            tokio::time::sleep(self.slow).await;
            self.todos.clone()
        })
    }

    fn queue(&self) -> BoxFuture<'_, Option<Vec<QueueNote>>> {
        Box::pin(async move {
            tokio::time::sleep(self.slow).await;
            self.queue.clone()
        })
    }
}

#[derive(Default)]
struct FakeRunner {
    results: Mutex<VecDeque<RunResult>>,
    runs: Mutex<Vec<Run>>,
    /// How many times the program was started with no model call.
    warmed: Mutex<usize>,
}

impl FakeRunner {
    fn giving(results: impl IntoIterator<Item = RunResult>) -> Arc<Self> {
        let runner = Self::default();
        runner.results.lock().unwrap().extend(results);
        Arc::new(runner)
    }

    fn prompts(&self) -> Vec<String> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .map(|run| run.prompt.clone())
            .collect()
    }
}

impl ModelRunner for FakeRunner {
    fn run<'a>(&'a self, run: &'a Run) -> BoxFuture<'a, RunResult> {
        Box::pin(async move {
            self.runs.lock().unwrap().push(run.clone());
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(RunResult::FailedToStart)
        })
    }

    fn warm(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            *self.warmed.lock().unwrap() += 1;
        })
    }
}

fn agent(folder: &str, state: EffectiveAgentState, summary: Option<&str>) -> AgentFact {
    AgentFact {
        pane_id: "%2".into(),
        tmux_target: "pods:0.1".into(),
        folder: folder.into(),
        provider_id: "claude".into(),
        reads_as_claude: true,
        state,
        state_since_ms: now_ms() - 90_000,
        work_summary: summary.map(str::to_string),
    }
}

fn pods() -> PodSnapshot {
    let mut snapshot = PodSnapshot::default();
    snapshot
        .slots
        .insert("harold".into(), SlotCounts { busy: 1, free: 2 });
    snapshot.specs.insert(
        "harold".into(),
        vec![SpecFacts {
            id: "log-keeps-last-day".into(),
            state: SpecState::Building,
            state_at: time::OffsetDateTime::now_utc(),
            ready_at: None,
            approved_at: None,
            merged_at: None,
            words: 1200,
            revisions: 0,
        }],
    );
    snapshot
}

fn sources() -> FakeSources {
    FakeSources {
        agents: vec![
            agent(
                "/Users/me/code/chief-of-pods/",
                EffectiveAgentState::Busy,
                Some(&"x".repeat(300)),
            ),
            agent("/Users/me/code/harold", EffectiveAgentState::Idle, None),
        ],
        pods: pods(),
        todos: Some(vec![
            Todo {
                id: "Q56".into(),
                product: "harold".into(),
                title: "agent-message-tool".into(),
                progress: "picks 2/2".into(),
            },
            Todo {
                id: "I7".into(),
                product: "harold".into(),
                title: "Install on the M1".into(),
                progress: "steps 0/6".into(),
            },
        ]),
        queue: Some(vec![QueueNote {
            state: "now".into(),
            id: "N1".into(),
            text: "Rerun the build.".into(),
        }]),
        slow: Duration::ZERO,
    }
}

fn answerer(sources: FakeSources, runner: &Arc<FakeRunner>) -> Answerer {
    Answerer::new(
        CallAnswererSettings::default(),
        Arc::clone(runner) as Arc<dyn ModelRunner>,
        Arc::new(sources),
    )
}

fn ok(stdout: &str) -> RunResult {
    RunResult::Exited {
        stdout: stdout.as_bytes().to_vec(),
        ok: true,
        exit: "exit code 0".into(),
    }
}

/// What Claude Code 2.1.308 printed for a run on Haiku with the schema.
const SAMPLE: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"say\":\"Yes, the chief is busy.\"}","structured_output":{"say":"Yes, the chief is busy."},"total_cost_usd":0.0002694,"usage":{"input_tokens":2,"cache_creation_input_tokens":1176,"cache_read_input_tokens":0,"output_tokens":68}}"#;

async fn ask(answerer: &Answerer, words: &str, call: &CallSoFar) -> Answer {
    let turn = tracing::Span::none();
    answerer
        .answer(Ask {
            words,
            call,
            turn: &turn,
            facts: None,
        })
        .await
}

fn prompt_json(runner: &FakeRunner) -> Value {
    serde_json::from_str(&runner.prompts()[0]).expect("the prompt is one JSON document")
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

fn texts(args: Vec<OsString>) -> Vec<String> {
    args.into_iter()
        .map(|arg| arg.to_str().unwrap().to_string())
        .collect()
}

fn run_of(prompt: &str) -> Run {
    Run {
        model: "haiku".into(),
        budget_usd: "0.02".into(),
        timeout: Duration::from_secs(15),
        prompt: prompt.into(),
    }
}

#[test]
fn a_run_is_safe_mode_with_no_tools_no_saved_transcript_and_the_schema() {
    let args = texts(claude_args(&run_of("x")));

    assert_eq!(
        &args[..3],
        ["--print", "--safe-mode", "--no-session-persistence"]
    );
    let value = |flag: &str| {
        let at = args.iter().position(|arg| arg == flag).unwrap();
        args[at + 1].clone()
    };
    assert_eq!(value("--tools"), "");
    assert_eq!(value("--model"), "haiku");
    assert_eq!(value("--output-format"), "json");
    assert_eq!(value("--json-schema"), SCHEMA);
    assert_eq!(value("--max-budget-usd"), "0.02");
    assert_eq!(value("--system-prompt"), RULES);
    for flag in ["--resume", "--continue", "--allowedTools", "--add-dir"] {
        assert!(!args.iter().any(|arg| arg == flag), "{flag}");
    }
}

#[test]
fn the_schema_is_the_specs_and_requires_only_say() {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    assert_eq!(schema["required"], json!(["say"]));
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(
        schema["properties"],
        json!({"say": {"type": "string"}, "hand_over": {"type": "string"}})
    );
}

#[test]
fn the_rules_carry_the_specs_rules() {
    for needle in [
        "one or two short spoken sentences",
        "Never say work is done or when it will be done",
        "I can only help with your pods and agents.",
        "Never say the chief has received it, read it or is working on it.",
        "could not be read",
    ] {
        assert!(RULES.contains(needle), "{needle}");
    }
}

#[test]
fn a_budget_is_written_without_trailing_zeros() {
    assert_eq!(usd_text(0.02), "0.02");
    assert_eq!(usd_text(0.1), "0.1");
    assert_eq!(usd_text(1.0), "1");
}

// ---------------------------------------------------------------------------
// Reading the output
// ---------------------------------------------------------------------------

#[test]
fn a_runs_structured_output_gives_the_line_the_cost_and_the_tokens() {
    let output = read_output(SAMPLE.as_bytes()).unwrap();

    assert_eq!(output.say, "Yes, the chief is busy.");
    assert_eq!(output.hand_over, None);
    assert_eq!(output.cost_micros, 269);
    assert_eq!(output.input_tokens, 1178);
    assert_eq!(output.output_tokens, 68);
}

#[test]
fn the_result_text_is_read_when_there_is_no_structured_output() {
    let out =
        r#"{"is_error":false,"result":"{\"say\":\"Passed on.\",\"hand_over\":\" rerun it \"}"}"#;

    let output = read_output(out.as_bytes()).unwrap();

    assert_eq!(output.say, "Passed on.");
    assert_eq!(output.hand_over.as_deref(), Some("rerun it"));
    assert_eq!(output.cost_micros, 0);
}

#[test]
fn output_with_no_say_or_that_is_an_error_or_not_json_gives_nothing() {
    for out in [
        "not json",
        "",
        r#"{"is_error":true,"structured_output":{"say":"x"}}"#,
        r#"{"structured_output":{"hand_over":"x"}}"#,
        r#"{"structured_output":{"say":"  "}}"#,
        r#"{"structured_output":{"say":7}}"#,
        r#"{"result":"plain words"}"#,
    ] {
        assert_eq!(read_output(out.as_bytes()), None, "{out}");
    }
}

#[test]
fn a_hand_over_with_no_line_is_kept_for_the_call_to_word() {
    let out = r#"{"structured_output":{"say":"","hand_over":"rerun it"}}"#;

    let output = read_output(out.as_bytes()).unwrap();

    assert_eq!(output.say, "");
    assert_eq!(output.hand_over.as_deref(), Some("rerun it"));
}

// ---------------------------------------------------------------------------
// The facts and the prompt
// ---------------------------------------------------------------------------

fn spec(id: &str, state: SpecState, age_hours: i64) -> SpecFacts {
    SpecFacts {
        id: id.into(),
        state,
        state_at: time::OffsetDateTime::now_utc() - time::Duration::hours(age_hours),
        ready_at: None,
        approved_at: None,
        merged_at: None,
        words: 100,
        revisions: 0,
    }
}

/// A pod with 100 old merged specs, 3 live ones, one merged and one rejected an hour ago,
/// and one rejected 30 hours ago.
fn crowded_pod() -> PodSnapshot {
    let mut specs: Vec<SpecFacts> = (0..100)
        .map(|n| spec(&format!("old-{n}"), SpecState::Merged, 48 + n))
        .collect();
    specs.extend([
        spec("drafted-one", SpecState::Drafted, 5),
        spec("waiting-one", SpecState::Waiting, 5),
        spec("building-one", SpecState::Building, 5),
        spec("just-merged", SpecState::Merged, 1),
        spec("just-rejected", SpecState::Rejected, 1),
        spec("long-rejected", SpecState::Rejected, 30),
    ]);
    let mut snapshot = PodSnapshot::default();
    snapshot
        .slots
        .insert("harold".into(), SlotCounts { busy: 1, free: 2 });
    snapshot.specs.insert("harold".into(), specs);
    snapshot
}

#[test]
fn the_facts_list_the_live_specs_and_those_finished_in_the_last_day() {
    let facts = super::pods(&crowded_pod(), now_ms());
    let ids: Vec<&str> = facts[0]["specs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|spec| spec["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "drafted-one",
            "waiting-one",
            "building-one",
            "just-merged",
            "just-rejected"
        ]
    );
    assert_eq!(facts[0]["slots"], json!({"busy": 1, "free": 2}));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_prompt_holds_the_agents_the_pods_the_todos_the_queue_this_call_and_the_words() {
    let runner = FakeRunner::giving([ok(SAMPLE)]);
    let call = CallSoFar {
        remarks: vec![
            Remark {
                who: Who::You,
                text: "ask the chief to rerun it".into(),
            },
            Remark {
                who: Who::Answerer,
                text: "Passed to the chief.".into(),
            },
            Remark {
                who: Who::Chief,
                text: "H1: done".into(),
            },
        ],
        hand_overs: vec![HandOverFact {
            id: "H1".into(),
            request: "rerun the build".into(),
            state: super::super::hand_over::HandOverState::Typed,
        }],
    };

    let answer = ask(&answerer(sources(), &runner), "is the chief busy", &call).await;

    assert_eq!(
        answer,
        Answer::Said {
            say: "Yes, the chief is busy.".into(),
            hand_over: None
        }
    );
    let prompt = prompt_json(&runner);
    let facts = &prompt["facts"];
    assert_eq!(facts["agents"][0]["agent"], "chief-of-pods");
    assert_eq!(facts["agents"][0]["state"], "busy");
    assert_eq!(facts["agents"][0]["for_seconds"], 90);
    assert_eq!(facts["agents"][0]["work"].as_str().unwrap().len(), 200);
    assert_eq!(facts["agents"][1]["agent"], "harold");
    assert_eq!(facts["agents"][1]["work"], Value::Null);
    assert_eq!(facts["pods"][0]["product"], "harold");
    assert_eq!(facts["pods"][0]["slots"], json!({"busy": 1, "free": 2}));
    assert_eq!(facts["pods"][0]["specs"][0]["id"], "log-keeps-last-day");
    assert_eq!(facts["pods"][0]["specs"][0]["state"], "building");
    assert_eq!(
        facts["todos"][0],
        json!({"id": "Q56", "product": "harold", "title": "agent-message-tool", "progress": "picks 2/2"})
    );
    assert_eq!(facts["todos"][1]["progress"], "steps 0/6");
    assert_eq!(
        facts["queue"][0],
        json!({"state": "now", "id": "N1", "text": "Rerun the build."})
    );
    assert!(facts["now"].as_str().unwrap().len() == 19);
    assert_eq!(
        prompt["this_call"]["hand_overs"][0],
        json!({"id": "H1", "request": "rerun the build", "state": "typed into the chief's pane"})
    );
    assert_eq!(
        prompt["this_call"]["so_far"],
        json!([
            {"who": "you", "text": "ask the chief to rerun it"},
            {"who": "answerer", "text": "Passed to the chief."},
            {"who": "chief", "text": "H1: done"},
        ])
    );
    assert_eq!(prompt["you_said"], "is the chief busy");
}

#[test]
fn a_hand_over_waiting_is_worded_waiting_and_never_received() {
    use super::super::hand_over::HandOverState;
    assert_eq!(HandOverState::Waiting.as_fact(), "waiting");
    assert_eq!(
        HandOverState::Typed.as_fact(),
        "typed into the chief's pane"
    );
}

fn listed_question() -> ListedQuestion {
    ListedQuestion {
        question: Some(Question {
            id: "Q56".into(),
            product: "harold".into(),
            spec: "agent-message-tool".into(),
            sha: "abc".into(),
            link: "https://example.com/SECRET-LINK".into(),
            intro: "SECRET-INTRO which way should the message go?".into(),
            decisions: vec![
                Decision {
                    ask: "SECRET-ASK first?".into(),
                    options: vec![DecisionOption {
                        key: "a".into(),
                        label: "SECRET-OPTION-LABEL".into(),
                        impact: "SECRET-IMPACT".into(),
                        suggested: true,
                        free: false,
                    }],
                },
                Decision {
                    ask: "SECRET-ASK second?".into(),
                    options: vec![],
                },
            ],
            title: String::new(),
            steps: vec![],
        }),
        version: 3,
        deferred: false,
        ticked: vec![],
        picks: vec![Pick {
            decision: 0,
            key: "a".into(),
            text: "SECRET-PICK-TEXT".into(),
        }],
    }
}

fn listed_instructions() -> ListedQuestion {
    ListedQuestion {
        question: Some(Question {
            id: "I7".into(),
            product: "personal".into(),
            spec: String::new(),
            sha: String::new(),
            link: String::new(),
            intro: "SECRET-INSTRUCTION-INTRO".into(),
            decisions: vec![],
            title: "Install on the M1".into(),
            steps: (0..6)
                .map(|n| Step {
                    text: format!("SECRET-STEP-{n}"),
                    detail: "SECRET-DETAIL".into(),
                })
                .collect(),
        }),
        version: 1,
        deferred: false,
        ticked: vec![1, 2],
        picks: vec![],
    }
}

#[test]
fn a_todo_is_its_id_product_title_and_progress_and_nothing_of_its_questions() {
    let question = todo_from(&listed_question()).unwrap();
    let instructions = todo_from(&listed_instructions()).unwrap();

    assert_eq!(
        question,
        Todo {
            id: "Q56".into(),
            product: "harold".into(),
            title: "agent-message-tool".into(),
            progress: "picks 1/2".into(),
        }
    );
    assert_eq!(
        instructions,
        Todo {
            id: "I7".into(),
            product: "personal".into(),
            title: "Install on the M1".into(),
            progress: "steps 2/6".into(),
        }
    );
    let all = format!("{question:?}{instructions:?}");
    assert!(!all.contains("SECRET"), "{all}");
    assert_eq!(todo_from(&ListedQuestion::default()), None);
}

#[test]
fn a_queue_note_is_its_state_id_and_first_sentence_cut_to_120_characters() {
    let item = |id: &str, text: &str| QueueItem {
        id: id.into(),
        state: String::new(),
        position: 0,
        text: text.into(),
        done_at_ms: 0,
        updated_at_ms: 0,
    };
    let long = format!("{} and on", "w".repeat(200));
    let queue = queue_from(
        Some(&item("N1", "Rerun the build. Then SECRET-SECOND-SENTENCE.")),
        &[
            item("N2", "Install on the M1? SECRET-BODY"),
            item("N3", &long),
            item("N4", "First line\nSECRET-SECOND-LINE"),
        ],
    );

    assert_eq!(queue.len(), 4);
    assert_eq!(
        (queue[0].state.as_str(), queue[0].text.as_str()),
        ("now", "Rerun the build.")
    );
    assert_eq!(queue[1].state, "parked");
    assert_eq!(queue[1].text, "Install on the M1?");
    assert_eq!(queue[2].text.chars().count(), 120);
    assert_eq!(queue[3].text, "First line");
    assert!(!format!("{queue:?}").contains("SECRET"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_prompt_holds_no_note_body_no_full_queue_note_and_no_todo_question_text_or_options() {
    let long = format!("Rerun the build. SECRET-NOTE-BODY {}", "z".repeat(300));
    let item = QueueItem {
        id: "N1".into(),
        state: "now".into(),
        position: 0,
        text: long,
        done_at_ms: 0,
        updated_at_ms: 0,
    };
    let mut sources = sources();
    sources.todos = Some(
        [listed_question(), listed_instructions()]
            .iter()
            .filter_map(todo_from)
            .collect(),
    );
    sources.queue = Some(queue_from(Some(&item), &[]));
    let runner = FakeRunner::giving([ok(SAMPLE)]);

    ask(
        &answerer(sources, &runner),
        "what is waiting for me",
        &CallSoFar::default(),
    )
    .await;

    let prompt = runner.prompts()[0].clone();
    assert!(!prompt.contains("SECRET"), "{prompt}");
    assert!(!prompt.contains("zzzz"), "{prompt}");
    assert!(prompt.contains("Rerun the build."));
    assert!(prompt.contains("agent-message-tool"));
    assert!(prompt.contains("Install on the M1"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_prompt_keeps_the_last_twelve_remarks_each_cut() {
    let runner = FakeRunner::giving([ok(SAMPLE)]);
    let call = CallSoFar {
        remarks: (0..20)
            .map(|n| Remark {
                who: Who::You,
                text: if n == 19 {
                    "q".repeat(900)
                } else {
                    format!("remark {n}")
                },
            })
            .collect(),
        hand_overs: vec![],
    };

    ask(&answerer(sources(), &runner), "x", &call).await;

    let prompt = prompt_json(&runner);
    let so_far = prompt["this_call"]["so_far"].as_array().unwrap();
    assert_eq!(so_far.len(), 12);
    assert_eq!(so_far[0]["text"], "remark 8");
    assert_eq!(so_far[11]["text"].as_str().unwrap().len(), 600);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_tool_that_takes_over_a_second_is_left_out_and_the_facts_say_so() {
    let mut slow = sources();
    slow.slow = Duration::from_secs(5);
    let runner = FakeRunner::giving([ok(SAMPLE)]);
    let started = tokio::time::Instant::now();

    let answer = ask(
        &answerer(slow, &runner),
        "what is waiting",
        &CallSoFar::default(),
    )
    .await;

    assert!(matches!(answer, Answer::Said { .. }));
    // Both tools were waited for together, for one second.
    assert_eq!(started.elapsed(), PART_LIMIT);
    let facts = &prompt_json(&runner)["facts"];
    assert_eq!(facts["todos"], "could not be read");
    assert_eq!(facts["queue"], "could not be read");
    assert_eq!(facts["agents"][0]["agent"], "chief-of-pods");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_tool_that_cannot_be_read_is_one_part_the_facts_say_could_not_be_read() {
    let mut broken = sources();
    broken.todos = None;
    let runner = FakeRunner::giving([ok(SAMPLE)]);

    ask(&answerer(broken, &runner), "x", &CallSoFar::default()).await;

    let facts = &prompt_json(&runner)["facts"];
    assert_eq!(facts["todos"], "could not be read");
    assert_eq!(facts["queue"][0]["id"], "N1");
}

#[test]
fn a_first_sentence_stops_at_the_first_full_stop_question_or_line() {
    assert_eq!(first_sentence("One. Two."), "One.");
    assert_eq!(first_sentence("Is it? Yes."), "Is it?");
    assert_eq!(first_sentence("No stop at all"), "No stop at all");
    assert_eq!(first_sentence("v1.2 is out"), "v1.2 is out");
    assert_eq!(first_sentence("  Line one\nLine two"), "Line one");
    assert_eq!(first_sentence(""), "");
}

// ---------------------------------------------------------------------------
// What a run gives, and how it fails
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_hand_over_comes_back_with_its_line() {
    let out = r#"{"is_error":false,"structured_output":{"say":"I've passed that to the chief.","hand_over":"rerun the harold build"}}"#;
    let runner = FakeRunner::giving([ok(out)]);

    let answer = ask(&answerer(sources(), &runner), "x", &CallSoFar::default()).await;

    assert_eq!(
        answer,
        Answer::Said {
            say: "I've passed that to the chief.".into(),
            hand_over: Some("rerun the harold build".into()),
        }
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn each_way_a_run_fails_has_its_reason_and_a_warning_without_the_words() {
    let logs = CapturedLogs::start();
    let cases = [
        (RunResult::FailedToStart, Failure::NotStarted),
        (RunResult::Killed, Failure::TimedOut),
        (
            RunResult::Exited {
                stdout: b"words SECRET-OUT".to_vec(),
                ok: false,
                exit: "exit code 1".into(),
            },
            Failure::Exited,
        ),
        (ok("not json SECRET-OUT"), Failure::NoAnswer),
        (
            ok(r#"{"structured_output":{"hand_over":"x"}}"#),
            Failure::NoAnswer,
        ),
    ];
    for (result, failure) in cases {
        let runner = FakeRunner::giving([result]);
        let answer = ask(
            &answerer(sources(), &runner),
            "SECRET-WORDS",
            &CallSoFar::default(),
        )
        .await;
        assert_eq!(answer, Answer::Failed(failure));
    }

    let text = logs.text();
    for reason in ["not_started", "timed_out", "exited", "no_answer"] {
        assert!(
            text.contains(&format!("call answer failed reason=\"{reason}\"")),
            "{reason}: {text}"
        );
    }
    assert!(text.contains("error=\"exit code 1\""), "{text}");
    assert!(!text.contains("SECRET"), "{text}");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_run_span_carries_the_result_the_times_the_cost_and_the_tokens_and_no_words() {
    test_support::install();
    let turn = tracing::info_span!(target: crate::telemetry::TRACE_TARGET, parent: None, "harold.call_turn");
    let trace = {
        use opentelemetry::trace::TraceContextExt as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        turn.context().span().span_context().trace_id()
    };
    let mut slow = sources();
    slow.slow = Duration::from_millis(200);
    let runner = FakeRunner::giving([
        ok(SAMPLE),
        RunResult::Killed,
        ok(r#"{"structured_output":{"say":"ok","hand_over":"SECRET-REQUEST"}}"#),
    ]);
    let answerer = answerer(slow, &runner);
    for words in ["SECRET-WORDS-ONE", "two", "three"] {
        answerer
            .answer(Ask {
                words,
                call: &CallSoFar::default(),
                turn: &turn,
                facts: None,
            })
            .await;
    }
    drop(turn);

    let spans: Vec<_> = test_support::spans()
        .into_iter()
        .filter(|span| span.name == "harold.call_answer" && span.span_context.trace_id() == trace)
        .collect();
    assert_eq!(spans.len(), 3);
    let attr = |n: usize, key: &str| test_support::span_attr(&spans[n], key);
    assert_eq!(attr(0, "result").as_deref(), Some("said"));
    assert_eq!(attr(0, "facts_ms").as_deref(), Some("200"));
    assert_eq!(attr(0, "model_ms").as_deref(), Some("0"));
    assert_eq!(attr(0, "cost_micros").as_deref(), Some("269"));
    assert_eq!(attr(0, "input_tokens").as_deref(), Some("1178"));
    assert_eq!(attr(0, "output_tokens").as_deref(), Some("68"));
    assert_eq!(attr(0, "reason"), None);
    assert_eq!(attr(1, "result").as_deref(), Some("failed"));
    assert_eq!(attr(1, "reason").as_deref(), Some("timed_out"));
    assert_eq!(attr(2, "result").as_deref(), Some("handed_over"));
    let exported = test_support::exported_text();
    assert!(!exported.contains("SECRET"), "{exported}");
}

// ---------------------------------------------------------------------------
// The real process, as a script
// ---------------------------------------------------------------------------

fn script(body: &str) -> (std::path::PathBuf, String) {
    let folder = std::env::temp_dir().join(format!("harold-answerer-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&folder).unwrap();
    let path = folder.join("claude");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let program = path.display().to_string();
    (folder, program)
}

#[tokio::test]
async fn the_real_runner_gives_the_prompt_on_standard_input_and_reads_standard_output() {
    let folder = std::env::temp_dir().join(format!("harold-answerer-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&folder).unwrap();
    let dir = folder.display();
    let (_, program) = script(&format!(
        "cat > {dir}/stdin\nprintf '%s\\n' \"$@\" > {dir}/args\nprintf '%s' \"$HOME-$SECRET_ENV\" > {dir}/env\nprintf '%s' '{SAMPLE}'"
    ));
    let runner = ClaudeRunner::new(program.clone(), vec![("HOME".into(), "/home/test".into())]);

    let ran = runner.run(&run_of("THE-PROMPT")).await;

    let RunResult::Exited { stdout, ok, .. } = ran else {
        panic!("{ran:?}");
    };
    assert!(ok);
    assert_eq!(read_output(&stdout).unwrap().say, "Yes, the chief is busy.");
    assert_eq!(
        std::fs::read_to_string(folder.join("stdin")).unwrap(),
        "THE-PROMPT"
    );
    let args = std::fs::read_to_string(folder.join("args")).unwrap();
    assert!(
        args.starts_with(
            "--print\n--safe-mode\n--no-session-persistence\n--tools\n\n--model\nhaiku\n"
        ),
        "{args}"
    );
    // Only the environment it was given, nothing of Harold's own.
    assert_eq!(
        std::fs::read_to_string(folder.join("env")).unwrap(),
        "/home/test-"
    );
    std::fs::remove_dir_all(&folder).unwrap();
    std::fs::remove_dir_all(std::path::Path::new(&program).parent().unwrap()).unwrap();
}

#[tokio::test]
async fn the_real_runner_kills_a_run_over_time_and_reports_one_that_cannot_start() {
    let (folder, program) = script("exec sleep 600");
    let runner = ClaudeRunner::new(program, vec![]);
    let mut run = run_of("x");
    run.timeout = Duration::from_millis(300);

    let killed = runner.run(&run).await;
    let missing = ClaudeRunner::new("/no/such/claude".into(), vec![])
        .run(&run_of("x"))
        .await;

    assert_eq!(killed, RunResult::Killed);
    assert_eq!(missing, RunResult::FailedToStart);
    std::fs::remove_dir_all(&folder).unwrap();
}

#[tokio::test]
async fn the_real_runner_reports_a_nonzero_exit_with_its_status() {
    let (folder, program) = script("cat > /dev/null\nexit 3");
    let runner = ClaudeRunner::new(program, vec![]);

    let ran = runner.run(&run_of("x")).await;

    assert!(
        matches!(&ran, RunResult::Exited { ok: false, exit, .. } if exit == "exit code 3"),
        "{ran:?}"
    );
    std::fs::remove_dir_all(&folder).unwrap();
}

#[test]
fn the_rules_say_to_answer_in_english() {
    assert!(RULES.contains(
        "7. Answer in English, even when the person's words look like another language. \
         Misheard speech can come out in another language."
    ));
}

#[tokio::test(start_paused = true)]
async fn the_warm_up_reads_the_facts_and_starts_the_program_once_and_the_answer_uses_them() {
    let runner = FakeRunner::giving([ok(SAMPLE)]);
    let answerer = answerer(sources(), &runner);

    let facts = CallAnswerer::warm(&answerer)
        .await
        .expect("facts were read");

    assert_eq!(*runner.warmed.lock().unwrap(), 1);
    assert!(
        runner.prompts().is_empty(),
        "the warm-up makes no model run"
    );
    assert!(facts["todos"].is_array(), "{facts}");

    // An answer given the kept facts does not read them again.
    let turn = tracing::Span::none();
    let call = CallSoFar::default();
    let kept = json!({"kept": true});
    answerer
        .answer(Ask {
            words: "hello",
            call: &call,
            turn: &turn,
            facts: Some(kept.clone()),
        })
        .await;
    assert_eq!(prompt_json(&runner)["facts"], kept);
}
