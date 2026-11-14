use super::*;

fn temp() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("harold-restart-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn the_receipt_path_follows_the_role_the_build_worker_and_the_chief() {
    let folder = std::path::Path::new("/pod");
    let pnem = Agent::Pnem("harold".into());
    assert_eq!(
        receipt_path(folder, &pnem, "20261114-1605"),
        folder.join("restarts/pnem-20261114-1605.md")
    );
    let coordination = Agent::PodCoordination("harold".into());
    assert_eq!(
        receipt_path(folder, &coordination, "20261114-1607"),
        folder.join("restarts/pod_coordination-20261114-1607.md")
    );
    let worker = Agent::BuildWorker {
        product: "harold".into(),
        spec: "decision-tool-a1".into(),
    };
    assert_eq!(
        receipt_path(folder, &worker, "20261114-1610"),
        folder.join("restarts/worker-decision-tool-a1-20261114-1610.md")
    );
    assert_eq!(
        receipt_path(
            std::path::Path::new("/chief"),
            &Agent::Chief,
            "20261114-1602"
        ),
        std::path::Path::new("/chief/.superpowers/pod/restarts/chief-20261114-1602.md")
    );
}

#[test]
fn the_receipt_file_holds_the_text_and_a_second_run_in_one_minute_replaces_it() {
    let folder = temp();
    let path = receipt_path(&folder, &Agent::Pnem("harold".into()), "20261114-1605");
    write_receipt(&path, "text one\n").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "text one\n");
    write_receipt(&path, "text two\n").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "text two\n");
    std::fs::remove_dir_all(&folder).unwrap();
}

#[test]
fn a_spec_id_that_is_not_a_file_name_part_is_refused() {
    let folder = std::path::Path::new("/pod");
    let worker = Agent::BuildWorker {
        product: "h".into(),
        spec: "../x".into(),
    };
    assert_eq!(
        receipt_path(folder, &worker, "20261114-1610"),
        folder.join("restarts/worker-x-20261114-1610.md")
    );
}
