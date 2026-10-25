use super::*;

#[test]
fn every_step_only_adds_and_the_numbers_run_from_one() {
    for (index, step) in STEPS.iter().enumerate() {
        assert_eq!(step.number as usize, index + 1);
        assert!(only_adds(step.define), "step {}", step.number);
    }
}

#[test]
fn a_step_that_removes_or_renames_is_refused() {
    assert!(only_adds("define attribute x, value string;"));
    assert!(!only_adds("undefine attribute x;"));
    assert!(!only_adds("define\n redefine entity y;"));
    assert!(!only_adds(
        "define attribute x, value string; rename x to z;"
    ));
    assert!(!only_adds("match $x isa y; delete $x;"));
}

#[test]
fn step_one_defines_the_five_relation_types() {
    for relation in super::super::records::RELATION_TYPES {
        assert!(STEPS[0].define.contains(&format!("relation {relation},")));
    }
}

#[test]
fn only_the_steps_not_in_the_log_are_pending() {
    assert_eq!(pending(&[]).len(), STEPS.len());
    let logged = [SchemaRecord {
        step: 1,
        at_ms: 0,
        define: String::new(),
    }];
    assert!(pending(&logged).is_empty());
}
