use std::collections::HashMap;
use std::path::Path;

use time::{OffsetDateTime, UtcOffset};

use super::ReleaseToolService;
use crate::pod_stats::{Git, GitOutput};
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

const DESCRIBE: [&str; 6] = [
    "describe",
    "--tags",
    "--abbrev=0",
    "--match",
    "v[0-9]*",
    "origin/main",
];
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// A `Git` that answers from one `FakeGit` for each repo folder name.
struct PerRepoGit(HashMap<&'static str, FakeGit>);

impl Git for PerRepoGit {
    async fn run(&self, repo: &Path, args: &[&str]) -> Option<GitOutput> {
        let name = repo.file_name()?.to_str()?;
        self.0.get(name)?.run(repo, args).await
    }
}

#[tokio::test]
async fn a_paused_product_has_its_release_and_a_git_failure_hides_no_other_product() {
    let folder = TempFolder::new();
    let root = std::fs::canonicalize(&folder.0).expect("canonical folder");
    let path = root.join("products.toml");
    let text = format!(
        "[products.buckwal]\nrepo = \"{0}/buckwal\"\nbase = \"main\"\npaused = true\n\n\
         [products.harold]\nrepo = \"{0}/harold\"\nbase = \"main\"\n\n\
         [products.keymap]\nrepo = \"{0}/keymap\"\nbase = \"main\"\n",
        root.display()
    );
    std::fs::write(&path, text).expect("write the registry");
    let harold = root.join("harold");
    let spec = harold.join(".superpowers/pod/specs/alpha.md");
    std::fs::create_dir_all(spec.parent().expect("a parent")).expect("create the folder");
    std::fs::write(&spec, "Waiting for your approval.\n\n# alpha: First\n").expect("write");

    let tagged = |fake: FakeGit| {
        fake.answer(&DESCRIBE, true, "v2.0.0\n").answer(
            &["log", "-1", "--format=%ct", "v2.0.0"],
            true,
            "1760918400",
        )
    };
    let listing = format!(
        "worktree {}\nHEAD {COMMIT}\nbranch refs/heads/main\n\n",
        harold.display()
    );
    let harold_git = tagged(FakeGit::default())
        .answer(&["worktree", "list", "--porcelain"], true, &listing)
        .answer(&["rev-parse", "--verify", "refs/heads/main"], true, COMMIT)
        .answer(
            &[
                "reflog",
                "show",
                "--date=unix",
                "--format=%H%x09%gd%x09%gs",
                "refs/heads/main",
            ],
            true,
            "",
        )
        .answer(&["rev-list", "--count", "v2.0.0..origin/main"], true, "0\n");
    let git = PerRepoGit(HashMap::from([
        ("buckwal", tagged(FakeGit::default())),
        ("harold", harold_git),
        ("keymap", FakeGit::default().no_answer(&DESCRIBE)),
    ]));
    let service = ReleaseToolService::with_git(Some(path), git);

    let lists = service
        .products("", now(), UtcOffset::UTC)
        .await
        .expect("the lists");

    let names: Vec<&str> = lists.iter().map(|list| list.name.as_str()).collect();
    assert_eq!(names, ["buckwal", "harold", "keymap"]);
    let (buckwal, harold, keymap) = (&lists[0], &lists[1], &lists[2]);
    assert!(buckwal.paused);
    assert_eq!(buckwal.release, "v2.0.0");
    assert_eq!(buckwal.unavailable, None);
    assert!(buckwal.merged.is_empty() && buckwal.awaiting.is_empty());
    assert!(!harold.paused);
    assert_eq!(harold.release, "v2.0.0");
    assert_eq!(harold.unavailable, None);
    assert_eq!(harold.awaiting.len(), 1, "{harold:?}");
    assert_eq!(harold.awaiting[0].id, "alpha");
    assert_eq!(keymap.unavailable, Some("git gave no answer"));
}

#[test]
fn the_reply_carries_the_evidence_missing_list() {
    let next = super::ProductNext {
        name: "harold".into(),
        evidence_missing: vec![crate::tools::release::model::Line {
            id: "alpha".into(),
            title: "First".into(),
            commits: None,
        }],
        ..Default::default()
    };

    let reply = super::to_proto(next);

    assert_eq!(reply.evidence_missing.len(), 1);
    assert_eq!(reply.evidence_missing[0].id, "alpha");
    assert_eq!(reply.evidence_missing[0].title, "First");
}
