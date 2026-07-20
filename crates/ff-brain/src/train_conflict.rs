//! Semantic conflict detection for merge trains.
//!
//! A merge train squashes a sequence of PRs onto a base branch and validates the
//! result as one unit. Before that train branch is created, this module checks the
//! PRs for conflicts with each other — both textual (overlapping diff hunks) and
//! semantic (same symbols, adjacent code regions, competing configuration keys).
//!
//! The detector is purely functional: feed it [`PrChange`] values, get back a
//! [`TrainConflictReport`] with every conflicting pair and the reason.

use std::collections::{HashMap, HashSet};

/// One contiguous changed region inside a file, in terms of 0-indexed line numbers
/// in the post-merge (i.e. "new") view of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hunk {
    /// Inclusive start line in the new file.
    pub start: usize,
    /// Exclusive end line in the new file.
    pub end: usize,
}

impl Hunk {
    /// Create a hunk spanning `[start, end)`.
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Whether this hunk overlaps another (shares at least one line).
    pub fn overlaps(&self, other: &Hunk) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// Whether this hunk is within `gap` lines of another.
    pub fn near(&self, other: &Hunk, gap: usize) -> bool {
        let expanded = Hunk {
            start: self.start.saturating_sub(gap),
            end: self.end.saturating_add(gap),
        };
        expanded.overlaps(other)
    }
}

/// A file changed by a PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    /// Repository-relative path, e.g. `src/db.rs`.
    pub path: String,
    /// Changed hunks in that file, typically derived from the PR diff.
    pub hunks: Vec<Hunk>,
    /// Symbols (functions, structs, modules, config keys) touched by the change.
    /// Populated by the caller from static analysis or diff parsing.
    pub symbols: HashSet<String>,
}

impl ChangedFile {
    /// Build a changed file with no symbols.
    pub fn with_hunks(path: impl Into<String>, hunks: Vec<Hunk>) -> Self {
        Self {
            path: path.into(),
            hunks,
            symbols: HashSet::new(),
        }
    }

    /// Build a changed file carrying symbols only (e.g. a lockfile or config map).
    pub fn with_symbols(path: impl Into<String>, symbols: HashSet<String>) -> Self {
        Self {
            path: path.into(),
            hunks: Vec::new(),
            symbols,
        }
    }

    /// True if any hunk overlaps `other`.
    pub fn hunk_overlap(&self, other: &ChangedFile) -> bool {
        self.hunks
            .iter()
            .any(|a| other.hunks.iter().any(|b| a.overlaps(b)))
    }

    /// True if any hunk is within `gap` lines of `other`.
    pub fn hunk_near(&self, other: &ChangedFile, gap: usize) -> bool {
        self.hunks
            .iter()
            .any(|a| other.hunks.iter().any(|b| a.near(b, gap)))
    }

    /// True if the symbol sets intersect.
    pub fn symbol_overlap(&self, other: &ChangedFile) -> bool {
        !self.symbols.is_empty()
            && !other.symbols.is_empty()
            && self.symbols.intersection(&other.symbols).next().is_some()
    }
}

/// A PR represented as the set of changes it introduces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrChange {
    /// PR number or identifier.
    pub id: String,
    /// Files changed by the PR.
    pub files: Vec<ChangedFile>,
}

impl PrChange {
    /// Create a PR change from a list of files.
    pub fn new(id: impl Into<String>, files: Vec<ChangedFile>) -> Self {
        Self {
            id: id.into(),
            files,
        }
    }

    /// All repository-relative paths changed by this PR.
    pub fn paths(&self) -> HashSet<&str> {
        self.files.iter().map(|f| f.path.as_str()).collect()
    }

    /// Look up a changed file by path.
    pub fn file(&self, path: &str) -> Option<&ChangedFile> {
        self.files.iter().find(|f| f.path == path)
    }
}

/// Kind of conflict detected between two PRs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictKind {
    /// Both PRs modify the same file with overlapping diff hunks. This is the
    /// classic "cannot be cleanly applied" textual conflict.
    TextualOverlap,
    /// Both PRs touch the same symbol (function, struct, module, config key).
    /// They may not overlap textually but are likely to step on each other
    /// semantically.
    SymbolOverlap,
    /// Both PRs touch the same file with hunks close enough that one PR's
    /// change is very likely to invalidate or complicate the other.
    AdjacentHunks,
}

impl std::fmt::Display for ConflictKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConflictKind::TextualOverlap => write!(f, "textual overlap"),
            ConflictKind::SymbolOverlap => write!(f, "symbol overlap"),
            ConflictKind::AdjacentHunks => write!(f, "adjacent hunks"),
        }
    }
}

/// One detected conflict between two PRs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// First PR identifier.
    pub a_id: String,
    /// Second PR identifier.
    pub b_id: String,
    /// File path where the conflict was detected.
    pub path: String,
    /// Kind of conflict.
    pub kind: ConflictKind,
    /// Human-readable explanation.
    pub reason: String,
}

/// Report returned by [`detect_train_conflicts`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrainConflictReport {
    /// Every conflict found between any pair of PRs in the train.
    pub conflicts: Vec<Conflict>,
    /// PRs that have no conflicts with any other PR in the train.
    pub clean: Vec<String>,
}

impl TrainConflictReport {
    /// True if no conflicts were detected.
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }

    /// Number of conflicting pairs (a single pair can produce multiple conflicts).
    pub fn conflict_count(&self) -> usize {
        self.conflicts.len()
    }

    /// Paths involved in any conflict.
    pub fn conflicted_paths(&self) -> HashSet<&str> {
        self.conflicts.iter().map(|c| c.path.as_str()).collect()
    }

    /// PR identifiers involved in any conflict.
    pub fn conflicted_prs(&self) -> HashSet<&str> {
        self.conflicts
            .iter()
            .flat_map(|c| [c.a_id.as_str(), c.b_id.as_str()])
            .collect()
    }
}

/// Configuration for [`detect_train_conflicts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictOptions {
    /// Lines of separation below which two non-overlapping hunks in the same
    /// file are flagged as adjacent (semantic risk). Default: 3.
    pub adjacent_gap: usize,
    /// Whether to flag symbol overlaps as conflicts. Default: true.
    pub check_symbols: bool,
}

impl Default for ConflictOptions {
    fn default() -> Self {
        Self {
            adjacent_gap: 3,
            check_symbols: true,
        }
    }
}

/// Detect conflicts among all pairs of PRs in a proposed merge train.
///
/// The function returns a report describing every conflicting pair and every
/// PR that is clean with respect to the rest of the train. It does not mutate
/// state and does not require a database, so it is safe to call in unit tests
/// and in CI.
///
/// # Example
///
/// ```rust
/// use ff_brain::train_conflict::{PrChange, ChangedFile, Hunk, detect_train_conflicts};
///
/// let pr1 = PrChange::new("pr/1", vec![
///     ChangedFile::with_hunks("src/lib.rs", vec![Hunk::new(10, 15)])
/// ]);
/// let pr2 = PrChange::new("pr/2", vec![
///     ChangedFile::with_hunks("src/lib.rs", vec![Hunk::new(14, 20)])
/// ]);
///
/// let report = detect_train_conflicts(&[pr1, pr2], None);
/// assert!(!report.is_clean());
/// ```
pub fn detect_train_conflicts(
    prs: &[PrChange],
    opts: Option<ConflictOptions>,
) -> TrainConflictReport {
    let opts = opts.unwrap_or_default();
    let mut conflicts = Vec::new();

    for i in 0..prs.len() {
        for j in (i + 1)..prs.len() {
            let a = &prs[i];
            let b = &prs[j];
            detect_pair_conflicts(a, b, opts, &mut conflicts);
        }
    }

    let conflicted: HashSet<&str> = conflicts
        .iter()
        .flat_map(|c| [c.a_id.as_str(), c.b_id.as_str()])
        .collect();
    let clean: Vec<String> = prs
        .iter()
        .filter(|p| !conflicted.contains(p.id.as_str()))
        .map(|p| p.id.clone())
        .collect();

    TrainConflictReport { conflicts, clean }
}

fn detect_pair_conflicts(
    a: &PrChange,
    b: &PrChange,
    opts: ConflictOptions,
    out: &mut Vec<Conflict>,
) {
    for file_a in &a.files {
        let Some(file_b) = b.file(&file_a.path) else {
            continue;
        };

        if file_a.hunk_overlap(file_b) {
            out.push(Conflict {
                a_id: a.id.clone(),
                b_id: b.id.clone(),
                path: file_a.path.clone(),
                kind: ConflictKind::TextualOverlap,
                reason: format!(
                    "{} and {} have overlapping diff hunks in {}",
                    a.id, b.id, file_a.path
                ),
            });
        } else if file_a.hunk_near(file_b, opts.adjacent_gap) {
            out.push(Conflict {
                a_id: a.id.clone(),
                b_id: b.id.clone(),
                path: file_a.path.clone(),
                kind: ConflictKind::AdjacentHunks,
                reason: format!(
                    "{} and {} have adjacent changes in {} (within {} lines)",
                    a.id, b.id, file_a.path, opts.adjacent_gap
                ),
            });
        }

        if opts.check_symbols && file_a.symbol_overlap(file_b) {
            let shared: Vec<String> = file_a
                .symbols
                .intersection(&file_b.symbols)
                .cloned()
                .collect();
            out.push(Conflict {
                a_id: a.id.clone(),
                b_id: b.id.clone(),
                path: file_a.path.clone(),
                kind: ConflictKind::SymbolOverlap,
                reason: format!(
                    "{} and {} both touch symbol(s) {} in {}",
                    a.id,
                    b.id,
                    shared.join(", "),
                    file_a.path
                ),
            });
        }
    }
}

/// Rank PRs in a train by how many conflicts they participate in.
///
/// Useful for deciding which PR to drop or reorder when building a clean train.
pub fn rank_by_conflict_count(report: &TrainConflictReport) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for c in &report.conflicts {
        *counts.entry(c.a_id.clone()).or_insert(0) += 1;
        *counts.entry(c.b_id.clone()).or_insert(0) += 1;
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, start: usize, end: usize) -> ChangedFile {
        ChangedFile::with_hunks(path, vec![Hunk::new(start, end)])
    }

    fn file_with_symbol(path: &str, symbol: &str) -> ChangedFile {
        let mut symbols = HashSet::new();
        symbols.insert(symbol.to_string());
        ChangedFile::with_symbols(path, symbols)
    }

    #[test]
    fn hunk_overlap_detected() {
        let a = Hunk::new(10, 15);
        let b = Hunk::new(14, 20);
        assert!(a.overlaps(&b));
    }

    #[test]
    fn hunk_no_overlap() {
        let a = Hunk::new(10, 15);
        let b = Hunk::new(20, 25);
        assert!(!a.overlaps(&b));
    }

    #[test]
    fn hunk_adjacency_detected() {
        let a = Hunk::new(10, 15);
        let b = Hunk::new(16, 20);
        assert!(a.near(&b, 3));
        assert!(!a.near(&b, 0));
    }

    #[test]
    fn empty_train_is_clean() {
        let report = detect_train_conflicts(&[], None);
        assert!(report.is_clean());
        assert!(report.clean.is_empty());
    }

    #[test]
    fn single_pr_is_clean() {
        let pr = PrChange::new("pr/1", vec![file("src/lib.rs", 10, 15)]);
        let report = detect_train_conflicts(&[pr], None);
        assert!(report.is_clean());
        assert_eq!(report.clean, vec!["pr/1"]);
    }

    #[test]
    fn distinct_files_are_clean() {
        let pr1 = PrChange::new("pr/1", vec![file("src/lib.rs", 10, 15)]);
        let pr2 = PrChange::new("pr/2", vec![file("src/db.rs", 10, 15)]);
        let report = detect_train_conflicts(&[pr1, pr2], None);
        assert!(report.is_clean());
        assert_eq!(report.clean, vec!["pr/1", "pr/2"]);
    }

    #[test]
    fn textual_overlap_in_same_file_is_conflict() {
        let pr1 = PrChange::new("pr/1", vec![file("src/lib.rs", 10, 15)]);
        let pr2 = PrChange::new("pr/2", vec![file("src/lib.rs", 14, 20)]);
        let report = detect_train_conflicts(&[pr1, pr2], None);
        assert!(!report.is_clean());
        assert_eq!(report.conflict_count(), 1);
        assert_eq!(report.conflicts[0].kind, ConflictKind::TextualOverlap);
        assert_eq!(report.conflicts[0].path, "src/lib.rs");
    }

    #[test]
    fn adjacent_hunks_are_flagged() {
        let pr1 = PrChange::new("pr/1", vec![file("src/lib.rs", 10, 15)]);
        let pr2 = PrChange::new("pr/2", vec![file("src/lib.rs", 16, 20)]);
        let report = detect_train_conflicts(&[pr1, pr2], None);
        assert!(!report.is_clean());
        assert_eq!(report.conflicts[0].kind, ConflictKind::AdjacentHunks);
    }

    #[test]
    fn distant_hunks_are_clean() {
        let pr1 = PrChange::new("pr/1", vec![file("src/lib.rs", 10, 15)]);
        let pr2 = PrChange::new("pr/2", vec![file("src/lib.rs", 100, 110)]);
        let report = detect_train_conflicts(&[pr1, pr2], None);
        assert!(report.is_clean());
    }

    #[test]
    fn symbol_overlap_is_conflict() {
        let pr1 = PrChange::new("pr/1", vec![file_with_symbol("src/db.rs", "UserStore")]);
        let pr2 = PrChange::new("pr/2", vec![file_with_symbol("src/db.rs", "UserStore")]);
        let report = detect_train_conflicts(&[pr1, pr2], None);
        assert!(!report.is_clean());
        assert_eq!(report.conflicts[0].kind, ConflictKind::SymbolOverlap);
    }

    #[test]
    fn symbol_check_can_be_disabled() {
        let pr1 = PrChange::new("pr/1", vec![file_with_symbol("src/db.rs", "UserStore")]);
        let pr2 = PrChange::new("pr/2", vec![file_with_symbol("src/db.rs", "UserStore")]);
        let opts = ConflictOptions {
            check_symbols: false,
            ..Default::default()
        };
        let report = detect_train_conflicts(&[pr1, pr2], Some(opts));
        assert!(report.is_clean());
    }

    #[test]
    fn multiple_files_can_produce_multiple_conflicts() {
        let pr1 = PrChange::new(
            "pr/1",
            vec![file("src/lib.rs", 10, 15), file("src/db.rs", 50, 60)],
        );
        let pr2 = PrChange::new(
            "pr/2",
            vec![file("src/lib.rs", 14, 20), file("src/db.rs", 55, 65)],
        );
        let report = detect_train_conflicts(&[pr1, pr2], None);
        assert_eq!(report.conflict_count(), 2);
        assert_eq!(report.conflicted_paths().len(), 2);
    }

    #[test]
    fn only_conflicted_prs_marked_dirty() {
        let pr1 = PrChange::new("pr/1", vec![file("src/lib.rs", 10, 15)]);
        let pr2 = PrChange::new("pr/2", vec![file("src/lib.rs", 14, 20)]);
        let pr3 = PrChange::new("pr/3", vec![file("src/db.rs", 1, 5)]);
        let report = detect_train_conflicts(&[pr1, pr2, pr3], None);
        assert!(!report.is_clean());
        assert_eq!(report.clean, vec!["pr/3"]);
        assert_eq!(report.conflicted_prs().len(), 2);
    }

    #[test]
    fn rank_by_conflict_count_orders_correctly() {
        let pr1 = PrChange::new(
            "pr/1",
            vec![file("src/lib.rs", 10, 15), file("src/db.rs", 1, 5)],
        );
        let pr2 = PrChange::new("pr/2", vec![file("src/lib.rs", 14, 20)]);
        let pr3 = PrChange::new("pr/3", vec![file("src/db.rs", 3, 8)]);
        let report = detect_train_conflicts(&[pr1, pr2, pr3], None);
        let ranked = rank_by_conflict_count(&report);
        assert_eq!(ranked.len(), 3);
        // pr/1 conflicts with both pr/2 (lib.rs) and pr/3 (db.rs).
        assert_eq!(ranked[0].0, "pr/1");
        assert_eq!(ranked[0].1, 2);
        assert_eq!(ranked[1].1, 1);
        assert_eq!(ranked[2].1, 1);
    }
}
