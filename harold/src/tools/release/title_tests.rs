use super::*;

const HEADING: &str =
    "# release-next: Show what each product's next release holds, with one command";
const TITLE: &str = "Show what each product's next release holds, with one command";

#[test]
fn title_of_a_heading_with_the_id_prefix() {
    let markdown = format!("Waiting for your approval.\n\n{HEADING}\n\nBody.\n");
    assert_eq!(spec_title("release-next", &markdown), TITLE);
}

#[test]
fn title_is_empty_without_a_heading() {
    assert_eq!(spec_title("release-next", "Waiting.\n\nBody only.\n"), "");
}

#[test]
fn title_is_empty_when_the_prefix_is_another_id() {
    assert_eq!(spec_title("other-spec", HEADING), "");
}

#[test]
fn title_ignores_a_later_heading() {
    let markdown = format!("# another-id: First\n\n{HEADING}\n");
    assert_eq!(spec_title("release-next", &markdown), "");
}
