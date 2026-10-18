use std::path::PathBuf;
use std::sync::Arc;

use harold_api::tools::backlog::v1 as pb;
use harold_api::tools::backlog::v1::backlog_service_server::BacklogService;
use tonic::{Code, Request};

use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::tools::backlog::store::TursoBacklogStore;
use crate::tools::backlog::test_support::{FakeChief, FakeFolders, FakeProducts, TempFolder};

const BACKLOG: &str = include_str!("fixtures/backlog.md");
const QUEUE: &str = include_str!("fixtures/queue.md");
const CHIEF: &str = "%2";
const HAROLD_PANE: &str = "%40";
const TURSO_PANE: &str = "%51";

type Service = BacklogToolService<TursoBacklogStore>;

/// A store in a temp folder, and a service over it: products `harold` and `turso`,
/// each with a repo folder and a pane in it, and the chief at `%2`.
struct Setup {
    tmp: TempFolder,
    store: Arc<TursoBacklogStore>,
    folders: Arc<FakeFolders>,
}

impl Setup {
    async fn new() -> Self {
        let tmp = TempFolder::new();
        for repo in ["harold", "turso"] {
            std::fs::create_dir_all(tmp.0.join(repo)).unwrap();
        }
        let store = Arc::new(TursoBacklogStore::open(&tmp.0.join("db")).await.unwrap());
        let folders = Arc::new(FakeFolders::default());
        {
            let mut map = folders.0.lock().unwrap();
            map.insert(HAROLD_PANE.into(), tmp.0.join("harold"));
            map.insert(TURSO_PANE.into(), tmp.0.join("turso"));
        }
        Self {
            tmp,
            store,
            folders,
        }
    }

    fn repo(&self, name: &str) -> (String, PathBuf) {
        (name.into(), self.tmp.0.join(name))
    }

    /// A service over the shared store with the given registry.
    fn service_with(&self, products: Vec<(String, PathBuf)>) -> Service {
        let guard = Guard::new(
            self.folders.clone(),
            Arc::new(FakeProducts(Ok(products))),
            Arc::new(FakeChief(Some(CHIEF.into()))),
        );
        BacklogToolService::new(Arc::clone(&self.store), guard)
    }

    fn service(&self) -> Service {
        self.service_with(vec![self.repo("harold"), self.repo("turso")])
    }
}

fn item(id: &str) -> pb::Item {
    pb::Item {
        id: id.into(),
        title: format!("Title {id}"),
        size: "small".into(),
        evidence: vec!["harold.log:1".into()],
        ..pb::Item::default()
    }
}

fn strings(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

fn refusal<T>(result: Result<T, tonic::Status>) -> (Code, String) {
    match result {
        Ok(_) => panic!("not refused"),
        Err(status) => (status.code(), status.message().to_string()),
    }
}

async fn put(
    service: &Service,
    product: &str,
    pane: &str,
    item: pb::Item,
) -> Result<pb::PutItemResponse, tonic::Status> {
    service
        .put_item(Request::new(pb::PutItemRequest {
            product: product.into(),
            item: Some(item),
            caller_pane: pane.into(),
        }))
        .await
        .map(|response| response.into_inner())
}

async fn listed_ids(service: &Service, product: &str) -> Vec<String> {
    service
        .list_items(Request::new(pb::ListItemsRequest {
            product: product.into(),
        }))
        .await
        .unwrap()
        .into_inner()
        .items
        .into_iter()
        .map(|item| item.id)
        .collect()
}

async fn notes(service: &Service, product: &str, kind: &str) -> Vec<pb::Note> {
    service
        .list_notes(Request::new(pb::ListNotesRequest {
            product: product.into(),
            kind: kind.into(),
            last: 0,
        }))
        .await
        .unwrap()
        .into_inner()
        .notes
}

async fn remove(
    service: &Service,
    id: &str,
    reason: &str,
) -> Result<pb::RemoveItemResponse, tonic::Status> {
    service
        .remove_item(Request::new(pb::RemoveItemRequest {
            product: "harold".into(),
            id: id.into(),
            reason: reason.into(),
            caller_pane: HAROLD_PANE.into(),
        }))
        .await
        .map(|response| response.into_inner())
}

async fn rank(service: &Service, ids: &[&str]) -> Result<pb::RankItemsResponse, tonic::Status> {
    service
        .rank_items(Request::new(pb::RankItemsRequest {
            product: "harold".into(),
            ids: strings(ids),
            caller_pane: HAROLD_PANE.into(),
        }))
        .await
        .map(|response| response.into_inner())
}

async fn add_queue(
    service: &Service,
    text: &str,
    now: bool,
    at: u32,
) -> Result<pb::QueueItem, tonic::Status> {
    service
        .add_queue_item(Request::new(pb::AddQueueItemRequest {
            text: text.into(),
            now,
            at,
            caller_pane: CHIEF.into(),
        }))
        .await
        .map(|response| response.into_inner().item.unwrap())
}

async fn done(service: &Service, id: &str) -> Result<pb::DoneQueueItemResponse, tonic::Status> {
    service
        .done_queue_item(Request::new(pb::DoneQueueItemRequest {
            id: id.into(),
            caller_pane: CHIEF.into(),
        }))
        .await
        .map(|response| response.into_inner())
}

async fn order(service: &Service, ids: &[&str]) -> Result<pb::OrderQueueResponse, tonic::Status> {
    service
        .order_queue(Request::new(pb::OrderQueueRequest {
            ids: strings(ids),
            caller_pane: CHIEF.into(),
        }))
        .await
        .map(|response| response.into_inner())
}

async fn list_queue(service: &Service, done_since_ms: i64) -> pb::ListQueueResponse {
    service
        .list_queue(Request::new(pb::ListQueueRequest { done_since_ms }))
        .await
        .unwrap()
        .into_inner()
}

async fn import(
    service: &Service,
    product: &str,
    queue: bool,
    markdown: &str,
    apply: bool,
) -> Result<pb::ImportResponse, tonic::Status> {
    service
        .import(Request::new(pb::ImportRequest {
            product: product.into(),
            queue,
            markdown: markdown.into(),
            apply,
            caller_pane: CHIEF.into(),
            utc_offset_seconds: 0,
        }))
        .await
        .map(|response| response.into_inner())
}

async fn export(service: &Service, product: &str, queue: bool) -> String {
    service
        .export(Request::new(pb::ExportRequest {
            product: product.into(),
            queue,
        }))
        .await
        .unwrap()
        .into_inner()
        .markdown
}

async fn put_all(service: &Service, ids: &[&str]) {
    for id in ids {
        put(service, "harold", HAROLD_PANE, item(id)).await.unwrap();
    }
}

#[tokio::test]
async fn put_list_rank_and_remove_through_the_service() {
    let setup = Setup::new().await;
    let service = setup.service();
    let first = put(&service, "harold", HAROLD_PANE, item("a"))
        .await
        .unwrap();
    assert_eq!((first.id.as_str(), first.rank), ("a", 1));
    put_all(&service, &["b", "c", "d", "e"]).await;

    let ranked = rank(&service, &["e", "a"]).await.unwrap();

    assert_eq!(ranked.count, 5);
    assert_eq!(
        listed_ids(&service, "harold").await,
        strings(&["e", "a", "b", "c", "d"])
    );
    remove(&service, "b", "done elsewhere").await.unwrap();
    assert_eq!(
        listed_ids(&service, "harold").await,
        strings(&["e", "a", "c", "d"])
    );
    let history = notes(&service, "harold", "history").await;
    assert_eq!(history.len(), 1);
    assert!(
        history[0].title.starts_with("removed b ("),
        "{}",
        history[0].title
    );
    assert_eq!(history[0].body, "removed b: done elsewhere");
}

#[tokio::test]
async fn an_empty_reason_writes_no_history_note() {
    let setup = Setup::new().await;
    let service = setup.service();
    put_all(&service, &["a", "b"]).await;

    remove(&service, "b", "").await.unwrap();

    assert_eq!(listed_ids(&service, "harold").await, strings(&["a"]));
    assert!(notes(&service, "harold", "").await.is_empty());
}

#[tokio::test]
async fn an_empty_or_repeated_id_list_is_invalid_ids() {
    let setup = Setup::new().await;
    let service = setup.service();
    put_all(&service, &["a", "b"]).await;
    add_queue(&service, "one", false, 0).await.unwrap();
    add_queue(&service, "two", false, 0).await.unwrap();

    for ids in [&[][..], &["a", "a"][..]] {
        assert_eq!(
            refusal(rank(&service, ids).await),
            (Code::InvalidArgument, "invalid ids".into())
        );
    }
    for ids in [&[][..], &["N1", "N1"][..]] {
        assert_eq!(
            refusal(order(&service, ids).await),
            (Code::InvalidArgument, "invalid ids".into())
        );
    }
    assert_eq!(listed_ids(&service, "harold").await, strings(&["a", "b"]));
}

#[tokio::test]
async fn a_sixth_item_is_backlog_full() {
    let setup = Setup::new().await;
    let service = setup.service();
    put_all(&service, &["a", "b", "c", "d", "e"]).await;

    let sixth = put(&service, "harold", HAROLD_PANE, item("f")).await;

    assert_eq!(
        refusal(sixth),
        (Code::FailedPrecondition, "backlog_full".into())
    );
    assert_eq!(listed_ids(&service, "harold").await.len(), 5);
}

#[tokio::test]
async fn no_evidence_is_invalid_evidence() {
    let setup = Setup::new().await;
    let service = setup.service();

    let put = put(
        &service,
        "harold",
        HAROLD_PANE,
        pb::Item {
            evidence: Vec::new(),
            ..item("a")
        },
    )
    .await;

    assert_eq!(
        refusal(put),
        (Code::InvalidArgument, "invalid evidence".into())
    );
}

#[tokio::test]
async fn a_2000_character_risk_is_invalid_risk_and_nothing_is_stored() {
    let setup = Setup::new().await;
    let service = setup.service();

    let put = put(
        &service,
        "harold",
        HAROLD_PANE,
        pb::Item {
            risk: "r".repeat(2_000),
            ..item("a")
        },
    )
    .await;

    assert_eq!(refusal(put), (Code::InvalidArgument, "invalid risk".into()));
    assert!(listed_ids(&service, "harold").await.is_empty());
}

#[tokio::test]
async fn a_wrong_product_write_is_permission_denied_and_logged() {
    let setup = Setup::new().await;
    let service = setup.service();
    let logs = CapturedLogs::start();

    let put = put(&service, "harold", TURSO_PANE, item("a")).await;

    assert_eq!(
        refusal(put),
        (Code::PermissionDenied, "wrong_product".into())
    );
    assert!(listed_ids(&service, "harold").await.is_empty());
    let text = logs.text();
    let refused: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("backlog write refused"))
        .collect();
    assert_eq!(refused.len(), 1, "{text}");
    assert!(refused[0].contains("tool=\"backlog\""), "{text}");
    assert!(refused[0].contains("product=\"harold\""), "{text}");
    assert!(refused[0].contains("reason=\"wrong_product\""), "{text}");
    println!("evidence: log line: {}", refused[0]);
}

#[tokio::test]
async fn unknown_product_is_not_found() {
    let setup = Setup::new().await;
    let service = setup.service();

    let put = put(&service, "nope", CHIEF, item("a")).await;

    assert_eq!(refusal(put), (Code::NotFound, "unknown_product".into()));
}

#[tokio::test]
async fn a_product_dropped_from_the_registry_stays_readable() {
    let setup = Setup::new().await;
    let before = setup.service();
    put(&before, "turso", TURSO_PANE, item("a")).await.unwrap();
    let after = setup.service_with(vec![setup.repo("harold")]);

    assert_eq!(listed_ids(&after, "turso").await, strings(&["a"]));
    assert!(export(&after, "turso", false).await.contains("a: Title a"));
    assert_eq!(
        refusal(put(&after, "turso", CHIEF, item("b")).await),
        (Code::NotFound, "unknown_product".into())
    );
}

#[tokio::test]
async fn no_pane_is_permission_denied() {
    let setup = Setup::new().await;
    let service = setup.service();

    assert_eq!(
        refusal(put(&service, "harold", "", item("a")).await),
        (Code::PermissionDenied, "no_pane".into())
    );
    let queued = service
        .add_queue_item(Request::new(pb::AddQueueItemRequest {
            text: "one".into(),
            ..pb::AddQueueItemRequest::default()
        }))
        .await;
    assert_eq!(refusal(queued), (Code::PermissionDenied, "no_pane".into()));
}

#[tokio::test]
async fn a_non_chief_queue_write_is_not_chief() {
    let setup = Setup::new().await;
    let service = setup.service();

    let queued = service
        .add_queue_item(Request::new(pb::AddQueueItemRequest {
            text: "one".into(),
            caller_pane: HAROLD_PANE.into(),
            ..pb::AddQueueItemRequest::default()
        }))
        .await;

    assert_eq!(
        refusal(queued),
        (Code::PermissionDenied, "not_chief".into())
    );
    assert!(list_queue(&service, 0).await.parked.is_empty());
}

#[tokio::test]
async fn the_queue_flows_through_the_service() {
    let setup = Setup::new().await;
    let service = setup.service();
    let logs = CapturedLogs::start();

    let first = add_queue(&service, "first", false, 0).await.unwrap();
    assert_eq!((first.id.as_str(), first.state.as_str()), ("N1", "parked"));
    let urgent = add_queue(&service, "urgent", true, 0).await.unwrap();
    assert_eq!((urgent.id.as_str(), urgent.state.as_str()), ("N2", "now"));
    add_queue(&service, "third", false, 0).await.unwrap();

    let now = service
        .set_now(Request::new(pb::SetNowRequest {
            id: "N1".into(),
            caller_pane: CHIEF.into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((now.now.as_str(), now.parked), ("N1", 2));
    // N2 was parked at 1 by `now`, N3 follows it.
    assert_eq!(order(&service, &["N3"]).await.unwrap().count, 2);
    let listing = list_queue(&service, 0).await;
    let parked: Vec<&str> = listing.parked.iter().map(|it| it.id.as_str()).collect();
    assert_eq!(parked, ["N3", "N2"]);

    done(&service, "N1").await.unwrap();
    assert_eq!(
        refusal(done(&service, "N1").await),
        (Code::FailedPrecondition, "already_done".into())
    );
    assert_eq!(
        refusal(order(&service, &["N1"]).await),
        (Code::FailedPrecondition, "not_parked".into())
    );
    assert_eq!(
        refusal(add_queue(&service, "far", false, 9).await),
        (Code::InvalidArgument, "invalid at".into())
    );
    assert_eq!(
        refusal(add_queue(&service, &"t".repeat(4_000), false, 0).await),
        (Code::InvalidArgument, "invalid text".into())
    );

    let listing = list_queue(&service, 0).await;
    assert!(listing.now.is_none());
    assert_eq!(listing.done.len(), 1);
    assert_eq!(listing.done[0].id, "N1");
    let later = list_queue(&service, listing.done[0].done_at_ms + 1).await;
    assert!(later.done.is_empty());
    assert_eq!(later.parked.len(), 2);

    // Each change logs the parked count, never the text.
    let text = logs.text();
    let changed: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("queue changed"))
        .collect();
    assert_eq!(changed.len(), 6, "{text}");
    assert!(changed.last().unwrap().contains("count=2"), "{text}");
    for word in ["first", "urgent", "third"] {
        assert!(!text.contains(word), "{text}");
    }
}

/// The `count` of each `queue changed` line in `text`.
fn queue_counts(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.contains("queue changed"))
        .map(|line| {
            let at = line.find("count=").expect(line) + "count=".len();
            line[at..].split_whitespace().next().unwrap().to_string()
        })
        .collect()
}

#[tokio::test]
async fn every_queue_write_logs_the_parked_count() {
    let setup = Setup::new().await;
    let service = setup.service();
    let logs = CapturedLogs::start();

    add_queue(&service, "a", false, 0).await.unwrap();
    add_queue(&service, "b", true, 0).await.unwrap();
    add_queue(&service, "c", false, 1).await.unwrap();
    service
        .update_queue_item(Request::new(pb::UpdateQueueItemRequest {
            id: "N1".into(),
            text: "changed".into(),
            caller_pane: CHIEF.into(),
        }))
        .await
        .unwrap();
    service
        .set_queue_note(Request::new(pb::SetQueueNoteRequest {
            text: "the note".into(),
            caller_pane: CHIEF.into(),
        }))
        .await
        .unwrap();
    done(&service, "N3").await.unwrap();
    done(&service, "N2").await.unwrap();

    assert_eq!(
        queue_counts(&logs.text()),
        strings(&["1", "1", "2", "2", "2", "1", "1"])
    );
}

#[tokio::test]
async fn a_refusal_for_an_unknown_product_logs_no_product() {
    let setup = Setup::new().await;
    let service = setup.service();
    let junk = format!("JUNK-{}", "x".repeat(5_000));
    let logs = CapturedLogs::start();

    let refused = put(&service, &junk, TURSO_PANE, item("a")).await;
    let no_pane = put(&service, &junk, "", item("a")).await;
    let known = put(&service, "harold", "", item("a")).await;

    assert_eq!(refusal(refused).1, "unknown_product");
    assert_eq!(refusal(no_pane).1, "no_pane");
    assert_eq!(refusal(known).1, "no_pane");
    let text = logs.text();
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("backlog write refused"))
        .collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(!text.contains("JUNK"), "{text}");
    assert!(!lines[0].contains("product="), "{text}");
    assert!(!lines[1].contains("product="), "{text}");
    assert!(lines[2].contains("product=\"harold\""), "{text}");
    println!("evidence: {}", lines[0]);
}

#[tokio::test]
async fn a_dry_run_import_stores_nothing_and_gives_the_counts() {
    let setup = Setup::new().await;
    let service = setup.service();

    let backlog = import(&service, "harold", false, BACKLOG, false)
        .await
        .unwrap();
    let queue = import(&service, "", true, QUEUE, false).await.unwrap();

    assert!(!backlog.applied);
    assert_eq!(
        backlog.counts.unwrap(),
        pb::ImportCounts {
            items: 5,
            rule: 2,
            history: 4,
            held: 3,
            left_out: 3,
            ..pb::ImportCounts::default()
        }
    );
    assert_eq!(
        queue.counts.unwrap(),
        pb::ImportCounts {
            now: 1,
            parked: 4,
            done: 2,
            ..pb::ImportCounts::default()
        }
    );
    assert!(listed_ids(&service, "harold").await.is_empty());
    assert!(notes(&service, "harold", "").await.is_empty());
    let listing = list_queue(&service, 0).await;
    assert!(listing.now.is_none() && listing.parked.is_empty() && listing.note.is_empty());
}

#[tokio::test]
async fn a_dry_run_needs_no_pane_but_a_known_product() {
    let setup = Setup::new().await;
    let service = setup.service();
    let dry = |product: &'static str| {
        service.import(Request::new(pb::ImportRequest {
            product: product.into(),
            markdown: BACKLOG.into(),
            ..pb::ImportRequest::default()
        }))
    };

    assert!(dry("harold").await.is_ok());
    assert_eq!(
        refusal(dry("nope").await),
        (Code::NotFound, "unknown_product".into())
    );
    assert_eq!(
        refusal(import(&service, "harold", true, QUEUE, false).await),
        (Code::InvalidArgument, "invalid product".into())
    );
}

#[tokio::test]
async fn an_applied_import_then_a_second_is_not_empty() {
    let setup = Setup::new().await;
    let service = setup.service();

    let applied = import(&service, "harold", false, BACKLOG, true)
        .await
        .unwrap();
    assert!(applied.applied);
    assert_eq!(listed_ids(&service, "harold").await.len(), 5);
    let queue = import(&service, "", true, QUEUE, true).await.unwrap();
    assert!(queue.applied);

    assert_eq!(
        refusal(import(&service, "harold", false, BACKLOG, false).await),
        (Code::FailedPrecondition, "not_empty".into())
    );
    assert_eq!(
        refusal(import(&service, "harold", false, BACKLOG, true).await),
        (Code::FailedPrecondition, "not_empty".into())
    );
    assert_eq!(
        refusal(import(&service, "", true, QUEUE, true).await),
        (Code::FailedPrecondition, "not_empty".into())
    );
}

#[tokio::test]
async fn the_imported_done_items_are_dated_at_the_callers_midnight() {
    let setup = Setup::new().await;
    let service = setup.service();

    service
        .import(Request::new(pb::ImportRequest {
            queue: true,
            markdown: QUEUE.into(),
            apply: true,
            caller_pane: CHIEF.into(),
            // UTC+10: local midnight of 2026-03-02 is 14:00 UTC on 2026-03-01.
            utc_offset_seconds: 36_000,
            ..pb::ImportRequest::default()
        }))
        .await
        .unwrap();

    let listing = list_queue(&service, 0).await;
    let midnight = time::Date::from_calendar_date(2026, time::Month::March, 1)
        .unwrap()
        .with_hms(14, 0, 0)
        .unwrap()
        .assume_utc()
        .unix_timestamp()
        * 1_000;
    assert_eq!(listing.done.len(), 2);
    assert!(listing.done.iter().all(|item| item.done_at_ms == midnight));
}

#[tokio::test]
async fn an_unparsed_line_stores_nothing() {
    let setup = Setup::new().await;
    let service = setup.service();
    // A line inside an item that is not a field.
    let broken = BACKLOG.replacen("- size: small\n", "- size: small\nstray line\n", 1);

    let refused = import(&service, "harold", false, &broken, true).await;

    let (code, message) = refusal(refused);
    assert_eq!(code, Code::InvalidArgument);
    assert_eq!(message, "unparsed line 11");
    assert!(listed_ids(&service, "harold").await.is_empty());
    assert!(notes(&service, "harold", "").await.is_empty());
}

#[tokio::test]
async fn export_gives_the_markdown() {
    let setup = Setup::new().await;
    let service = setup.service();
    import(&service, "harold", false, BACKLOG, true)
        .await
        .unwrap();
    import(&service, "", true, QUEUE, true).await.unwrap();

    let backlog = export(&service, "harold", false).await;
    let queue = export(&service, "", true).await;

    assert!(backlog.starts_with("# Backlog: harold\n"), "{backlog}");
    assert_eq!(
        markdown::content_lines(&backlog),
        markdown::content_lines(BACKLOG)
    );
    assert!(queue.starts_with("# Queue\n"), "{queue}");
    assert_eq!(
        markdown::content_lines(&queue),
        markdown::content_lines(QUEUE)
    );
}

#[tokio::test]
async fn list_products_gives_each_with_its_item_count() {
    let setup = Setup::new().await;
    let service = setup.service();
    put_all(&service, &["a", "b"]).await;

    let products = service
        .list_products(Request::new(pb::ListProductsRequest {}))
        .await
        .unwrap()
        .into_inner()
        .products;

    let listed: Vec<(&str, u32)> = products
        .iter()
        .map(|product| (product.name.as_str(), product.item_count))
        .collect();
    assert_eq!(listed, [("harold", 2), ("turso", 0)]);
    assert_eq!(PathBuf::from(&products[0].repo), setup.tmp.0.join("harold"));
}
