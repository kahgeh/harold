//! The graph's schema, as numbered steps in Harold's code. A step is appended to the schema log
//! once, then applied. Steps only add: a step may define a type or an attribute, and may never
//! remove or rename one, because a rebuild replays every step before any data.

use super::records::SchemaRecord;

pub(crate) struct SchemaStep {
    pub number: u32,
    pub define: &'static str,
}

/// Step 1: `vault_note`, `note_link`, the five typed relations and the graph position.
const STEP_1: &str = "define
  attribute path, value string;
  attribute title, value string;
  attribute kind, value string;
  attribute missing, value boolean;
  attribute schema_sequence, value integer;
  attribute data_sequence, value integer;
  attribute position_id, value string;
  relation note_link, relates source, relates target;
  relation relates_to, relates source, relates target;
  relation supersedes, relates source, relates target;
  relation part_of, relates source, relates target;
  relation inspired_by, relates source, relates target;
  relation contradicts, relates source, relates target;
  entity vault_note,
    owns path @key,
    owns title,
    owns kind,
    owns missing,
    plays note_link:source,
    plays note_link:target,
    plays relates_to:source,
    plays relates_to:target,
    plays supersedes:source,
    plays supersedes:target,
    plays part_of:source,
    plays part_of:target,
    plays inspired_by:source,
    plays inspired_by:target,
    plays contradicts:source,
    plays contradicts:target;
  entity graph_position,
    owns position_id @key,
    owns schema_sequence,
    owns data_sequence;
";

pub(crate) const STEPS: &[SchemaStep] = &[SchemaStep {
    number: 1,
    define: STEP_1,
}];

/// The steps the schema log does not hold yet.
pub(crate) fn pending(logged: &[SchemaRecord]) -> Vec<&'static SchemaStep> {
    STEPS
        .iter()
        .filter(|step| !logged.iter().any(|record| record.step == step.number))
        .collect()
}

/// Whether a step's TypeQL removes, renames or changes a type. Such a step is refused.
pub(crate) fn only_adds(define: &str) -> bool {
    let lower = define.to_ascii_lowercase();
    let first = lower.split_whitespace().next();
    first == Some("define")
        && !["undefine", "redefine", "rename", "delete"]
            .iter()
            .any(|word| {
                lower
                    .split(|c: char| !c.is_alphanumeric())
                    .any(|w| w == *word)
            })
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
