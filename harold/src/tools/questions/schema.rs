//! The question, the rules it must meet, and the answer it resolves to.

use harold_api::tools::questions::v1 as pb;
use serde::{Deserialize, Serialize};

const MAX_ID: usize = 32;
/// Text fields must be shorter than this.
const MAX_TEXT: usize = 2000;
const MAX_OPTIONS: usize = 9;
const MAX_STEPS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Choice {
    pub key: String,
    pub label: String,
    pub impact: String,
    pub suggested: bool,
    pub free: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Decision {
    pub ask: String,
    pub options: Vec<Choice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Step {
    pub text: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Question {
    pub id: String,
    pub product: String,
    pub spec: String,
    pub sha: String,
    pub link: String,
    pub intro: String,
    pub decisions: Vec<Decision>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub steps: Vec<Step>,
}

impl Question {
    /// Instructions hold steps; a question holds decisions.
    pub(super) fn is_instructions(&self) -> bool {
        !self.steps.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Kind {
    Answer,
    Revise,
    Reject,
    FreeText,
    Done,
    Ignore,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Picked {
    pub ask: String,
    pub label: String,
    pub free: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Answer {
    pub kind: Kind,
    pub picks: Vec<Picked>,
    pub note: String,
    #[serde(default)]
    pub ticked: u32,
}

fn too_long(text: &str) -> bool {
    text.chars().count() >= MAX_TEXT
}

/// Checks the rules and converts; the error is the name of the first field that breaks one.
pub(super) fn from_proto(q: pb::Question) -> Result<Question, String> {
    let id_ok = !q.id.is_empty()
        && q.id.chars().count() <= MAX_ID
        && q.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !id_ok {
        return Err("id".into());
    }
    if !q.steps.is_empty() {
        return instructions_from_proto(q);
    }
    if q.decisions.is_empty() {
        return Err("decisions".into());
    }
    let required = [
        ("product", &q.product),
        ("spec", &q.spec),
        ("sha", &q.sha),
        ("link", &q.link),
    ];
    for (name, value) in required {
        if value.trim().is_empty() || too_long(value) {
            return Err(name.into());
        }
    }
    if !is_http(&q.link) {
        return Err("link".into());
    }
    if too_long(&q.intro) {
        return Err("intro".into());
    }
    if !q.title.is_empty() {
        return Err("title".into());
    }
    let mut decisions = Vec::with_capacity(q.decisions.len());
    for (i, d) in q.decisions.into_iter().enumerate() {
        decisions.push(decision_from_proto(i + 1, d)?);
    }
    Ok(Question {
        id: q.id,
        product: q.product,
        spec: q.spec,
        sha: q.sha,
        link: q.link,
        intro: q.intro,
        decisions,
        title: String::new(),
        steps: Vec::new(),
    })
}

fn is_http(link: &str) -> bool {
    link.starts_with("https://") || link.starts_with("http://")
}

fn instructions_from_proto(q: pb::Question) -> Result<Question, String> {
    if !q.decisions.is_empty() || q.steps.len() > MAX_STEPS {
        return Err("steps".into());
    }
    for (name, value) in [("product", &q.product), ("title", &q.title)] {
        if value.trim().is_empty() || too_long(value) {
            return Err(name.into());
        }
    }
    for (name, value) in [("spec", &q.spec), ("sha", &q.sha)] {
        if too_long(value) {
            return Err(name.into());
        }
    }
    if !q.link.is_empty() && (too_long(&q.link) || !is_http(&q.link)) {
        return Err("link".into());
    }
    if too_long(&q.intro) {
        return Err("intro".into());
    }
    let mut steps = Vec::with_capacity(q.steps.len());
    for (i, s) in q.steps.into_iter().enumerate() {
        let n = i + 1;
        if s.text.trim().is_empty() || too_long(&s.text) {
            return Err(format!("steps.{n}.text"));
        }
        if too_long(&s.detail) {
            return Err(format!("steps.{n}.detail"));
        }
        steps.push(Step {
            text: s.text,
            detail: s.detail,
        });
    }
    Ok(Question {
        id: q.id,
        product: q.product,
        spec: q.spec,
        sha: q.sha,
        link: q.link,
        intro: q.intro,
        decisions: Vec::new(),
        title: q.title,
        steps,
    })
}

fn decision_from_proto(n: usize, d: pb::Decision) -> Result<Decision, String> {
    if d.ask.trim().is_empty() || too_long(&d.ask) {
        return Err(format!("decisions.{n}.ask"));
    }
    if d.options.is_empty() || d.options.len() > MAX_OPTIONS {
        return Err(format!("decisions.{n}.options"));
    }
    let mut options: Vec<Choice> = Vec::with_capacity(d.options.len());
    for (i, o) in d.options.into_iter().enumerate() {
        let m = i + 1;
        let field = |name: &str| format!("decisions.{n}.options.{m}.{name}");
        if o.key.trim().is_empty() || too_long(&o.key) {
            return Err(field("key"));
        }
        if o.label.trim().is_empty() || too_long(&o.label) {
            return Err(field("label"));
        }
        if too_long(&o.impact) {
            return Err(field("impact"));
        }
        if options.iter().any(|earlier| earlier.key == o.key) {
            return Err(format!("decisions.{n}.key"));
        }
        if o.suggested && options.iter().any(|earlier| earlier.suggested) {
            return Err(format!("decisions.{n}.suggested"));
        }
        options.push(Choice {
            key: o.key,
            label: o.label,
            impact: o.impact,
            suggested: o.suggested,
            free: o.free,
        });
    }
    Ok(Decision {
        ask: d.ask,
        options,
    })
}

pub(super) fn to_proto(q: &Question) -> pb::Question {
    pb::Question {
        id: q.id.clone(),
        product: q.product.clone(),
        spec: q.spec.clone(),
        sha: q.sha.clone(),
        link: q.link.clone(),
        intro: q.intro.clone(),
        decisions: q
            .decisions
            .iter()
            .map(|d| pb::Decision {
                ask: d.ask.clone(),
                options: d
                    .options
                    .iter()
                    .map(|o| pb::DecisionOption {
                        key: o.key.clone(),
                        label: o.label.clone(),
                        impact: o.impact.clone(),
                        suggested: o.suggested,
                        free: o.free,
                    })
                    .collect(),
            })
            .collect(),
        title: q.title.clone(),
        steps: q
            .steps
            .iter()
            .map(|s| pb::Step {
                text: s.text.clone(),
                detail: s.detail.clone(),
            })
            .collect(),
    }
}

/// Resolves an answer for a question or for instructions; the error is the field at fault.
pub(super) fn resolve(
    q: &Question,
    kind: Kind,
    picks: &[pb::Pick],
    note: &str,
) -> Result<Answer, String> {
    if q.is_instructions() {
        resolve_instructions(q, kind, picks, note)
    } else {
        resolve_picks(q, kind, picks, note)
    }
}

/// Instructions accept `Done` or `Ignore` only, with no picks and no note.
pub(super) fn resolve_instructions(
    _q: &Question,
    kind: Kind,
    picks: &[pb::Pick],
    note: &str,
) -> Result<Answer, String> {
    if !matches!(kind, Kind::Done | Kind::Ignore) {
        return Err("kind".into());
    }
    if !picks.is_empty() {
        return Err("picks".into());
    }
    if !note.is_empty() {
        return Err("note".into());
    }
    Ok(Answer {
        kind,
        picks: Vec::new(),
        note: String::new(),
        ticked: 0,
    })
}

/// Matches the picks to the question's decisions. The error is `picks` or `note`.
pub(super) fn resolve_picks(
    q: &Question,
    kind: Kind,
    picks: &[pb::Pick],
    note: &str,
) -> Result<Answer, String> {
    if matches!(kind, Kind::Done | Kind::Ignore) {
        return Err("kind".into());
    }
    let strict = matches!(kind, Kind::Answer | Kind::FreeText);
    if strict && picks.len() != q.decisions.len() {
        return Err("picks".into());
    }
    let mut picked = Vec::with_capacity(picks.len());
    let mut next_decision = 0usize;
    for pick in picks {
        let index = pick.decision as usize;
        let in_order = if strict {
            index == next_decision
        } else {
            index >= next_decision
        };
        let decision = q.decisions.get(index).filter(|_| in_order);
        let option = decision.and_then(|d| d.options.iter().find(|o| o.key == pick.key));
        let (Some(decision), Some(option)) = (decision, option) else {
            return Err("picks".into());
        };
        next_decision = index + 1;
        if option.free {
            if pick.text.trim().is_empty() || too_long(&pick.text) {
                return Err("picks".into());
            }
            picked.push(Picked {
                ask: decision.ask.clone(),
                label: pick.text.clone(),
                free: true,
            });
        } else {
            picked.push(Picked {
                ask: decision.ask.clone(),
                label: option.label.clone(),
                free: false,
            });
        }
    }
    match kind {
        Kind::Answer => {
            if picked.iter().any(|p| p.free) {
                return Err("picks".into());
            }
        }
        Kind::FreeText => {
            if !picked.iter().any(|p| p.free) {
                return Err("picks".into());
            }
        }
        Kind::Done | Kind::Ignore => return Err("kind".into()),
        Kind::Revise | Kind::Reject => {
            if note.trim().is_empty() || too_long(note) {
                return Err("note".into());
            }
        }
    }
    if too_long(note) {
        return Err("note".into());
    }
    Ok(Answer {
        kind,
        picks: picked,
        note: note.to_string(),
        ticked: 0,
    })
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
