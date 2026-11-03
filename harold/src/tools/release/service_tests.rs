use time::{OffsetDateTime, UtcOffset};

use super::ReleaseToolService;
use crate::tools::release::gather::gather_tests::FakeGit;
use crate::tools::tests::TempFolder;

/// A registry in a temporary folder: `harold` and `buckwal` (paused), with fake repos.
fn registry(folder: &TempFolder) -> std::path::PathBuf {
    let root = &folder.0;
    let path = root.join("products.toml");
    let text = format!(
        "[products.harold]\nrepo = \"{0}/harold\"\nbase = \"main\"\n\n\
         [products.buckwal]\nrepo = \"{0}/buckwal\"\nbase = \"main\"\npaused = true\n",
        root.display()
    );
    std::fs::write(&path, text).expect("write the registry");
    path
}

fn now() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH
}

#[tokio::test]
async fn unknown_product_is_refused() {
    let folder = TempFolder::new();
    let service = ReleaseToolService::with_git(Some(registry(&folder)), FakeGit::default());

    let error = service
        .products("nothing", now(), UtcOffset::UTC)
        .await
        .expect_err("a refusal");

    assert_eq!(error.code(), tonic::Code::NotFound);
    assert_eq!(error.message(), "unknown_product");
}

#[tokio::test]
async fn unset_registry_is_refused() {
    let service = ReleaseToolService::with_git(None, FakeGit::default());

    let error = service
        .products("", now(), UtcOffset::UTC)
        .await
        .expect_err("a refusal");

    assert_eq!(error.code(), tonic::Code::Unavailable);
    assert_eq!(error.message(), "registry_unreadable");
}

#[tokio::test]
async fn unreadable_registry_is_refused() {
    let folder = TempFolder::new();
    let service =
        ReleaseToolService::with_git(Some(folder.0.join("absent.toml")), FakeGit::default());

    let error = service
        .products("", now(), UtcOffset::UTC)
        .await
        .expect_err("a refusal");

    assert_eq!(error.code(), tonic::Code::Unavailable);
    assert_eq!(error.message(), "registry_unreadable");
}

#[tokio::test]
async fn products_come_in_alphabetical_order_and_a_paused_one_has_no_lists() {
    let folder = TempFolder::new();
    let git = FakeGit::default()
        .answer(
            &[
                "describe",
                "--tags",
                "--abbrev=0",
                "--match",
                "v[0-9]*",
                "origin/main",
            ],
            true,
            "v2.0.0\n",
        )
        .no_answer(&["log", "-1", "--format=%ct", "v2.0.0"]);
    let service = ReleaseToolService::with_git(Some(registry(&folder)), git);

    let lists = service
        .products("", now(), UtcOffset::UTC)
        .await
        .expect("the lists");

    let names: Vec<&str> = lists.iter().map(|list| list.name.as_str()).collect();
    assert_eq!(names, ["buckwal", "harold"]);
    assert!(lists[0].paused);
    assert_eq!(lists[0].release, "");
    assert_eq!(lists[0].unavailable, Some("git gave no answer"));
    assert!(!lists[1].paused);
    assert_eq!(lists[1].unavailable, Some("git gave no answer"));
}

#[tokio::test]
async fn a_named_product_is_the_only_one_listed() {
    let folder = TempFolder::new();
    let service = ReleaseToolService::with_git(Some(registry(&folder)), FakeGit::default());

    let lists = service
        .products("buckwal", now(), UtcOffset::UTC)
        .await
        .expect("the lists");

    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0].name, "buckwal");
}
