//! Plain result types of the release tool. They mirror the proto messages.

/// One spec in a list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Line {
    pub id: String,
    /// Empty when the spec has no readable title.
    pub title: String,
    /// The branch commits over the base branch. Only an in-progress line sets it. `None`
    /// means the count is unknown, which is not the same as a count of 0.
    pub commits: Option<u32>,
}

/// The lists of one product.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ProductNext {
    pub name: String,
    pub paused: bool,
    /// The newest release tag. Empty when the product has no release.
    pub release: String,
    pub merged: Vec<Line>,
    /// The merge times of the specs in `merged`, in no set order. A spec with no merge time
    /// has no entry, so this list may be shorter than `merged`.
    pub merged_at: Vec<time::OffsetDateTime>,
    /// The specs whose evidence file says `missing`, by id. They have no commit count.
    pub evidence_missing: Vec<Line>,
    pub in_progress: Vec<Line>,
    pub approved: Vec<Line>,
    pub parked: Vec<Line>,
    pub awaiting: Vec<Line>,
    pub not_clear: Vec<Line>,
    pub other_commits: u32,
    /// The reason when git gave no answer.
    pub unavailable: Option<&'static str>,
}
