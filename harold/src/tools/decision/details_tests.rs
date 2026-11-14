use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::tools::Products;
use crate::tools::decision::test_support::Temp;

struct FakeProducts(Result<Vec<(String, PathBuf)>, &'static str>);

impl Products for FakeProducts {
    fn products(&self) -> Result<Vec<(String, PathBuf)>, &'static str> {
        self.0.clone()
    }
}

const ID: &str = "d-20261114-02";

/// A pod registry with the product `harold`, a vault and a personal repository, each in a
/// temporary folder, and one entry in each.
struct Fixture {
    root: Temp,
    repo: PathBuf,
    vault: PathBuf,
    personal: PathBuf,
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn entry(id: &str) -> String {
    format!("# Decisions\n\n## {id}: Fold the naming fix\n\nThe question.\n")
}

fn fixture() -> Fixture {
    let root = Temp::new();
    let repo = root.0.join("harold");
    let vault = root.0.join("vault");
    let personal = root.0.join("personal");
    write(
        &repo.join(".superpowers/pod/specs/spec-a.decisions.md"),
        &entry(ID),
    );
    write(&repo.join(".superpowers/pod/decisions.md"), &entry(ID));
    write(&vault.join("decisions.md"), &entry(ID));
    write(&personal.join("notes/decisions.md"), &entry(ID));
    Fixture {
        root,
        repo,
        vault,
        personal,
    }
}

fn details(fx: &Fixture) -> FsDetails {
    FsDetails::new(
        true,
        Arc::new(FakeProducts(Ok(vec![("harold".into(), fx.repo.clone())]))),
        Some(fx.vault.clone()),
        Some(fx.personal.clone()),
    )
}

async fn reason(details: &FsDetails, value: &str, owner: &str) -> &'static str {
    details
        .check(value, owner)
        .await
        .expect_err("refused")
        .reason
}

#[tokio::test]
async fn a_heading_with_the_id_passes_in_each_of_the_four_places() {
    let fx = fixture();
    let d = details(&fx);
    for (value, owner, place, tail) in [
        (
            "pod:harold/specs/spec-a.decisions.md#d-20261114-02",
            "pnem:harold",
            "pod",
            ".superpowers/pod/specs/spec-a.decisions.md",
        ),
        (
            "pod:harold/decisions.md#d-20261114-02",
            "pnem:harold",
            "pod",
            ".superpowers/pod/decisions.md",
        ),
        (
            "vault:decisions.md#d-20261114-02",
            "chief",
            "vault",
            "vault/decisions.md",
        ),
        (
            "personal:notes/decisions.md#d-20261114-02",
            "chief",
            "personal",
            "personal/notes/decisions.md",
        ),
    ] {
        let found = d
            .check(value, owner)
            .await
            .unwrap_or_else(|e| panic!("{value}: {}", e.reason));
        assert_eq!(found.place, place);
        assert!(found.full_path.ends_with(tail), "{:?}", found.full_path);
        assert_eq!(d.path_of(value), Some(found.full_path));
    }
}

#[tokio::test]
async fn each_refusal_has_its_reason() {
    let fx = fixture();
    let d = details(&fx);
    for (value, owner, expected) in [
        ("elsewhere:x.md#d-20261114-02", "chief", "unknown_place"),
        ("no-colon#d-20261114-02", "chief", "unknown_place"),
        (
            "pod:unknown/decisions.md#d-20261114-02",
            "pnem:unknown",
            "unknown_product",
        ),
        (
            "vault:../outside.md#d-20261114-02",
            "chief",
            "path_outside_place",
        ),
        (
            "vault:notes/../../x.md#d-20261114-02",
            "chief",
            "path_outside_place",
        ),
        (
            "vault:/etc/passwd#d-20261114-02",
            "chief",
            "path_outside_place",
        ),
        (
            "pod:harold/../other/x.md#d-20261114-02",
            "pnem:harold",
            "path_outside_place",
        ),
        (
            "vault:nothing-here.md#d-20261114-02",
            "chief",
            "details_unreadable",
        ),
        (
            "vault:decisions.md#d-20261114-09",
            "chief",
            "details_missing",
        ),
        ("vault:decisions.md", "chief", "details_missing"),
        ("vault:decisions.md#", "chief", "details_missing"),
        (
            "pod:harold/decisions.md#d-20261114-02",
            "chief",
            "details_owner_mismatch",
        ),
        (
            "pod:harold/decisions.md#d-20261114-02",
            "pnem:other",
            "details_owner_mismatch",
        ),
        (
            "vault:decisions.md#d-20261114-02",
            "pnem:harold",
            "details_owner_mismatch",
        ),
        (
            "personal:notes/decisions.md#d-20261114-02",
            "pnem:harold",
            "details_owner_mismatch",
        ),
    ] {
        assert_eq!(reason(&d, value, owner).await, expected, "{value} {owner}");
    }
}

#[tokio::test]
async fn a_heading_that_only_starts_with_the_id_is_not_the_heading() {
    let fx = fixture();
    write(
        &fx.vault.join("longer.md"),
        "## d-20261114-020: Another\n\n## x d-20261114-02: no\nd-20261114-02: plain text\n",
    );
    let d = details(&fx);
    assert_eq!(
        reason(&d, "vault:longer.md#d-20261114-02", "chief").await,
        "details_missing"
    );
}

#[tokio::test]
async fn a_symlink_that_leaves_the_place_is_refused() {
    let fx = fixture();
    let outside = fx.root.0.join("outside.md");
    write(&outside, &entry(ID));
    std::os::unix::fs::symlink(&outside, fx.vault.join("escape.md")).unwrap();
    let d = details(&fx);
    assert_eq!(
        reason(&d, "vault:escape.md#d-20261114-02", "chief").await,
        "path_outside_place"
    );
}

#[tokio::test]
async fn a_place_that_is_not_set_is_refused_with_its_reason() {
    let fx = fixture();
    let products = || Arc::new(FakeProducts(Ok(vec![("harold".into(), fx.repo.clone())])));
    let no_registry = FsDetails::new(
        false,
        products(),
        Some(fx.vault.clone()),
        Some(fx.personal.clone()),
    );
    assert_eq!(
        reason(
            &no_registry,
            "pod:harold/decisions.md#d-20261114-02",
            "pnem:harold"
        )
        .await,
        "no_pod_registry"
    );
    let unreadable = FsDetails::new(
        true,
        Arc::new(FakeProducts(Err("registry_unreadable"))),
        None,
        None,
    );
    assert_eq!(
        reason(
            &unreadable,
            "pod:harold/decisions.md#d-20261114-02",
            "pnem:harold"
        )
        .await,
        "no_pod_registry"
    );
    let bare = FsDetails::new(true, products(), None, None);
    assert_eq!(
        reason(&bare, "vault:decisions.md#d-20261114-02", "chief").await,
        "no_vault"
    );
    assert_eq!(
        reason(&bare, "personal:notes/decisions.md#d-20261114-02", "chief").await,
        "no_personal_repository"
    );
    // A Harold without `knowledge.vault` still takes `pod:` details.
    assert!(
        bare.check("pod:harold/decisions.md#d-20261114-02", "pnem:harold")
            .await
            .is_ok()
    );
    assert_eq!(bare.path_of("vault:decisions.md#d-20261114-02"), None);
}
