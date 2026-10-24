use std::fs;
use std::os::unix::fs::symlink;
use std::time::{Duration, SystemTime};

use super::*;
use crate::tools::questions::test_support::{FakeProducts, TempFolder, sha12};

/// A registered product whose specs folder holds the files a test writes.
struct Fixture {
    root: TempFolder,
    checker: SpecChecker,
}

impl Fixture {
    fn new() -> Self {
        let root = TempFolder::new();
        let specs = root.0.join("repo").join(".superpowers/pod/specs");
        fs::create_dir_all(&specs).unwrap();
        // The repo is reached through its resolved path, as on a Mac whose temp folder is a link.
        let repo = fs::canonicalize(root.0.join("repo")).unwrap();
        let checker = SpecChecker::new(Arc::new(FakeProducts(Ok(vec![("harold".into(), repo)]))));
        Self { root, checker }
    }

    fn specs(&self) -> PathBuf {
        fs::canonicalize(self.root.0.join("repo").join(".superpowers/pod/specs")).unwrap()
    }

    fn write(&self, name: &str, bytes: &[u8]) -> String {
        let path = self.specs().join(name);
        fs::write(&path, bytes).unwrap();
        path.to_str().unwrap().to_owned()
    }

    fn outside(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let folder = self.root.0.join("elsewhere");
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}

#[test]
fn a_file_with_the_asked_checksum_is_current_and_one_without_is_changed() {
    let fixture = Fixture::new();
    let file = fixture.write("a.md", b"first");
    assert_eq!(
        fixture.checker.state(&file, &sha12(b"first")),
        State::Current
    );
    assert_eq!(
        fixture.checker.state(&file, &sha12(b"other")),
        State::Changed
    );
}

#[test]
fn a_checksum_in_capitals_still_matches() {
    let fixture = Fixture::new();
    let file = fixture.write("a.md", b"first");
    let asked = sha12(b"first").to_ascii_uppercase();
    assert_eq!(fixture.checker.state(&file, &asked), State::Current);
}

#[test]
fn the_spec_changing_and_changing_back_is_current_again() {
    let fixture = Fixture::new();
    let file = fixture.write("a.md", b"first");
    let asked = sha12(b"first");
    fixture.write("a.md", b"second, longer");
    assert_eq!(fixture.checker.state(&file, &asked), State::Changed);
    fixture.write("a.md", b"first");
    assert_eq!(fixture.checker.state(&file, &asked), State::Current);
}

#[test]
fn a_sha_that_is_not_twelve_or_more_hex_digits_says_nothing_about_the_file() {
    let fixture = Fixture::new();
    let file = fixture.write("a.md", b"first");
    let right = sha12(b"first");
    for asked in [&right[..1], &right[..11], "not-hex-at-all", "zzzzzzzzzzzz"] {
        assert_eq!(
            fixture.checker.state(&file, asked),
            State::Unknown(None),
            "{asked}"
        );
    }
    let whole = hex::encode(Sha256::digest(b"first"));
    assert_eq!(fixture.checker.state(&file, &whole), State::Current);
    assert_eq!(fixture.checker.state(&file, &whole[..40]), State::Current);
}

#[test]
fn a_pipe_in_place_of_the_file_is_not_a_file_and_is_never_waited_on() {
    let fixture = Fixture::new();
    let pipe = fixture.specs().join("pipe.md");
    let status = std::process::Command::new("mkfifo")
        .arg(&pipe)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        fixture.checker.state(pipe.to_str().unwrap(), &sha12(b"x")),
        State::Unknown(Some(Reason::NotAFile))
    );
}

#[test]
fn a_todo_with_no_spec_file_or_no_sha_is_unknown_without_a_reason() {
    let fixture = Fixture::new();
    let file = fixture.write("a.md", b"first");
    assert_eq!(fixture.checker.state("", "abc"), State::Unknown(None));
    assert_eq!(fixture.checker.state(&file, ""), State::Unknown(None));
}

#[test]
fn a_missing_file_a_folder_and_a_big_file_are_unknown_with_their_reason() {
    let fixture = Fixture::new();
    let asked = sha12(b"x");
    let missing = fixture.specs().join("gone.md");
    assert_eq!(
        fixture.checker.state(missing.to_str().unwrap(), &asked),
        State::Unknown(Some(Reason::Missing))
    );
    let folder = fixture.specs().join("folder.md");
    fs::create_dir(&folder).unwrap();
    assert_eq!(
        fixture.checker.state(folder.to_str().unwrap(), &asked),
        State::Unknown(Some(Reason::NotAFile))
    );
    let big = fixture.write("big.md", &vec![b'a'; 1024 * 1024 + 1]);
    assert_eq!(
        fixture.checker.state(&big, &asked),
        State::Unknown(Some(Reason::TooLarge))
    );
    let limit = fixture.write("limit.md", &vec![b'a'; 1024 * 1024]);
    assert_eq!(
        fixture
            .checker
            .state(&limit, &sha12(&vec![b'a'; 1024 * 1024])),
        State::Current,
        "1 MB exactly is read"
    );
}

#[test]
fn a_file_outside_every_specs_folder_is_never_read() {
    let fixture = Fixture::new();
    let outside = fixture.outside("secret.md", b"outside");
    // Its checksum matches, so reading it would give `current`.
    assert_eq!(
        fixture
            .checker
            .state(outside.to_str().unwrap(), &sha12(b"outside")),
        State::Unknown(Some(Reason::OutsideSpecs))
    );
    assert_eq!(
        fixture.checker.may_put(outside.to_str().unwrap()),
        Err(Reason::OutsideSpecs)
    );
}

#[test]
fn a_symlink_inside_the_specs_folder_leading_out_is_refused_at_put_and_unknown_after() {
    let fixture = Fixture::new();
    let outside = fixture.outside("secret.md", b"outside");
    let link = fixture.specs().join("link.md");
    symlink(&outside, &link).unwrap();
    assert_eq!(
        fixture.checker.may_put(link.to_str().unwrap()),
        Err(Reason::OutsideSpecs)
    );

    // A link that led inside at put and is changed later to lead out.
    let inside = fixture.write("real.md", b"inside");
    let moving = fixture.specs().join("moving.md");
    symlink(&inside, &moving).unwrap();
    let moving = moving.to_str().unwrap();
    assert_eq!(fixture.checker.may_put(moving), Ok(()));
    assert_eq!(
        fixture.checker.state(moving, &sha12(b"inside")),
        State::Current
    );
    fs::remove_file(moving).unwrap();
    symlink(&outside, moving).unwrap();
    assert_eq!(
        fixture.checker.state(moving, &sha12(b"outside")),
        State::Unknown(Some(Reason::OutsideSpecs)),
        "the outside file matches the checksum, so it was not read"
    );
}

#[test]
fn a_dangling_symlink_is_not_accepted_at_put() {
    let fixture = Fixture::new();
    let link = fixture.specs().join("dangling.md");
    symlink(fixture.root.0.join("nowhere.md"), &link).unwrap();
    assert_eq!(
        fixture.checker.may_put(link.to_str().unwrap()),
        Err(Reason::OutsideSpecs)
    );
}

#[test]
fn a_file_not_written_yet_passes_put_when_its_folder_is_a_specs_folder() {
    let fixture = Fixture::new();
    let later = fixture.specs().join("later.md");
    assert_eq!(fixture.checker.may_put(later.to_str().unwrap()), Ok(()));
    let nowhere = fixture.specs().join("sub").join("later.md");
    assert_eq!(
        fixture.checker.may_put(nowhere.to_str().unwrap()),
        Err(Reason::OutsideSpecs)
    );
}

#[test]
fn a_dotdot_path_that_climbs_out_is_refused() {
    let fixture = Fixture::new();
    let outside = fixture.outside("secret.md", b"outside");
    let climbing = format!(
        "{}/../../../../elsewhere/secret.md",
        fixture.specs().display()
    );
    assert!(outside.exists());
    assert_eq!(
        fixture.checker.may_put(&climbing),
        Err(Reason::OutsideSpecs)
    );
}

#[test]
fn a_registry_that_cannot_be_read_gives_unknown_never_current() {
    let fixture = Fixture::new();
    let file = fixture.write("a.md", b"first");
    let broken = SpecChecker::new(Arc::new(FakeProducts(Err("registry_unreadable"))));
    assert_eq!(
        broken.state(&file, &sha12(b"first")),
        State::Unknown(Some(Reason::RegistryUnreadable))
    );
    assert_eq!(broken.may_put(&file), Err(Reason::RegistryUnreadable));
}

#[test]
fn a_product_with_a_relative_repo_has_no_specs_folder() {
    let fixture = Fixture::new();
    let file = fixture.write("a.md", b"first");
    let relative = SpecChecker::new(Arc::new(FakeProducts(Ok(vec![(
        "harold".into(),
        PathBuf::from("repo"),
    )]))));
    assert_eq!(
        relative.state(&file, &sha12(b"first")),
        State::Unknown(Some(Reason::OutsideSpecs))
    );
}

#[test]
fn the_file_is_read_again_only_when_its_size_or_time_changes() {
    let fixture = Fixture::new();
    let file = fixture.write("a.md", b"aaaa");
    let asked = sha12(b"aaaa");
    let then = SystemTime::now() - Duration::from_secs(3600);
    let set_time = |at: SystemTime| {
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(at)
            .unwrap();
    };
    set_time(then);
    assert_eq!(fixture.checker.state(&file, &asked), State::Current);

    // Same size, same time: the cached checksum stands, so the file was not read.
    fs::write(&file, b"bbbb").unwrap();
    set_time(then);
    assert_eq!(fixture.checker.state(&file, &asked), State::Current);

    // A new time: read again.
    set_time(then + Duration::from_secs(5));
    assert_eq!(fixture.checker.state(&file, &asked), State::Changed);

    // A new size: read again.
    fs::write(&file, b"aaaa").unwrap();
    set_time(then);
    assert_eq!(fixture.checker.state(&file, &asked), State::Current);
    fs::write(&file, b"aaaaa").unwrap();
    set_time(then);
    assert_eq!(fixture.checker.state(&file, &asked), State::Changed);
}
