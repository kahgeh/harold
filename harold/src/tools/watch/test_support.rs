//! Fakes for the watch tool's tests: the agents and pods ports, the chief's pane, the
//! typing, and the model runner. None runs `claude` or types into a pane.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::agent::domain::EffectiveAgentState;
use crate::pod_stats::PodSnapshot;
use crate::timed_prompts::InputBox;
use crate::tools::{AgentFact, Agents, BoxFuture, ChiefPane, PaneTyping, Pods, Typing};

use super::runner::{ModelJob, ModelResult, ModelRunner};

pub(super) struct TempFolder(pub std::path::PathBuf);

impl TempFolder {
    pub(super) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-watchtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }

    /// A folder holding `.claude/skills/<skill>/SKILL.md` with `text`.
    pub(super) fn with_skill(skill: &str, text: &str) -> Self {
        let folder = Self::new();
        folder.write_skill(skill, text);
        folder
    }

    pub(super) fn write_skill(&self, skill: &str, text: &str) {
        let dir = self.0.join(".claude/skills").join(skill);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), text).unwrap();
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) fn agent(pane: &str, folder: &str, state: EffectiveAgentState) -> AgentFact {
    AgentFact {
        pane_id: pane.into(),
        tmux_target: format!("pods:0.{}", pane.trim_start_matches('%')),
        folder: folder.into(),
        provider_id: "claude".into(),
        reads_as_claude: true,
        state,
        state_since_ms: 0,
        work_summary: None,
    }
}

#[derive(Default)]
pub(super) struct FakeAgents {
    pub agents: Mutex<Vec<AgentFact>>,
    /// The input box of each pane, `Empty` unless set.
    pub boxes: Mutex<HashMap<String, InputBox>>,
}

impl FakeAgents {
    pub(super) fn with(agents: Vec<AgentFact>) -> Arc<Self> {
        Arc::new(Self {
            agents: Mutex::new(agents),
            boxes: Mutex::default(),
        })
    }

    pub(super) fn set_box(&self, pane: &str, reading: InputBox) {
        self.boxes.lock().unwrap().insert(pane.into(), reading);
    }

    pub(super) fn set_state(&self, pane: &str, state: EffectiveAgentState) {
        for agent in self.agents.lock().unwrap().iter_mut() {
            if agent.pane_id == pane {
                agent.state = state;
            }
        }
    }
}

impl Agents for FakeAgents {
    fn agents(&self) -> Vec<AgentFact> {
        self.agents.lock().unwrap().clone()
    }

    fn input_box<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, Option<InputBox>> {
        let reading = self
            .boxes
            .lock()
            .unwrap()
            .get(pane_id)
            .copied()
            .unwrap_or(InputBox::Empty);
        Box::pin(async move { Some(reading) })
    }
}

#[derive(Default)]
pub(super) struct FakePods(pub Mutex<PodSnapshot>);

impl Pods for FakePods {
    fn pods(&self) -> PodSnapshot {
        self.0.lock().unwrap().clone()
    }
}

pub(super) struct FakeChief(pub Mutex<Option<String>>);

impl FakeChief {
    pub(super) fn at(pane: Option<&str>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(pane.map(String::from))))
    }
}

impl ChiefPane for FakeChief {
    fn chief_pane(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }
}

/// Types into nothing: records the lines it was asked to type and answers from a script.
#[derive(Default)]
pub(super) struct FakeTyping {
    pub lines: Mutex<Vec<(String, String)>>,
    /// Answers in order; `Typed` once the script is empty.
    pub script: Mutex<VecDeque<Typing>>,
}

impl FakeTyping {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Only the lines that were typed (not refused).
    pub(super) fn typed(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .map(|(_, line)| line.clone())
            .collect()
    }
}

impl PaneTyping for FakeTyping {
    fn type_line<'a>(&'a self, pane_id: &'a str, text: &'a str) -> BoxFuture<'a, Typing> {
        let answer = if *self.busy.lock().unwrap() {
            Typing::PaneBusy
        } else {
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Typing::Typed)
        };
        if matches!(answer, Typing::Typed | Typing::EnterFailed) {
            self.lines
                .lock()
                .unwrap()
                .push((pane_id.to_owned(), text.to_owned()));
        }
        Box::pin(async move { answer })
    }
}

/// What a fake model run does.
#[derive(Clone)]
pub(super) enum Step {
    /// Exits ok with Claude Code's JSON holding `result` and `cost`.
    Answer {
        result: String,
        cost: f64,
        after: Duration,
    },
    /// Exits ok with this exact output.
    Raw {
        stdout: String,
        ok: bool,
    },
    /// Runs past the limit.
    Hang,
    NoStart,
}

pub(super) fn answer(result: &str, cost: f64) -> Step {
    Step::Answer {
        result: result.into(),
        cost,
        after: Duration::ZERO,
    }
}

/// Plays a script of steps, repeating the last one, and records every job it was given.
pub(super) struct FakeRunner {
    pub jobs: Mutex<Vec<ModelJob>>,
    steps: Mutex<VecDeque<Step>>,
    last: Mutex<Step>,
    running: Mutex<usize>,
    pub max_running: Mutex<usize>,
}

impl FakeRunner {
    pub(super) fn new(steps: Vec<Step>) -> Arc<Self> {
        let last = steps
            .last()
            .cloned()
            .unwrap_or_else(|| answer("QUIET", 0.01));
        Arc::new(Self {
            jobs: Mutex::default(),
            steps: Mutex::new(steps.into()),
            last: Mutex::new(last),
            running: Mutex::new(0),
            max_running: Mutex::new(0),
        })
    }

    pub(super) fn calls(&self) -> usize {
        self.jobs.lock().unwrap().len()
    }
}

impl ModelRunner for FakeRunner {
    fn run<'a>(&'a self, job: &'a ModelJob) -> BoxFuture<'a, ModelResult> {
        self.jobs.lock().unwrap().push(job.clone());
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.last.lock().unwrap().clone());
        Box::pin(async move {
            {
                let mut running = self.running.lock().unwrap();
                *running += 1;
                let mut max = self.max_running.lock().unwrap();
                *max = (*max).max(*running);
            }
            struct Done<'a>(&'a Mutex<usize>);
            impl Drop for Done<'_> {
                fn drop(&mut self) {
                    *self.0.lock().unwrap() -= 1;
                }
            }
            let _done = Done(&self.running);
            match step {
                Step::Answer {
                    result,
                    cost,
                    after,
                } => {
                    tokio::time::sleep(after).await;
                    let json = serde_json::json!({
                        "subtype": "success",
                        "is_error": false,
                        "result": result,
                        "total_cost_usd": cost,
                    });
                    ModelResult::Exited {
                        stdout: json.to_string().into_bytes(),
                        ok: true,
                    }
                }
                Step::Raw { stdout, ok } => ModelResult::Exited {
                    stdout: stdout.into_bytes(),
                    ok,
                },
                Step::Hang => {
                    tokio::time::sleep(job.timeout).await;
                    ModelResult::Killed
                }
                Step::NoStart => ModelResult::FailedToStart,
            }
        })
    }
}
