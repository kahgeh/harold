use std::os::unix::fs::PermissionsExt;

use super::*;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("harold-pairing-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn secrets_equal_compares_by_value() {
    assert!(secrets_equal("abc", "abc"));
    assert!(!secrets_equal("abc", "abd"));
    assert!(!secrets_equal("abc", "abcd"));
    assert!(!secrets_equal("", "a"));
    assert!(secrets_equal("", ""));
}

#[test]
fn written_secret_has_mode_0600() {
    let dir = temp_dir("mode");
    let path = dir.join("slaves").join("omarchy.secret");
    write_secret(&path, "s3cret").unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(read_secret(&path).as_deref(), Some("s3cret"));
    // A second write replaces the secret and keeps the mode.
    write_secret(&path, "other").unwrap();
    assert_eq!(read_secret(&path).as_deref(), Some("other"));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn read_secret_is_none_for_a_missing_or_empty_file() {
    let dir = temp_dir("read");
    assert_eq!(read_secret(&dir.join("none.secret")), None);
    std::fs::write(dir.join("empty.secret"), "\n").unwrap();
    assert_eq!(read_secret(&dir.join("empty.secret")), None);
}

#[test]
fn new_secret_is_64_hex_and_differs_each_call() {
    let first = new_secret();
    let second = new_secret();
    assert_eq!(first.len(), 64);
    assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(first, second);
}

#[test]
fn secret_paths_sit_in_the_data_folder() {
    let data = Path::new("/data");
    assert_eq!(
        master_secret_path(data, "omarchy"),
        Path::new("/data/slaves/omarchy.secret")
    );
    assert_eq!(slave_secret_path(data), Path::new("/data/master.secret"));
}

#[test]
fn pair_then_unpair_removes_the_file() {
    let data = temp_dir("pair");
    let names = vec!["omarchy".to_string()];
    let secret = pair_slave(&data, &names, "omarchy").unwrap();
    assert_eq!(secret.len(), 64);
    let path = master_secret_path(&data, "omarchy");
    assert_eq!(read_secret(&path).as_deref(), Some(secret.as_str()));
    unpair_slave(&data, "omarchy").unwrap();
    assert!(!path.exists());
    // A second unpair is not an error.
    unpair_slave(&data, "omarchy").unwrap();
}

#[test]
fn pair_refuses_a_name_that_is_not_listed_or_not_safe() {
    let data = temp_dir("refuse");
    let names = vec!["omarchy".to_string()];
    assert!(pair_slave(&data, &names, "other").is_err());
    assert!(pair_slave(&data, &["..".to_string()], "..").is_err());
    assert!(unpair_slave(&data, "../x").is_err());
    assert!(!data.join("slaves").exists());
}

#[test]
fn the_pair_result_text_is_the_secret_only_for_pair() {
    let data = temp_dir("text");
    let names = vec!["omarchy".to_string()];
    let text = run_slaves(
        &data,
        &names,
        &SlavesCommand::Pair {
            name: "omarchy".into(),
        },
    )
    .unwrap();
    assert!(text.contains(&read_secret(&master_secret_path(&data, "omarchy")).unwrap()));
    let text = run_slaves(
        &data,
        &names,
        &SlavesCommand::Unpair {
            name: "omarchy".into(),
        },
    )
    .unwrap();
    assert!(!text.is_empty());
}

#[test]
fn master_pair_writes_the_secret_and_the_address() {
    let data = temp_dir("master");
    let local = data.join("local.toml");
    std::fs::write(&local, "machine_name = \"omarchy\"\n\n[master]\naddress = \"old:1\"\nextra = 1\n\n[store]\npath = \"x\"\n").unwrap();
    let secret = "a".repeat(64);
    let text = run_master(&data, &local, &secret, "mac.tail.ts.net:4333").unwrap();
    assert!(!text.contains(&secret));
    assert_eq!(
        read_secret(&slave_secret_path(&data)).as_deref(),
        Some(secret.as_str())
    );
    let written = std::fs::read_to_string(&local).unwrap();
    assert!(written.contains("machine_name = \"omarchy\""));
    assert!(written.contains("[store]\npath = \"x\""));
    assert!(written.contains("[master]\naddress = \"mac.tail.ts.net:4333\""));
    assert!(!written.contains("old:1"));
    assert_eq!(written.matches("[master]").count(), 1);
}

#[test]
fn master_pair_makes_local_toml_when_absent_and_refuses_a_bad_input() {
    let data = temp_dir("master-new");
    let local = data.join("local.toml");
    let secret = "b".repeat(64);
    run_master(&data, &local, &secret, "host:4333").unwrap();
    assert_eq!(
        std::fs::read_to_string(&local).unwrap(),
        "[master]\naddress = \"host:4333\"\n"
    );
    for bad in ["", "nohost", "host:", "ho\"st:1", "a b:1", "host:1\n[x]"] {
        assert!(run_master(&data, &local, &secret, bad).is_err(), "{bad}");
    }
    assert!(run_master(&data, &local, "short", "host:4333").is_err());
}
