use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;
use crate::tools::backlog::test_support::{FakeChief, FakeFolders, FakeProducts, TempFolder};

struct Setup {
    tmp: TempFolder,
    folders: Arc<FakeFolders>,
}

impl Setup {
    fn new() -> Self {
        let tmp = TempFolder::new();
        for sub in ["harold/sub", "harold-tasks", "turso"] {
            std::fs::create_dir_all(tmp.0.join(sub)).unwrap();
        }
        Self {
            tmp,
            folders: Arc::new(FakeFolders::default()),
        }
    }

    fn path(&self, sub: &str) -> PathBuf {
        self.tmp.0.join(sub)
    }

    fn pane(&self, id: &str, folder: impl AsRef<Path>) {
        self.folders
            .0
            .lock()
            .unwrap()
            .insert(id.into(), folder.as_ref().to_path_buf());
    }

    fn guard_with(
        &self,
        registry: Result<Vec<(String, PathBuf)>, &'static str>,
        chief: Option<&str>,
    ) -> Guard {
        Guard::new(
            self.folders.clone(),
            Arc::new(FakeProducts(registry)),
            Arc::new(FakeChief(chief.map(String::from))),
        )
    }

    fn guard(&self) -> Guard {
        self.guard_with(
            Ok(vec![
                ("harold".into(), self.path("harold")),
                ("turso".into(), self.path("turso")),
            ]),
            Some("%2"),
        )
    }
}

#[test]
fn a_pane_in_harolds_folder_writes_harold() {
    let s = Setup::new();
    s.pane("%40", s.path("harold"));
    assert_eq!(s.guard().may_write_product("%40", "harold"), Ok(()));
}

#[test]
fn a_pane_in_another_products_folder_is_wrong_product() {
    let s = Setup::new();
    s.pane("%51", s.path("turso"));
    assert_eq!(
        s.guard().may_write_product("%51", "harold"),
        Err(Refused::WrongProduct)
    );
}

#[test]
fn harold_tasks_is_not_inside_harold_and_a_subfolder_is() {
    let s = Setup::new();
    s.pane("%41", s.path("harold-tasks"));
    s.pane("%42", s.path("harold/sub"));
    let guard = s.guard();
    assert_eq!(
        guard.may_write_product("%41", "harold"),
        Err(Refused::WrongProduct)
    );
    assert_eq!(guard.may_write_product("%42", "harold"), Ok(()));
}

#[test]
fn a_trailing_slash_or_a_symlink_is_still_inside() {
    let s = Setup::new();
    s.pane("%43", format!("{}/", s.path("harold").display()));
    std::os::unix::fs::symlink(s.path("harold"), s.path("link")).unwrap();
    s.pane("%44", s.path("link"));
    s.pane("%45", s.path("link/sub"));
    let guard = s.guard();
    for pane in ["%43", "%44", "%45"] {
        assert_eq!(guard.may_write_product(pane, "harold"), Ok(()), "{pane}");
    }
}

#[test]
fn no_pane_is_no_pane() {
    let s = Setup::new();
    let guard = s.guard();
    assert_eq!(guard.may_write_product("", "harold"), Err(Refused::NoPane));
    assert_eq!(guard.may_write_queue(""), Err(Refused::NoPane));
}

#[test]
fn a_product_not_in_the_registry_is_unknown_product() {
    let s = Setup::new();
    s.pane("%40", s.path("harold"));
    let guard = s.guard();
    assert_eq!(
        guard.may_write_product("%40", "nope"),
        Err(Refused::UnknownProduct)
    );
    assert_eq!(
        guard.may_write_product("%2", "nope"),
        Err(Refused::UnknownProduct)
    );
    assert_eq!(guard.product_known("nope"), Err(Refused::UnknownProduct));
    assert_eq!(guard.product_known("harold"), Ok(()));
}

#[test]
fn the_chief_writes_any_product_in_the_registry() {
    let s = Setup::new();
    let guard = s.guard();
    assert_eq!(guard.may_write_product("%2", "harold"), Ok(()));
    assert_eq!(guard.may_write_product("%2", "turso"), Ok(()));
}

#[test]
fn a_gone_pane_is_wrong_product() {
    let s = Setup::new();
    assert_eq!(
        s.guard().may_write_product("%99", "harold"),
        Err(Refused::WrongProduct)
    );
}

#[test]
fn only_the_chief_writes_the_queue() {
    let s = Setup::new();
    let guard = s.guard();
    assert_eq!(guard.may_write_queue("%40"), Err(Refused::NotChief));
    assert_eq!(guard.may_write_queue("%2"), Ok(()));
    let no_chief = s.guard_with(Ok(vec![]), None);
    assert_eq!(no_chief.may_write_queue("%2"), Err(Refused::NotChief));
}

#[test]
fn an_unreadable_registry_refuses_writes() {
    let s = Setup::new();
    s.pane("%40", s.path("harold"));
    let guard = s.guard_with(Err("registry_unreadable"), Some("%2"));
    for pane in ["%40", "%2"] {
        assert_eq!(
            guard.may_write_product(pane, "harold"),
            Err(Refused::RegistryUnreadable)
        );
    }
    assert_eq!(
        guard.product_known("harold"),
        Err(Refused::RegistryUnreadable)
    );
    assert_eq!(guard.products(), Err(Refused::RegistryUnreadable));
}

#[test]
fn reasons_are_the_fixed_words() {
    assert_eq!(Refused::NoPane.reason(), "no_pane");
    assert_eq!(Refused::UnknownProduct.reason(), "unknown_product");
    assert_eq!(Refused::WrongProduct.reason(), "wrong_product");
    assert_eq!(Refused::NotChief.reason(), "not_chief");
    assert_eq!(Refused::RegistryUnreadable.reason(), "registry_unreadable");
}
