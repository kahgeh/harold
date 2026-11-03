use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::*;

struct Folder {
    root: PathBuf,
}

impl Folder {
    fn new() -> Self {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("harold-collections-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("pages")).unwrap();
        Self { root }
    }

    fn pages(&self) -> PathBuf {
        self.root.join("pages")
    }

    fn write(&self, name: &str, bytes: &[u8]) {
        std::fs::write(self.pages().join(name), bytes).unwrap();
    }

    /// Sets the file's time to `secs` seconds after the epoch.
    fn touch(&self, name: &str, secs: u64) {
        let file = File::options()
            .write(true)
            .open(self.pages().join(name))
            .unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    fn collection_at(&self, folder: &Path) -> Collection {
        Collection {
            name: "explainers".into(),
            title: "Explainers".into(),
            folder: folder.to_path_buf(),
            extensions: vec![Ext::Html, Ext::Md],
            listed: true,
        }
    }

    fn collection(&self) -> Collection {
        self.collection_at(&self.pages())
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn settings(name: &str, folder: &Path, listed: bool) -> CollectionSettings {
    CollectionSettings {
        name: name.into(),
        title: name.to_uppercase(),
        folder: folder.to_string_lossy().into_owned(),
        extensions: vec!["html".into(), "md".into()],
        listed,
    }
}

#[test]
fn outcome_words_are_the_log_words() {
    assert_eq!(Outcome::Ok.word(), "ok");
    assert_eq!(Outcome::NotFound.word(), "not_found");
    assert_eq!(Outcome::Refused.word(), "refused");
    assert_eq!(Outcome::TooBig.word(), "too_big");
    assert_eq!(Outcome::Unreadable.word(), "unreadable");
}

#[test]
fn collections_find_by_name_and_hide_the_unlisted() {
    let dir = Folder::new();
    let all = Collections::new(vec![
        settings("a", &dir.pages(), true),
        settings("b", &dir.pages(), false),
    ]);
    assert_eq!(all.names(), vec!["a", "b"]);
    assert!(all.get("b").is_some());
    assert!(all.get("c").is_none());
    let listed: Vec<_> = all.listed().map(|c| c.name.as_str()).collect();
    assert_eq!(listed, vec!["a"]);
    assert_eq!(all.get("a").unwrap().extensions, vec![Ext::Html, Ext::Md]);
}

#[test]
fn list_is_newest_first_with_titles() {
    let dir = Folder::new();
    dir.write("old.html", b"<html><head><title> Old  page </title></head>");
    dir.write("mid.md", b"intro\n# The middle\nbody");
    dir.write("new-name.html", b"<p>no title</p>");
    dir.touch("old.html", 1_000);
    dir.touch("mid.md", 2_000);
    dir.touch("new-name.html", 3_000);
    let listing = list_blocking(&dir.collection()).unwrap();
    let rows: Vec<_> = listing
        .rows
        .iter()
        .map(|r| (r.file.as_str(), r.title.as_str()))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("new-name.html", "new-name.html"),
            ("mid.md", "The middle"),
            ("old.html", "Old page"),
        ]
    );
    assert_eq!(listing.total, 3);
    assert_eq!(listing.rows[0].href, "/c/explainers/new-name.html");
    assert!(!listing.rows[0].when.is_empty());
}

#[test]
fn a_file_name_with_odd_characters_is_encoded_in_the_href() {
    let dir = Folder::new();
    dir.write("a b&c.md", b"x");
    let listing = list_blocking(&dir.collection()).unwrap();
    assert_eq!(listing.rows[0].href, "/c/explainers/a%20b%26c.md");
}

#[test]
fn list_escapes_titles() {
    let dir = Folder::new();
    dir.write("t.html", b"<title><b>&\"</title>");
    let listing = list_blocking(&dir.collection()).unwrap();
    // The row holds the raw text. The template escapes it (Task 4).
    assert_eq!(listing.rows[0].title, "<b>&\"");
}

#[test]
fn a_very_long_title_is_cut() {
    let dir = Folder::new();
    dir.write("t.md", format!("# {}\n", "x".repeat(5000)).as_bytes());
    let listing = list_blocking(&dir.collection()).unwrap();
    assert_eq!(listing.rows[0].title.chars().count(), 200);
}

#[test]
fn title_past_16_kib_is_not_read() {
    let dir = Folder::new();
    let mut text = "a".repeat(16 * 1024);
    text.push_str("\n# Late heading\n");
    dir.write("late.md", text.as_bytes());
    let mut html = "a".repeat(16 * 1024);
    html.push_str("<title>Late</title>");
    dir.write("late.html", html.as_bytes());
    let listing = list_blocking(&dir.collection()).unwrap();
    for row in &listing.rows {
        assert_eq!(row.title, row.file);
    }
}

#[test]
fn a_title_in_broken_utf8_cut_at_16_kib_does_not_panic() {
    let dir = Folder::new();
    let mut bytes = b"<title>".to_vec();
    // A three-byte character cut in the middle by the 16 KiB limit.
    while bytes.len() < 16 * 1024 - 1 {
        bytes.push(b'x');
    }
    bytes.extend_from_slice("€€".as_bytes());
    dir.write("cut.html", &bytes);
    dir.write("bad.md", b"# bad \xff\xfe bytes\n");
    let listing = list_blocking(&dir.collection()).unwrap();
    assert_eq!(listing.rows.len(), 2);
    let bad = listing.rows.iter().find(|r| r.file == "bad.md").unwrap();
    assert!(bad.title.starts_with("bad "));
}

#[test]
fn list_keeps_200_newest_and_reports_total() {
    let dir = Folder::new();
    for i in 0..201u64 {
        let name = format!("p{i:03}.md");
        dir.write(&name, b"x");
        dir.touch(&name, 1_000 + i);
    }
    let listing = list_blocking(&dir.collection()).unwrap();
    assert_eq!(listing.rows.len(), 200);
    assert_eq!(listing.total, 201);
    assert_eq!(listing.rows[0].file, "p200.md");
    assert!(listing.rows.iter().all(|r| r.file != "p000.md"));
    assert_eq!(count_files(&dir.collection()), Ok(201));
}

#[test]
fn list_hides_hidden_files_and_other_extensions_and_symlinks_out() {
    let dir = Folder::new();
    dir.write("shown.html", b"x");
    dir.write(".hidden.html", b"x");
    dir.write("other.txt", b"x");
    dir.write("UPPER.HTML", b"x");
    std::fs::create_dir(dir.pages().join("sub.html")).unwrap();
    std::fs::write(dir.root.join("outside.html"), b"x").unwrap();
    std::os::unix::fs::symlink(dir.root.join("outside.html"), dir.pages().join("link.html"))
        .unwrap();
    let listing = list_blocking(&dir.collection()).unwrap();
    let files: Vec<_> = listing.rows.iter().map(|r| r.file.as_str()).collect();
    assert_eq!(files, vec!["shown.html"]);
    assert_eq!(listing.total, 1);
    assert_eq!(count_files(&dir.collection()), Ok(1));
}

#[test]
fn list_skips_a_file_over_2_mib() {
    let dir = Folder::new();
    dir.write("big.html", &vec![b'x'; 3 * 1024 * 1024]);
    dir.write("ok.html", b"x");
    let listing = list_blocking(&dir.collection()).unwrap();
    assert_eq!(listing.total, 1);
}

#[test]
fn list_skips_a_file_name_that_is_not_utf8() {
    use std::os::unix::ffi::OsStrExt;
    let dir = Folder::new();
    let name = std::ffi::OsStr::from_bytes(b"bad\xff.html");
    // Some file systems refuse such a name; there is then nothing to test.
    if std::fs::write(dir.pages().join(name), b"x").is_err() {
        return;
    }
    dir.write("good.html", b"x");
    let listing = list_blocking(&dir.collection()).unwrap();
    assert_eq!(listing.total, 1);
    assert_eq!(listing.rows[0].file, "good.html");
}

#[test]
fn the_empty_folder_lists_no_rows() {
    let dir = Folder::new();
    let listing = list_blocking(&dir.collection()).unwrap();
    assert!(listing.rows.is_empty());
    assert_eq!(listing.total, 0);
    assert_eq!(count_files(&dir.collection()), Ok(0));
}

#[test]
fn a_missing_folder_is_unreadable_for_the_list_and_the_count() {
    let dir = Folder::new();
    let c = dir.collection_at(&dir.root.join("nope"));
    assert_eq!(list_blocking(&c).unwrap_err(), Outcome::Unreadable);
    assert_eq!(count_files(&c), Err(Outcome::Unreadable));
}

#[test]
fn two_collections_on_one_folder_both_work() {
    let dir = Folder::new();
    dir.write("a.html", b"x");
    let all = Collections::new(vec![
        settings("one", &dir.pages(), true),
        settings("two", &dir.pages(), true),
    ]);
    for name in ["one", "two"] {
        let c = all.get(name).unwrap();
        assert_eq!(list_blocking(c).unwrap().total, 1);
        assert_eq!(load_blocking(c, "a.html").unwrap().bytes, b"x");
    }
}

#[test]
fn load_returns_the_bytes_and_the_extension() {
    let dir = Folder::new();
    dir.write("a.html", b"<p>hi</p>");
    dir.write("b.md", b"# hi");
    let c = dir.collection();
    let a = load_blocking(&c, "a.html").unwrap();
    assert_eq!((a.bytes.as_slice(), a.ext), (&b"<p>hi</p>"[..], Ext::Html));
    assert_eq!(load_blocking(&c, "b.md").unwrap().ext, Ext::Md);
}

#[test]
fn load_refuses_a_symlink_out_of_the_folder() {
    let dir = Folder::new();
    std::fs::write(dir.root.join("outside.html"), b"secret").unwrap();
    std::os::unix::fs::symlink(dir.root.join("outside.html"), dir.pages().join("link.html"))
        .unwrap();
    assert_eq!(
        load_blocking(&dir.collection(), "link.html").unwrap_err(),
        Outcome::Refused
    );
}

#[test]
fn load_follows_a_symlink_that_stays_inside() {
    let dir = Folder::new();
    dir.write("real.html", b"in");
    std::os::unix::fs::symlink(dir.pages().join("real.html"), dir.pages().join("link.html"))
        .unwrap();
    assert_eq!(
        load_blocking(&dir.collection(), "link.html").unwrap().bytes,
        b"in"
    );
}

#[test]
fn load_refuses_a_hidden_file() {
    let dir = Folder::new();
    dir.write(".secret.html", b"x");
    assert_eq!(
        load_blocking(&dir.collection(), ".secret.html").unwrap_err(),
        Outcome::Refused
    );
}

#[test]
fn load_refuses_another_extension() {
    let dir = Folder::new();
    dir.write("a.txt", b"x");
    dir.write("b.md", b"x");
    let mut c = dir.collection();
    c.extensions = vec![Ext::Html];
    for name in ["a.txt", "b.md", "a", "a.HTML", "html", ""] {
        assert_eq!(
            load_blocking(&c, name).unwrap_err(),
            Outcome::Refused,
            "{name}"
        );
    }
}

#[test]
fn load_refuses_dot_dot_and_slash_in_the_file() {
    let dir = Folder::new();
    std::fs::write(dir.root.join("x.html"), b"x").unwrap();
    let c = dir.collection();
    // `..%2f` and `%2e%2e` reach the core already decoded.
    for name in [
        "..",
        "a/b.html",
        "../x.html",
        "..\\x.html",
        "a\\b.html",
        "a\0b.html",
        "\0",
        "/etc/passwd",
    ] {
        assert_eq!(
            load_blocking(&c, name).unwrap_err(),
            Outcome::Refused,
            "{name:?}"
        );
    }
}

#[test]
fn load_returns_too_big_over_2_mib() {
    let dir = Folder::new();
    dir.write("big.html", &vec![b'x'; 3 * 1024 * 1024]);
    dir.write("edge.html", &vec![b'x'; 2 * 1024 * 1024]);
    let c = dir.collection();
    assert_eq!(load_blocking(&c, "big.html").unwrap_err(), Outcome::TooBig);
    assert_eq!(load_blocking(&c, "edge.html").unwrap().bytes.len(), 2 << 20);
}

#[test]
fn load_returns_not_found_for_a_missing_file() {
    let dir = Folder::new();
    assert_eq!(
        load_blocking(&dir.collection(), "gone.html").unwrap_err(),
        Outcome::NotFound
    );
}

#[test]
fn a_file_removed_between_the_list_and_the_open_is_not_found() {
    let dir = Folder::new();
    dir.write("a.html", b"x");
    let c = dir.collection();
    let listing = list_blocking(&c).unwrap();
    std::fs::remove_file(dir.pages().join("a.html")).unwrap();
    assert_eq!(
        load_blocking(&c, &listing.rows[0].file).unwrap_err(),
        Outcome::NotFound
    );
}

#[test]
fn load_returns_unreadable_for_a_missing_folder() {
    let dir = Folder::new();
    let c = dir.collection_at(&dir.root.join("nope"));
    assert_eq!(
        load_blocking(&c, "a.html").unwrap_err(),
        Outcome::Unreadable
    );
}

#[test]
fn load_refuses_a_folder_named_like_a_page() {
    let dir = Folder::new();
    std::fs::create_dir(dir.pages().join("sub.html")).unwrap();
    assert_eq!(
        load_blocking(&dir.collection(), "sub.html").unwrap_err(),
        Outcome::Refused
    );
}

#[test]
fn titles_from_html_and_markdown() {
    assert_eq!(title_of_html("<TITLE>A\n b</Title>"), Some("A b".into()));
    assert_eq!(title_of_html("<title>  </title>"), None);
    assert_eq!(title_of_html("<title>open"), None);
    assert_eq!(title_of_html("no title"), None);
    assert_eq!(title_of_md("x\n# One\n# Two"), Some("One".into()));
    assert_eq!(title_of_md("## Two\n#Three"), None);
    assert_eq!(title_of_md("# \n"), None);
}

#[test]
fn the_time_reads_as_day_month_and_clock() {
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_762_202_400); // 2025-11-03 20:40 UTC
    assert_eq!(format_when(at, time::UtcOffset::UTC), "3 Nov 20:40");
    let plus11 = time::UtcOffset::from_hms(11, 0, 0).unwrap();
    assert_eq!(format_when(at, plus11), "4 Nov 07:40");
}

#[test]
fn a_folder_with_more_files_than_the_descriptor_limit_is_counted_whole() {
    let dir = Folder::new();
    for i in 0..600 {
        dir.write(&format!("p{i:03}.md"), b"# t");
    }
    let c = dir.collection();
    assert_eq!(count_files(&c), Ok(600));
    let listing = list_blocking(&c).unwrap();
    assert_eq!(listing.total, 600);
    assert_eq!(listing.rows.len(), 200);
    assert!(listing.rows.iter().all(|r| r.title == "t"));
}

#[test]
fn a_far_future_or_far_past_time_lists_without_a_panic() {
    let dir = Folder::new();
    dir.write("future.html", b"x");
    // About the year 300000.
    dir.touch("future.html", 9_000_000_000_000);
    let listing = list_blocking(&dir.collection()).unwrap();
    // The file system may clamp the time, so only the lack of a panic is certain here.
    assert!(!listing.rows[0].when.is_empty());
    let year_300000 = SystemTime::UNIX_EPOCH + Duration::from_secs(9_000_000_000_000);
    assert_eq!(format_when(year_300000, time::UtcOffset::UTC), "-");
    let far = SystemTime::UNIX_EPOCH + Duration::from_secs(u64::MAX / 2);
    assert_eq!(format_when(far, time::UtcOffset::UTC), "-");
    let past = SystemTime::UNIX_EPOCH - Duration::from_secs(u64::MAX / 2);
    assert_eq!(format_when(past, time::UtcOffset::UTC), "-");
}

#[test]
fn a_symlink_to_a_hidden_file_or_another_extension_is_refused_and_not_listed() {
    let dir = Folder::new();
    dir.write(".hidden.html", b"secret");
    dir.write("data.txt", b"secret");
    dir.write("real.md", b"ok");
    for (link, target) in [
        ("to-hidden.html", ".hidden.html"),
        ("to-txt.html", "data.txt"),
        ("to-md.html", "real.md"),
    ] {
        std::os::unix::fs::symlink(dir.pages().join(target), dir.pages().join(link)).unwrap();
    }
    let mut c = dir.collection();
    c.extensions = vec![Ext::Html, Ext::Md];
    for link in ["to-hidden.html", "to-txt.html"] {
        assert_eq!(load_blocking(&c, link).unwrap_err(), Outcome::Refused);
    }
    // A target with an allowed extension is served.
    assert!(load_blocking(&c, "to-md.html").is_ok());
    let mut files: Vec<_> = list_blocking(&c)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r.file)
        .collect();
    files.sort();
    assert_eq!(files, vec!["real.md", "to-md.html"]);
}
