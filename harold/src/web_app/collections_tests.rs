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

// ---------------------------------------------------------------------------
// The pages, through the web app's router
// ---------------------------------------------------------------------------

mod pages {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use tokio::net::TcpListener;
    use tokio::sync::watch;
    use tokio::task::JoinHandle;

    use super::{Folder, settings};
    use crate::outbound::fake::CapturedLogs;
    use crate::settings::GrpcSettings;
    use crate::store::HaroldStore;
    use crate::web_app::collections::Collections;
    use crate::web_app::notes::Notes;
    use crate::web_app::origin::loopback_hosts;
    use crate::web_app::pages::{
        COLLECTION_PAGE_CONTENT_SECURITY_POLICY, COLLECTION_SHELL_CONTENT_SECURITY_POLICY,
        SPEC_CONTENT_SECURITY_POLICY,
    };
    use crate::web_app::{AppState, WebAppIdentity, router};

    const LOGIN: &str = "chief@example.com";

    struct Served {
        addr: SocketAddr,
        handle: JoinHandle<()>,
        _shutdown: watch::Sender<()>,
        _store: Folder,
    }

    impl Drop for Served {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn serve(
        collections: Vec<crate::settings::CollectionSettings>,
        notes_on: bool,
    ) -> Served {
        serve_with(collections, notes_on, false).await
    }

    /// The web app; with `paired` the device check is on and no device is paired.
    async fn serve_with(
        collections: Vec<crate::settings::CollectionSettings>,
        notes_on: bool,
        paired: bool,
    ) -> Served {
        crate::telemetry::test_support::install();
        let store_dir = Folder::new();
        let store = Arc::new(
            HaroldStore::open(store_dir.root.join("store"))
                .await
                .unwrap(),
        );
        let (shutdown, shutdown_rx) = watch::channel(());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut state = AppState::for_tests(
            store,
            LOGIN,
            "BPublicKey",
            Arc::from(loopback_hosts("127.0.0.1:0", addr)),
            shutdown_rx,
            None,
            WebAppIdentity::default(),
        );
        state.collections = Arc::new(Collections::new(collections));
        if paired {
            let pairing = crate::web_app::pair::Pairing::load(
                store_dir.root.join("devices.json"),
                store_dir.root.join("pairing.env"),
                true,
                Arc::new(std::time::Instant::now),
            )
            .unwrap();
            state = state.with_pairing(pairing);
        }
        if notes_on {
            state.notes = Some(Notes::lazy(&GrpcSettings {
                host: "127.0.0.1".into(),
                port: 1,
            }));
        }
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router(state)).await;
        });
        Served {
            addr,
            handle,
            _shutdown: shutdown,
            _store: store_dir,
        }
    }

    async fn get(server: &Served, path: &str) -> reqwest::Response {
        reqwest::Client::new()
            .get(format!("http://{}{path}", server.addr))
            .header("Tailscale-User-Login", LOGIN)
            .send()
            .await
            .unwrap()
    }

    fn header(response: &reqwest::Response, name: &str) -> String {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    }

    async fn one(dir: &Folder) -> Served {
        serve(vec![settings("explainers", &dir.pages(), true)], false).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn html_raw_has_content_type_nosniff_and_the_sandbox_policy() {
        let dir = Folder::new();
        dir.write("a.html", b"<html><body>hello</body></html>");
        let server = one(&dir).await;
        let response = get(&server, "/c/explainers/a.html/raw").await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            header(&response, "content-type"),
            "text/html; charset=utf-8"
        );
        assert_eq!(header(&response, "x-content-type-options"), "nosniff");
        assert_eq!(
            header(&response, "content-security-policy"),
            "sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:; script-src 'none'; connect-src 'none'; form-action 'none'; base-uri 'none'; frame-ancestors 'self'"
        );
        assert_eq!(
            header(&response, "content-security-policy"),
            COLLECTION_PAGE_CONTENT_SECURITY_POLICY
        );
        assert_eq!(
            response.text().await.unwrap(),
            "<html><body>hello</body></html>"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_page_with_a_script_is_served_with_script_src_none() {
        let dir = Folder::new();
        dir.write("s.html", b"<p>x</p><script>alert(1)</script>");
        let server = one(&dir).await;
        let response = get(&server, "/c/explainers/s.html/raw").await;
        assert_eq!(response.status(), 200);
        assert!(header(&response, "content-security-policy").contains("script-src 'none'"));
        assert!(response.text().await.unwrap().contains("<script>"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_shell_holds_the_sandboxed_frame_and_frame_src_self() {
        let dir = Folder::new();
        dir.write("a b.html", b"<title>My page</title>");
        let server = one(&dir).await;
        let response = get(&server, "/c/explainers/a%20b.html").await;
        assert_eq!(response.status(), 200);
        let policy = header(&response, "content-security-policy");
        assert_eq!(policy, COLLECTION_SHELL_CONTENT_SECURITY_POLICY);
        assert!(policy.contains("frame-src 'self'"));
        let body = response.text().await.unwrap();
        assert!(
            body.contains(
                "<iframe class=\"collection-frame\" sandbox src=\"/c/explainers/a%20b.html/raw\""
            ),
            "{body}"
        );
        assert!(!body.contains("allow-"), "{body}");
        assert!(body.contains("<title>My page</title>"), "{body}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn md_page_shows_raw_html_as_text_and_drops_a_javascript_link() {
        let dir = Folder::new();
        dir.write(
            "n.md",
            b"# Note\n\nA <b>raw</b> word and [bad](javascript:alert(1)) and [good](https://example.com/x).\n",
        );
        let server = one(&dir).await;
        let body = get(&server, "/c/explainers/n.md")
            .await
            .text()
            .await
            .unwrap();
        assert!(body.contains("&lt;b&gt;raw&lt;/b&gt;"), "{body}");
        assert!(!body.contains("<b>raw"), "{body}");
        assert!(!body.contains("javascript:"), "{body}");
        assert!(body.contains("href=\"https://example.com/x\""), "{body}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn md_page_uses_the_spec_template_and_policy() {
        let dir = Folder::new();
        dir.write("n.md", b"# Note title\n\ntext\n");
        let server = one(&dir).await;
        let response = get(&server, "/c/explainers/n.md").await;
        assert_eq!(
            header(&response, "content-security-policy"),
            SPEC_CONTENT_SECURITY_POLICY
        );
        let body = response.text().await.unwrap();
        assert!(body.contains("notes-page vault-note spec-page"), "{body}");
        assert!(body.contains("/static/specs.js"), "{body}");
        assert!(body.contains("<h3>Note title</h3>"), "{body}");
        assert!(!body.contains("share"), "{body}");
        assert!(!body.contains("<iframe"), "{body}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn raw_for_an_md_file_is_404() {
        let dir = Folder::new();
        dir.write("n.md", b"# Note\n");
        let server = one(&dir).await;
        assert_eq!(get(&server, "/c/explainers/n.md/raw").await.status(), 404);
    }

    fn bad_paths(dir: &Folder) -> Vec<String> {
        std::fs::write(dir.root.join("secret.html"), b"SECRETBODY").unwrap();
        std::os::unix::fs::symlink(dir.root.join("secret.html"), dir.pages().join("out.html"))
            .unwrap();
        dir.write(".hidden.html", b"x");
        dir.write("notes.txt", b"x");
        dir.write("big.html", &vec![b'a'; 3 * 1024 * 1024]);
        let mut paths = Vec::new();
        for prefix in ["/c/explainers/", "/c/nosuch/"] {
            for file in [
                "out.html",
                "%2e%2e%2Fsecret.html",
                "..%2Fsecret.html",
                "a%2Fb.html",
                "%2Fetc%2Fpasswd",
                "x%00.html",
                "%FF.html",
                ".hidden.html",
                "notes.txt",
                "big.html",
                "missing.html",
            ] {
                paths.push(format!("{prefix}{file}"));
                paths.push(format!("{prefix}{file}/raw"));
            }
        }
        paths.push("/c/%FF".to_owned());
        paths.push("/c/nosuch".to_owned());
        paths
    }

    #[tokio::test(flavor = "current_thread")]
    async fn every_bad_request_is_404_and_shows_no_path() {
        let dir = Folder::new();
        let server = one(&dir).await;
        for path in bad_paths(&dir) {
            let response = get(&server, &path).await;
            assert_eq!(response.status(), 404, "{path}");
            let body = response.text().await.unwrap();
            let root = dir.root.to_string_lossy().into_owned();
            assert!(!body.contains(&root), "{path}: {body}");
            assert!(!body.contains("SECRETBODY"), "{path}");
            assert!(!body.contains("passwd"), "{path}");
        }
        let hidden = get(&server, "/c/explainers").await.text().await.unwrap();
        assert!(!hidden.contains(".hidden"), "{hidden}");
        assert!(!hidden.contains("notes.txt"), "{hidden}");
        assert!(!hidden.contains("big.html"), "{hidden}");
        assert!(!hidden.contains("out.html"), "{hidden}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bad_requests_log_the_result_words_and_count_them() {
        let dir = Folder::new();
        std::fs::write(dir.root.join("secret.html"), b"x").unwrap();
        std::os::unix::fs::symlink(
            dir.root.join("secret.html"),
            dir.pages().join("out-link.html"),
        )
        .unwrap();
        dir.write("big-one.html", &vec![b'a'; 3 * 1024 * 1024]);
        let name = "count-bad";
        let server = serve(vec![settings(name, &dir.pages(), true)], false).await;
        crate::telemetry::register_collection_counters(&[name]);
        let counted = |result: &str| {
            crate::telemetry::test_support::counter_value(
                "harold.web_app.collection_pages",
                &[("collection", name), ("result", result)],
            )
            .unwrap_or(0)
        };
        let before: Vec<u64> = ["refused", "not_found", "too_big"]
            .iter()
            .map(|r| counted(r))
            .collect();
        let unknown = || {
            crate::telemetry::test_support::counter_value(
                "harold.web_app.collection_pages",
                &[("collection", "unknown"), ("result", "not_found")],
            )
            .unwrap_or(0)
        };
        let unknown_before = unknown();
        let logs = CapturedLogs::start();
        for (path, status) in [
            ("/c/count-bad/out-link.html", 404),
            ("/c/count-bad/%2e%2e%2Fsecret.html", 404),
            ("/c/count-bad/missing-page.html", 404),
            ("/c/count-bad/big-one.html", 404),
            ("/c/unknown-secret/anything.html", 404),
            ("/c/unknown-secret", 404),
        ] {
            assert_eq!(get(&server, path).await.status(), status, "{path}");
        }
        let text = logs.text();
        assert_eq!(text.matches("app page shown").count(), 6, "{text}");
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("app page shown"))
            .collect();
        let has = |collection: &str, result: &str| {
            lines
                .iter()
                .filter(|l| {
                    l.contains(&format!("collection=\"{collection}\""))
                        && l.contains(&format!("result=\"{result}\""))
                })
                .count()
        };
        assert_eq!(has(name, "refused"), 2, "{text}");
        assert_eq!(has(name, "not_found"), 1, "{text}");
        assert_eq!(has(name, "too_big"), 1, "{text}");
        assert_eq!(has("unknown", "not_found"), 2, "{text}");
        assert!(
            lines
                .iter()
                .all(|l| l.contains("listener=\"web_app\"") && l.contains("ms="))
        );
        for secret in [
            "out-link",
            "missing-page",
            "big-one",
            "unknown-secret",
            "secret.html",
        ] {
            assert!(!text.contains(secret), "{secret}: {text}");
        }
        let after: Vec<u64> = ["refused", "not_found", "too_big"]
            .iter()
            .map(|r| counted(r))
            .collect();
        assert_eq!(after[0] - before[0], 2);
        assert_eq!(after[1] - before[1], 1);
        assert_eq!(after[2] - before[2], 1);
        // Other tests share the global counter, so check for at least the two requests.
        assert!(unknown() >= unknown_before + 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_good_page_and_a_list_log_ok_and_the_index_logs_nothing() {
        let dir = Folder::new();
        dir.write("a.html", b"<title>A</title>");
        let server = one(&dir).await;
        let logs = CapturedLogs::start();
        for path in [
            "/c/explainers",
            "/c/explainers/a.html",
            "/c/explainers/a.html/raw",
            "/c",
        ] {
            assert_eq!(get(&server, path).await.status(), 200, "{path}");
        }
        let text = logs.text();
        assert_eq!(text.matches("app page shown").count(), 3, "{text}");
        assert_eq!(text.matches("result=\"ok\"").count(), 3, "{text}");
        assert!(!text.contains("a.html"), "{text}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_unreadable_folder_gives_503_and_a_calm_message() {
        let dir = Folder::new();
        let missing = dir.root.join("not-there");
        let server = serve(vec![settings("explainers", &missing, true)], false).await;
        let logs = CapturedLogs::start();
        let response = get(&server, "/c/explainers").await;
        assert_eq!(response.status(), 503);
        let body = response.text().await.unwrap();
        assert!(body.contains("read this folder"), "{body}");
        assert!(!body.contains("not-there"), "{body}");
        assert!(logs.text().contains("result=\"unreadable\""));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_empty_folder_shows_a_calm_message() {
        let dir = Folder::new();
        let server = one(&dir).await;
        let body = get(&server, "/c/explainers").await.text().await.unwrap();
        assert!(body.contains("No pages in this collection"), "{body}");
    }

    fn back_of(body: &str) -> String {
        let at = body.find("class=\"back-button\"").expect("a back control");
        let start = body[..at].rfind("<a ").unwrap();
        let end = body[at..].find("</a>").unwrap() + at;
        body[start..end].to_owned()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn each_screen_header_has_its_back_control() {
        let dir = Folder::new();
        dir.write("a.html", b"x");
        dir.write("n.md", b"# n");
        let server = one(&dir).await;
        let index = back_of(&get(&server, "/c").await.text().await.unwrap());
        assert!(
            index.contains("href=\"/\"") && index.contains("</svg>"),
            "{index}"
        );
        let list = back_of(&get(&server, "/c/explainers").await.text().await.unwrap());
        assert!(
            list.contains("href=\"/c\"") && list.ends_with("Pages"),
            "{list}"
        );
        for page in ["/c/explainers/a.html", "/c/explainers/n.md"] {
            let back = back_of(&get(&server, page).await.text().await.unwrap());
            assert!(
                back.contains("href=\"/c/explainers\"") && back.ends_with("EXPLAINERS"),
                "{page}: {back}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_index_shows_a_badge_with_each_page_count() {
        let dir = Folder::new();
        dir.write("a.html", b"x");
        dir.write("b.md", b"x");
        dir.write("c.txt", b"x");
        let other = Folder::new();
        let server = serve(
            vec![
                settings("explainers", &dir.pages(), true),
                settings("empty", &other.pages(), true),
                settings("broken", &other.root.join("gone"), true),
            ],
            false,
        )
        .await;
        let body = get(&server, "/c").await.text().await.unwrap();
        assert!(
            body.contains(
                "<span class=\"note-title\">EXPLAINERS</span><span class=\"badge\">2</span>"
            ),
            "{body}"
        );
        assert!(body.contains("<span class=\"badge\">0</span>"), "{body}");
        // A count that fails shows no badge.
        assert!(
            body.contains("<span class=\"note-title\">BROKEN</span></a>"),
            "{body}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_collection_that_is_not_listed_is_off_the_index_but_answers() {
        let dir = Folder::new();
        dir.write("a.html", b"<title>Hidden mock</title>");
        let server = serve(
            vec![
                settings("explainers", &dir.pages(), true),
                settings("mocks", &dir.pages(), false),
            ],
            false,
        )
        .await;
        let index = get(&server, "/c").await.text().await.unwrap();
        assert!(index.contains("href=\"/c/explainers\""), "{index}");
        assert!(!index.contains("/c/mocks"), "{index}");
        let list = get(&server, "/c/mocks").await;
        assert_eq!(list.status(), 200);
        assert!(list.text().await.unwrap().contains("Hidden mock"));
        assert_eq!(get(&server, "/c/mocks/a.html").await.status(), 200);
        assert_eq!(get(&server, "/c/mocks/a.html/raw").await.status(), 200);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_index_lists_specs_then_notes_only_when_notes_are_on() {
        let dir = Folder::new();
        let off = serve(vec![settings("explainers", &dir.pages(), true)], false).await;
        let body = get(&off, "/c").await.text().await.unwrap();
        let (c, s) = (
            body.find("/c/explainers").unwrap(),
            body.find("href=\"/specs\"").unwrap(),
        );
        assert!(c < s, "{body}");
        assert!(!body.contains("/notes"), "{body}");
        let on = serve(vec![settings("explainers", &dir.pages(), true)], true).await;
        let body = get(&on, "/c").await.text().await.unwrap();
        let (s, n) = (
            body.find("href=\"/specs\"").unwrap(),
            body.find("href=\"/notes\"").unwrap(),
        );
        assert!(s < n, "{body}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_list_shows_200_newest_of_201() {
        let dir = Folder::new();
        for i in 0..201 {
            let name = format!("p{i:03}.html");
            dir.write(&name, b"x");
            dir.touch(&name, 1_000 + i);
        }
        let server = one(&dir).await;
        let body = get(&server, "/c/explainers").await.text().await.unwrap();
        assert_eq!(body.matches("class=\"note-row\"").count(), 200);
        assert!(body.contains("200 newest of 201"), "{body}");
        assert!(body.contains("p200.html") && !body.contains("p000.html"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_title_with_html_is_escaped_in_the_list() {
        let dir = Folder::new();
        dir.write("t.html", b"<title><img src=x onerror=alert(1)></title>");
        let server = one(&dir).await;
        let body = get(&server, "/c/explainers").await.text().await.unwrap();
        assert!(body.contains("&#60;img src=x"), "{body}");
        assert!(!body.contains("<img"), "{body}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_app_collection_routes_need_the_login_and_the_device() {
        let dir = Folder::new();
        dir.write("a.html", b"<title>A</title>");
        dir.write("n.md", b"# N\n");
        let paths = [
            "/c",
            "/c/explainers",
            "/c/explainers/a.html",
            "/c/explainers/n.md",
            "/c/explainers/a.html/raw",
        ];
        // No login header: refused as `/specs` is.
        let open = one(&dir).await;
        for path in paths {
            let response = reqwest::get(format!("http://{}{path}", open.addr))
                .await
                .unwrap();
            assert_ne!(response.status(), 200, "{path}");
            assert!(
                !response.text().await.unwrap().contains("explainers"),
                "{path}"
            );
        }
        let spec = reqwest::get(format!("http://{}/specs", open.addr))
            .await
            .unwrap()
            .status();
        let none = reqwest::get(format!("http://{}/c", open.addr))
            .await
            .unwrap()
            .status();
        assert_eq!(none, spec);
        // A login but no paired device: refused, with no page.
        let paired = serve_with(
            vec![settings("explainers", &dir.pages(), true)],
            false,
            true,
        )
        .await;
        for path in paths {
            let response = get(&paired, path).await;
            let status = response.status();
            let body = response.text().await.unwrap();
            assert!(
                !body.contains("A</title>") && !body.contains("<h3>N</h3>"),
                "{path}"
            );
            assert!(!body.contains("explainers"), "{path}: {body}");
            if path != "/c" {
                assert_eq!(status, 401, "{path}");
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_index_lists_collections_specs_and_notes() {
        let dir = Folder::new();
        dir.write("a.html", b"x");
        let hidden = Folder::new();
        let both = vec![
            settings("explainers", &dir.pages(), true),
            settings("secret", &hidden.pages(), false),
        ];
        let server = serve(both.clone(), false).await;
        let body = get(&server, "/c").await.text().await.unwrap();
        assert!(body.contains("href=\"/c/explainers\""), "{body}");
        assert!(!body.contains("secret"), "{body}");
        assert!(body.contains("href=\"/specs\""), "{body}");
        assert!(!body.contains("href=\"/notes\""), "{body}");
        let server = serve(both, true).await;
        let body = get(&server, "/c").await.text().await.unwrap();
        let (s, n) = (
            body.find("href=\"/specs\"").unwrap(),
            body.find("href=\"/notes\"").unwrap(),
        );
        assert!(
            body.find("href=\"/c/explainers\"").unwrap() < s && s < n,
            "{body}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_thread_header_is_unchanged() {
        let dir = Folder::new();
        let server = one(&dir).await;
        let body = get(&server, "/").await.text().await.unwrap();
        let header = &body[body.find("id=\"to-todos\"").expect("Todos button")..];
        let header = &header[..header.find("id=\"to-thread\"").expect("thread button")];
        assert!(header.contains("Todos"), "{header}");
        assert!(!header.contains("href=\"/c\""), "{header}");
        assert!(!header.contains("Pages"), "{header}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_encoded_md_name_gets_the_spec_policy() {
        let dir = Folder::new();
        dir.write("n.md", b"# Note title\n");
        let server = one(&dir).await;
        let response = get(&server, "/c/explainers/n%2Emd").await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            header(&response, "content-security-policy"),
            SPEC_CONTENT_SECURITY_POLICY
        );
        assert!(
            response
                .text()
                .await
                .unwrap()
                .contains("<h3>Note title</h3>")
        );
        // A name that does not decode to UTF-8 is not Markdown.
        let response = get(&server, "/c/explainers/n%FF%2Emd").await;
        assert_eq!(
            header(&response, "content-security-policy"),
            COLLECTION_SHELL_CONTENT_SECURITY_POLICY
        );
    }
}
