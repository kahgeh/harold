use super::*;
use crate::outbound::fake::CapturedLogs;
use crate::tools::job::test_support::Temp;

const SECOND: i64 = 1_000;

fn settings(folder: &std::path::Path) -> JobSettings {
    JobSettings {
        keep_awake_folder: folder.display().to_string(),
        keep_awake_every_s: 300,
    }
}

fn awake(dir: &Temp) -> KeepAwake {
    KeepAwake::new(&settings(&dir.0)).expect("a folder is set")
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_string()).collect()
}

fn content(dir: &Temp, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.0.join(format!("harold-job-{name}"))).ok()
}

#[test]
fn an_empty_setting_writes_nothing() {
    assert!(KeepAwake::new(&JobSettings::default()).is_none());
}

#[test]
fn a_running_job_writes_its_file_at_once_and_again_after_the_interval() {
    let dir = Temp::new();
    let awake = awake(&dir);
    awake.tick(&names(&["cal"]), 1_000 * SECOND);
    assert_eq!(content(&dir, "cal").as_deref(), Some("1000000"));
    assert!(awake.has_file("cal"));
    awake.tick(&names(&["cal"]), 1_299 * SECOND);
    assert_eq!(content(&dir, "cal").as_deref(), Some("1000000"), "not due");
    awake.tick(&names(&["cal"]), 1_300 * SECOND);
    assert_eq!(content(&dir, "cal").as_deref(), Some("1300000"));
}

#[test]
fn an_end_or_a_remove_deletes_the_file() {
    let dir = Temp::new();
    let awake = awake(&dir);
    awake.tick(&names(&["a", "b"]), 0);
    awake.tick(&names(&["b"]), 15 * SECOND);
    assert_eq!(content(&dir, "a"), None, "a ended");
    assert!(!awake.has_file("a"));
    awake.clear("b");
    assert_eq!(content(&dir, "b"), None, "b was removed");
}

#[test]
fn start_deletes_the_files_of_no_running_job_and_leaves_everything_else() {
    let dir = Temp::new();
    for file in ["harold-job-old", "harold-job-run", "idle-activity.json"] {
        std::fs::write(dir.0.join(file), "x").unwrap();
    }
    std::fs::create_dir(dir.0.join("harold-job-dir")).unwrap();
    let target = std::env::temp_dir().join(format!("harold-target-{}", uuid::Uuid::new_v4()));
    std::fs::write(&target, "keep me").unwrap();
    std::os::unix::fs::symlink(&target, dir.0.join("harold-job-link")).unwrap();
    awake(&dir).sweep(&names(&["run"]));
    assert!(!dir.0.join("harold-job-old").exists());
    assert!(dir.0.join("harold-job-run").exists());
    assert!(dir.0.join("idle-activity.json").exists());
    assert!(dir.0.join("harold-job-dir").is_dir());
    assert!(
        dir.0.join("harold-job-link").symlink_metadata().is_err(),
        "the link is gone"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me");
    let _ = std::fs::remove_file(target);
}

#[test]
fn the_first_file_logs_once_and_a_rewrite_logs_nothing() {
    let logs = CapturedLogs::start();
    let dir = Temp::new();
    let awake = awake(&dir);
    awake.tick(&names(&["cal"]), 0);
    awake.tick(&names(&["cal"]), 300 * SECOND);
    let text = logs.text();
    assert_eq!(text.matches("job keeps machine awake").count(), 1, "{text}");
    assert!(text.contains("job.name=cal"), "{text}");
}

#[test]
fn a_failed_write_logs_once_for_each_job_with_an_error_kind_and_no_path() {
    let logs = CapturedLogs::start();
    let dir = Temp::new();
    let folder = dir.0.join("agent-activity");
    std::fs::create_dir(&folder).unwrap();
    let mut permissions = std::fs::metadata(&folder).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o555);
    std::fs::set_permissions(&folder, permissions).unwrap();
    let awake = KeepAwake::new(&JobSettings {
        keep_awake_folder: "~/.local/state/agent-activity".into(),
        keep_awake_every_s: 300,
    })
    .unwrap();
    // Point the real path at the read-only temp folder. The log shows the setting text.
    let awake = KeepAwake {
        folder: folder.clone(),
        ..awake
    };
    awake.tick(&names(&["cal"]), 0);
    awake.tick(&names(&["cal"]), 400 * SECOND);
    let text = logs.text();
    let failed: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("job keep awake failed"))
        .collect();
    // A user who runs the test as root can write: the folder check then proves nothing.
    if std::fs::write(folder.join("probe"), "x").is_ok() {
        return;
    }
    assert_eq!(failed.len(), 1, "{text}");
    assert!(
        failed[0].contains("reason=permission_denied"),
        "{}",
        failed[0]
    );
    assert!(failed[0].contains("job.name=cal"), "{}", failed[0]);
    assert!(
        !text.contains(&dir.0.display().to_string()),
        "no path in a log: {text}"
    );
    assert!(!text.contains("Permission denied"), "{text}");
    // A second job fails and logs on its own.
    awake.tick(&names(&["cal", "two"]), 500 * SECOND);
    assert_eq!(logs.text().matches("job keep awake failed").count(), 2);
}

#[test]
fn a_folder_that_is_not_there_gives_not_found() {
    let logs = CapturedLogs::start();
    let dir = Temp::new();
    let awake = KeepAwake::new(&settings(&dir.0.join("missing"))).unwrap();
    awake.tick(&names(&["cal"]), 0);
    let text = logs.text();
    assert!(text.contains("reason=not_found"), "{text}");
    assert!(!awake.has_file("cal"));
}
