use super::*;

fn row(name: &str, title: &str, description: &str) -> NoteRow {
    NoteRow {
        name: name.into(),
        path: format!("{name}.md"),
        title: title.into(),
        kind: "idea".into(),
        description: description.into(),
        created: String::new(),
        links: Vec::new(),
        hash: String::new(),
        changed_at_ms: 0,
        body: String::new(),
    }
}

fn near(note: &str, text: &str, similarity: f64) -> Nearest {
    Nearest {
        note: note.into(),
        text: text.into(),
        similarity,
    }
}

fn notes() -> HashMap<String, NoteRow> {
    [
        row("a", "buckwal", "keeps data safe"),
        row("b", "other", "nothing"),
    ]
    .into_iter()
    .map(|n| (n.name.clone(), n))
    .collect()
}

#[test]
fn only_the_best_passage_of_each_note_counts() {
    let hits = rank(
        vec![
            near("a", "weak", 0.2),
            near("a", "strong", 0.7),
            near("b", "mid", 0.5),
        ],
        &notes(),
        "",
        10,
    );
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].note.name, "a");
    assert_eq!(hits[0].passage, "strong");
    assert!((hits[0].score - 0.7).abs() < 1e-9);
}

#[test]
fn a_query_word_in_the_title_or_description_adds_five_hundredths_and_in_the_passage_two() {
    let hits = rank(
        vec![near("a", "the buckwal text", 0.5)],
        &notes(),
        "buckwal",
        10,
    );
    assert!(
        (hits[0].score - (0.5 + 0.05 + 0.02)).abs() < 1e-9,
        "{hits:?}"
    );
    let hits = rank(vec![near("a", "plain", 0.5)], &notes(), "safe", 10);
    assert!((hits[0].score - 0.55).abs() < 1e-9);
    let hits = rank(vec![near("b", "plain", 0.5)], &notes(), "buck", 10);
    assert!((hits[0].score - 0.5).abs() < 1e-9);
}

#[test]
fn words_under_four_letters_earn_nothing() {
    let hits = rank(vec![near("a", "buckwal", 0.5)], &notes(), "the buc", 10);
    assert!((hits[0].score - 0.5).abs() < 1e-9);
}

#[test]
fn the_bonus_can_reorder_and_the_list_is_cut_to_the_limit() {
    let hits = rank(
        vec![near("a", "x", 0.40), near("b", "y", 0.42)],
        &notes(),
        "buckwal",
        1,
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].note.name, "a");
}

#[test]
fn a_passage_of_a_note_that_is_gone_is_skipped() {
    let hits = rank(vec![near("gone", "x", 0.9)], &notes(), "", 10);
    assert!(hits.is_empty());
}

#[test]
fn a_link_reaches_a_note_by_name_or_by_file_name() {
    let links = vec!["hyperdx".to_string(), "plain".to_string()];
    assert!(links_to(&links, "references/hyperdx"));
    assert!(links_to(&links, "plain"));
    assert!(!links_to(&links, "references/other"));
}

#[test]
fn limits_default_to_ten_and_never_pass_ten() {
    assert_eq!(clamp(0), 10);
    assert_eq!(clamp(3), 3);
    assert_eq!(clamp(50), 10);
}
