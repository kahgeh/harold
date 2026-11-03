use super::*;

fn mv(from: &str, to: &str, at_ms: i64) -> Move {
    Move {
        from_status: from.into(),
        to_status: to.into(),
        at_ms,
    }
}

fn full_path() -> Vec<Move> {
    vec![
        mv("", "idea", 0),
        mv("idea", "discovery", 100),
        mv("discovery", "spec drafted", 300),
        mv("spec drafted", "architect checked", 350),
        mv("architect checked", "waiting for the user", 400),
        mv("waiting for the user", "approved", 1000),
        mv("approved", "building", 1100),
        mv("building", "merged", 2000),
        mv("merged", "released", 2100),
        mv("released", "checked by the user", 5000),
    ]
}

#[test]
fn each_phase_sums_the_time_of_its_statuses() {
    assert_eq!(
        phases(&full_path()),
        Phases {
            discovery: 300,
            spec_and_approval: 700,
            build_and_merge: 1000,
            release_and_check: 3000,
        }
    );
}

#[test]
fn move_back_adds_to_the_phase_again() {
    let moves = [
        mv("", "spec drafted", 0),
        mv("spec drafted", "building", 100),
        mv("building", "spec drafted", 300),
        mv("spec drafted", "building", 350),
        mv("building", "checked by the user", 500),
    ];
    let got = phases(&moves);
    assert_eq!(got.spec_and_approval, 100 + 50);
    assert_eq!(got.build_and_merge, 200 + 150);
}

#[test]
fn imported_record_counts_only_phases_after_import() {
    let moves = [
        mv("imported", "spec drafted", 1000),
        mv("spec drafted", "checked by the user", 1500),
    ];
    assert!(is_imported(&moves));
    assert_eq!(
        phases(&moves),
        Phases {
            spec_and_approval: 500,
            ..Phases::default()
        }
    );
}

#[test]
fn a_new_record_is_not_imported() {
    assert!(!is_imported(&full_path()));
    assert!(!is_imported(&[]));
}

#[test]
fn skipped_phases_are_zero() {
    let moves = [mv("", "idea", 0), mv("idea", "checked by the user", 400)];
    assert_eq!(
        phases(&moves),
        Phases {
            discovery: 400,
            ..Phases::default()
        }
    );
    assert_eq!(phases(&[]), Phases::default());
}

#[test]
fn slices_count_spec_links_only() {
    let links: Vec<String> = ["spec:a", "spec:b", "pr:1", "note:x"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(slices(&links), 2);
}
