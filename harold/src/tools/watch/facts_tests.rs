use std::collections::BTreeMap;

use super::*;
use crate::pod_stats::{SlotCounts, SpecFacts, SpecState};
use crate::tools::watch::test_support::{FakeAgents, FakeChief, FakePods, agent};

fn sources<'a>(
    agents: &'a Arc<dyn Agents>,
    pods: &'a Arc<dyn Pods>,
    chief: &'a Arc<dyn ChiefPane>,
) -> Sources<'a> {
    Sources {
        agents,
        pods,
        chief_pane: chief,
        offset: UtcOffset::UTC,
    }
}

#[tokio::test]
async fn the_facts_hold_every_field_the_spec_names() {
    let now_ms = 1_800_000_000_000;
    let mut worker = agent("%3", "/work/harold-x", EffectiveAgentState::Idle);
    worker.state_since_ms = now_ms - 1_250_000;
    worker.work_summary = Some("é".repeat(300));
    let mut other = agent("%4", "/work/other", EffectiveAgentState::Busy);
    other.reads_as_claude = false;
    other.provider_id = "codex".into();
    other.state_since_ms = now_ms + 5_000;
    let fake = FakeAgents::with(vec![worker, other]);
    fake.set_box("%3", InputBox::NotFound);
    let pods_fake = FakePods::default();
    {
        let mut snapshot = pods_fake.0.lock().unwrap();
        snapshot
            .slots
            .insert("harold".into(), SlotCounts { busy: 1, free: 2 });
        let at = time::OffsetDateTime::from_unix_timestamp(now_ms / 1000 - 600).unwrap();
        snapshot.specs = BTreeMap::from([(
            "harold".to_string(),
            vec![SpecFacts {
                id: "watch-jobs".into(),
                state: SpecState::Approved,
                state_at: at,
                ready_at: None,
                approved_at: None,
                merged_at: None,
                words: 10,
                revisions: 0,
            }],
        )]);
    }
    let agents: Arc<dyn Agents> = fake;
    let pods: Arc<dyn Pods> = Arc::new(pods_fake);
    let chief: Arc<dyn ChiefPane> = FakeChief::at(Some("%1"));

    let facts = gather(
        &sources(&agents, &pods, &chief),
        now_ms,
        WatchEntry {
            run: 3,
            runs_left: 9,
            previous_outcome: Some("quiet".into()),
        },
    )
    .await;

    assert!(facts.chief);
    assert_eq!(facts.agents.len(), 2);
    let first = &facts.agents[0];
    assert_eq!(first.folder, "/work/harold-x");
    assert_eq!(first.pane, "pods:0.3");
    assert_eq!(first.provider, "claude");
    assert_eq!(first.state, "idle");
    assert_eq!(first.seconds_in_state, 1250);
    assert_eq!(first.work_summary.as_ref().unwrap().chars().count(), 200);
    assert_eq!(first.input_box, Some("not_found"));
    let second = &facts.agents[1];
    assert_eq!((second.state, second.seconds_in_state), ("busy", 0));
    assert_eq!(second.input_box, None, "only a Claude Code pane has one");
    assert_eq!(facts.pods.len(), 1);
    let pod = &facts.pods[0];
    assert_eq!(
        pod.slots,
        Slots {
            busy: 1,
            free: 2,
            total: 3
        }
    );
    assert_eq!(
        pod.specs,
        vec![SpecEntry {
            id: "watch-jobs".into(),
            state: "approved",
            seconds_in_state: 600
        }]
    );
    let json = serde_json::to_value(&facts).unwrap();
    assert_eq!(json["watch"]["run"], 3);
    assert_eq!(json["watch"]["runs_left"], 9);
    assert_eq!(json["watch"]["previous_outcome"], "quiet");
    assert!(json["now"].as_str().unwrap().starts_with("2027-"));
    assert!(json["agents"][1].get("input_box").is_none());
}

#[tokio::test]
async fn no_chief_and_no_agents_give_an_empty_document() {
    let agents: Arc<dyn Agents> = FakeAgents::with(Vec::new());
    let pods: Arc<dyn Pods> = Arc::new(FakePods::default());
    let chief: Arc<dyn ChiefPane> = FakeChief::at(None);
    let facts = gather(
        &sources(&agents, &pods, &chief),
        1_800_000_000_000,
        WatchEntry {
            run: 1,
            runs_left: 0,
            previous_outcome: None,
        },
    )
    .await;
    assert!(!facts.chief);
    assert!(facts.agents.is_empty() && facts.pods.is_empty());
}
