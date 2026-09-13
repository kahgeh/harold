//! Ephemeral submitted-block identity; never retains capture text or candidate strings.
use std::time::{Duration, Instant};

use super::super::domain::ObservedAgentState;
use super::super::screen::PromptScan;

const RETRY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Default)]
pub(super) struct PromptAcquisitionCheckpoint {
    fingerprints: Option<Vec<[u8; 32]>>,
    last_attempt: Option<Instant>,
    visible_state: Option<ObservedAgentState>,
    busy_needs_idle_retry: bool,
    source_append_pending: bool,
}

impl PromptAcquisitionCheckpoint {
    pub(super) fn baseline_due(&self, now: Instant) -> bool {
        self.fingerprints.is_none() && self.retry_due(now)
    }

    fn retry_due(&self, now: Instant) -> bool {
        self.last_attempt
            .is_none_or(|last| now.duration_since(last) >= RETRY_INTERVAL)
    }

    pub(super) fn should_scan(&self, state: Option<ObservedAgentState>, now: Instant) -> bool {
        if self.fingerprints.is_none() {
            return self.baseline_due(now);
        }
        let busy_edge = state == Some(ObservedAgentState::Busy)
            && self.visible_state != Some(ObservedAgentState::Busy);
        let idle_retry = state == Some(ObservedAgentState::Idle)
            && self.visible_state == Some(ObservedAgentState::Busy)
            && self.busy_needs_idle_retry;
        busy_edge
            || idle_retry
            || (self.source_append_pending && self.retry_due(now))
            || (state.or(self.visible_state) == Some(ObservedAgentState::Busy)
                && self.retry_due(now))
    }

    pub(super) fn attempted(&mut self, now: Instant) {
        self.last_attempt = Some(now);
    }

    pub(super) fn source_append_failed(&mut self, state: Option<ObservedAgentState>) {
        self.source_append_pending = true;
        self.capture_failed(state);
        self.observe_state(state);
    }

    pub(super) fn capture_failed(&mut self, state: Option<ObservedAgentState>) {
        if state.or(self.visible_state) == Some(ObservedAgentState::Busy) {
            self.busy_needs_idle_retry = true;
        }
    }

    pub(super) fn observe_state(&mut self, state: Option<ObservedAgentState>) {
        if let Some(state) = state {
            self.visible_state = Some(state);
        }
    }

    /// Called on a tentative clone. The caller commits this only after any source event appends.
    pub(super) fn acquire(
        &mut self,
        scan: PromptScan,
        state: Option<ObservedAgentState>,
    ) -> Option<String> {
        let current: Vec<_> = scan.blocks.iter().map(|block| block.fingerprint).collect();
        let candidate = self
            .fingerprints
            .as_deref()
            .and_then(|previous| new_blocks_start(previous, &current))
            .and_then(|start| {
                scan.blocks
                    .into_iter()
                    .skip(start)
                    .rev()
                    .find_map(|block| block.candidate)
            });
        self.fingerprints = Some(current);
        self.source_append_pending = false;
        self.busy_needs_idle_retry =
            state.or(self.visible_state) == Some(ObservedAgentState::Busy) && candidate.is_none();
        candidate
    }
}

fn new_blocks_start(previous: &[[u8; 32]], current: &[[u8; 32]]) -> Option<usize> {
    if previous.is_empty() {
        return Some(0);
    }
    if current.is_empty() {
        return None;
    }
    // Prefix-function alignment bounds work linearly even for thousands of repeated prompts.
    let mut prefixes = vec![0; current.len()];
    for index in 1..current.len() {
        let mut matched = prefixes[index - 1];
        while matched > 0 && current[index] != current[matched] {
            matched = prefixes[matched - 1];
        }
        if current[index] == current[matched] {
            matched += 1;
        }
        prefixes[index] = matched;
    }
    let mut matched = 0;
    for fingerprint in previous {
        while matched > 0 && (matched == current.len() || *fingerprint != current[matched]) {
            matched = prefixes[matched - 1];
        }
        if *fingerprint == current[matched] {
            matched += 1;
        }
    }
    (matched > 0).then_some(matched)
}

#[cfg(test)]
mod tests {
    use super::super::super::screen::PromptBlock;
    use super::*;

    fn scan(ids: &[u8]) -> PromptScan {
        PromptScan {
            blocks: ids
                .iter()
                .map(|id| PromptBlock {
                    fingerprint: [*id; 32],
                    candidate: Some(format!("task {id}")),
                })
                .collect(),
        }
    }

    #[test]
    fn baseline_sliding_overlap_identical_occurrence_and_lost_anchor() {
        let mut checkpoint = PromptAcquisitionCheckpoint::default();
        assert_eq!(checkpoint.acquire(scan(&[1, 2]), None), None);
        assert_eq!(
            checkpoint.acquire(scan(&[1, 2, 2]), None),
            Some("task 2".into())
        );
        assert_eq!(
            checkpoint.acquire(scan(&[2, 2, 3]), None),
            Some("task 3".into())
        );
        assert_eq!(checkpoint.acquire(scan(&[8, 9]), None), None);
        assert_eq!(
            checkpoint.acquire(scan(&[8, 9, 10]), None),
            Some("task 10".into())
        );
    }

    #[test]
    fn empty_baseline_allows_first_prompt_and_ineligible_blocks_still_anchor() {
        let mut checkpoint = PromptAcquisitionCheckpoint::default();
        assert_eq!(checkpoint.acquire(scan(&[]), None), None);
        let mut ineligible = scan(&[1]);
        ineligible.blocks[0].candidate = None;
        assert_eq!(checkpoint.acquire(ineligible, None), None);
        assert_eq!(
            checkpoint.acquire(scan(&[1, 2]), None),
            Some("task 2".into())
        );
    }

    #[test]
    fn failed_baseline_retries_at_thirty_seconds_even_idle() {
        let mut checkpoint = PromptAcquisitionCheckpoint::default();
        let start = Instant::now();
        assert!(checkpoint.should_scan(Some(ObservedAgentState::Idle), start));
        checkpoint.attempted(start);
        assert!(!checkpoint.should_scan(
            Some(ObservedAgentState::Busy),
            start + Duration::from_secs(29)
        ));
        assert!(checkpoint.should_scan(
            Some(ObservedAgentState::Idle),
            start + Duration::from_secs(30)
        ));
        checkpoint.acquire(scan(&[1]), None);
        assert!(!checkpoint.should_scan(
            Some(ObservedAgentState::Idle),
            start + Duration::from_secs(60)
        ));
    }

    #[test]
    fn busy_edges_retry_idle_once_and_sustained_busy_is_bounded() {
        let mut checkpoint = PromptAcquisitionCheckpoint::default();
        let start = Instant::now();
        checkpoint.acquire(scan(&[1]), None);
        assert!(checkpoint.should_scan(Some(ObservedAgentState::Busy), start));
        checkpoint.attempted(start);
        checkpoint.acquire(scan(&[1]), Some(ObservedAgentState::Busy));
        checkpoint.observe_state(Some(ObservedAgentState::Busy));
        assert!(!checkpoint.should_scan(None, start + Duration::from_secs(29)));
        assert!(checkpoint.should_scan(None, start + Duration::from_secs(30)));
        assert!(checkpoint.should_scan(
            Some(ObservedAgentState::Idle),
            start + Duration::from_secs(1)
        ));
        checkpoint.acquire(scan(&[1]), Some(ObservedAgentState::Idle));
        checkpoint.observe_state(Some(ObservedAgentState::Idle));
        assert!(!checkpoint.should_scan(
            Some(ObservedAgentState::Idle),
            start + Duration::from_secs(90)
        ));
    }
    #[test]
    fn alignment_matches_exhaustive_small_sequences() {
        let sequences: Vec<Vec<[u8; 32]>> = (0..=6)
            .flat_map(|length| {
                (0..(1 << length)).map(move |bits| {
                    (0..length)
                        .map(|bit| [((bits >> bit) & 1) as u8; 32])
                        .collect()
                })
            })
            .collect();
        for previous in &sequences {
            for current in &sequences {
                let expected = if previous.is_empty() {
                    Some(0)
                } else {
                    (1..=previous.len().min(current.len()))
                        .rev()
                        .find(|overlap| previous[previous.len() - overlap..] == current[..*overlap])
                };
                assert_eq!(new_blocks_start(previous, current), expected);
            }
        }
    }
    #[test]
    fn failed_idle_source_append_retains_bounded_retry_without_consuming_candidate() {
        let mut checkpoint = PromptAcquisitionCheckpoint::default();
        let start = Instant::now();
        checkpoint.acquire(scan(&[1]), None);
        checkpoint.attempted(start);
        checkpoint.acquire(scan(&[1]), Some(ObservedAgentState::Busy));
        checkpoint.observe_state(Some(ObservedAgentState::Busy));
        assert!(checkpoint.should_scan(Some(ObservedAgentState::Idle), start));
        let mut tentative = checkpoint.clone();
        assert_eq!(
            tentative.acquire(scan(&[1, 2]), Some(ObservedAgentState::Idle)),
            Some("task 2".into())
        );
        checkpoint.source_append_failed(Some(ObservedAgentState::Idle));
        assert!(!checkpoint.should_scan(
            Some(ObservedAgentState::Idle),
            start + Duration::from_secs(29)
        ));
        assert!(checkpoint.should_scan(
            Some(ObservedAgentState::Idle),
            start + Duration::from_secs(30)
        ));
        checkpoint.attempted(start + Duration::from_secs(30));
        assert_eq!(
            checkpoint.acquire(scan(&[1, 2]), Some(ObservedAgentState::Idle)),
            Some("task 2".into())
        );
        assert!(!checkpoint.should_scan(
            Some(ObservedAgentState::Idle),
            start + Duration::from_secs(60)
        ));
    }
}
