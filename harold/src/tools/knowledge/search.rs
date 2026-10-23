//! Searching the index: closest passages, the best passage per vault note, the word bonus,
//! related notes and suggested links.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tracing::Instrument;

use super::embed::EmbedError;
use super::index::{Failure, Index, blocking};
use super::store::{NEAREST, Nearest, NoteRow, StoreError};
use super::text::{bonus_words, cut_passage, query_prompt};

/// The most vault notes any list holds.
pub(crate) const MOST: usize = 10;
/// The score from which a related note is suggested as a link.
pub(crate) const SUGGEST_FROM: f64 = 0.60;
/// The most suggested links.
const SUGGEST_MOST: usize = 5;
/// How many passages related notes look at.
const RELATED_PASSAGES: usize = 400;
/// The bonus for a query word in a vault note's title or description, and in the passage.
const BONUS_TITLE: f64 = 0.05;
const BONUS_PASSAGE: f64 = 0.02;
/// How long the matching passage is shown.
const PASSAGE_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Hit {
    pub note: NoteRow,
    pub passage: String,
    pub score: f64,
    pub linked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchError {
    /// The embedding worker could not answer; the word says why.
    Resting(&'static str),
    NotFound,
    Store(StoreError),
}

impl From<Failure> for SearchError {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::BucketUnreachable => SearchError::Store(StoreError::BucketUnreachable),
            _ => SearchError::Store(StoreError::StoreFailed),
        }
    }
}

/// The best passage per vault note, with the word bonus, best score first.
pub(crate) fn rank(
    nearest: Vec<Nearest>,
    notes: &HashMap<String, NoteRow>,
    query: &str,
    limit: usize,
) -> Vec<Hit> {
    let words = bonus_words(query);
    let mut best: HashMap<String, Nearest> = HashMap::new();
    for passage in nearest {
        match best.get(&passage.note) {
            Some(held) if held.similarity >= passage.similarity => {}
            _ => {
                best.insert(passage.note.clone(), passage);
            }
        }
    }
    let mut hits: Vec<Hit> = best
        .into_values()
        .filter_map(|passage| {
            let note = notes.get(&passage.note)?;
            let head = format!("{} {}", note.title, note.description).to_lowercase();
            let body = passage.text.to_lowercase();
            let bonus: f64 = words
                .iter()
                .map(|word| {
                    let mut b = 0.0;
                    if head.contains(word.as_str()) {
                        b += BONUS_TITLE;
                    }
                    if body.contains(word.as_str()) {
                        b += BONUS_PASSAGE;
                    }
                    b
                })
                .sum();
            Some(Hit {
                note: note.clone(),
                passage: cut_passage(&passage.text, PASSAGE_CHARS),
                score: passage.similarity + bonus,
                linked: false,
            })
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.note.name.cmp(&b.note.name))
    });
    hits.truncate(limit);
    hits
}

/// Whether `links` (as written in a vault note) reach the vault note `name`: by its name, or
/// by its file name alone.
pub(crate) fn links_to(links: &[String], name: &str) -> bool {
    let file = name.rsplit('/').next().unwrap_or(name);
    links.iter().any(|link| link == name || link == file)
}

pub(crate) fn clamp(limit: u32) -> usize {
    match limit {
        0 => MOST,
        n => (n as usize).min(MOST),
    }
}

/// What the gRPC service, and so every client, asks of the index.
pub(crate) struct Knowledge {
    pub(crate) index: Arc<Index>,
}

impl Knowledge {
    pub(crate) fn new(index: Arc<Index>) -> Self {
        Self { index }
    }

    pub(crate) async fn recent(&self, limit: usize) -> Result<Vec<NoteRow>, SearchError> {
        Ok(blocking(&self.index.store, move |s| s.recent(limit)).await?)
    }

    pub(crate) async fn show(&self, name: &str) -> Result<NoteRow, SearchError> {
        let wanted = name.to_string();
        blocking(&self.index.store, move |s| s.note(&wanted))
            .await?
            .ok_or(SearchError::NotFound)
    }

    /// The closest vault notes to `query`, at most `limit`. A query under two characters
    /// finds nothing. `source` is `chief`, `app` or `dash`, and goes only to the span.
    pub(crate) async fn search(
        &self,
        query: &str,
        limit: usize,
        source: &str,
    ) -> Result<Vec<Hit>, SearchError> {
        let span = tracing::info_span!(
            target: crate::telemetry::TRACE_TARGET,
            "harold.knowledge.search",
            source = %source,
            results = tracing::field::Empty,
            embed_ms = tracing::field::Empty,
            search_ms = tracing::field::Empty,
        );
        let recorder = span.clone();
        async move {
            if query.trim().chars().count() < 2 {
                recorder.record("results", 0_u64);
                return Ok(Vec::new());
            }
            let started = Instant::now();
            let vectors = self
                .index
                .embedder
                .embed(vec![query_prompt(query.trim())])
                .await
                .map_err(|error| match error {
                    EmbedError::Unavailable(reason) => SearchError::Resting(reason),
                    EmbedError::Failed => SearchError::Resting("failed"),
                })?;
            let embed_ms = started.elapsed().as_millis() as u64;
            let vector = vectors.into_iter().next().unwrap_or_default();
            let started = Instant::now();
            let nearest = blocking(&self.index.store, move |s| s.nearest(&vector, NEAREST)).await?;
            let names: Vec<String> = nearest.iter().map(|n| n.note.clone()).collect();
            let notes = blocking(&self.index.store, move |s| s.notes(&names)).await?;
            let hits = rank(nearest, &notes, query, limit.min(MOST));
            recorder.record("embed_ms", embed_ms);
            recorder.record("search_ms", started.elapsed().as_millis() as u64);
            recorder.record("results", hits.len() as u64);
            Ok(hits)
        }
        .instrument(span)
        .await
    }

    /// The vault notes closest to `name`'s own passages, `name` left out, each marked linked
    /// or not by `name`'s links.
    pub(crate) async fn related(&self, name: &str, limit: usize) -> Result<Vec<Hit>, SearchError> {
        let note = self.show(name).await?;
        let asked = name.to_string();
        let nearest = blocking(&self.index.store, move |s| {
            s.nearest_to_note(&asked, RELATED_PASSAGES)
        })
        .await?;
        let names: Vec<String> = nearest.iter().map(|n| n.note.clone()).collect();
        let notes = blocking(&self.index.store, move |s| s.notes(&names)).await?;
        let mut hits = rank(nearest, &notes, "", limit.min(MOST));
        for hit in &mut hits {
            hit.linked = links_to(&note.links, &hit.note.name);
        }
        Ok(hits)
    }

    /// Related notes scoring 0.60 or more that `name` does not link to yet, at most 5.
    pub(crate) async fn suggest_links(&self, name: &str) -> Result<Vec<Hit>, SearchError> {
        let mut hits = self.related(name, MOST).await?;
        hits.retain(|hit| !hit.linked && hit.score >= SUGGEST_FROM);
        hits.truncate(SUGGEST_MOST);
        Ok(hits)
    }
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
