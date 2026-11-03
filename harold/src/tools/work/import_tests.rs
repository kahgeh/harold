use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use harold_api::tools::backlog::v1 as bp;
use harold_api::tools::backlog::v1::backlog_service_client::BacklogServiceClient;

use super::*;
use crate::tools::BoxFuture;
use crate::tools::work::files::WorkFiles;
use crate::tools::work::rotation::{Clock, Rotator};
use crate::tools::work::schema::{Mark, Priority, Status};
use crate::tools::work::test_support::{TempFolder, record};
use crate::tools::{ChiefPane, PaneFolder, Products};

/// Noon UTC of 2026-11-03.
const NOW_MS: i64 = 1_793_707_200_000;

struct FixedClock;

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        NOW_MS
    }
}

/// One fixture file and the work record it must give.
struct Expected {
    file: &'static str,
    name: &'static str,
    title: &'static str,
    priority: &'static str,
    status: &'static str,
    waits_for: &'static str,
    next_step: &'static str,
    /// Each decision as (mark, who, at, text).
    decisions: &'static [(&'static str, &'static str, &'static str, &'static str)],
    links: &'static [&'static str],
}

/// The fields of the six fixtures. When the real files replace the fixtures,
/// update this table only.
const EXPECTED: [Expected; 6] = [
    Expected {
        file: "fixture-one.md",
        name: "fixture-one",
        title: "Keep the work records beside the queue",
        priority: "focus",
        status: "building",
        waits_for: "nothing",
        next_step: "Finish the import and run the check on the six files.",
        decisions: &[
            (
                "agreed",
                "the user",
                "2026-10-28",
                "The work records live in Turso files of their own.",
            ),
            (
                "draft",
                "the chief",
                "2026-10-30",
                "The import reads only the files directly inside the folder.",
            ),
        ],
        links: &[
            "spec:harold/work-records",
            "question:q-0412",
            "note:The queue stays until the chief applies the import.",
        ],
    },
    Expected {
        file: "fixture-two.md",
        name: "fixture-two",
        title: "Show the pane switcher on the second screen",
        priority: "focus",
        status: "waiting for the user",
        waits_for: "the user's answer on question q-0418",
        next_step: "Read the answer, then update the spec.",
        decisions: &[
            (
                "open",
                "the chief",
                "2026-10-31",
                "Which key opens the switcher on the second screen?",
            ),
            (
                "dropped",
                "the user",
                "2026-11-01",
                "A mouse-only switcher.",
            ),
        ],
        links: &["spec:harold/pane-switcher", "question:q-0418"],
    },
    Expected {
        file: "fixture-three.md",
        name: "fixture-three",
        title: "Count the time at each status",
        priority: "next",
        status: "spec drafted",
        waits_for: "the architect's check",
        next_step: "Ask the architect to check the spec.",
        decisions: &[(
            "agreed",
            "the chief",
            "2026-10-29",
            "Record the duration when a work record leaves a status.",
        )],
        links: &[
            "spec:harold/work-telemetry",
            "note:Buckets follow the other time cards.",
        ],
    },
    Expected {
        file: "fixture-four.md",
        name: "fixture-four",
        title: "Move old notes out of the digest",
        priority: "next",
        status: "idea",
        waits_for: "",
        next_step: "Write down what the digest shows today.",
        decisions: &[
            (
                "draft",
                "pod-digest",
                "2026-10-27",
                "The digest keeps only the last seven days.",
            ),
            (
                "open",
                "the chief",
                "2026-10-28",
                "Where do the older notes go?",
            ),
        ],
        links: &["note:The chief asked for this after the last digest."],
    },
    Expected {
        file: "fixture-five.md",
        name: "fixture-five",
        title: "Alert when a rotation fails",
        priority: "later",
        status: "released",
        waits_for: "the user's check of the alert in HyperDX",
        next_step: "Ask the user to check the alert.",
        decisions: &[
            (
                "agreed",
                "the user",
                "2026-10-20",
                "The alert fires once for each failure, not for each retry.",
            ),
            (
                "dropped",
                "the chief",
                "2026-10-21",
                "An alert for each retry.",
            ),
            ("agreed", "the chief", "2026-10-22", "Severity is warn."),
        ],
        links: &[
            "spec:harold/work-alerts",
            "question:q-0397",
            "note:Expected count is zero.",
        ],
    },
    Expected {
        file: "fixture-six.md",
        name: "fixture-six",
        title: "Read the backlog from the CLI",
        priority: "later",
        status: "checked by the user",
        waits_for: "nothing",
        next_step: "None. The user checked it on 2026-10-15.",
        decisions: &[
            (
                "agreed",
                "the user",
                "2026-10-10",
                "The CLI prints the backlog as plain text.",
            ),
            (
                "open",
                "the chief",
                "2026-10-12",
                "Should the CLI also print JSON?",
            ),
        ],
        links: &["spec:harold/backlog-cli"],
    },
];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tools/work/fixtures")
}

/// A queue from a set list of open items, that counts its reads.
#[derive(Default)]
struct FakeQueue {
    items: Vec<QueueItem>,
    unreadable: bool,
    reads: AtomicUsize,
}

impl FakeQueue {
    fn with(items: &[(&str, &str)]) -> Self {
        Self {
            items: items
                .iter()
                .map(|(id, text)| QueueItem {
                    id: (*id).into(),
                    text: (*text).into(),
                })
                .collect(),
            ..Self::default()
        }
    }
}

impl OpenQueue for FakeQueue {
    fn open_items(&self) -> BoxFuture<'_, Result<Vec<QueueItem>, QueueUnreadable>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            match self.unreadable {
                true => Err(QueueUnreadable),
                false => Ok(self.items.clone()),
            }
        })
    }
}

/// A work folder with its rotator, and a folder for the chief's files.
struct Setup {
    tmp: TempFolder,
    rotator: Rotator,
}

impl Setup {
    fn new() -> Self {
        let tmp = TempFolder::new();
        let files = WorkFiles::new(tmp.0.join("work"));
        let rotator = Rotator::new(files, Arc::new(FixedClock));
        std::fs::create_dir_all(tmp.0.join("chief")).unwrap();
        Self { tmp, rotator }
    }

    fn chief(&self) -> PathBuf {
        self.tmp.0.join("chief")
    }

    fn write(&self, file: &str, text: &str) {
        std::fs::write(self.chief().join(file), text).unwrap();
    }

    async fn run(
        &self,
        folder: &Path,
        queue_ids: &[&str],
        apply: bool,
        queue: &dyn OpenQueue,
    ) -> Result<ImportReport, ImportError> {
        let ids: Vec<String> = queue_ids.iter().map(|id| (*id).to_string()).collect();
        let deps = ImportDeps {
            rotator: &self.rotator,
            queue,
            now_ms: NOW_MS,
        };
        import(folder, &ids, apply, &deps).await
    }

    async fn stored(&self) -> Vec<Record> {
        let file = self.rotator.current().await.unwrap();
        file.list(true).await.unwrap()
    }
}

fn check_record(record: &Record, expected: &Expected) {
    let at = expected.file;
    assert_eq!(record.name, expected.name, "{at}");
    assert_eq!(record.title, expected.title, "{at}");
    assert_eq!(record.priority.as_str(), expected.priority, "{at}");
    assert_eq!(record.status.as_str(), expected.status, "{at}");
    assert_eq!(record.waits_for, expected.waits_for, "{at}");
    assert_eq!(record.next_step, expected.next_step, "{at}");
    let decisions: Vec<(&str, &str, &str, &str)> = record
        .decisions
        .iter()
        .map(|d| {
            (
                d.mark.as_str(),
                d.who.as_str(),
                d.at.as_str(),
                d.text.as_str(),
            )
        })
        .collect();
    assert_eq!(decisions, expected.decisions, "{at}");
    assert_eq!(record.links, expected.links, "{at}");
}

/// A small file of the chief's form, with `body` after the heading.
fn file_with(body: &str) -> String {
    format!(
        "# small-one: A small one\n\nPriority: next\nStatus: idea\nWaits for:\nNext step: Go.\n{body}"
    )
}

#[tokio::test]
async fn the_six_fixtures_import_with_no_unplaced_line_and_each_field_matches() {
    // Each fixture alone.
    for expected in &EXPECTED {
        let text = std::fs::read_to_string(fixtures().join(expected.file)).unwrap();
        let size = text.len();
        assert!(
            (300..=1_100).contains(&size),
            "{}: {size} bytes",
            expected.file
        );
        let parsed = parse_chief_file(expected.file, &text);
        assert!(parsed.unplaced.is_empty(), "{:?}", parsed.unplaced);
        check_record(parsed.record.as_ref().unwrap(), expected);
    }

    // The whole folder, as a dry run and then applied.
    let setup = Setup::new();
    let queue = FakeQueue::default();
    let dry = setup.run(&fixtures(), &[], false, &queue).await.unwrap();
    assert!(dry.unplaced.is_empty(), "{:?}", dry.unplaced);
    assert!(dry.in_use.is_empty());
    assert_eq!(dry.records.len(), EXPECTED.len());
    assert!(setup.stored().await.is_empty(), "a dry run stores nothing");

    let applied = setup.run(&fixtures(), &[], true, &queue).await.unwrap();
    assert_eq!(applied.records.len(), EXPECTED.len());
    let stored = setup.stored().await;
    assert_eq!(stored.len(), EXPECTED.len());
    for expected in &EXPECTED {
        let record = stored.iter().find(|r| r.name == expected.name).unwrap();
        check_record(record, expected);
    }
}

#[test]
fn every_status_priority_and_mark_parses() {
    for status in [
        "idea",
        "discovery",
        "spec drafted",
        "architect checked",
        "waiting for the user",
        "approved",
        "building",
        "merged",
        "released",
        "checked by the user",
    ] {
        for priority in ["focus", "next", "later"] {
            let text = format!(
                "# a: A\nPriority: {priority}\nStatus: {status}\nDecisions:\n\
                 - agreed, u, 2026-01-01: a\n- draft, u, 2026-01-01: b\n\
                 - dropped, u, 2026-01-01: c\n- open, u, 2026-01-01: d\n"
            );
            let parsed = parse_chief_file("a.md", &text);
            assert!(parsed.unplaced.is_empty(), "{:?}", parsed.unplaced);
            let record = parsed.record.unwrap();
            assert_eq!(record.status.as_str(), status);
            assert_eq!(record.priority.as_str(), priority);
            let marks: Vec<Mark> = record.decisions.iter().map(|d| d.mark).collect();
            assert_eq!(
                marks,
                [Mark::Agreed, Mark::Draft, Mark::Dropped, Mark::Open]
            );
        }
    }
}

#[tokio::test]
async fn a_line_that_fits_no_field_is_reported_with_file_and_line() {
    let parsed = parse_chief_file(
        "odd.md",
        &file_with("Owner: the chief\n\nLinks:\n- web:example\n"),
    );
    let lines: Vec<(&str, u32, &str)> = parsed
        .unplaced
        .iter()
        .map(|u| (u.file.as_str(), u.line, u.text.as_str()))
        .collect();
    assert_eq!(
        lines,
        [
            ("odd.md", 7, "Owner: the chief"),
            ("odd.md", 10, "- web:example")
        ]
    );
    // The record is still shown, so a dry run lists what would be stored.
    assert_eq!(parsed.record.unwrap().name, "small-one");

    // Through the folder: the dry run names the file and the line.
    let setup = Setup::new();
    setup.write("odd.md", &file_with("Owner: the chief\n"));
    let report = setup
        .run(&setup.chief(), &[], false, &FakeQueue::default())
        .await
        .unwrap();
    assert_eq!(report.unplaced.len(), 1);
    assert_eq!(report.unplaced[0].file, "odd.md");
    assert_eq!(report.unplaced[0].line, 7);
}

#[tokio::test]
async fn apply_refuses_while_a_line_is_unplaced() {
    let setup = Setup::new();
    setup.write(
        "good.md",
        "# good-one: Good\nPriority: later\nStatus: idea\n",
    );
    setup.write("odd.md", &file_with("Owner: the chief\n"));

    let result = setup
        .run(&setup.chief(), &[], true, &FakeQueue::default())
        .await;

    assert!(
        matches!(result, Err(ImportError::Unplaced(1))),
        "{result:?}"
    );
    assert!(setup.stored().await.is_empty(), "nothing is stored");
}

#[tokio::test]
async fn no_queue_flag_imports_no_queue_item() {
    let setup = Setup::new();
    setup.write(
        "good.md",
        "# good-one: Good\nPriority: later\nStatus: idea\n",
    );
    let queue = FakeQueue::with(&[("N12", "Fix the bar"), ("N34", "Tidy the logs")]);

    let report = setup.run(&setup.chief(), &[], true, &queue).await.unwrap();

    assert_eq!(
        queue.reads.load(Ordering::SeqCst),
        0,
        "the queue is not read"
    );
    let names: Vec<String> = setup.stored().await.into_iter().map(|r| r.name).collect();
    assert_eq!(names, ["good-one"]);
    assert_eq!(report.records.len(), 1);
}

#[tokio::test]
async fn queue_n12_becomes_one_idea_record_named_queue_n12() {
    let record = queue_record("N12", "Fix the bar\nsecond line");
    assert_eq!(record.name, "queue-n12");
    assert_eq!(record.status, Status::Idea);
    assert_eq!(record.priority, Priority::Later);
    assert_eq!(record.next_step, "Fix the bar");

    let setup = Setup::new();
    let queue = FakeQueue::with(&[("N12", "Fix the bar\r\nmore"), ("N34", "Tidy the logs")]);
    let report = setup
        .run(&setup.chief(), &["N12"], true, &queue)
        .await
        .unwrap();

    assert_eq!(report.records.len(), 1);
    let stored = setup.stored().await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].name, "queue-n12");
    assert_eq!(stored[0].status, Status::Idea);
    assert_eq!(stored[0].priority, Priority::Later);
    assert_eq!(stored[0].next_step, "Fix the bar");
}

#[tokio::test]
async fn a_done_or_unknown_queue_id_is_refused() {
    let setup = Setup::new();
    // N5 is done, so the open items do not hold it. N99 is not there.
    let queue = FakeQueue::with(&[("N12", "Fix the bar")]);
    for id in ["N5", "N99"] {
        for apply in [false, true] {
            let result = setup.run(&setup.chief(), &["N12", id], apply, &queue).await;
            match result {
                Err(ImportError::NotOpenQueueItem(named)) => assert_eq!(named, id),
                other => panic!("{id}: {other:?}"),
            }
        }
    }
    assert!(setup.stored().await.is_empty(), "nothing is stored");

    let unreadable = FakeQueue {
        unreadable: true,
        ..FakeQueue::default()
    };
    let result = setup
        .run(&setup.chief(), &["N12"], false, &unreadable)
        .await;
    assert!(
        matches!(result, Err(ImportError::QueueUnreadable)),
        "{result:?}"
    );
}

#[test]
fn crlf_and_missing_links_section_parse() {
    let text = "# crlf-one: Windows lines\r\nPriority: focus\r\nStatus: approved\r\n\
                Waits for: the build\r\nNext step: Start it.\r\nDecisions:\r\n\
                - agreed, the user, 2026-11-02: Go ahead.\r\n";
    let parsed = parse_chief_file("crlf.md", text);
    assert!(parsed.unplaced.is_empty(), "{:?}", parsed.unplaced);
    let record = parsed.record.unwrap();
    assert_eq!(record.name, "crlf-one");
    assert_eq!(record.title, "Windows lines");
    assert_eq!(record.status, Status::Approved);
    assert_eq!(record.waits_for, "the build");
    assert_eq!(record.next_step, "Start it.");
    assert_eq!(record.decisions.len(), 1);
    assert_eq!(record.decisions[0].text, "Go ahead.");
    assert!(record.links.is_empty());
}

#[test]
fn decision_marks_outside_the_four_are_unplaced() {
    let parsed = parse_chief_file(
        "marks.md",
        &file_with(
            "Decisions:\n- agreed, the user, 2026-11-01: Fine.\n\
             - maybe, the user, 2026-11-01: Not a mark.\n\
             - Done, the chief, 2026-11-02: Not a mark either.\n",
        ),
    );
    let lines: Vec<u32> = parsed.unplaced.iter().map(|u| u.line).collect();
    assert_eq!(lines, [9, 10]);
    assert_eq!(parsed.record.unwrap().decisions.len(), 1);
}

#[tokio::test]
async fn a_name_in_use_is_reported_and_apply_refuses() {
    let setup = Setup::new();
    {
        let file = setup.rotator.current().await.unwrap();
        file.put(record("good-one"), NOW_MS).await.unwrap().unwrap();
    }
    setup.write(
        "good.md",
        "# good-one: New title\nPriority: later\nStatus: idea\n",
    );
    setup.write(
        "other.md",
        "# other-one: Other\nPriority: later\nStatus: idea\n",
    );

    let dry = setup
        .run(&setup.chief(), &[], false, &FakeQueue::default())
        .await
        .unwrap();
    assert_eq!(dry.in_use, ["good-one"]);

    let result = setup
        .run(&setup.chief(), &[], true, &FakeQueue::default())
        .await;
    match result {
        Err(ImportError::NamesInUse(names)) => assert_eq!(names, ["good-one"]),
        other => panic!("{other:?}"),
    }
    let stored = setup.stored().await;
    assert_eq!(stored.len(), 1, "nothing new is stored");
    assert_eq!(stored[0].title, "Title good-one", "nothing is overwritten");
}

#[tokio::test]
async fn the_folder_is_read_flat_with_limits() {
    let setup = Setup::new();
    setup.write(
        "good.md",
        "# good-one: Good\nPriority: later\nStatus: idea\n",
    );
    setup.write("notes.txt", "not read");
    std::fs::create_dir_all(setup.chief().join("deeper")).unwrap();
    std::fs::write(
        setup.chief().join("deeper/deep.md"),
        "# deep-one: Deep\nPriority: later\nStatus: idea\n",
    )
    .unwrap();
    setup.write("big.md", &"x".repeat(64 * 1024 + 1));
    let report = setup
        .run(&setup.chief(), &[], false, &FakeQueue::default())
        .await
        .unwrap();
    let names: Vec<&str> = report.records.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["good-one"]);
    let whole: Vec<(&str, u32)> = report
        .unplaced
        .iter()
        .map(|u| (u.file.as_str(), u.line))
        .collect();
    assert_eq!(whole, [("big.md", 0)]);

    let many = Setup::new();
    for n in 0..=200 {
        many.write(&format!("f{n}.md"), "");
    }
    let result = many
        .run(&many.chief(), &[], false, &FakeQueue::default())
        .await;
    assert!(
        matches!(result, Err(ImportError::TooManyFiles)),
        "{result:?}"
    );

    let result = setup
        .run(Path::new("relative"), &[], false, &FakeQueue::default())
        .await;
    assert!(matches!(result, Err(ImportError::BadFolder)), "{result:?}");
}

#[tokio::test]
async fn a_hidden_or_upper_case_md_file_is_unplaced_not_dropped() {
    let setup = Setup::new();
    let text = "# good-one: Good\nPriority: later\nStatus: idea\n";
    setup.write("good.md", text);
    setup.write(".hidden.md", text);
    setup.write("UPPER.MD", text);
    setup.write("notes.txt", "not read");
    let report = setup
        .run(&setup.chief(), &[], false, &FakeQueue::default())
        .await
        .unwrap();
    let mut unplaced: Vec<(&str, u32)> = report
        .unplaced
        .iter()
        .map(|u| (u.file.as_str(), u.line))
        .collect();
    unplaced.sort();
    assert_eq!(unplaced, [(".hidden.md", 0), ("UPPER.MD", 0)]);
    let result = setup
        .run(&setup.chief(), &[], true, &FakeQueue::default())
        .await;
    assert!(
        matches!(result, Err(ImportError::Unplaced(2))),
        "{result:?}"
    );
    assert!(setup.stored().await.is_empty());
}

#[tokio::test]
async fn a_queue_id_that_makes_an_invalid_name_is_refused_with_the_id() {
    let setup = Setup::new();
    let queue = FakeQueue::with(&[("N 1", "Fix the bar")]);
    for apply in [false, true] {
        let result = setup.run(&setup.chief(), &["N 1"], apply, &queue).await;
        match result {
            Err(ImportError::BadQueueName(id)) => assert_eq!(id, "N 1"),
            other => panic!("{other:?}"),
        }
    }
    assert!(setup.stored().await.is_empty());
}

// --- The queue over loopback, with the real backlog tool --------------------

struct Chief;

impl ChiefPane for Chief {
    fn chief_pane(&self) -> Option<String> {
        Some("%2".into())
    }
}

struct NoFolders;

impl PaneFolder for NoFolders {
    fn folder_of(&self, _pane_id: &str) -> Option<PathBuf> {
        None
    }
}

struct NoProducts;

impl Products for NoProducts {
    fn products(&self) -> Result<Vec<(String, PathBuf)>, &'static str> {
        Ok(Vec::new())
    }
}

/// Serves the backlog tool over `folder` on a loopback port, and gives its
/// address.
async fn serve_backlog(
    folder: &Path,
    shutdown: tokio::sync::watch::Receiver<()>,
) -> std::net::SocketAddr {
    let handle = crate::tools::backlog::start(crate::tools::ToolContext {
        data_folder: folder.to_path_buf(),
        typing: None,
        chief_pane: Arc::new(Chief),
        pane_folder: Arc::new(NoFolders),
        products: Arc::new(NoProducts),
        agents: Arc::new(crate::tools::tests::NoAgents),
        pods: Arc::new(crate::tools::tests::NoPods),
        watch_settings: Default::default(),
        model_environment: Vec::new(),
        cli_path: String::new(),
        pod_registry: None,
        grpc_addr: None,
        shutdown,
    })
    .await
    .unwrap_or_else(|e| panic!("start failed: {}", e.reason));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, connections) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            if sender.send(Ok::<_, std::io::Error>(socket)).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_routes(handle.routes)
            .serve_with_incoming(tokio_stream::wrappers::ReceiverStream::new(connections)),
    );
    address
}

async fn queue_listing(client: &mut BacklogServiceClient<tonic::transport::Channel>) -> String {
    let listing = client
        .list_queue(bp::ListQueueRequest { done_since_ms: 0 })
        .await
        .unwrap()
        .into_inner();
    format!("{listing:?}")
}

#[tokio::test]
async fn the_queue_keeps_every_item() {
    let setup = Setup::new();
    let (_stop, shutdown) = tokio::sync::watch::channel(());
    let address = serve_backlog(&setup.tmp.0.join("backlog"), shutdown).await;
    let mut client = BacklogServiceClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let mut ids = Vec::new();
    for (text, now) in [
        ("Fix the bar", true),
        ("Tidy the logs", false),
        ("Old one", false),
    ] {
        let item = client
            .add_queue_item(bp::AddQueueItemRequest {
                text: text.into(),
                now,
                at: 0,
                caller_pane: "%2".into(),
            })
            .await
            .unwrap()
            .into_inner()
            .item
            .unwrap();
        ids.push(item.id);
    }
    client
        .done_queue_item(bp::DoneQueueItemRequest {
            id: ids[2].clone(),
            caller_pane: "%2".into(),
        })
        .await
        .unwrap();
    let before = queue_listing(&mut client).await;

    let queue = LoopbackQueue::new(Some(address));
    let wanted = [ids[0].as_str(), ids[1].as_str()];
    let report = setup
        .run(&setup.chief(), &wanted, true, &queue)
        .await
        .unwrap();
    assert_eq!(report.records.len(), 2);
    let done = setup
        .run(&setup.chief(), &[ids[2].as_str()], false, &queue)
        .await;
    assert!(
        matches!(done, Err(ImportError::NotOpenQueueItem(_))),
        "{done:?}"
    );

    assert_eq!(
        queue_listing(&mut client).await,
        before,
        "the queue is unchanged"
    );
    let stored: Vec<String> = setup.stored().await.into_iter().map(|r| r.name).collect();
    let mut expected: Vec<String> = ids[..2]
        .iter()
        .map(|id| format!("queue-{}", id.to_ascii_lowercase()))
        .collect();
    expected.sort();
    let mut stored = stored;
    stored.sort();
    assert_eq!(stored, expected);

    // No address: the queue cannot be read.
    let none = LoopbackQueue::new(None);
    let result = setup.run(&setup.chief(), &["N1"], false, &none).await;
    assert!(
        matches!(result, Err(ImportError::QueueUnreadable)),
        "{result:?}"
    );
}

/// The (line, reason) pairs of a parsed file.
fn reasons(text: &str) -> Vec<(u32, &'static str)> {
    parse_chief_file("a.md", text)
        .unplaced
        .iter()
        .map(|u| (u.line, u.reason))
        .collect()
}

const HEAD: &str = "# a: A\nPriority: next\nStatus: idea\n";

#[test]
fn the_parser_gives_one_reason_word_for_each_unplaced_line() {
    let cases: [(&str, String, (u32, &str)); 13] = [
        ("no_heading", "Priority: next\n".into(), (1, "no_heading")),
        ("bad_heading", "# Bad Name: x\n".into(), (1, "bad_heading")),
        (
            "second_heading",
            format!("{HEAD}# b: B\n"),
            (4, "second_heading"),
        ),
        (
            "unknown_line",
            format!("{HEAD}Owner: me\n"),
            (4, "unknown_line"),
        ),
        (
            "bad_priority",
            "# a: A\nPriority: soon\nStatus: idea\n".into(),
            (2, "bad_priority"),
        ),
        (
            "bad_status",
            "# a: A\nPriority: next\nStatus: odd\n".into(),
            (3, "bad_status"),
        ),
        (
            "second_value",
            format!("{HEAD}Status: idea\n"),
            (4, "second_value"),
        ),
        (
            "bad_decision",
            format!("{HEAD}Decisions:\n- agreed, me, 2026-13-45: x\n"),
            (5, "bad_decision"),
        ),
        (
            "bad_link",
            format!("{HEAD}Links:\n- web:a b\n"),
            (5, "bad_link"),
        ),
        (
            "item_outside_section",
            format!("{HEAD}- loose\n"),
            (4, "item_outside_section"),
        ),
        (
            "inline_value_after_section",
            format!("{HEAD}Links: none\n"),
            (4, "inline_value_after_section"),
        ),
        (
            "missing_priority",
            "# a: A\nStatus: idea\n".into(),
            (1, "missing_priority"),
        ),
        (
            "missing_status",
            "# a: A\nPriority: next\n".into(),
            (1, "missing_status"),
        ),
    ];
    for (name, text, expected) in cases {
        let got = reasons(&text);
        assert!(got.contains(&expected), "{name}: {got:?}");
    }
    // A second Decisions: heading is a second value.
    assert_eq!(
        reasons(&format!("{HEAD}Decisions:\nDecisions:\n")),
        [(5, "second_value")]
    );
    // A clean file has none.
    assert!(reasons(HEAD).is_empty());
    // The text of a placed-wrong line is the line, and of a missing field the heading.
    let parsed = parse_chief_file("a.md", "# a: A\nStatus: idea\n");
    assert_eq!(parsed.unplaced[0].text, "# a: A");
}

#[tokio::test]
async fn the_folder_gives_a_reason_for_each_unreadable_file() {
    let setup = Setup::new();
    setup.write("Upper.MD", HEAD);
    setup.write(".hidden.md", HEAD);
    std::fs::write(setup.chief().join("binary.md"), [0xffu8, 0xfe, 0x00]).unwrap();
    std::fs::write(
        setup.chief().join("big.md"),
        vec![b'a'; (FILE_MAX + 1) as usize],
    )
    .unwrap();
    std::fs::create_dir(setup.chief().join("dir.md")).unwrap();
    setup.write("one.md", HEAD);
    setup.write("two.md", HEAD);
    let report = setup
        .run(&setup.chief(), &[], false, &FakeQueue::default())
        .await
        .unwrap();
    let mut got: Vec<(String, u32, &str)> = report
        .unplaced
        .iter()
        .map(|u| (u.file.clone(), u.line, u.reason))
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            (".hidden.md".to_string(), 0, "hidden_or_upper_case_name"),
            ("Upper.MD".to_string(), 0, "hidden_or_upper_case_name"),
            ("big.md".to_string(), 0, "file_too_large"),
            ("binary.md".to_string(), 0, "not_utf8"),
            ("dir.md".to_string(), 0, "not_a_file"),
            ("two.md".to_string(), 0, "name_used_by_another_file"),
        ]
    );
}

#[tokio::test]
async fn a_dry_run_leaves_the_work_file_unchanged() {
    let setup = Setup::new();
    setup.write("good.md", HEAD);
    setup.write("odd.md", &file_with("Owner: the chief\n"));
    let before = setup.stored().await;
    let report = setup
        .run(&setup.chief(), &[], false, &FakeQueue::default())
        .await
        .unwrap();
    assert_eq!(report.records.len(), 2);
    assert_eq!(report.unplaced.len(), 1);
    assert_eq!(setup.stored().await, before, "the work file is unchanged");
    assert!(setup.stored().await.is_empty());
}
