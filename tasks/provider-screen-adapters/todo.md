# Provider Screen Adapters

- [x] Capture the approved behavior and architecture in `spec.md` and `design.md`.
- [x] Obtain an independent consistency review of the saved spec and design; final verdict: approved with no material ambiguity or contradiction.
- [ ] Obtain user review and approval of the saved documents.
- [ ] Write a detailed implementation plan after approval.
- [ ] Implement through RED/GREEN TDD with independent completion review.
- [ ] Run live Codex scrollback recovery acceptance and migrate verified durable documentation.

## Review Record

- Verified the exact default capture command, 2,000-row default, 10,000-row maximum, and visible-grid addition are stated consistently.
- Verified safe late-join baselining, bounded baseline retries, Busy/Idle recovery triggers, and semantic-recency behavior are explicit.
- Verified only proven submitted blocks become checkpoint anchors and that the adapter/runtime/reducer ownership diagrams match the written contract.
- `git diff --check` passed before commit.
