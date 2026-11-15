//! Code reviews of a git worktree: comments on its commits and lines, grouped
//! into reviews. Each worktree's reviews live in its own git directory (for a
//! linked worktree, `.git/worktrees/<name>`), so they follow the worktree and
//! never show up in its status. A global index in Zed's data directory lists
//! the worktrees that have reviews, so they can be found without opening them,
//! and remembers every tag used so far.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use imara_diff::{Algorithm, Diff, InternedInput, sources::lines};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

const REVIEW_DIR_NAME: &str = "zed-review";
const STATE_FILE_NAME: &str = "review.json";
const SNAPSHOTS_DIR_NAME: &str = "snapshots";
const INDEX_DIR_NAME: &str = "git_reviews";
const INDEX_FILE_NAME: &str = "index.json";
const STATE_VERSION: u32 = 1;
const SHORT_SHA_LEN: usize = 12;

/// Serializes writes, so that concurrent saves of the same file can't
/// interleave.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WorktreeReviews {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub worktree_path: PathBuf,
    /// The base branch the user picked, which only applies while the
    /// worktree is on the branch it was picked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_override: Option<BaseOverride>,
    /// Oldest first. Only the last one can be a draft.
    #[serde(default)]
    pub reviews: Vec<Review>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseOverride {
    pub branch: String,
    pub base: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub id: String,
    pub created_at: DateTime<Utc>,
    /// `None` while the review is a draft.
    #[serde(default)]
    pub submitted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub base_branch: Option<String>,
    /// HEAD when the review was submitted.
    #[serde(default)]
    pub head_commit: Option<String>,
    #[serde(default)]
    pub comments: Vec<Comment>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// The text, starting with its tags.
    pub text: String,
    /// Without their `#`.
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub addressed_at: Option<DateTime<Utc>>,
    pub target: CommentTarget,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommentTarget {
    /// Not about specific lines: about the commit that was HEAD.
    General {
        commit: Option<String>,
    },
    Line(LineTarget),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LineTarget {
    /// Relative to the repository's root, with `/` separators.
    pub path: String,
    pub side: Side,
    /// 1-based and inclusive, in `commit`'s version of the file: the commit
    /// that last changed these lines for new lines, the base for deleted ones.
    /// While `commit` is `None` (the lines aren't committed yet), in the file
    /// on disk when the comment was made.
    pub start_line: u32,
    pub end_line: u32,
    #[serde(default)]
    pub commit: Option<String>,
    /// The commented lines, as they were when the comment was made.
    #[serde(default)]
    pub excerpt: String,
    /// Where the comment is in a snapshot of the file it was made on, which
    /// is what lets it be found again as the file changes.
    pub anchor: SnapshotAnchor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// Lines of the file's current version.
    New,
    /// Deleted lines.
    Old,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotAnchor {
    /// Key of the snapshot, see [`write_snapshot`].
    pub snapshot: String,
    /// 0-based and inclusive.
    pub start_row: u32,
    pub end_row: u32,
}

impl WorktreeReviews {
    pub fn new(worktree_path: PathBuf) -> Self {
        Self {
            version: STATE_VERSION,
            worktree_path,
            base_override: None,
            reviews: Vec::new(),
        }
    }

    pub fn draft(&self) -> Option<&Review> {
        self.reviews.last().filter(|review| review.is_draft())
    }

    /// The draft review, started if there is none.
    pub fn draft_mut(&mut self, now: DateTime<Utc>) -> &mut Review {
        if self.draft().is_none() {
            self.reviews.push(Review {
                id: new_review_id(),
                created_at: now,
                submitted_at: None,
                branch: None,
                base_branch: None,
                head_commit: None,
                comments: Vec::new(),
            });
        }
        let last = self.reviews.len() - 1;
        &mut self.reviews[last]
    }

    pub fn submitted(&self) -> impl DoubleEndedIterator<Item = &Review> {
        self.reviews.iter().filter(|review| !review.is_draft())
    }

    pub fn review(&self, review_id: &str) -> Option<&Review> {
        self.reviews.iter().find(|review| review.id == review_id)
    }

    pub fn comment(&self, comment_id: &str) -> Option<(&Review, &Comment)> {
        self.reviews.iter().find_map(|review| {
            review
                .comments
                .iter()
                .find(|comment| comment.id == comment_id)
                .map(|comment| (review, comment))
        })
    }

    pub fn comment_mut(&mut self, comment_id: &str) -> Option<&mut Comment> {
        self.reviews
            .iter_mut()
            .flat_map(|review| review.comments.iter_mut())
            .find(|comment| comment.id == comment_id)
    }

    pub fn comments_mut(&mut self) -> impl Iterator<Item = &mut Comment> {
        self.reviews
            .iter_mut()
            .flat_map(|review| review.comments.iter_mut())
    }

    /// Adds a comment to the draft review, returning its id.
    pub fn add_comment(&mut self, text: &str, target: CommentTarget, now: DateTime<Utc>) -> String {
        let (text, tags) = normalize_comment_text(text);
        let id = new_comment_id();
        self.draft_mut(now).comments.push(Comment {
            id: id.clone(),
            created_at: now,
            updated_at: now,
            text,
            tags,
            addressed_at: None,
            target,
        });
        id
    }

    pub fn edit_comment(&mut self, comment_id: &str, text: &str, now: DateTime<Utc>) -> bool {
        let Some(comment) = self.comment_mut(comment_id) else {
            return false;
        };
        let (text, tags) = normalize_comment_text(text);
        if comment.text != text {
            comment.text = text;
            comment.tags = tags;
            comment.updated_at = now;
        }
        true
    }

    pub fn delete_comment(&mut self, comment_id: &str) -> bool {
        let mut deleted = false;
        for review in &mut self.reviews {
            let count = review.comments.len();
            review.comments.retain(|comment| comment.id != comment_id);
            deleted |= review.comments.len() != count;
        }
        // An emptied draft has nothing left to submit.
        if self.draft().is_some_and(|draft| draft.comments.is_empty()) {
            self.reviews.pop();
        }
        deleted
    }

    pub fn set_addressed(&mut self, comment_id: &str, addressed: bool, now: DateTime<Utc>) -> bool {
        let Some(comment) = self.comment_mut(comment_id) else {
            return false;
        };
        comment.addressed_at = addressed.then_some(now);
        true
    }

    /// Submits the draft, returning its id.
    pub fn submit_draft(
        &mut self,
        now: DateTime<Utc>,
        branch: Option<String>,
        base_branch: Option<String>,
        head_commit: Option<String>,
    ) -> Option<String> {
        let draft = self.reviews.last_mut().filter(|review| review.is_draft())?;
        if draft.comments.is_empty() {
            return None;
        }
        draft.submitted_at = Some(now);
        draft.branch = branch;
        draft.base_branch = base_branch;
        draft.head_commit = head_commit;
        Some(draft.id.clone())
    }

    /// The base branch the user picked for `branch`, if any.
    pub fn base_for_branch(&self, branch: &str) -> Option<&str> {
        self.base_override
            .as_ref()
            .filter(|base_override| base_override.branch == branch)
            .map(|base_override| base_override.base.as_str())
    }

    pub fn tags(&self) -> BTreeSet<String> {
        self.reviews
            .iter()
            .flat_map(|review| review.comments.iter())
            .flat_map(|comment| comment.tags.iter().cloned())
            .collect()
    }

    pub fn snapshot_keys(&self) -> HashSet<String> {
        self.reviews
            .iter()
            .flat_map(|review| review.comments.iter())
            .filter_map(|comment| match &comment.target {
                CommentTarget::Line(line) => Some(line.anchor.snapshot.clone()),
                CommentTarget::General { .. } => None,
            })
            .collect()
    }
}

impl Review {
    pub fn is_draft(&self) -> bool {
        self.submitted_at.is_none()
    }

    pub fn unaddressed_count(&self) -> usize {
        self.comments
            .iter()
            .filter(|comment| !comment.is_addressed())
            .count()
    }

    pub fn tags(&self) -> BTreeSet<String> {
        self.comments
            .iter()
            .flat_map(|comment| comment.tags.iter().cloned())
            .collect()
    }
}

impl Comment {
    pub fn is_addressed(&self) -> bool {
        self.addressed_at.is_some()
    }

    /// Whether the comment has `tag`, given with or without its `#`, ignoring case.
    pub fn has_tag(&self, tag: &str) -> bool {
        let tag = tag.trim_start_matches('#');
        self.tags
            .iter()
            .any(|comment_tag| comment_tag.eq_ignore_ascii_case(tag))
    }

    pub fn commit(&self) -> Option<&str> {
        match &self.target {
            CommentTarget::General { commit } => commit.as_deref(),
            CommentTarget::Line(line) => line.commit.as_deref(),
        }
    }
}

pub fn new_review_id() -> String {
    format!("rv-{}", random_id())
}

pub fn new_comment_id() -> String {
    format!("cm-{}", random_id())
}

fn random_id() -> String {
    let mut id = uuid::Uuid::new_v4().simple().to_string();
    id.truncate(12);
    id
}

pub fn short_sha(sha: &str) -> &str {
    sha.get(..SHORT_SHA_LEN).unwrap_or(sha)
}

/// Where a worktree's reviews are kept, given its git directory.
pub fn review_dir(git_dir: &Path) -> PathBuf {
    git_dir.join(REVIEW_DIR_NAME)
}

/// The file in [`review_dir`] that holds the reviews.
pub const STATE_FILE: &str = STATE_FILE_NAME;

/// The file in [`review_dir`] that holds the snapshot with `key`.
pub fn snapshot_file(key: &str) -> String {
    format!("{SNAPSHOTS_DIR_NAME}/{key}")
}

/// The key a snapshot of `text` is stored under, which is the same for
/// identical versions of a file.
pub fn snapshot_key(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Checks that `name` is one of the files kept in [`review_dir`], so that
/// requests (e.g. from a remote client) can't reach anything else.
pub fn validate_review_file(name: &str) -> Result<()> {
    let is_valid = name == STATE_FILE_NAME
        || name
            .strip_prefix(SNAPSHOTS_DIR_NAME)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|key| {
                !key.is_empty() && key.chars().all(|character| character.is_ascii_hexdigit())
            });
    anyhow::ensure!(is_valid, "{name:?} isn't a review file");
    Ok(())
}

/// Reads one of the files in the worktree's [`review_dir`], `None` if it
/// doesn't exist.
pub fn read_review_file(git_dir: &Path, name: &str) -> Result<Option<String>> {
    validate_review_file(name)?;
    let path = review_dir(git_dir).join(name);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Writes one of the files in the worktree's [`review_dir`], replacing it at
/// once so readers never see half of it.
pub fn write_review_file(git_dir: &Path, name: &str, contents: &str) -> Result<()> {
    validate_review_file(name)?;
    let path = review_dir(git_dir).join(name);
    let _lock = WRITE_LOCK.lock();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    write_atomically(&path, contents)
}

/// Reads reviews as [`serialize`] wrote them.
pub fn parse(text: &str, worktree_path: &Path) -> Result<WorktreeReviews> {
    let mut reviews: WorktreeReviews =
        serde_json::from_str(text).context("failed to parse the reviews")?;
    if reviews.worktree_path.as_os_str().is_empty() {
        reviews.worktree_path = worktree_path.to_path_buf();
    }
    Ok(reviews)
}

pub fn serialize(reviews: &WorktreeReviews) -> Result<String> {
    let mut reviews = reviews.clone();
    reviews.version = STATE_VERSION;
    Ok(serde_json::to_string_pretty(&reviews)?)
}

/// Reads the reviews of a local worktree; a worktree without reviews has none.
pub fn load(git_dir: &Path, worktree_path: &Path) -> Result<WorktreeReviews> {
    match read_review_file(git_dir, STATE_FILE_NAME)? {
        Some(text) => parse(&text, worktree_path),
        None => Ok(WorktreeReviews::new(worktree_path.to_path_buf())),
    }
}

fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let temp_path = path.with_extension(format!("{}.tmp", random_id()));
    std::fs::write(&temp_path, contents)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    std::fs::rename(&temp_path, path).with_context(|| {
        if let Err(error) = std::fs::remove_file(&temp_path) {
            log::error!("failed to remove {}: {error}", temp_path.display());
        }
        format!("failed to write {}", path.display())
    })
}

pub fn read_snapshot(git_dir: &Path, key: &str) -> Result<String> {
    read_review_file(git_dir, &snapshot_file(key))?.with_context(|| format!("no snapshot {key:?}"))
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Index {
    #[serde(default)]
    pub worktrees: Vec<IndexedWorktree>,
    /// Every tag used so far, without its `#`.
    #[serde(default)]
    pub tags: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexedWorktree {
    pub worktree_path: PathBuf,
    /// On `host`, for remote worktrees.
    pub git_dir: PathBuf,
    /// The machine a remote worktree is on, as Zed names its connection.
    /// Their reviews are mirrored locally, to be read without connecting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl IndexedWorktree {
    pub fn new(worktree_path: PathBuf, git_dir: PathBuf, host: Option<String>) -> Self {
        Self {
            worktree_path,
            git_dir,
            host,
            updated_at: Utc::now(),
        }
    }

    fn is_same_worktree(&self, other: &Self) -> bool {
        self.host == other.host && self.git_dir == other.git_dir
    }

    fn mirror_path(&self, index_path: &Path) -> Option<PathBuf> {
        let host = self.host.as_ref()?;
        let key = snapshot_key(&format!("{host}\n{}", self.git_dir.to_string_lossy()));
        Some(
            index_path
                .parent()?
                .join(MIRRORS_DIR_NAME)
                .join(format!("{key}.json")),
        )
    }
}

const MIRRORS_DIR_NAME: &str = "mirrors";

fn index_path() -> PathBuf {
    paths::data_dir().join(INDEX_DIR_NAME).join(INDEX_FILE_NAME)
}

pub fn load_index() -> Result<Index> {
    load_index_at(&index_path())
}

fn load_index_at(path: &Path) -> Result<Index> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Index::default()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Lists the worktree in the global index (once it has reviews) and
/// remembers its tags. A remote worktree's reviews are mirrored too.
pub fn record_in_index(worktree: &IndexedWorktree, reviews: &WorktreeReviews) -> Result<()> {
    record_in_index_at(worktree, reviews, &index_path())
}

fn record_in_index_at(
    worktree: &IndexedWorktree,
    reviews: &WorktreeReviews,
    path: &Path,
) -> Result<()> {
    if reviews.reviews.is_empty() && reviews.base_override.is_none() {
        return Ok(());
    }
    let _lock = WRITE_LOCK.lock();
    let mut index = load_index_at(path).unwrap_or_else(|error| {
        log::error!("starting a new review index: {error:#}");
        Index::default()
    });
    let mut entry = worktree.clone();
    entry.updated_at = Utc::now();
    match index
        .worktrees
        .iter_mut()
        .find(|existing| existing.is_same_worktree(&entry))
    {
        Some(existing) => *existing = entry.clone(),
        None => index.worktrees.push(entry.clone()),
    }
    index.tags.extend(reviews.tags());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if let Some(mirror_path) = entry.mirror_path(path) {
        if let Some(parent) = mirror_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        write_atomically(&mirror_path, &serialize(reviews)?)?;
    }
    write_atomically(path, &serde_json::to_string_pretty(&index)?)
}

/// The reviews of every indexed worktree that still has them: read from the
/// worktree for local ones, from their mirror for remote ones.
pub fn load_all() -> Result<Vec<(IndexedWorktree, WorktreeReviews)>> {
    load_all_at(&index_path())
}

fn load_all_at(path: &Path) -> Result<Vec<(IndexedWorktree, WorktreeReviews)>> {
    let index = load_index_at(path)?;
    Ok(index
        .worktrees
        .into_iter()
        .filter_map(|worktree| {
            let text = match worktree.mirror_path(path) {
                Some(mirror_path) => std::fs::read_to_string(mirror_path).ok(),
                None => read_review_file(&worktree.git_dir, STATE_FILE_NAME)
                    .log_err_with("skipping reviews")
                    .flatten(),
            }?;
            match parse(&text, &worktree.worktree_path) {
                Ok(reviews) => Some((worktree, reviews)),
                Err(error) => {
                    log::error!("skipping reviews: {error:#}");
                    None
                }
            }
        })
        .collect())
}

trait LogErrWith<T> {
    fn log_err_with(self, message: &str) -> Option<T>;
}

impl<T> LogErrWith<T> for Result<T> {
    fn log_err_with(self, message: &str) -> Option<T> {
        self.map_err(|error| log::error!("{message}: {error:#}"))
            .ok()
    }
}

/// Splits the tags out of a comment's text, returning the text with its tags
/// moved to the start, and the tags without their `#`.
pub fn normalize_comment_text(text: &str) -> (String, Vec<String>) {
    let mut tags: Vec<String> = Vec::new();
    let mut body_lines = Vec::new();
    for line in text.trim().lines() {
        let mut words = Vec::new();
        for word in line.split(' ') {
            match parse_tag(word) {
                Some(tag) => {
                    if !tags
                        .iter()
                        .any(|existing| existing.eq_ignore_ascii_case(tag))
                    {
                        tags.push(tag.to_string());
                    }
                }
                None => words.push(word),
            }
        }
        body_lines.push(words.join(" ").trim_end().to_string());
    }
    let body = body_lines.join("\n").trim().to_string();
    let prefix = tags
        .iter()
        .map(|tag| format!("#{tag}"))
        .collect::<Vec<_>>()
        .join(" ");
    let text = match (prefix.is_empty(), body.is_empty()) {
        (true, _) => body,
        (false, true) => prefix,
        (false, false) => format!("{prefix} {body}"),
    };
    (text, tags)
}

/// The tag a word is, without its `#`. Numbers like `#123` usually refer to
/// issues, so they aren't tags.
pub fn parse_tag(word: &str) -> Option<&str> {
    let tag = word.strip_prefix('#')?;
    let is_tag = !tag.is_empty()
        && tag.chars().all(is_tag_character)
        && !tag.chars().all(|character| character.is_ascii_digit());
    is_tag.then_some(tag)
}

pub fn is_tag_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '-' | '_' | '/' | '.')
}

/// Where rows of `old_text` are in `new_text`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Relocation {
    /// 0-based and inclusive.
    pub start_row: u32,
    pub end_row: u32,
    /// Whether every row is still there unchanged, rather than the nearest
    /// rows to changed ones.
    pub exact: bool,
}

/// Follows rows of `old_text` to `new_text`.
pub fn relocate_rows(old_text: &str, new_text: &str, start_row: u32, end_row: u32) -> Relocation {
    let (start_row, end_row) = (start_row.min(end_row), start_row.max(end_row));
    if old_text == new_text {
        return Relocation {
            start_row,
            end_row,
            exact: true,
        };
    }
    let input = InternedInput::new(lines(old_text), lines(new_text));
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let hunks = diff.hunks().collect::<Vec<_>>();
    let new_row_count = (input.after.len() as u32).max(1);

    // Changed rows map to the rows that replaced them, or to the row after
    // the ones that were deleted.
    let map_row = |row: u32| -> u32 {
        let mut delta: i64 = 0;
        for hunk in &hunks {
            if row < hunk.before.start {
                break;
            }
            if row < hunk.before.end {
                let offset = row - hunk.before.start;
                return if hunk.after.is_empty() {
                    hunk.after.start
                } else {
                    hunk.after.start + offset.min(hunk.after.end - hunk.after.start - 1)
                };
            }
            delta = hunk.after.end as i64 - hunk.before.end as i64;
        }
        (row as i64 + delta).max(0) as u32
    };

    let mapped_start = map_row(start_row);
    let mapped_end = map_row(end_row);
    let exact = hunks.iter().all(|hunk| {
        if hunk.before.is_empty() {
            // Insertions before the first row or after the last don't touch the range.
            hunk.before.start <= start_row || hunk.before.start > end_row
        } else {
            hunk.before.end <= start_row || hunk.before.start > end_row
        }
    });
    let start_row = mapped_start.min(new_row_count - 1);
    let end_row = mapped_end.min(new_row_count - 1).max(start_row);
    Relocation {
        start_row,
        end_row,
        exact,
    }
}

/// A line of `git blame --porcelain`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameLine {
    pub sha: String,
    /// 1-based, in `sha`'s version of the file.
    pub original_line: u32,
    /// 1-based, in the blamed version.
    pub final_line: u32,
    /// The file's path in `sha`, if it was renamed since.
    pub original_path: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Blame {
    pub lines: Vec<BlameLine>,
    pub committer_times: HashMap<String, i64>,
}

pub fn parse_blame_porcelain(output: &str) -> Blame {
    let mut blame = Blame::default();
    let mut current_sha: Option<String> = None;
    for line in output.lines() {
        if line.starts_with('\t') {
            continue;
        }
        let mut parts = line.split(' ');
        let first = parts.next().unwrap_or_default();
        let is_sha = (first.len() == 40 || first.len() == 64)
            && first.chars().all(|character| character.is_ascii_hexdigit());
        if is_sha
            && let (Some(Ok(original_line)), Some(Ok(final_line))) = (
                parts.next().map(str::parse::<u32>),
                parts.next().map(str::parse::<u32>),
            )
        {
            current_sha = Some(first.to_string());
            blame.lines.push(BlameLine {
                sha: first.to_string(),
                original_line,
                final_line,
                original_path: None,
            });
            continue;
        }
        let Some(sha) = current_sha.as_ref() else {
            continue;
        };
        if let Some(time) = line.strip_prefix("committer-time ") {
            if let Ok(time) = time.trim().parse() {
                blame.committer_times.insert(sha.clone(), time);
            }
        } else if let Some(path) = line.strip_prefix("filename ")
            && let Some(last) = blame.lines.last_mut()
        {
            last.original_path = Some(path.to_string());
        }
    }
    // Only the first line of each group carries the file name.
    let mut paths_by_sha: HashMap<String, String> = HashMap::new();
    for line in &mut blame.lines {
        match &line.original_path {
            Some(path) => {
                paths_by_sha.insert(line.sha.clone(), path.clone());
            }
            None => line.original_path = paths_by_sha.get(&line.sha).cloned(),
        }
    }
    blame
}

pub fn is_uncommitted_sha(sha: &str) -> bool {
    sha.chars().all(|character| character == '0')
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlameChoice {
    /// Some of the lines aren't committed yet.
    Uncommitted,
    /// The newest of the commits that last changed the lines among the
    /// reviewed ones.
    Commit(String),
    /// None of the lines changed in the reviewed commits.
    NotReviewed,
}

/// Which commit lines that were blamed belong to.
pub fn choose_blame_commit(blame: &Blame, is_reviewed: impl Fn(&str) -> bool) -> BlameChoice {
    if blame.lines.is_empty() {
        return BlameChoice::NotReviewed;
    }
    if blame.lines.iter().any(|line| is_uncommitted_sha(&line.sha)) {
        return BlameChoice::Uncommitted;
    }
    blame
        .lines
        .iter()
        .filter(|line| is_reviewed(&line.sha))
        .max_by_key(|line| blame.committer_times.get(&line.sha).copied().unwrap_or(0))
        .map_or(BlameChoice::NotReviewed, |line| {
            BlameChoice::Commit(line.sha.clone())
        })
}

fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// Where a comment is, e.g. `src/main.rs:10-12 @ 1a2b3c4d5e6f`.
pub fn comment_location(comment: &Comment) -> String {
    match &comment.target {
        CommentTarget::General { commit } => match commit {
            Some(commit) => format!("commit {}", short_sha(commit)),
            None => "the branch".to_string(),
        },
        CommentTarget::Line(line) => {
            let mut location = line.path.clone();
            if line.start_line == line.end_line {
                write!(location, ":{}", line.start_line).ok();
            } else {
                write!(location, ":{}-{}", line.start_line, line.end_line).ok();
            }
            match (&line.commit, line.side) {
                (Some(commit), Side::New) => write!(location, " @ {}", short_sha(commit)),
                (Some(commit), Side::Old) => {
                    write!(location, " (deleted, as of {})", short_sha(commit))
                }
                (None, Side::New) => write!(location, " (uncommitted)"),
                (None, Side::Old) => write!(location, " (deleted)"),
            }
            .ok();
            location
        }
    }
}

/// The comment as text to paste elsewhere: where it is, what it says, and
/// the lines it is about.
pub fn comment_as_text(comment: &Comment) -> String {
    let mut text = format!(
        "[{}] {} ({}{})\n{}",
        comment.id,
        comment_location(comment),
        format_timestamp(comment.created_at),
        if comment.is_addressed() {
            ", addressed"
        } else {
            ""
        },
        comment.text
    );
    if let CommentTarget::Line(line) = &comment.target
        && !line.excerpt.is_empty()
    {
        text.push('\n');
        for excerpt_line in line.excerpt.lines() {
            text.push_str("> ");
            text.push_str(excerpt_line);
            text.push('\n');
        }
        text.pop();
    }
    text
}

pub fn review_as_text(review: &Review) -> String {
    let mut text = format!("Review {}", review.id);
    match review.submitted_at {
        Some(submitted_at) => write!(text, ", submitted {}", format_timestamp(submitted_at)),
        None => write!(
            text,
            ", draft started {}",
            format_timestamp(review.created_at)
        ),
    }
    .ok();
    if let Some(branch) = &review.branch {
        write!(text, "\nBranch: {branch}").ok();
    }
    if let Some(base) = &review.base_branch {
        write!(text, "\nBase: {base}").ok();
    }
    if let Some(head) = &review.head_commit {
        write!(text, "\nHead: {head}").ok();
    }
    write!(
        text,
        "\n{} comment{}, {} unaddressed",
        review.comments.len(),
        if review.comments.len() == 1 { "" } else { "s" },
        review.unaddressed_count()
    )
    .ok();
    for comment in &review.comments {
        text.push_str("\n\n");
        text.push_str(&comment_as_text(comment));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_normalize_comment_text() {
        assert_eq!(
            normalize_comment_text("fix this #bug please #perf"),
            (
                "#bug #perf fix this please".to_string(),
                vec!["bug".to_string(), "perf".to_string()]
            )
        );
        assert_eq!(
            normalize_comment_text("  #a #A see #123\nsecond #b line "),
            (
                "#a #b see #123\nsecond line".to_string(),
                vec!["a".to_string(), "b".to_string()]
            )
        );
        assert_eq!(
            normalize_comment_text("#only"),
            ("#only".to_string(), vec!["only".to_string()])
        );
        assert_eq!(
            normalize_comment_text("no tags, just a # sign"),
            ("no tags, just a # sign".to_string(), vec![])
        );
    }

    #[test]
    fn test_relocate_rows() {
        let old = "a\nb\nc\nd\ne\n";
        assert_eq!(
            relocate_rows(old, old, 1, 2),
            Relocation {
                start_row: 1,
                end_row: 2,
                exact: true
            }
        );
        // Lines inserted above move the comment down.
        assert_eq!(
            relocate_rows(old, "x\ny\na\nb\nc\nd\ne\n", 2, 3),
            Relocation {
                start_row: 4,
                end_row: 5,
                exact: true
            }
        );
        // Lines removed above move it up.
        assert_eq!(
            relocate_rows(old, "c\nd\ne\n", 3, 3),
            Relocation {
                start_row: 1,
                end_row: 1,
                exact: true
            }
        );
        // A changed line maps to where it was replaced.
        assert_eq!(
            relocate_rows(old, "a\nb\nC\nd\ne\n", 2, 2),
            Relocation {
                start_row: 2,
                end_row: 2,
                exact: false
            }
        );
        // A line inserted inside the range keeps the ends but isn't exact.
        assert_eq!(
            relocate_rows(old, "a\nb\nnew\nc\nd\ne\n", 1, 2),
            Relocation {
                start_row: 1,
                end_row: 3,
                exact: false
            }
        );
        // A deleted line maps to the line after it.
        assert_eq!(
            relocate_rows(old, "a\nb\nd\ne\n", 2, 2),
            Relocation {
                start_row: 2,
                end_row: 2,
                exact: false
            }
        );
    }

    #[test]
    fn test_parse_blame_porcelain() {
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let zero = "0".repeat(40);
        let output = format!(
            "{sha_a} 3 1 2\nauthor A\ncommitter-time 100\nfilename old.rs\n\tline one\n\
             {sha_a} 4 2\n\tline two\n\
             {sha_b} 7 3 1\nauthor B\ncommitter-time 200\nfilename new.rs\n\tline three\n\
             {zero} 4 4 1\nauthor Not Committed Yet\ncommitter-time 300\nfilename new.rs\n\tline four\n"
        );
        let blame = parse_blame_porcelain(&output);
        assert_eq!(blame.lines.len(), 4);
        assert_eq!(blame.lines[1].original_line, 4);
        assert_eq!(blame.lines[1].final_line, 2);
        assert_eq!(blame.lines[1].original_path.as_deref(), Some("old.rs"));
        assert_eq!(blame.committer_times[&sha_b], 200);

        assert_eq!(
            choose_blame_commit(&blame, |_| true),
            BlameChoice::Uncommitted
        );
        let committed = Blame {
            lines: blame.lines[..3].to_vec(),
            committer_times: blame.committer_times.clone(),
        };
        assert_eq!(
            choose_blame_commit(&committed, |_| true),
            BlameChoice::Commit(sha_b)
        );
        assert_eq!(
            choose_blame_commit(&committed, |sha| sha == sha_a),
            BlameChoice::Commit(sha_a)
        );
        assert_eq!(
            choose_blame_commit(&committed, |_| false),
            BlameChoice::NotReviewed
        );
    }

    fn line_target(snapshot: &str) -> CommentTarget {
        CommentTarget::Line(LineTarget {
            path: "src/main.rs".into(),
            side: Side::New,
            start_line: 3,
            end_line: 4,
            commit: Some("c".repeat(40)),
            excerpt: "let a = 1;\nlet b = 2;".into(),
            anchor: SnapshotAnchor {
                snapshot: snapshot.into(),
                start_row: 2,
                end_row: 3,
            },
        })
    }

    #[test]
    fn test_review_lifecycle() {
        let now = Utc::now();
        let mut reviews = WorktreeReviews::new("/repo".into());
        let first = reviews.add_comment("rename #naming", line_target("abc"), now);
        let second = reviews.add_comment(
            "looks good",
            CommentTarget::General {
                commit: Some("d".repeat(40)),
            },
            now,
        );
        assert_eq!(reviews.reviews.len(), 1);
        assert!(reviews.draft().is_some());
        assert_eq!(reviews.comment(&first).unwrap().1.text, "#naming rename");

        assert!(reviews.edit_comment(&second, "#nit looks good", now));
        assert_eq!(reviews.comment(&second).unwrap().1.tags, vec!["nit"]);

        let review_id = reviews
            .submit_draft(now, Some("feature".into()), Some("main".into()), None)
            .unwrap();
        assert!(reviews.draft().is_none());
        assert!(reviews.set_addressed(&first, true, now));
        assert_eq!(reviews.review(&review_id).unwrap().unaddressed_count(), 1);
        assert_eq!(
            reviews.tags().into_iter().collect::<Vec<_>>(),
            vec!["naming", "nit"]
        );

        let text = review_as_text(reviews.review(&review_id).unwrap());
        assert!(text.contains(&format!("[{first}] src/main.rs:3-4 @ cccccccccccc")));
        assert!(text.contains("> let a = 1;\n> let b = 2;"));
        assert!(text.contains("2 comments, 1 unaddressed"));

        // Deleting the only comment of a new draft drops the draft.
        let third = reviews.add_comment("again", line_target("abc"), now);
        assert_eq!(reviews.reviews.len(), 2);
        assert!(reviews.delete_comment(&third));
        assert_eq!(reviews.reviews.len(), 1);
    }

    #[test]
    fn test_persistence() {
        let git_dir = tempfile::tempdir().unwrap();
        let index_dir = tempfile::tempdir().unwrap();
        let mut reviews = load(git_dir.path(), Path::new("/repo")).unwrap();
        assert!(reviews.reviews.is_empty());

        let key = snapshot_key("a\nb\n");
        assert_eq!(snapshot_key("a\nb\n"), key);
        write_review_file(git_dir.path(), &snapshot_file(&key), "a\nb\n").unwrap();
        assert_eq!(read_snapshot(git_dir.path(), &key).unwrap(), "a\nb\n");
        assert!(read_review_file(git_dir.path(), "../config").is_err());
        assert!(read_review_file(git_dir.path(), "snapshots/../../config").is_err());
        assert!(write_review_file(git_dir.path(), "other.json", "{}").is_err());

        reviews.add_comment("#tag text", line_target(&key), Utc::now());
        write_review_file(git_dir.path(), STATE_FILE, &serialize(&reviews).unwrap()).unwrap();
        assert_eq!(load(git_dir.path(), Path::new("/repo")).unwrap(), reviews);

        let index_path = index_dir.path().join("index.json");
        let local = IndexedWorktree::new("/repo".into(), git_dir.path().to_path_buf(), None);
        record_in_index_at(&local, &reviews, &index_path).unwrap();
        record_in_index_at(&local, &reviews, &index_path).unwrap();

        // Remote worktrees are read from their mirror.
        let mut remote_reviews = WorktreeReviews::new("/remote/repo".into());
        remote_reviews.add_comment("#other far away", line_target(&key), Utc::now());
        let remote = IndexedWorktree::new(
            "/remote/repo".into(),
            "/remote/repo/.git".into(),
            Some("devbox".into()),
        );
        record_in_index_at(&remote, &remote_reviews, &index_path).unwrap();

        let index = load_index_at(&index_path).unwrap();
        assert_eq!(index.worktrees.len(), 2);
        assert_eq!(
            index.tags.into_iter().collect::<Vec<_>>(),
            vec!["other", "tag"]
        );
        let all = load_all_at(&index_path).unwrap();
        assert_eq!(all.len(), 2);
        let (remote_entry, loaded_remote) = all
            .iter()
            .find(|(worktree, _)| worktree.host.is_some())
            .unwrap();
        assert_eq!(remote_entry.worktree_path, Path::new("/remote/repo"));
        assert_eq!(loaded_remote.reviews[0].comments[0].tags, vec!["other"]);
    }
}
