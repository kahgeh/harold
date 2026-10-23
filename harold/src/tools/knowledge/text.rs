//! Reading a vault note: its front matter, its `[[links]]` and its passages.

/// The most words in one passage.
const PASSAGE_WORDS: usize = 400;

/// What Harold keeps of one vault note's file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedNote {
    /// The path under the vault without `.md`.
    pub name: String,
    pub title: String,
    pub kind: String,
    pub description: String,
    pub created: String,
    /// The targets of the `[[links]]`, as written, once each.
    pub links: Vec<String>,
    /// The text after the front matter.
    pub body: String,
}

/// Reads a vault note. Without front matter the title is the file name and the kind
/// `unknown`.
pub(crate) fn parse_note(name: &str, text: &str) -> ParsedNote {
    let (front, body) = split_front_matter(text);
    let field = |key: &str| front.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    let file_name = name.rsplit('/').next().unwrap_or(name);
    ParsedNote {
        name: name.to_string(),
        title: field("name")
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| file_name.to_string()),
        kind: field("kind")
            .filter(|kind| !kind.is_empty())
            .unwrap_or_else(|| "unknown".into()),
        description: field("description").unwrap_or_default(),
        created: field("created").unwrap_or_default(),
        links: links_in(text),
        body: body.to_string(),
    }
}

/// The top-level `key: value` lines of a leading `---` block, and the text after it.
fn split_front_matter(text: &str) -> (Vec<(String, String)>, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (Vec::new(), text);
    };
    let Some(end) = rest
        .find("\n---\n")
        .map(|at| (at, at + 5))
        .or_else(|| rest.strip_suffix("\n---").map(|r| (r.len(), rest.len())))
    else {
        return (Vec::new(), text);
    };
    let fields = rest[..end.0]
        .lines()
        .filter(|line| !line.starts_with([' ', '\t']))
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), unquote(value.trim())))
        .collect();
    (fields, rest[end.1..].trim_start_matches('\n'))
}

fn unquote(value: &str) -> String {
    let inner = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .map(|v| v.replace("\\\"", "\"").replace("\\\\", "\\"));
    inner
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
                .map(|v| v.replace("''", "'"))
        })
        .unwrap_or_else(|| value.to_string())
}

/// The targets of `[[target]]`, `[[target|label]]` and `[[target#heading]]`, once each, in
/// order, with a trailing `.md` removed.
pub(crate) fn links_in(text: &str) -> Vec<String> {
    let mut links: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("[[") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("]]") else { break };
        let inner = &after[..close];
        rest = &after[close + 2..];
        if inner.contains('\n') {
            continue;
        }
        let target = inner
            .split(['|', '#'])
            .next()
            .unwrap_or("")
            .trim()
            .trim_end_matches(".md");
        if !target.is_empty() && !links.iter().any(|known| known == target) {
            links.push(target.to_string());
        }
    }
    links
}

/// A note of at most 400 words is one passage. A longer one is split at its headings, then
/// at paragraphs, into passages of at most 400 words; one paragraph longer than that is cut
/// at a word.
pub(crate) fn split_passages(body: &str) -> Vec<String> {
    let body = body.trim();
    if body.split_whitespace().count() <= PASSAGE_WORDS {
        return vec![body.to_string()];
    }
    let mut passages = Vec::new();
    for section in sections(body) {
        if words(&section) <= PASSAGE_WORDS {
            passages.push(section);
            continue;
        }
        let mut current: Vec<String> = Vec::new();
        let mut count = 0;
        for paragraph in section
            .split("\n\n")
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            let size = words(paragraph);
            if size > PASSAGE_WORDS {
                flush(&mut current, &mut count, &mut passages);
                passages.extend(cut_at_words(paragraph));
                continue;
            }
            if count + size > PASSAGE_WORDS {
                flush(&mut current, &mut count, &mut passages);
            }
            current.push(paragraph.to_string());
            count += size;
        }
        flush(&mut current, &mut count, &mut passages);
    }
    passages
}

fn words(text: &str) -> usize {
    text.split_whitespace().count()
}

fn flush(current: &mut Vec<String>, count: &mut usize, passages: &mut Vec<String>) {
    if !current.is_empty() {
        passages.push(current.join("\n\n"));
        current.clear();
    }
    *count = 0;
}

fn cut_at_words(paragraph: &str) -> Vec<String> {
    let all: Vec<&str> = paragraph.split_whitespace().collect();
    all.chunks(PASSAGE_WORDS)
        .map(|chunk| chunk.join(" "))
        .collect()
}

/// The body cut before each heading line, empty pieces dropped. A line in a fenced code
/// block is not a heading.
fn sections(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut fenced = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        }
        let heading =
            !fenced && line.starts_with('#') && line.trim_start_matches('#').starts_with(' ');
        if heading && !current.trim().is_empty() {
            out.push(current.trim().to_string());
            current.clear();
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

/// What the model embeds for a passage (EmbeddingGemma's document prompt).
pub(crate) fn passage_prompt(title: &str, passage: &str) -> String {
    format!("title: {title} | text: {passage}")
}

/// What the model embeds for a search (EmbeddingGemma's query prompt).
pub(crate) fn query_prompt(query: &str) -> String {
    format!("task: search result | query: {query}")
}

/// The words of a query that earn the word bonus: 4 letters or more, lower case, once each.
pub(crate) fn bonus_words(query: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for word in query.split(|c: char| !c.is_alphanumeric()) {
        let word = word.to_lowercase();
        if word.chars().count() >= 4 && !found.contains(&word) {
            found.push(word);
        }
    }
    found
}

/// Cuts text to about `max` characters at a word, adding an ellipsis when it cut.
pub(crate) fn cut_passage(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{cut}…")
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;
