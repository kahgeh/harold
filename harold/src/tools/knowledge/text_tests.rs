use super::*;

#[test]
fn a_note_with_front_matter_gives_its_fields() {
    let text = "---\nname: a-note\ndescription: \"Say \\\"hi\\\": now\"\nkind: idea\ncreated: 2026-10-23\nmetadata:\n  type: project\n---\n\nBody with [[other]] and [[refs/x|label]] and [[other]].\n";
    let note = parse_note("a-note", text);
    assert_eq!(note.title, "a-note");
    assert_eq!(note.kind, "idea");
    assert_eq!(note.description, "Say \"hi\": now");
    assert_eq!(note.created, "2026-10-23");
    assert_eq!(note.links, vec!["other".to_string(), "refs/x".to_string()]);
    assert!(note.body.starts_with("Body with"));
}

#[test]
fn a_note_without_front_matter_is_titled_by_its_file_name_and_unknown() {
    let note = parse_note("references/hyperdx", "Just text.\n");
    assert_eq!(note.title, "hyperdx");
    assert_eq!(note.kind, "unknown");
    assert_eq!(note.body, "Just text.\n");
}

#[test]
fn an_unclosed_front_matter_is_text() {
    let note = parse_note("x", "---\nname: y\nno end");
    assert_eq!(note.title, "x");
    assert!(note.body.starts_with("---"));
}

#[test]
fn links_ignore_labels_headings_extensions_and_broken_brackets() {
    assert_eq!(
        links_in("[[a#h]] [[b.md]] [[ c |x]] [[]] [[unclosed\nz]] [[d"),
        vec!["a", "b", "c"]
    );
}

#[test]
fn a_short_note_is_one_passage() {
    assert_eq!(split_passages("one two three\n"), vec!["one two three"]);
}

fn long(words: usize, prefix: &str) -> String {
    (0..words)
        .map(|i| format!("{prefix}{i}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn a_long_note_is_split_at_headings_then_paragraphs_under_400_words() {
    let body = format!(
        "# One\n\n{}\n\n# Two\n\n{}\n\n{}\n\n{}",
        long(300, "a"),
        long(300, "b"),
        long(300, "c"),
        long(50, "d")
    );
    let passages = split_passages(&body);
    assert_eq!(passages.len(), 3, "{passages:?}");
    assert!(passages.iter().all(|p| words(p) <= PASSAGE_WORDS + 2));
    assert!(passages[0].starts_with("# One"));
    assert!(passages[1].starts_with("# Two"));
}

#[test]
fn one_huge_paragraph_is_cut_at_words() {
    let passages = split_passages(&long(900, "w"));
    assert_eq!(passages.len(), 3);
    assert!(passages.iter().all(|p| words(p) <= PASSAGE_WORDS));
}

#[test]
fn a_hash_line_in_a_code_block_is_not_a_heading() {
    let body = format!(
        "{}\n\n```\n# not a heading\n```\n\n{}",
        long(300, "a"),
        long(300, "b")
    );
    let passages = split_passages(&body);
    assert!(
        passages
            .iter()
            .any(|p| p.contains("# not a heading") && p.contains("```"))
    );
}

#[test]
fn prompts_follow_the_models_training() {
    assert_eq!(passage_prompt("T", "p"), "title: T | text: p");
    assert_eq!(query_prompt("q"), "task: search result | query: q");
}

#[test]
fn bonus_words_are_four_letters_or_more_once_each() {
    assert_eq!(
        bonus_words("the Mac is Lost, lost? Q56 buckwal"),
        vec!["lost", "buckwal"]
    );
}

#[test]
fn a_cut_passage_ends_at_a_word() {
    assert_eq!(cut_passage("a  b\nc", 20), "a b c");
    let cut = cut_passage(&"word ".repeat(100), 22);
    assert!(cut.ends_with("word…"), "{cut}");
}
