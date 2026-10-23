use std::os::unix::fs::PermissionsExt;

use super::super::test_support::TempFolder;
use super::*;

fn file_with_mode(dir: &Path, name: &str, mode: u32, content: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

#[test]
fn a_private_file_outside_the_vault_and_any_checkout_is_accepted() {
    let secrets = TempFolder::new();
    let vault = TempFolder::new();
    let file = file_with_mode(&secrets.0, "key", 0o600, "k");
    assert_eq!(check_secret_file(&file, &vault.0), Ok(()));
}

#[test]
fn a_file_others_can_read_is_refused() {
    let secrets = TempFolder::new();
    let vault = TempFolder::new();
    for mode in [0o640, 0o604, 0o644] {
        let file = file_with_mode(&secrets.0, "key", mode, "k");
        assert_eq!(
            check_secret_file(&file, &vault.0),
            Err(Refusal::ReadableByOthers)
        );
    }
}

#[test]
fn a_missing_file_is_refused() {
    let vault = TempFolder::new();
    assert_eq!(
        check_secret_file(Path::new("/no/such/file"), &vault.0),
        Err(Refusal::Unreadable)
    );
}

#[test]
fn a_file_inside_the_vault_is_refused() {
    let vault = TempFolder::new();
    let file = file_with_mode(&vault.0, "key", 0o600, "k");
    assert_eq!(
        check_secret_file(&file, &vault.0),
        Err(Refusal::InsideVault)
    );
}

#[test]
fn a_file_inside_a_git_checkout_is_refused() {
    let checkout = TempFolder::new();
    std::fs::create_dir(checkout.0.join(".git")).unwrap();
    let vault = TempFolder::new();
    let file = file_with_mode(&checkout.0, "key", 0o600, "k");
    assert_eq!(
        check_secret_file(&file, &vault.0),
        Err(Refusal::InsideGitCheckout)
    );
}

#[test]
fn credentials_need_both_lines() {
    let dir = TempFolder::new();
    let both = file_with_mode(&dir.0, "a", 0o600, "access_key=AK\nsecret_key = SK\n");
    assert_eq!(read_credentials(&both), Some(("AK".into(), "SK".into())));
    let one = file_with_mode(&dir.0, "b", 0o600, "access_key=AK\n");
    assert_eq!(read_credentials(&one), None);
}
