use std::collections::BTreeMap;

use super::*;
use crate::tools::work::schema::{Mark, Status};
use crate::tools::work::test_support::{decision, record};

#[derive(Default)]
struct Fake {
    specs: BTreeMap<String, SpecFacts>,
    questions: BTreeMap<String, QuestionFact>,
}

impl Facts for Fake {
    fn spec(&self, product: &str, id: &str) -> SpecFacts {
        self.specs
            .get(&format!("{product}/{id}"))
            .copied()
            .unwrap_or(SpecFacts {
                registry_ok: true,
                ..SpecFacts::default()
            })
    }

    fn question(&self, id: &str) -> QuestionFact {
        self.questions
            .get(id)
            .copied()
            .unwrap_or(QuestionFact::Unknown)
    }
}

fn spec_facts(change: impl FnOnce(&mut SpecFacts)) -> SpecFacts {
    let mut facts = SpecFacts {
        exists: true,
        registry_ok: true,
        ..SpecFacts::default()
    };
    change(&mut facts);
    facts
}

fn fake_spec(facts: SpecFacts) -> Fake {
    let mut fake = Fake::default();
    fake.specs.insert("p/s1".into(), facts);
    fake
}

fn linked(name: &str, status: Status, link: &str) -> Record {
    let mut r = record(name);
    r.status = status;
    r.links.push(link.into());
    r
}

fn one(records: &[Record], fake: &Fake) -> Mismatch {
    let found = check(records, fake);
    assert_eq!(found.len(), 1, "{found:?}");
    found.into_iter().next().unwrap()
}

#[test]
fn each_rule_gives_its_line() {
    let mut fake = Fake::default();
    fake.questions.insert("Q1".into(), QuestionFact::Waiting);
    let m = one(&[linked("a", Status::Approved, "question:Q1")], &fake);
    assert_eq!(m.kind, "waiting_question");
    assert_eq!(m.line, "a is approved but question Q1 still waits.");

    let fake = fake_spec(spec_facts(|s| s.merged = true));
    let m = one(&[linked("b", Status::Building, "spec:p/s1")], &fake);
    assert_eq!(m.kind, "merged_status_behind");
    assert_eq!(m.line, "b is building but spec p/s1 is merged.");

    let fake = fake_spec(spec_facts(|s| s.withdrawn = true));
    let m = one(&[linked("c", Status::Merged, "spec:p/s1")], &fake);
    assert_eq!(m.kind, "withdrawn_open");
    assert_eq!(m.line, "c is open but spec p/s1 is withdrawn.");

    let fake = fake_spec(spec_facts(|s| s.approved = true));
    let m = one(&[linked("d", Status::Discovery, "spec:p/s1")], &fake);
    assert_eq!(m.kind, "approved_status_behind");
    assert_eq!(m.line, "d is discovery but spec p/s1 is approved.");

    let mut closed = record("e");
    closed.status = Status::CheckedByTheUser;
    let mut open = decision("x");
    open.mark = Mark::Open;
    closed.decisions.push(decision("y"));
    closed.decisions.push(open);
    let m = one(&[closed], &Fake::default());
    assert_eq!(m.kind, "closed_open_decision");
    assert_eq!(m.line, "e is closed but decision 2 is still open.");

    let m = one(
        &[linked("f", Status::Idea, "spec:p/gone")],
        &Fake::default(),
    );
    assert_eq!(m.kind, "missing_link");
    assert_eq!(m.line, "f links spec p/gone, which does not exist.");
    let m = one(
        &[linked("g", Status::Idea, "question:Q9")],
        &Fake::default(),
    );
    assert_eq!(m.kind, "missing_link");
    assert_eq!(m.line, "g links question Q9, which does not exist.");
}

#[test]
fn clean_set_gives_nothing() {
    let mut fake = fake_spec(spec_facts(|_| {}));
    fake.questions.insert("Q1".into(), QuestionFact::Answered);
    let mut a = linked("a", Status::Approved, "spec:p/s1");
    a.links.push("question:Q1".into());
    a.links.push("note:free text".into());
    assert!(check(&[a, record("b")], &fake).is_empty());
}

#[test]
fn unreadable_registry_gives_no_missing_link() {
    let fake = fake_spec(SpecFacts::default());
    assert!(check(&[linked("a", Status::Idea, "spec:p/s1")], &fake).is_empty());
}

#[test]
fn closed_record_with_withdrawn_spec_is_not_flagged() {
    let fake = fake_spec(spec_facts(|s| s.withdrawn = true));
    let r = linked("a", Status::CheckedByTheUser, "spec:p/s1");
    assert!(check(&[r], &fake).is_empty());
}

#[test]
fn status_ordering_uses_ordinal() {
    // `waiting for the user` is before `approved`: a waiting question is fine.
    let mut fake = Fake::default();
    fake.questions.insert("Q1".into(), QuestionFact::Waiting);
    let r = linked("a", Status::WaitingForTheUser, "question:Q1");
    assert!(check(&[r], &fake).is_empty());
    // `released` is after `approved`: it is flagged.
    let r = linked("a", Status::Released, "question:Q1");
    assert_eq!(one(&[r], &fake).kind, "waiting_question");
    // `merged` is not before `merged`: a merged spec is fine.
    let fake = fake_spec(spec_facts(|s| s.merged = true));
    let r = linked("a", Status::Merged, "spec:p/s1");
    assert!(check(&[r], &fake).is_empty());
}
