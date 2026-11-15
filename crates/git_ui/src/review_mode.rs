//! The git panel's review mode: comments on the current branch's commits and
//! lines, grouped into reviews that are submitted once everything they are
//! about is committed. Reviews are kept by [`git_review`] in the worktree's
//! git directory, which for remote projects is on the remote machine.
//!
//! [`ReviewModel`] holds a worktree's reviews and does everything with them,
//! for the panel's [`ReviewView`] as well as for other clients like the
//! mobile web UI.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Utc};
use editor::{CompletionProvider, Editor, EditorEvent, REVIEW_COMMENT_KEY_CONTEXT};
use git::repository::RepoPath;
use git::status::FileStatus;
use git_review::{
    BaseOverride, BlameChoice, Comment, CommentTarget, IndexedWorktree, LineTarget, Relocation,
    Review, Side, SnapshotAnchor, WorktreeReviews,
};
use gpui::{
    App, AppContext as _, AsyncApp, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable,
    Global, ScrollHandle, SharedString, Subscription, Task, WeakEntity,
};
use language::{Buffer, BufferSnapshot, Point, ToOffset as _, ToPoint as _};
use multi_buffer::{MultiBufferSnapshot, ToPoint as _};
use project::git_store::{GitStoreEvent, Repository, RepositoryEvent};
use project::{CompletionDisplayOptions, CompletionResponse, Project, ProjectPath};
use ui::{Chip, Divider, Tab, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{ItemHandle, Workspace, notifications::NotifyTaskExt as _};

use crate::branch_diff::BranchDiff;
use crate::branch_picker;
use crate::commit_view::CommitView;
use crate::project_diff::ProjectDiff;

const MAX_REVIEWED_COMMITS: usize = 1000;
const COMMENT_EDITOR_MAX_LINES: usize = 10;
const MAX_DIFF_FILES: usize = 300;
const MAX_DIFF_FILE_BYTES: usize = 1 << 20;

pub fn init(cx: &mut App) {
    cx.set_global(ReviewStore::default());
    cx.spawn(async move |cx| {
        let index = cx
            .background_spawn(async move { git_review::load_index() })
            .await;
        match index {
            Ok(index) => cx.update_global::<ReviewStore, _>(|store, _| {
                store.known_tags.extend(index.tags);
            }),
            Err(error) => log::error!("failed to load the review index: {error:#}"),
        }
    })
    .detach();
}

/// The reviews of every worktree that is being reviewed, shared by the
/// windows and other clients reviewing it.
#[derive(Default)]
struct ReviewStore {
    /// By host (for remote worktrees) and git directory.
    models: HashMap<(Option<String>, PathBuf), WeakEntity<ReviewModel>>,
    /// Without their `#`.
    known_tags: BTreeSet<String>,
}

impl Global for ReviewStore {}

/// Every tag used so far, without its `#`.
pub fn known_tags(cx: &App) -> Vec<String> {
    cx.try_global::<ReviewStore>()
        .map(|store| store.known_tags.iter().cloned().collect())
        .unwrap_or_default()
}

/// Reviews live in the repository's git directory, which Zed can reach for
/// local projects and through a remote server, but not as a collaborator.
pub fn can_review(project: &Project) -> bool {
    project.is_local() || project.is_via_remote_server()
}

async fn run_git(
    repository: &Entity<Repository>,
    args: Vec<String>,
    stdin: Option<String>,
    cx: &mut AsyncApp,
) -> Result<String> {
    repository
        .update(cx, |repository, _| {
            repository.run_read_only_git_command(args, stdin)
        })
        .await
        .map_err(|_| anyhow!("the repository was closed"))?
}

fn args(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

/// The branch HEAD most likely grew out of: of the other local branches, the
/// one with the fewest commits that only HEAD has, then the fewest that only
/// it has, preferring the default branch.
async fn closest_ancestor_branch(
    repository: &Entity<Repository>,
    current_branch: Option<String>,
    cx: &mut AsyncApp,
) -> Result<Option<String>> {
    let default_branch = repository
        .update(cx, |repository, _| repository.default_branch(false))
        .await
        .ok()
        .and_then(|branch| branch.log_err().flatten())
        .map(|branch| branch.to_string());
    let is_default =
        |name: &str| default_branch.as_deref() == Some(name) || matches!(name, "main" | "master");

    // (name, commits only HEAD has, commits only the branch has)
    let mut candidates: Vec<(String, u64, u64)> = Vec::new();
    match run_git(
        repository,
        args(&[
            "for-each-ref",
            "--format=%(refname:short)%09%(ahead-behind:HEAD)",
            "refs/heads",
        ]),
        None,
        cx,
    )
    .await
    {
        Ok(output) => {
            for line in output.lines() {
                let Some((name, counts)) = line.split_once('\t') else {
                    continue;
                };
                let mut counts = counts.split(' ').map(str::parse::<u64>);
                if let (Some(Ok(only_branch)), Some(Ok(only_head))) = (counts.next(), counts.next())
                {
                    candidates.push((name.to_string(), only_head, only_branch));
                }
            }
        }
        // `ahead-behind` needs git 2.41, so count the recently used branches one by one.
        Err(_) => {
            let names = run_git(
                repository,
                args(&[
                    "for-each-ref",
                    "--sort=-committerdate",
                    "--count=50",
                    "--format=%(refname:short)",
                    "refs/heads",
                ]),
                None,
                cx,
            )
            .await?;
            for name in names.lines().map(str::trim).filter(|name| !name.is_empty()) {
                if Some(name) == current_branch.as_deref() {
                    continue;
                }
                let Ok(counts) = run_git(
                    repository,
                    args(&[
                        "rev-list",
                        "--left-right",
                        "--count",
                        &format!("{name}...HEAD"),
                    ]),
                    None,
                    cx,
                )
                .await
                else {
                    continue;
                };
                let mut counts = counts.split_whitespace().map(str::parse::<u64>);
                if let (Some(Ok(only_branch)), Some(Ok(only_head))) = (counts.next(), counts.next())
                {
                    candidates.push((name.to_string(), only_head, only_branch));
                }
            }
        }
    }

    let best = candidates
        .into_iter()
        .enumerate()
        .filter(|(_, (name, _, _))| Some(name.as_str()) != current_branch.as_deref())
        .min_by_key(|(order, (name, only_head, only_branch))| {
            (*only_head, *only_branch, !is_default(name), *order)
        })
        .map(|(_, (name, _, _))| name);
    Ok(best.or(default_branch.filter(|name| Some(name) != current_branch.as_ref())))
}

/// A commit on top of the base.
#[derive(Clone, Debug)]
pub struct ReviewCommit {
    pub sha: String,
    pub subject: SharedString,
    pub author: SharedString,
    /// Unix seconds.
    pub timestamp: i64,
}

async fn load_reviewed_commits(
    repository: &Entity<Repository>,
    base: &str,
    cx: &mut AsyncApp,
) -> Result<(Option<String>, Vec<ReviewCommit>)> {
    let merge_base = run_git(repository, args(&["merge-base", base, "HEAD"]), None, cx)
        .await
        .ok()
        .map(|sha| sha.trim().to_string())
        .filter(|sha| !sha.is_empty());
    let log = run_git(
        repository,
        args(&[
            "log",
            &format!("--max-count={MAX_REVIEWED_COMMITS}"),
            "--format=%H%x1f%s%x1f%an%x1f%ct%x1e",
            &format!("{base}..HEAD"),
            "--",
        ]),
        None,
        cx,
    )
    .await?;
    let commits = log
        .split('\x1e')
        .filter_map(|record| {
            let mut fields = record.trim().split('\x1f');
            let sha = fields.next()?.to_string();
            if sha.is_empty() {
                return None;
            }
            Some(ReviewCommit {
                sha,
                subject: fields.next().unwrap_or_default().to_string().into(),
                author: fields.next().unwrap_or_default().to_string().into(),
                timestamp: fields
                    .next()
                    .and_then(|time| time.parse().ok())
                    .unwrap_or(0),
            })
        })
        .collect();
    Ok((merge_base, commits))
}

/// Which commit the rows of `text` (the file at `repo_path` as it is now, or
/// at `revision`) belong to, and where they are in that commit's version.
/// `rows` is inclusive.
async fn associate_lines(
    repository: &Entity<Repository>,
    repo_path: &str,
    revision: Option<&str>,
    text: &str,
    rows: Range<u32>,
    reviewed: &HashSet<String>,
    head_sha: Option<&str>,
    cx: &mut AsyncApp,
) -> (Option<String>, u32, u32) {
    let line_range = format!("{},{}", rows.start + 1, rows.end + 1);
    let mut blame_args = args(&["blame", "--porcelain", "-L", &line_range]);
    let stdin = match revision {
        Some(revision) => {
            blame_args.push(revision.to_string());
            None
        }
        None => {
            blame_args.extend(args(&["--contents", "-"]));
            Some(text.to_string())
        }
    };
    blame_args.extend(args(&["--", repo_path]));
    // Files git doesn't know yet can't be blamed: nothing of them is committed.
    let blame = match run_git(repository, blame_args, stdin, cx).await {
        Ok(output) => git_review::parse_blame_porcelain(&output),
        Err(_) => return (None, rows.start + 1, rows.end + 1),
    };
    let choice = git_review::choose_blame_commit(&blame, |sha| reviewed.contains(sha));
    let (commit, original_path) = match choice {
        BlameChoice::Uncommitted => return (None, rows.start + 1, rows.end + 1),
        BlameChoice::Commit(sha) => {
            let original_path = blame
                .lines
                .iter()
                .find(|line| line.sha == sha)
                .and_then(|line| line.original_path.clone());
            (sha, original_path)
        }
        BlameChoice::NotReviewed => match head_sha {
            Some(head_sha) => (head_sha.to_string(), None),
            None => return (None, rows.start + 1, rows.end + 1),
        },
    };
    let original_path = original_path.unwrap_or_else(|| repo_path.to_string());
    let lines = match run_git(
        repository,
        args(&["show", &format!("{commit}:{original_path}")]),
        None,
        cx,
    )
    .await
    {
        Ok(committed_text) => {
            let relocation = git_review::relocate_rows(text, &committed_text, rows.start, rows.end);
            (relocation.start_row + 1, relocation.end_row + 1)
        }
        Err(_) => (rows.start + 1, rows.end + 1),
    };
    (Some(commit), lines.0, lines.1)
}

/// Moves a comment on lines that weren't committed yet to the commit they're
/// in now, anchoring it in HEAD's version of the file.
async fn reassociate_comment(
    model: &Entity<ReviewModel>,
    repository: &Entity<Repository>,
    target: &LineTarget,
    reviewed: &HashSet<String>,
    head_sha: Option<&str>,
    cx: &mut AsyncApp,
) -> Result<LineTarget> {
    let snapshot_text = model
        .read_with(cx, |model, _| {
            model.snapshot(&target.anchor.snapshot).cloned()
        })
        .context("the file the comment was made on is missing")?;
    let head_text = run_git(
        repository,
        args(&["show", &format!("HEAD:{}", target.path)]),
        None,
        cx,
    )
    .await
    .with_context(|| format!("{} isn't committed", target.path))?;
    let relocation = git_review::relocate_rows(
        &snapshot_text,
        &head_text,
        target.anchor.start_row,
        target.anchor.end_row,
    );
    let rows = relocation.start_row..relocation.end_row;
    let (commit, start_line, end_line) = associate_lines(
        repository,
        &target.path,
        Some("HEAD"),
        &head_text,
        rows.clone(),
        reviewed,
        head_sha,
        cx,
    )
    .await;
    let head_text = Arc::<str>::from(head_text);
    let key = model
        .update(cx, |model, cx| model.store_snapshot(head_text, cx))
        .await?;
    Ok(LineTarget {
        path: target.path.clone(),
        side: target.side,
        start_line,
        end_line,
        commit: commit.or_else(|| head_sha.map(str::to_string)),
        excerpt: target.excerpt.clone(),
        anchor: SnapshotAnchor {
            snapshot: key,
            start_row: rows.start,
            end_row: rows.end,
        },
    })
}

/// Which version of a file a comment was made on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommentSource {
    /// The working tree, e.g. in a diff against the base.
    WorkingTree,
    Commit(String),
}

#[derive(Clone, Debug)]
pub enum CapturedSide {
    New,
    /// Deleted lines, at `base_rows` (inclusive) of the old version.
    Old {
        base_rows: Range<u32>,
    },
}

/// The lines a new comment is on.
#[derive(Clone, Debug)]
pub struct LineCapture {
    /// Relative to the repository's root, with `/` separators.
    pub repo_path: String,
    pub source: CommentSource,
    /// The file as the comment was made on it.
    pub text: Arc<str>,
    /// Inclusive, in `text`. For deleted lines, where they were.
    pub rows: Range<u32>,
    pub side: CapturedSide,
    pub excerpt: String,
}

fn lines_text(text: &str, rows: Range<u32>) -> String {
    text.lines()
        .skip(rows.start as usize)
        .take((rows.end.saturating_sub(rows.start) + 1) as usize)
        .collect::<Vec<_>>()
        .join("\n")
}

/// How a file differs from the base, for clients that show the diff themselves.
#[derive(Clone, Debug)]
pub struct ReviewDiffFile {
    pub path: String,
    pub status: ReviewFileStatus,
    /// In the base, unless the file was added.
    pub old_text: Option<String>,
    /// In the working tree, unless the file was deleted.
    pub new_text: Option<String>,
    /// Binary and very large files come without their text.
    pub is_binary: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewFileStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, PartialEq, Eq)]
enum LoadState {
    Loading,
    Loaded,
    Error(SharedString),
}

pub enum ReviewModelEvent {
    CommentEdited { comment_id: String, text: String },
    CommentDeleted { comment_id: String },
}

/// A worktree's reviews and the branch they're about, saved whenever they
/// change. There is one per worktree, shared by everything reviewing it.
pub struct ReviewModel {
    repository: Entity<Repository>,
    worktree: IndexedWorktree,
    reviews: WorktreeReviews,
    /// The versions of files comments are anchored in, by key.
    snapshots: HashMap<String, Arc<str>>,
    is_loaded: bool,
    error: Option<SharedString>,
    branch: Option<String>,
    base: Option<String>,
    is_base_picked: bool,
    merge_base: Option<String>,
    commits: Vec<ReviewCommit>,
    commits_state: LoadState,
    is_submitting: bool,
    save_task: Option<Task<()>>,
    refresh_task: Task<()>,
    reassociate_task: Task<()>,
    _load_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ReviewModelEvent> for ReviewModel {}

impl ReviewModel {
    /// The reviews of the repository's worktree, shared with everything else
    /// reviewing it.
    pub fn for_repository(
        repository: &Entity<Repository>,
        project: &Entity<Project>,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        let project = project.read(cx);
        anyhow::ensure!(
            can_review(project),
            "reviews are only available in local projects and projects on a remote server"
        );
        let host = project
            .remote_connection_options(cx)
            .map(|options| options.display_name());
        let snapshot = repository.read(cx);
        let git_dir = snapshot.repository_dir_abs_path.to_path_buf();
        let worktree_path = snapshot.work_directory_abs_path.to_path_buf();
        let key = (host.clone(), git_dir.clone());
        if let Some(model) = cx
            .try_global::<ReviewStore>()
            .and_then(|store| store.models.get(&key))
            .and_then(WeakEntity::upgrade)
        {
            return Ok(model);
        }
        let worktree = IndexedWorktree::new(worktree_path, git_dir, host);
        let repository = repository.clone();
        let model = cx.new(|cx| Self::new(repository, worktree, cx));
        if cx.has_global::<ReviewStore>() {
            cx.update_global::<ReviewStore, _>(|store, _| {
                store.models.insert(key, model.downgrade());
            });
        }
        Ok(model)
    }

    fn new(
        repository: Entity<Repository>,
        worktree: IndexedWorktree,
        cx: &mut Context<Self>,
    ) -> Self {
        let load_task = cx.spawn({
            let repository = repository.clone();
            let worktree_path = worktree.worktree_path.clone();
            async move |this, cx| {
                let result = async {
                    let text = repository
                        .update(cx, |repository, _| {
                            repository.load_review_file(git_review::STATE_FILE.to_string())
                        })
                        .await
                        .map_err(|_| anyhow!("the repository was closed"))??;
                    let reviews = match text {
                        Some(text) => git_review::parse(&text, &worktree_path)?,
                        None => WorktreeReviews::new(worktree_path),
                    };
                    let mut snapshots: HashMap<String, Arc<str>> = HashMap::new();
                    for key in reviews.snapshot_keys() {
                        let text = repository
                            .update(cx, |repository, _| {
                                repository.load_review_file(git_review::snapshot_file(&key))
                            })
                            .await;
                        match text {
                            Ok(Ok(Some(text))) => {
                                snapshots.insert(key, Arc::<str>::from(text));
                            }
                            Ok(Ok(None)) => log::error!("review snapshot {key} is missing"),
                            Ok(Err(error)) => {
                                log::error!("failed to load a review snapshot: {error:#}")
                            }
                            Err(_) => {}
                        }
                    }
                    anyhow::Ok((reviews, snapshots))
                }
                .await;
                this.update(cx, |this, cx| {
                    match result {
                        Ok((reviews, snapshots)) => {
                            this.reviews = reviews;
                            this.snapshots.extend(snapshots);
                            if cx.has_global::<ReviewStore>() {
                                let tags = this.reviews.tags();
                                cx.update_global::<ReviewStore, _>(|store, _| {
                                    store.known_tags.extend(tags)
                                });
                            }
                        }
                        Err(error) => {
                            log::error!("failed to load reviews: {error:#}");
                            this.error = Some(format!("Failed to load reviews: {error:#}").into());
                        }
                    }
                    this.is_loaded = true;
                    this.refresh(cx);
                    cx.notify();
                })
                .ok();
            }
        });
        let subscriptions = vec![cx.subscribe(
            &repository,
            |this, _, event: &RepositoryEvent, cx| match event {
                RepositoryEvent::HeadChanged | RepositoryEvent::BranchListChanged => {
                    this.refresh(cx);
                }
                RepositoryEvent::StatusesChanged => {
                    this.reassociate_committed_comments(cx);
                    cx.notify();
                }
                _ => {}
            },
        )];
        Self {
            reviews: WorktreeReviews::new(worktree.worktree_path.clone()),
            repository,
            worktree,
            snapshots: HashMap::default(),
            is_loaded: false,
            error: None,
            branch: None,
            base: None,
            is_base_picked: false,
            merge_base: None,
            commits: Vec::new(),
            commits_state: LoadState::Loading,
            is_submitting: false,
            save_task: None,
            refresh_task: Task::ready(()),
            reassociate_task: Task::ready(()),
            _load_task: load_task,
            _subscriptions: subscriptions,
        }
    }

    pub fn repository(&self) -> &Entity<Repository> {
        &self.repository
    }

    pub fn is_loaded(&self) -> bool {
        self.is_loaded
    }

    pub fn reviews(&self) -> &WorktreeReviews {
        &self.reviews
    }

    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    pub fn base(&self) -> Option<&str> {
        self.base.as_deref()
    }

    /// Whether the user picked the base, rather than it being the closest
    /// ancestor branch.
    pub fn is_base_picked(&self) -> bool {
        self.is_base_picked
    }

    /// The commits on top of the base, newest first.
    pub fn commits(&self) -> &[ReviewCommit] {
        &self.commits
    }

    pub fn is_loading_commits(&self) -> bool {
        self.commits_state == LoadState::Loading
    }

    pub fn commits_error(&self) -> Option<&SharedString> {
        match &self.commits_state {
            LoadState::Error(error) => Some(error),
            LoadState::Loading | LoadState::Loaded => None,
        }
    }

    pub fn is_submitting(&self) -> bool {
        self.is_submitting
    }

    pub fn snapshot(&self, key: &str) -> Option<&Arc<str>> {
        self.snapshots.get(key)
    }

    /// Where a comment's lines are in `current_text`, a newer version of its file.
    pub fn current_location(&self, target: &LineTarget, current_text: &str) -> Option<Relocation> {
        let snapshot = self.snapshot(&target.anchor.snapshot)?;
        Some(git_review::relocate_rows(
            snapshot,
            current_text,
            target.anchor.start_row,
            target.anchor.end_row,
        ))
    }

    fn head_sha(&self, cx: &App) -> Option<String> {
        self.repository
            .read(cx)
            .head_commit
            .as_ref()
            .map(|commit| commit.sha.to_string())
    }

    fn current_branch(&self, cx: &App) -> Option<String> {
        self.repository
            .read(cx)
            .branch
            .as_ref()
            .map(|branch| branch.name().to_string())
    }

    fn reviewed_shas(&self) -> HashSet<String> {
        self.commits
            .iter()
            .map(|commit| commit.sha.clone())
            .collect()
    }

    fn change(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut WorktreeReviews)) {
        change(&mut self.reviews);
        if cx.has_global::<ReviewStore>() {
            let tags = self.reviews.tags();
            cx.update_global::<ReviewStore, _>(|store, _| store.known_tags.extend(tags));
        }
        self.save(cx);
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if !self.is_loaded {
            return;
        }
        let text = match git_review::serialize(&self.reviews) {
            Ok(text) => text,
            Err(error) => {
                log::error!("failed to serialize reviews: {error:#}");
                return;
            }
        };
        // The repository runs its jobs in order, so saves land in order.
        let save = self.repository.update(cx, |repository, _| {
            repository.save_review_file(git_review::STATE_FILE.to_string(), text)
        });
        let reviews = self.reviews.clone();
        let worktree = self.worktree.clone();
        self.save_task = Some(cx.spawn(async move |this, cx| {
            let result = save
                .await
                .unwrap_or_else(|_| Err(anyhow!("the repository was closed")));
            // Tests mustn't list their worktrees in the user's data directory.
            if result.is_ok() && !cfg!(test) {
                cx.background_spawn(
                    async move { git_review::record_in_index(&worktree, &reviews) },
                )
                .await
                .log_err();
            }
            this.update(cx, |this, cx| {
                this.error = result
                    .map_err(|error| {
                        log::error!("failed to save reviews: {error:#}");
                        SharedString::from(format!("Failed to save reviews: {error:#}"))
                    })
                    .err();
                cx.notify();
            })
            .ok();
        }));
    }

    fn store_snapshot(&mut self, text: Arc<str>, cx: &mut Context<Self>) -> Task<Result<String>> {
        let key = git_review::snapshot_key(&text);
        if self.snapshots.contains_key(&key) {
            return Task::ready(Ok(key));
        }
        self.snapshots.insert(key.clone(), text.clone());
        let save = self.repository.update(cx, |repository, _| {
            repository.save_review_file(git_review::snapshot_file(&key), text.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = save
                .await
                .unwrap_or_else(|_| Err(anyhow!("the repository was closed")));
            if let Err(error) = result {
                this.update(cx, |this, _| this.snapshots.remove(&key)).ok();
                return Err(error);
            }
            Ok(key)
        })
    }

    /// Picks the base and loads the commits on top of it.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        // The base the user picked is in the reviews, so this waits for them.
        if !self.is_loaded {
            return;
        }
        let branch = self.current_branch(cx);
        let picked = match (&self.reviews.base_override, &branch) {
            (Some(base_override), Some(branch)) if &base_override.branch == branch => {
                Some(base_override.base.clone())
            }
            (Some(_), Some(_)) => {
                // It was picked for another branch, which the worktree left.
                self.change(cx, |reviews| reviews.base_override = None);
                None
            }
            // HEAD is detached, e.g. in the middle of a rebase.
            (Some(base_override), None) => Some(base_override.base.clone()),
            (None, _) => None,
        };
        let repository = self.repository.clone();
        self.refresh_task = cx.spawn(async move |this, cx| {
            let is_base_picked = picked.is_some();
            let base = match picked {
                Some(base) => Some(base),
                None => closest_ancestor_branch(&repository, branch.clone(), cx)
                    .await
                    .log_err()
                    .flatten(),
            };
            let commits = match &base {
                Some(base) => Some(load_reviewed_commits(&repository, base, cx).await),
                None => None,
            };
            this.update(cx, |this, cx| {
                this.branch = branch;
                this.base = base;
                this.is_base_picked = is_base_picked;
                match commits {
                    Some(Ok((merge_base, commits))) => {
                        this.merge_base = merge_base;
                        this.commits = commits;
                        this.commits_state = LoadState::Loaded;
                    }
                    Some(Err(error)) => {
                        this.merge_base = None;
                        this.commits.clear();
                        this.commits_state =
                            LoadState::Error(format!("Failed to load commits: {error:#}").into());
                    }
                    None => {
                        this.merge_base = None;
                        this.commits.clear();
                        this.commits_state = LoadState::Error("No base branch found".into());
                    }
                }
                this.reassociate_committed_comments(cx);
                cx.notify();
            })
            .ok();
        });
    }

    /// Makes `base` the base for the current branch.
    pub fn set_base(&mut self, base: String, cx: &mut Context<Self>) {
        // With a detached HEAD, the pick lasts until a branch is checked out.
        let branch = self.current_branch(cx).unwrap_or_default();
        self.change(cx, |reviews| {
            reviews.base_override = Some(BaseOverride { branch, base });
        });
        self.refresh(cx);
    }

    /// Comments on the commit that's HEAD, returning the comment's id.
    pub fn add_general_comment(&mut self, text: &str, cx: &mut Context<Self>) -> Option<String> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let commit = self.head_sha(cx);
        let mut comment_id = None;
        self.change(cx, |reviews| {
            comment_id =
                Some(reviews.add_comment(text, CommentTarget::General { commit }, Utc::now()));
        });
        comment_id
    }

    /// Comments on lines, returning the comment's id.
    pub fn add_line_comment(
        &mut self,
        capture: LineCapture,
        text: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<String>> {
        let repository = self.repository.clone();
        let reviewed = self.reviewed_shas();
        let head_sha = self.head_sha(cx);
        let merge_base = self.merge_base.clone();
        let store = self.store_snapshot(capture.text.clone(), cx);
        cx.spawn(async move |this, cx| {
            let key = store.await?;
            let (commit, start_line, end_line) = match (&capture.side, &capture.source) {
                (CapturedSide::New, CommentSource::Commit(sha)) => (
                    Some(sha.clone()),
                    capture.rows.start + 1,
                    capture.rows.end + 1,
                ),
                (CapturedSide::New, CommentSource::WorkingTree) => {
                    associate_lines(
                        &repository,
                        &capture.repo_path,
                        None,
                        &capture.text,
                        capture.rows.clone(),
                        &reviewed,
                        head_sha.as_deref(),
                        cx,
                    )
                    .await
                }
                (CapturedSide::Old { base_rows }, CommentSource::WorkingTree) => {
                    (merge_base.clone(), base_rows.start + 1, base_rows.end + 1)
                }
                (CapturedSide::Old { base_rows }, CommentSource::Commit(sha)) => {
                    let parent = run_git(
                        &repository,
                        args(&["rev-parse", &format!("{sha}^")]),
                        None,
                        cx,
                    )
                    .await
                    .ok()
                    .map(|parent| parent.trim().to_string());
                    (parent, base_rows.start + 1, base_rows.end + 1)
                }
            };
            let target = CommentTarget::Line(LineTarget {
                path: capture.repo_path.clone(),
                side: match capture.side {
                    CapturedSide::New => Side::New,
                    CapturedSide::Old { .. } => Side::Old,
                },
                start_line,
                end_line,
                commit,
                excerpt: capture.excerpt.clone(),
                anchor: SnapshotAnchor {
                    snapshot: key,
                    start_row: capture.rows.start,
                    end_row: capture.rows.end,
                },
            });
            this.update(cx, |this, cx| {
                let mut comment_id = String::new();
                this.change(cx, |reviews| {
                    comment_id = reviews.add_comment(&text, target, Utc::now());
                });
                comment_id
            })
        })
    }

    /// Comments on lines of a file's new version: in the working tree, or in
    /// `commit`. `rows` is 0-based and inclusive.
    pub fn add_line_comment_at(
        &mut self,
        project: &Entity<Project>,
        repo_path: String,
        commit: Option<String>,
        rows: Range<u32>,
        text: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<String>> {
        let parsed_path = match RepoPath::new(&repo_path) {
            Ok(path) => path,
            Err(error) => return Task::ready(Err(error)),
        };
        let repository = self.repository.clone();
        let file_text: Task<Result<String>> = match &commit {
            Some(sha) => {
                let revision = format!("{sha}:{repo_path}");
                cx.spawn(async move |_, cx| {
                    run_git(&repository, args(&["show", &revision]), None, cx).await
                })
            }
            None => {
                let Some(project_path) = repository
                    .read(cx)
                    .repo_path_to_project_path(&parsed_path, cx)
                else {
                    return Task::ready(Err(anyhow!("{repo_path} isn't in the project")));
                };
                let open_buffer =
                    project.update(cx, |project, cx| project.open_buffer(project_path, cx));
                cx.spawn(async move |_, cx| {
                    let buffer = open_buffer.await?;
                    Ok(buffer.read_with(cx, |buffer, _| buffer.text()))
                })
            }
        };
        let rows = rows.start.min(rows.end)..rows.start.max(rows.end);
        cx.spawn(async move |this, cx| {
            let file_text = file_text.await?;
            let capture = LineCapture {
                excerpt: lines_text(&file_text, rows.clone()),
                repo_path,
                source: commit.map_or(CommentSource::WorkingTree, CommentSource::Commit),
                text: Arc::from(file_text),
                rows,
                side: CapturedSide::New,
            };
            this.update(cx, |this, cx| this.add_line_comment(capture, text, cx))?
                .await
        })
    }

    pub fn edit_comment(&mut self, comment_id: &str, text: &str, cx: &mut Context<Self>) {
        let mut edited = None;
        self.change(cx, |reviews| {
            if reviews.edit_comment(comment_id, text, Utc::now()) {
                edited = reviews
                    .comment(comment_id)
                    .map(|(_, comment)| comment.text.clone());
            }
        });
        if let Some(text) = edited {
            cx.emit(ReviewModelEvent::CommentEdited {
                comment_id: comment_id.to_string(),
                text,
            });
        }
    }

    pub fn delete_comment(&mut self, comment_id: &str, cx: &mut Context<Self>) {
        let mut deleted = false;
        self.change(cx, |reviews| deleted = reviews.delete_comment(comment_id));
        if deleted {
            cx.emit(ReviewModelEvent::CommentDeleted {
                comment_id: comment_id.to_string(),
            });
        }
    }

    pub fn set_addressed(&mut self, comment_id: &str, addressed: bool, cx: &mut Context<Self>) {
        self.change(cx, |reviews| {
            reviews.set_addressed(comment_id, addressed, Utc::now());
        });
    }

    /// Re-anchors draft comments on lines that have been committed since.
    fn reassociate_committed_comments(&mut self, cx: &mut Context<Self>) {
        if !self.is_loaded || self.commits_state != LoadState::Loaded || self.is_submitting {
            return;
        }
        let repository = self.repository.read(cx);
        let pending = self
            .reviews
            .draft()
            .into_iter()
            .flat_map(|draft| draft.comments.iter())
            .filter_map(|comment| match &comment.target {
                CommentTarget::Line(target) if target.commit.is_none() => {
                    let repo_path = RepoPath::new(&target.path).ok()?;
                    repository
                        .status_for_path(&repo_path)
                        .is_none()
                        .then(|| (comment.id.clone(), target.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return;
        }
        let reviewed = self.reviewed_shas();
        let head_sha = self.head_sha(cx);
        let repository = self.repository.clone();
        self.reassociate_task = cx.spawn(async move |this, cx| {
            let Some(model) = this.upgrade() else {
                return;
            };
            let mut updates = Vec::new();
            for (comment_id, target) in pending {
                match reassociate_comment(
                    &model,
                    &repository,
                    &target,
                    &reviewed,
                    head_sha.as_deref(),
                    cx,
                )
                .await
                {
                    Ok(target) => updates.push((comment_id, target)),
                    Err(error) => log::debug!("comment stays uncommitted: {error:#}"),
                }
            }
            if updates.is_empty() {
                return;
            }
            model.update(cx, |model, cx| {
                model.change(cx, |reviews| {
                    for (comment_id, target) in updates {
                        if let Some(comment) = reviews.comment_mut(&comment_id) {
                            comment.target = CommentTarget::Line(target);
                        }
                    }
                })
            });
        });
    }

    /// Why the draft can't be submitted, if it can't. `has_input` tells
    /// whether the client has a comment it would add first.
    pub fn submit_blocker(&self, has_input: bool, cx: &App) -> Option<&'static str> {
        if self.is_submitting {
            return Some("Submitting…");
        }
        if !self.is_loaded {
            return Some("Loading…");
        }
        let has_draft_comments = self
            .reviews
            .draft()
            .is_some_and(|draft| !draft.comments.is_empty());
        if !has_draft_comments && !has_input {
            return Some("Add comments to submit a review");
        }
        let commented_paths = self
            .reviews
            .draft()
            .into_iter()
            .flat_map(|draft| draft.comments.iter())
            .filter_map(|comment| match &comment.target {
                CommentTarget::Line(target) => Some(target.path.as_str()),
                CommentTarget::General { .. } => None,
            })
            .collect::<HashSet<_>>();
        let has_uncommitted_changes = self.repository.read(cx).cached_status().any(|entry| {
            entry.status != FileStatus::Untracked
                || commented_paths.contains(entry.repo_path.as_unix_str())
        });
        if has_uncommitted_changes {
            return Some("Commit all changes before submitting the review");
        }
        if self.head_sha(cx).is_none() {
            return Some("Nothing is committed yet");
        }
        None
    }

    /// Submits the draft, returning the review's id. Comments on lines that
    /// were committed since they were made move to their commit first.
    pub fn submit(&mut self, cx: &mut Context<Self>) -> Task<Result<String>> {
        if let Some(blocker) = self.submit_blocker(false, cx) {
            return Task::ready(Err(anyhow!("{blocker}")));
        }
        self.is_submitting = true;
        cx.notify();
        let reviewed = self.reviewed_shas();
        let head_sha = self.head_sha(cx);
        let branch = self.branch.clone();
        let base = self.base.clone();
        let repository = self.repository.clone();
        let pending = self
            .reviews
            .draft()
            .into_iter()
            .flat_map(|draft| draft.comments.iter())
            .filter_map(|comment| match &comment.target {
                CommentTarget::Line(target) if target.commit.is_none() => {
                    Some((comment.id.clone(), target.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        cx.spawn(async move |this, cx| {
            let model = this.upgrade().context("the review was closed")?;
            let mut updates = Vec::new();
            for (comment_id, mut target) in pending {
                match reassociate_comment(
                    &model,
                    &repository,
                    &target,
                    &reviewed,
                    head_sha.as_deref(),
                    cx,
                )
                .await
                {
                    Ok(reassociated) => target = reassociated,
                    // E.g. the file was deleted since: the comment is about
                    // what HEAD did to it.
                    Err(error) => {
                        log::warn!("submitting a comment as of HEAD: {error:#}");
                        target.commit = head_sha.clone();
                    }
                }
                updates.push((comment_id, target));
            }
            model.update(cx, |model, cx| {
                model.is_submitting = false;
                let mut submitted = None;
                model.change(cx, |reviews| {
                    for (comment_id, target) in updates {
                        if let Some(comment) = reviews.comment_mut(&comment_id) {
                            comment.target = CommentTarget::Line(target);
                        }
                    }
                    submitted = reviews.submit_draft(Utc::now(), branch, base, head_sha);
                });
                submitted.context("the review has no comments")
            })
        })
    }

    /// The files that differ between the base and the working tree, with
    /// their text in both. Untracked files count as added.
    pub fn diff_files(
        &self,
        project: &Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<ReviewDiffFile>>> {
        let Some(merge_base) = self.merge_base.clone() else {
            return Task::ready(Err(anyhow!("there is no base to compare with")));
        };
        let repository = self.repository.clone();
        let project = project.clone();
        let untracked = repository
            .read(cx)
            .cached_status()
            .filter(|entry| entry.status == FileStatus::Untracked)
            .map(|entry| entry.repo_path.as_unix_str().to_string())
            .collect::<Vec<_>>();
        cx.spawn(async move |_, cx| {
            let output = run_git(
                &repository,
                args(&[
                    "diff",
                    "--name-status",
                    "-z",
                    "--no-renames",
                    &merge_base,
                    "--",
                ]),
                None,
                cx,
            )
            .await?;
            let mut entries = Vec::new();
            let mut fields = output.split('\0').filter(|field| !field.is_empty());
            while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
                let status = match status.chars().next() {
                    Some('A') => ReviewFileStatus::Added,
                    Some('D') => ReviewFileStatus::Deleted,
                    _ => ReviewFileStatus::Modified,
                };
                entries.push((path.to_string(), status));
            }
            for path in untracked {
                if !entries.iter().any(|(existing, _)| existing == &path) {
                    entries.push((path, ReviewFileStatus::Added));
                }
            }
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            entries.truncate(MAX_DIFF_FILES);

            let mut files = Vec::with_capacity(entries.len());
            for (path, status) in entries {
                let old_text = if status == ReviewFileStatus::Added {
                    None
                } else {
                    run_git(
                        &repository,
                        args(&["show", &format!("{merge_base}:{path}")]),
                        None,
                        cx,
                    )
                    .await
                    .ok()
                };
                let new_text = if status == ReviewFileStatus::Deleted {
                    None
                } else {
                    let project_path = RepoPath::new(&path).ok().and_then(|repo_path| {
                        repository.read_with(cx, |repository, cx| {
                            repository.repo_path_to_project_path(&repo_path, cx)
                        })
                    });
                    match project_path {
                        Some(project_path) => {
                            let open_buffer = project
                                .update(cx, |project, cx| project.open_buffer(project_path, cx));
                            match open_buffer.await {
                                Ok(buffer) => Some(buffer.read_with(cx, |buffer, _| buffer.text())),
                                Err(_) => None,
                            }
                        }
                        None => None,
                    }
                };
                let is_binary = [&old_text, &new_text].iter().any(|text| {
                    text.as_ref()
                        .is_some_and(|text| text.len() > MAX_DIFF_FILE_BYTES || text.contains('\0'))
                });
                files.push(ReviewDiffFile {
                    path,
                    status,
                    old_text: if is_binary { None } else { old_text },
                    new_text: if is_binary { None } else { new_text },
                    is_binary,
                });
            }
            Ok(files)
        })
    }
}

/// An editor whose review comments go into the review.
struct AttachedEditor {
    editor: WeakEntity<Editor>,
    source: CommentSource,
    /// The editor's ids of comments, to the review's.
    comment_ids: HashMap<usize, String>,
    /// Edits (or deletions, as `None`) made before the review had the comment.
    early_changes: HashMap<usize, Option<String>>,
    _subscription: Subscription,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReviewTab {
    Review,
    Commits,
}

pub(crate) enum ReviewViewEvent {
    Exit,
}

/// The git panel in review mode.
pub(crate) struct ReviewView {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    repository: Entity<Repository>,
    model: Entity<ReviewModel>,
    is_model_loaded: bool,
    focus_handle: FocusHandle,
    active_tab: ReviewTab,
    comment_editor: Entity<Editor>,
    editing: Option<(String, Entity<Editor>, Subscription)>,
    expanded_comments: HashSet<String>,
    collapsed_reviews: HashSet<String>,
    attached: Vec<AttachedEditor>,
    scroll_handle: ScrollHandle,
    submit_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ReviewViewEvent> for ReviewView {}

impl Focusable for ReviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// An editor for a review comment, which can span lines: enter inserts a
/// newline and `SubmitDiffReviewComment` (ctrl-enter) calls `on_confirm`.
fn comment_editor(
    placeholder: &str,
    view: WeakEntity<ReviewView>,
    on_confirm: impl Fn(&mut ReviewView, &mut Window, &mut Context<ReviewView>) + 'static,
    window: &mut Window,
    cx: &mut Context<ReviewView>,
) -> (Entity<Editor>, Subscription) {
    let editor = cx.new(|cx| {
        let mut editor = Editor::auto_height(1, COMMENT_EDITOR_MAX_LINES, window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor.set_show_gutter(false, cx);
        editor.set_completion_provider(Some(Rc::new(TagCompletionProvider)));
        editor
    });
    let on_confirm = Rc::new(on_confirm);
    let subscription = editor.update(cx, |editor, _| {
        editor.register_action(
            move |_: &editor::actions::SubmitDiffReviewComment, window, cx| {
                let on_confirm = on_confirm.clone();
                view.update(cx, |view, cx| on_confirm(view, window, cx))
                    .ok();
            },
        )
    });
    (editor, subscription)
}

impl ReviewView {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        model: Entity<ReviewModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let repository = model.read(cx).repository().clone();
        let (comment_editor, comment_editor_subscription) = comment_editor(
            "Comment on the latest commit… (ctrl-enter to add)",
            cx.weak_entity(),
            |this, window, cx| this.add_general_comment(window, cx),
            window,
            cx,
        );

        let mut subscriptions = vec![
            comment_editor_subscription,
            cx.observe_in(&model, window, |this, model, window, cx| {
                if !this.is_model_loaded && model.read(cx).is_loaded() {
                    this.is_model_loaded = true;
                    let editors = this
                        .attached
                        .iter()
                        .filter_map(|attached| attached.editor.upgrade())
                        .collect::<Vec<_>>();
                    for editor in editors {
                        this.restore_comments(&editor, window, cx);
                    }
                }
                this.prune_editing(cx);
                cx.notify();
            }),
            cx.subscribe_in(&model, window, Self::handle_model_event),
            cx.observe(&repository, |_, _, cx| cx.notify()),
        ];
        if let Some(workspace_entity) = workspace.upgrade() {
            subscriptions.push(cx.subscribe_in(
                &workspace_entity,
                window,
                |this, _, event: &workspace::Event, window, cx| {
                    if let workspace::Event::ItemAdded { item } = event {
                        this.attach_item(item.as_ref(), window, cx);
                    }
                },
            ));
        }
        let git_store = project.read(cx).git_store().clone();
        subscriptions.push(
            cx.subscribe(&git_store, |this, _, event: &GitStoreEvent, cx| {
                if let GitStoreEvent::RepositoryRemoved(id) = event
                    && *id == this.repository.read(cx).id
                {
                    cx.emit(ReviewViewEvent::Exit);
                }
            }),
        );

        let is_model_loaded = model.read(cx).is_loaded();
        let mut this = Self {
            workspace,
            project,
            repository,
            model,
            is_model_loaded,
            focus_handle: cx.focus_handle(),
            active_tab: ReviewTab::Review,
            comment_editor,
            editing: None,
            expanded_comments: HashSet::default(),
            collapsed_reviews: HashSet::default(),
            attached: Vec::new(),
            scroll_handle: ScrollHandle::new(),
            submit_task: None,
            _subscriptions: subscriptions,
        };
        this.attach_open_items(window, cx);
        this
    }

    fn pick_base(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let model = self.model.downgrade();
        let repository = self.repository.clone();
        let selected = self
            .model
            .read(cx)
            .base()
            .map(|base| SharedString::from(base.to_string()));
        let on_select: branch_picker::SelectBranchCallback =
            Arc::new(move |branch, _window, cx| {
                let base = branch.name().to_string();
                model.update(cx, |model, cx| model.set_base(base, cx)).ok();
            });
        let workspace_handle = workspace.downgrade();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                branch_picker::select_modal(
                    workspace_handle,
                    Some(repository),
                    selected,
                    on_select,
                    window,
                    cx,
                )
            });
        });
    }

    fn handle_model_event(
        &mut self,
        _: &Entity<ReviewModel>,
        event: &ReviewModelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (comment_id, text) = match event {
            ReviewModelEvent::CommentEdited { comment_id, text } => (comment_id, Some(text)),
            ReviewModelEvent::CommentDeleted { comment_id } => (comment_id, None),
        };
        for attached in &mut self.attached {
            let Some(local_id) = attached
                .comment_ids
                .iter()
                .find_map(|(local_id, id)| (id == comment_id).then_some(*local_id))
            else {
                continue;
            };
            let Some(editor) = attached.editor.upgrade() else {
                continue;
            };
            match text {
                Some(text) => editor.update(cx, |editor, cx| {
                    editor.set_review_comment_text(local_id, text.clone(), window, cx)
                }),
                None => {
                    attached.comment_ids.remove(&local_id);
                    editor.update(cx, |editor, cx| {
                        editor.forget_review_comment(local_id, window, cx)
                    });
                }
            }
        }
    }

    fn attach_open_items(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let items = workspace
            .read(cx)
            .items(cx)
            .map(|item| item.boxed_clone())
            .collect::<Vec<_>>();
        for item in items {
            self.attach_item(item.as_ref(), window, cx);
        }
    }

    fn attach_item(&mut self, item: &dyn ItemHandle, window: &mut Window, cx: &mut Context<Self>) {
        let repository_id = self.repository.read(cx).id;
        let (editor, source) = if let Some(branch_diff) = item.downcast::<BranchDiff>() {
            let branch_diff = branch_diff.read(cx);
            if branch_diff
                .repo(cx)
                .is_none_or(|repo| repo.read(cx).id != repository_id)
            {
                return;
            }
            let editor = branch_diff
                .diff_multibuffer()
                .read(cx)
                .editor()
                .read(cx)
                .rhs_editor()
                .clone();
            (editor, CommentSource::WorkingTree)
        } else if let Some(commit_view) = item.downcast::<CommitView>() {
            let commit_view = commit_view.read(cx);
            if commit_view.repository().read(cx).id != repository_id {
                return;
            }
            let editor = commit_view.editor().read(cx).rhs_editor().clone();
            (
                editor,
                CommentSource::Commit(commit_view.commit().sha.to_string()),
            )
        } else {
            return;
        };
        self.attached
            .retain(|attached| attached.editor.upgrade().is_some());
        if self
            .attached
            .iter()
            .any(|attached| attached.editor == editor.downgrade())
        {
            return;
        }
        editor.update(cx, |editor, _| {
            editor.set_review_comment_completion_provider(Some(Rc::new(TagCompletionProvider)));
        });
        let subscription = cx.subscribe_in(&editor, window, Self::handle_editor_event);
        self.attached.push(AttachedEditor {
            editor: editor.downgrade(),
            source,
            comment_ids: HashMap::default(),
            early_changes: HashMap::default(),
            _subscription: subscription,
        });
        self.restore_comments(&editor, window, cx);
    }

    fn handle_editor_event(
        &mut self,
        editor: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(attached_ix) = self
            .attached
            .iter()
            .position(|attached| attached.editor == editor.downgrade())
        else {
            return;
        };
        match event {
            EditorEvent::ReviewCommentAdded { id, range, text } => {
                self.capture_comment(
                    attached_ix,
                    editor,
                    *id,
                    range.clone(),
                    text.clone(),
                    window,
                    cx,
                );
            }
            EditorEvent::ReviewCommentEdited { id, text } => {
                let attached = &mut self.attached[attached_ix];
                match attached.comment_ids.get(id).cloned() {
                    Some(comment_id) => self
                        .model
                        .update(cx, |model, cx| model.edit_comment(&comment_id, text, cx)),
                    None => {
                        attached.early_changes.insert(*id, Some(text.clone()));
                    }
                }
            }
            EditorEvent::ReviewCommentDeleted { id } => {
                let attached = &mut self.attached[attached_ix];
                match attached.comment_ids.remove(id) {
                    Some(comment_id) => self
                        .model
                        .update(cx, |model, cx| model.delete_comment(&comment_id, cx)),
                    None => {
                        attached.early_changes.insert(*id, None);
                    }
                }
            }
            EditorEvent::BufferRangesUpdated { .. } => {
                self.restore_comments(editor, window, cx);
            }
            _ => {}
        }
    }

    fn repo_path_for_buffer(
        &self,
        source: &CommentSource,
        buffer: &BufferSnapshot,
        cx: &App,
    ) -> Option<RepoPath> {
        let file = buffer.file()?;
        match source {
            // Commits' files are already named by their path in the repository.
            CommentSource::Commit(_) => Some(RepoPath::from_rel_path(file.path())),
            CommentSource::WorkingTree => self.repository.read(cx).project_path_to_repo_path(
                &ProjectPath {
                    worktree_id: file.worktree_id(cx),
                    path: file.path().clone(),
                },
                cx,
            ),
        }
    }

    /// Adds a comment made in an editor to the draft review.
    fn capture_comment(
        &mut self,
        attached_ix: usize,
        editor: &Entity<Editor>,
        local_id: usize,
        range: Range<multi_buffer::Anchor>,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = self.attached[attached_ix].source.clone();
        let Some(capture) = self.comment_lines(source, editor, &range, cx) else {
            log::error!("couldn't find the lines of a review comment");
            return;
        };
        let editor = editor.downgrade();
        let workspace = self.workspace.clone();
        let early_change = self.attached[attached_ix].early_changes.remove(&local_id);
        if matches!(early_change, Some(None)) {
            return;
        }
        let text = match early_change {
            Some(Some(edited)) => edited,
            _ => text,
        };
        let add = self.model.update(cx, |model, cx| {
            model.add_line_comment(capture, text.clone(), cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let comment_id = match add.await {
                Ok(comment_id) => comment_id,
                Err(error) => {
                    if let Some(workspace) = workspace.upgrade() {
                        cx.update(|_, cx| {
                            crate::git_panel::show_error_toast(
                                workspace,
                                "save review comment",
                                error,
                                cx,
                            )
                        })
                        .ok();
                    }
                    return;
                }
            };
            this.update_in(cx, |this, window, cx| {
                let Some(attached) = this
                    .attached
                    .iter_mut()
                    .find(|attached| attached.editor == editor)
                else {
                    return;
                };
                attached.comment_ids.insert(local_id, comment_id.clone());
                // Changes made while the comment was being saved.
                match attached.early_changes.remove(&local_id) {
                    Some(None) => {
                        this.model
                            .update(cx, |model, cx| model.delete_comment(&comment_id, cx));
                        return;
                    }
                    Some(Some(edited)) => {
                        this.model
                            .update(cx, |model, cx| model.edit_comment(&comment_id, &edited, cx));
                        return;
                    }
                    None => {}
                }
                let normalized = this
                    .model
                    .read(cx)
                    .reviews()
                    .comment(&comment_id)
                    .map(|(_, comment)| comment.text.clone());
                if let (Some(normalized), Some(editor)) = (normalized, editor.upgrade())
                    && normalized != text
                {
                    editor.update(cx, |editor, cx| {
                        editor.set_review_comment_text(local_id, normalized, window, cx)
                    });
                }
            })
            .ok();
        })
        .detach();
    }

    /// What lines of which file a comment in `editor` is on.
    fn comment_lines(
        &self,
        source: CommentSource,
        editor: &Entity<Editor>,
        range: &Range<multi_buffer::Anchor>,
        cx: &App,
    ) -> Option<LineCapture> {
        let multibuffer = editor.read(cx).buffer().read(cx);
        let snapshot = multibuffer.snapshot(cx);
        let (main_anchor, main_buffer) = snapshot.anchor_to_buffer_anchor(range.start)?;
        let main_buffer = main_buffer.clone();
        let repo_path = self.repo_path_for_buffer(&source, &main_buffer, cx)?;
        let start = range.start.to_point(&snapshot);
        let end = range.end.to_point(&snapshot);
        let (start_buffer, start_point) = snapshot.point_to_buffer_point(start)?;
        let end_point = snapshot
            .point_to_buffer_point(end)
            .filter(|(end_buffer, _)| end_buffer.remote_id() == start_buffer.remote_id())
            .map_or(start_point, |(_, end_point)| end_point);
        let rows = start_point.row..end_point.row.max(start_point.row);
        let excerpt = buffer_lines_text(start_buffer, rows.clone());

        let (side, rows) = if start_buffer.remote_id() == main_buffer.remote_id() {
            (CapturedSide::New, rows)
        } else {
            // Deleted lines are anchored where they were in the file.
            let row = main_anchor.to_point(&main_buffer).row;
            (CapturedSide::Old { base_rows: rows }, row..row)
        };
        Some(LineCapture {
            repo_path: repo_path.as_unix_str().to_string(),
            source,
            text: Arc::from(main_buffer.text()),
            rows,
            side,
            excerpt,
        })
    }

    /// Shows the review's comments that are in `editor`.
    fn restore_comments(
        &mut self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(attached_ix) = self
            .attached
            .iter()
            .position(|attached| attached.editor == editor.downgrade())
        else {
            return;
        };
        let source = self.attached[attached_ix].source.clone();
        let shown = self.attached[attached_ix]
            .comment_ids
            .values()
            .cloned()
            .collect::<HashSet<_>>();
        let model = self.model.read(cx);
        if !model.is_loaded() {
            return;
        }

        let multibuffer = editor.read(cx).buffer().read(cx);
        let snapshot = multibuffer.snapshot(cx);
        let buffers_by_path = multibuffer
            .all_buffers()
            .into_iter()
            .filter_map(|buffer| {
                let buffer_snapshot = buffer.read(cx).snapshot();
                let repo_path = self.repo_path_for_buffer(&source, &buffer_snapshot, cx)?;
                Some((repo_path.as_unix_str().to_string(), buffer_snapshot))
            })
            .collect::<HashMap<_, _>>();

        let mut to_restore = Vec::new();
        for review in &model.reviews().reviews {
            for comment in &review.comments {
                if shown.contains(&comment.id) || comment.is_addressed() {
                    continue;
                }
                let CommentTarget::Line(target) = &comment.target else {
                    continue;
                };
                let Some(buffer) = buffers_by_path.get(&target.path) else {
                    continue;
                };
                let rows = match &source {
                    CommentSource::Commit(sha) => {
                        if target.commit.as_deref() != Some(sha.as_str())
                            || target.side != Side::New
                        {
                            continue;
                        }
                        target.start_line.saturating_sub(1)..target.end_line.saturating_sub(1)
                    }
                    CommentSource::WorkingTree => {
                        let Some(relocation) = model.current_location(target, &buffer.text())
                        else {
                            continue;
                        };
                        relocation.start_row..relocation.end_row
                    }
                };
                let Some(range) = multibuffer_range(&snapshot, buffer, rows) else {
                    continue;
                };
                to_restore.push((comment.id.clone(), comment.text.clone(), range));
            }
        }

        for (comment_id, text, range) in to_restore {
            let local_id = editor.update(cx, |editor, cx| {
                editor.restore_review_comment(range, text, window, cx)
            });
            if let Some(local_id) = local_id {
                self.attached[attached_ix]
                    .comment_ids
                    .insert(local_id, comment_id);
            }
        }
    }

    fn has_input(&self, cx: &App) -> bool {
        !self.comment_editor.read(cx).text(cx).trim().is_empty()
    }

    fn submit_blocker(&self, cx: &App) -> Option<&'static str> {
        if self.submit_task.is_some() {
            return Some("Submitting…");
        }
        self.model.read(cx).submit_blocker(self.has_input(cx), cx)
    }

    fn add_general_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.comment_editor.read(cx).text(cx);
        let added = self
            .model
            .update(cx, |model, cx| model.add_general_comment(&text, cx));
        if added.is_some() {
            self.comment_editor
                .update(cx, |editor, cx| editor.clear(window, cx));
        }
    }

    fn submit_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.submit_blocker(cx).is_some() {
            return;
        }
        self.add_general_comment(window, cx);
        let submit = self.model.update(cx, |model, cx| model.submit(cx));
        let workspace = self.workspace.clone();
        self.submit_task = Some(cx.spawn_in(window, async move |this, cx| {
            if let Err(error) = submit.await
                && let Some(workspace) = workspace.upgrade()
            {
                cx.update(|_, cx| {
                    crate::git_panel::show_error_toast(workspace, "submit review", error, cx)
                })
                .ok();
            }
            this.update(cx, |this, cx| {
                this.submit_task = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn start_editing(&mut self, comment_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self
            .model
            .read(cx)
            .reviews()
            .comment(comment_id)
            .map(|(_, comment)| comment.text.clone())
        else {
            return;
        };
        let edited_id = comment_id.to_string();
        let (editor, subscription) = comment_editor(
            "Edit comment…",
            cx.weak_entity(),
            move |this, window, cx| this.finish_editing(&edited_id, window, cx),
            window,
            cx,
        );
        editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            editor.move_to_end(&Default::default(), window, cx);
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.editing = Some((comment_id.to_string(), editor, subscription));
        cx.notify();
    }

    fn finish_editing(&mut self, comment_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((editing_id, editor, _)) = self.editing.take() else {
            return;
        };
        if editing_id == comment_id {
            let text = editor.read(cx).text(cx).trim().to_string();
            if !text.is_empty() {
                self.model
                    .update(cx, |model, cx| model.edit_comment(comment_id, &text, cx));
            }
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn cancel_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    fn prune_editing(&mut self, cx: &App) {
        if let Some((comment_id, _, _)) = &self.editing
            && self.model.read(cx).reviews().comment(comment_id).is_none()
        {
            self.editing = None;
        }
    }

    fn toggle_addressed(&mut self, comment_id: &str, cx: &mut Context<Self>) {
        self.expanded_comments.remove(comment_id);
        self.model.update(cx, |model, cx| {
            let addressed = model
                .reviews()
                .comment(comment_id)
                .is_some_and(|(_, comment)| comment.is_addressed());
            model.set_addressed(comment_id, !addressed, cx);
        });
    }

    fn delete_comment(&mut self, comment_id: &str, cx: &mut Context<Self>) {
        self.model
            .update(cx, |model, cx| model.delete_comment(comment_id, cx));
    }

    fn copy(text: String, cx: &mut App) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn open_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_branch_diff(None, window, cx);
    }

    fn open_branch_diff(
        &mut self,
        scroll_to: Option<(RepoPath, Option<Point>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(base) = self.model.read(cx).base().map(str::to_string) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let repository = self.repository.clone();
        let task = workspace.update(cx, |workspace, cx| {
            BranchDiff::open_for_review(workspace, repository, base.into(), scroll_to, window, cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let branch_diff = task.await?;
            this.update_in(cx, |this, window, cx| {
                this.attach_item(&branch_diff, window, cx)
            })?;
            anyhow::Ok(())
        })
        .detach_and_notify_err(self.workspace.clone(), window, cx);
    }

    fn open_commit(
        &mut self,
        sha: String,
        path: Option<(RepoPath, Option<Point>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = CommitView::open_at_path(
            sha,
            self.repository.downgrade(),
            self.workspace.clone(),
            path,
            window,
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let commit_view = task.await?;
            this.update_in(cx, |this, window, cx| {
                this.attach_item(&commit_view, window, cx)
            })?;
            anyhow::Ok(())
        })
        .detach_and_notify_err(self.workspace.clone(), window, cx);
    }

    fn open_uncommitted_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let repository = self.repository.clone();
        workspace.update(cx, |workspace, cx| {
            ProjectDiff::deploy_at_repo_path(workspace, repository, None, window, cx);
        });
    }

    /// Shows the comment's lines where they are now.
    fn jump_to_comment(&mut self, comment_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, comment)) = self.model.read(cx).reviews().comment(comment_id) else {
            return;
        };
        let target = match &comment.target {
            CommentTarget::General { commit } => {
                if let Some(commit) = commit.clone() {
                    self.open_commit(commit, None, window, cx);
                }
                return;
            }
            CommentTarget::Line(target) => target.clone(),
        };
        let Ok(repo_path) = RepoPath::new(&target.path) else {
            return;
        };
        let has_base = self.model.read(cx).base().is_some();
        // The diff shows files as they are in their buffers, which works for
        // remote projects too.
        let open_buffer = self
            .repository
            .read(cx)
            .repo_path_to_project_path(&repo_path, cx)
            .map(|project_path| {
                self.project
                    .update(cx, |project, cx| project.open_buffer(project_path, cx))
            });
        cx.spawn_in(window, async move |this, cx| {
            let current_text = match open_buffer {
                Some(open_buffer) => open_buffer
                    .await
                    .ok()
                    .map(|buffer| buffer.read_with(cx, |buffer, _| buffer.text())),
                None => None,
            };
            this.update_in(cx, |this, window, cx| {
                let relocated = current_text.as_ref().and_then(|current_text| {
                    this.model.read(cx).current_location(&target, current_text)
                });
                match (relocated, &target.commit) {
                    (Some(relocation), _) if has_base => this.open_branch_diff(
                        Some((repo_path, Some(Point::new(relocation.start_row, 0)))),
                        window,
                        cx,
                    ),
                    // The file is gone: show it in the commit the comment is about.
                    (_, Some(commit)) => this.open_commit(
                        commit.clone(),
                        Some((
                            repo_path,
                            Some(Point::new(target.start_line.saturating_sub(1), 0)),
                        )),
                        window,
                        cx,
                    ),
                    (_, None) => {}
                }
            })
            .ok();
        })
        .detach();
    }

    fn comment_counts_by_commit(&self, cx: &App) -> (HashMap<String, usize>, usize) {
        let mut counts = HashMap::default();
        let mut uncommitted = 0;
        for review in &self.model.read(cx).reviews().reviews {
            for comment in &review.comments {
                match comment.commit() {
                    Some(commit) => *counts.entry(commit.to_string()).or_default() += 1,
                    None => uncommitted += 1,
                }
            }
        }
        (counts, uncommitted)
    }
}

fn buffer_lines_text(buffer: &BufferSnapshot, rows: Range<u32>) -> String {
    let end_row = rows.end.min(buffer.max_point().row);
    let start = Point::new(rows.start.min(end_row), 0);
    let end = Point::new(end_row, buffer.line_len(end_row));
    buffer.text_for_range(start..end).collect()
}

fn multibuffer_range(
    snapshot: &MultiBufferSnapshot,
    buffer: &BufferSnapshot,
    rows: Range<u32>,
) -> Option<Range<multi_buffer::Anchor>> {
    let max_row = buffer.max_point().row;
    let start_row = rows.start.min(max_row);
    let end_row = rows.end.clamp(start_row, max_row);
    let start = snapshot.anchor_in_excerpt(buffer.anchor_after(Point::new(start_row, 0)))?;
    let end = snapshot
        .anchor_in_excerpt(buffer.anchor_before(Point::new(end_row, buffer.line_len(end_row))))
        .unwrap_or(start);
    Some(start..end)
}

fn relative_time(timestamp: i64) -> SharedString {
    let Ok(time) = time::OffsetDateTime::from_unix_timestamp(timestamp) else {
        return SharedString::default();
    };
    let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    time_format::format_localized_timestamp(
        time,
        time::OffsetDateTime::now_utc(),
        local_offset,
        time_format::TimestampFormat::Relative,
    )
    .into()
}

fn absolute_time(timestamp: DateTime<Utc>) -> SharedString {
    timestamp
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
        .into()
}

/// The text of a comment without the tags it starts with.
fn comment_body(comment: &Comment) -> &str {
    let mut body = comment.text.as_str();
    loop {
        let trimmed = body.trim_start_matches(' ');
        let word_end = trimmed.find([' ', '\n']).unwrap_or(trimmed.len());
        if git_review::parse_tag(&trimmed[..word_end]).is_none() {
            return trimmed;
        }
        body = &trimmed[word_end..];
    }
}

impl ReviewView {
    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let draft_count = model
            .reviews()
            .draft()
            .map_or(0, |draft| draft.comments.len());
        let commit_count = model.commits().len();
        let tab = |id: &'static str, label: &'static str, count: usize, tab: ReviewTab| {
            let active = self.active_tab == tab;
            h_flex()
                .id(id)
                .cursor_pointer()
                .h_full()
                .py_1()
                .gap_1()
                .flex_1()
                .justify_center()
                .hover(|style| style.bg(cx.theme().colors().element_hover))
                .border_b_1()
                .when(!active, |style| {
                    style
                        .bg(cx.theme().colors().editor_background.opacity(0.6))
                        .border_color(cx.theme().colors().border.opacity(0.6))
                })
                .child(Label::new(label).when(!active, |label| label.color(Color::Muted)))
                .when(count > 0, |this| {
                    this.child(
                        Label::new(format!("({count})"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.active_tab = tab;
                    cx.notify();
                }))
        };
        h_flex()
            .h(Tab::container_height(cx))
            .w_full()
            .child(tab("review-tab", "Review", draft_count, ReviewTab::Review))
            .child(
                Divider::vertical()
                    .color(ui::DividerColor::BorderFaded)
                    .h_full(),
            )
            .child(tab(
                "commits-tab",
                "Commits",
                commit_count,
                ReviewTab::Commits,
            ))
            .child(
                h_flex()
                    .h_full()
                    .px_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border.opacity(0.6))
                    .child(
                        IconButton::new("exit-review-mode", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Exit Review Mode"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ReviewViewEvent::Exit))),
                    ),
            )
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let base = model.base().map(str::to_string);
        let is_base_picked = model.is_base_picked();
        let base_label: SharedString = match &base {
            Some(base) => base.clone().into(),
            None if model.is_loading_commits() => "…".into(),
            None => "Pick a base".into(),
        };
        h_flex()
            .min_h(Tab::container_height(cx))
            .w_full()
            .pl_1()
            .pr_2()
            .gap_1()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Button::new("review-base", base_label)
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .start_icon(
                        Icon::new(IconName::GitBranch)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text(if is_base_picked {
                        "Base branch (picked for this branch)"
                    } else {
                        "Base branch (the closest ancestor branch)"
                    }))
                    .on_click(cx.listener(|this, _, window, cx| this.pick_base(window, cx))),
            )
            .child(
                Button::new("review-view-diff", "View Diff")
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .start_icon(
                        Icon::new(IconName::Diff)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .disabled(base.is_none())
                    .tooltip(Tooltip::text("Review the changes since the base"))
                    .on_click(cx.listener(|this, _, window, cx| this.open_diff(window, cx))),
            )
    }

    fn render_comment(&self, comment: &Comment, cx: &mut Context<Self>) -> AnyElement {
        let comment_id = comment.id.clone();
        let is_addressed = comment.is_addressed();
        let is_collapsed = is_addressed && !self.expanded_comments.contains(&comment.id);
        let is_editing = self
            .editing
            .as_ref()
            .is_some_and(|(editing_id, _, _)| editing_id == &comment.id);
        let location = git_review::comment_location(comment);
        let icon = match comment.target {
            CommentTarget::General { .. } => IconName::Chat,
            CommentTarget::Line(_) => IconName::FileCode,
        };
        let created = comment.created_at;
        let edited = (comment.updated_at != comment.created_at).then_some(comment.updated_at);
        let body = comment_body(comment).to_string();
        let tags = comment.tags.clone();
        let colors = cx.theme().colors();

        let action_button = |id: &str, icon: IconName, tooltip: &'static str| {
            IconButton::new(SharedString::from(format!("{id}-{}", comment.id)), icon)
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text(tooltip))
        };

        let header = h_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .child(
                IconButton::new(
                    SharedString::from(format!("addressed-{}", comment.id)),
                    if is_addressed {
                        IconName::CheckDouble
                    } else {
                        IconName::Circle
                    },
                )
                .icon_size(IconSize::XSmall)
                .icon_color(if is_addressed {
                    Color::Success
                } else {
                    Color::Muted
                })
                .tooltip(Tooltip::text(if is_addressed {
                    "Mark as Unaddressed"
                } else {
                    "Mark as Addressed"
                }))
                .on_click(cx.listener({
                    let comment_id = comment_id.clone();
                    move |this, _, _, cx| this.toggle_addressed(&comment_id, cx)
                })),
            )
            .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(location)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                ),
            )
            .child(
                div()
                    .id(SharedString::from(format!("comment-time-{}", comment.id)))
                    .flex_none()
                    .child(
                        Label::new(relative_time(created.timestamp()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text(match edited {
                        Some(edited) => format!(
                            "Created {}, edited {}",
                            absolute_time(created),
                            absolute_time(edited)
                        ),
                        None => format!("Created {}", absolute_time(created)),
                    })),
            )
            .when(is_addressed, |this| {
                this.child(
                    IconButton::new(
                        SharedString::from(format!("expand-{}", comment.id)),
                        if is_collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        },
                    )
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text(if is_collapsed {
                        "Expand"
                    } else {
                        "Collapse"
                    }))
                    .on_click(cx.listener({
                        let comment_id = comment_id.clone();
                        move |this, _, _, cx| {
                            if !this.expanded_comments.remove(&comment_id) {
                                this.expanded_comments.insert(comment_id.clone());
                            }
                            cx.notify();
                        }
                    })),
                )
            });

        let content =
            if is_collapsed {
                div().pl_5().child(
                    Label::new(body.lines().next().unwrap_or_default().to_string())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
            } else if is_editing {
                let editor = self.editing.as_ref().map(|(_, editor, _)| editor.clone());
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(colors.border_focused)
                    .bg(colors.editor_background)
                    .key_context(REVIEW_COMMENT_KEY_CONTEXT)
                    .children(editor)
                    .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                        this.cancel_editing(window, cx)
                    }))
            } else {
                div().pl_5().child(
                    v_flex()
                        .gap_1()
                        .when(!tags.is_empty(), |this| {
                            this.child(
                                h_flex().flex_wrap().gap_1().children(
                                    tags.iter()
                                        .map(|tag| Chip::new(format!("#{tag}")).truncate()),
                                ),
                            )
                        })
                        .when(!body.is_empty(), |this| {
                            this.child(Label::new(body.clone()).size(LabelSize::Small))
                        }),
                )
            };

        let actions = (!is_collapsed && !is_editing).then(|| {
            h_flex()
                .pl_4()
                .gap_0p5()
                .child(
                    action_button("edit", IconName::Pencil, "Edit").on_click(cx.listener({
                        let comment_id = comment_id.clone();
                        move |this, _, window, cx| this.start_editing(&comment_id, window, cx)
                    })),
                )
                .child(
                    action_button("delete", IconName::Trash, "Delete").on_click(cx.listener({
                        let comment_id = comment_id.clone();
                        move |this, _, _, cx| this.delete_comment(&comment_id, cx)
                    })),
                )
                .child(
                    action_button("copy-id", IconName::Hash, "Copy Comment ID").on_click({
                        let comment_id = comment_id.clone();
                        move |_, _, cx| Self::copy(comment_id.clone(), cx)
                    }),
                )
                .child(
                    action_button("copy-text", IconName::Copy, "Copy Comment as Text").on_click({
                        let text = git_review::comment_as_text(comment);
                        move |_, _, cx| Self::copy(text.clone(), cx)
                    }),
                )
        });

        v_flex()
            .id(SharedString::from(format!("review-comment-{}", comment.id)))
            .w_full()
            .p_1()
            .gap_0p5()
            .rounded_sm()
            .border_1()
            .border_color(colors.border_variant)
            .when(is_addressed, |this| this.opacity(0.7))
            .when(!is_editing, |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(colors.element_hover))
                    .tooltip(Tooltip::text("Show in Diff"))
                    .on_click(cx.listener({
                        let comment_id = comment_id.clone();
                        move |this, _, window, cx| this.jump_to_comment(&comment_id, window, cx)
                    }))
            })
            .child(header)
            .child(content)
            .children(actions)
            .into_any_element()
    }

    fn render_review_header(
        &self,
        review: &Review,
        is_collapsed: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let review_id = review.id.clone();
        let (title, time) = match review.submitted_at {
            Some(submitted_at) => ("Review", submitted_at),
            None => ("Pending Review", review.created_at),
        };
        let unaddressed = review.unaddressed_count();
        let summary = if review.comments.is_empty() {
            "no comments".to_string()
        } else {
            format!(
                "{} comment{}, {unaddressed} unaddressed",
                review.comments.len(),
                if review.comments.len() == 1 { "" } else { "s" }
            )
        };
        h_flex()
            .id(SharedString::from(format!("review-header-{}", review.id)))
            .w_full()
            .min_w_0()
            .gap_1()
            .py_0p5()
            .cursor_pointer()
            .on_click(cx.listener({
                let review_id = review_id.clone();
                move |this, _, _, cx| {
                    if !this.collapsed_reviews.remove(&review_id) {
                        this.collapsed_reviews.insert(review_id.clone());
                    }
                    cx.notify();
                }
            }))
            .child(
                Icon::new(if is_collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(Label::new(title).size(LabelSize::Small))
            .child(
                div()
                    .id(SharedString::from(format!("review-time-{}", review.id)))
                    .child(
                        Label::new(relative_time(time.timestamp()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text(format!(
                        "{} {}",
                        if review.is_draft() {
                            "Started"
                        } else {
                            "Submitted"
                        },
                        absolute_time(time)
                    ))),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(summary)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                ),
            )
            .child(
                IconButton::new(
                    SharedString::from(format!("copy-review-id-{}", review.id)),
                    IconName::Hash,
                )
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text("Copy Review ID"))
                .on_click(move |_, _, cx| Self::copy(review_id.clone(), cx)),
            )
            .child(
                IconButton::new(
                    SharedString::from(format!("copy-review-text-{}", review.id)),
                    IconName::Copy,
                )
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text("Copy Review as Text"))
                .on_click({
                    let text = git_review::review_as_text(review);
                    move |_, _, cx| Self::copy(text.clone(), cx)
                }),
            )
    }

    fn render_review_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        let model = self.model.read(cx);
        if !model.is_loaded() {
            return Self::render_placeholder("Loading Reviews…").into_any_element();
        }
        let reviews = model.reviews().clone();
        let error = model.error().cloned();
        let mut sections = Vec::new();
        let ordered = reviews
            .draft()
            .into_iter()
            .chain(reviews.submitted().rev())
            .collect::<Vec<_>>();
        for review in ordered {
            let is_collapsed = self.collapsed_reviews.contains(&review.id);
            let mut section = v_flex().w_full().gap_1().child(self.render_review_header(
                review,
                is_collapsed,
                cx,
            ));
            if !is_collapsed {
                for comment in &review.comments {
                    section = section.child(self.render_comment(comment, cx));
                }
            }
            sections.push(section);
        }
        let is_empty = sections.is_empty();
        v_flex()
            .id("review-comments")
            .flex_1()
            .min_h_0()
            .w_full()
            .p_1p5()
            .gap_2()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .when_some(error, |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .when(is_empty, |this| {
                this.child(
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .child(Label::new("No comments yet").color(Color::Muted))
                        .child(
                            Label::new(
                                "Comment on lines in the diff, or on the latest commit below.",
                            )
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                )
            })
            .children(sections)
            .into_any_element()
    }

    fn render_placeholder(message: impl Into<SharedString>) -> impl IntoElement {
        h_flex()
            .flex_1()
            .justify_center()
            .child(Label::new(message.into()).color(Color::Muted))
    }

    fn render_commits_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        let uncommitted_files = self.repository.read(cx).cached_status().count();
        let (comment_counts, uncommitted_comments) = self.comment_counts_by_commit(cx);
        let row = |id: SharedString,
                   marker: &'static str,
                   title: SharedString,
                   detail: SharedString,
                   comments: usize| {
            h_flex()
                .id(id)
                .w_full()
                .min_w_0()
                .px_2()
                .py_1()
                .gap_2()
                .cursor_pointer()
                .hover(|style| style.bg(cx.theme().colors().element_hover))
                .child(
                    Label::new(marker)
                        .size(LabelSize::Small)
                        .color(Color::Accent)
                        .flex_none(),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(Label::new(title).truncate())
                        .child(
                            Label::new(detail)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .when(comments > 0, |this| {
                    this.child(
                        h_flex()
                            .flex_none()
                            .gap_0p5()
                            .child(
                                Icon::new(IconName::Chat)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(comments.to_string())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                })
        };

        let model = self.model.read(cx);
        let commits = model.commits().to_vec();
        let content = match model.commits_error().cloned() {
            _ if model.is_loading_commits() => {
                Self::render_placeholder("Loading Commits…").into_any_element()
            }
            Some(error) if uncommitted_files == 0 => {
                Self::render_placeholder(error).into_any_element()
            }
            _ => v_flex()
                .w_full()
                .when(uncommitted_files > 0, |this| {
                    this.child(
                        row(
                            "review-uncommitted".into(),
                            "@",
                            "Uncommitted Changes".into(),
                            format!(
                                "{uncommitted_files} file{}",
                                if uncommitted_files == 1 { "" } else { "s" }
                            )
                            .into(),
                            uncommitted_comments,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_uncommitted_changes(window, cx)
                        })),
                    )
                })
                .children(commits.iter().map(|commit| {
                    let sha = commit.sha.clone();
                    let detail = format!(
                        "{} • {} • {}",
                        commit.author,
                        relative_time(commit.timestamp),
                        git_review::short_sha(&commit.sha)
                    );
                    row(
                        SharedString::from(format!("review-commit-{}", commit.sha)),
                        "○",
                        commit.subject.clone(),
                        detail.into(),
                        comment_counts.get(&commit.sha).copied().unwrap_or(0),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_commit(sha.clone(), None, window, cx)
                    }))
                }))
                .when(commits.is_empty() && uncommitted_files == 0, |this| {
                    this.child(Self::render_placeholder("No commits on top of the base"))
                })
                .into_any_element(),
        };
        v_flex()
            .id("review-commits")
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .child(content)
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let blocker = self.submit_blocker(cx);
        let can_comment = !self.comment_editor.read(cx).text(cx).trim().is_empty();
        let colors = cx.theme().colors();
        v_flex()
            .w_full()
            .border_t_1()
            .border_color(colors.border)
            .child(
                div()
                    .id("review-comment-editor")
                    .w_full()
                    .px_2()
                    .py_2()
                    .bg(colors.editor_background)
                    .cursor_text()
                    .on_click(cx.listener(|this, _, window, cx| {
                        window.focus(&this.comment_editor.focus_handle(cx), cx);
                    }))
                    .key_context(REVIEW_COMMENT_KEY_CONTEXT)
                    .child(self.comment_editor.clone()),
            )
            .child(
                h_flex()
                    .w_full()
                    .p_1p5()
                    .gap_1()
                    .justify_end()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(
                        Button::new("review-add-comment", "Comment")
                            .label_size(LabelSize::Small)
                            .disabled(!can_comment)
                            .tooltip(Tooltip::text("Comment on the latest commit"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_general_comment(window, cx)
                            })),
                    )
                    .child(
                        Button::new("review-submit", "Submit Review")
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Filled)
                            .disabled(blocker.is_some())
                            .tooltip(Tooltip::text(
                                blocker.unwrap_or("Submit the pending review"),
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.submit_review(window, cx)),
                            ),
                    ),
            )
    }
}

impl Render for ReviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("GitReview")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .child(self.render_tab_bar(cx))
            .child(self.render_header(cx))
            .map(|this| match self.active_tab {
                ReviewTab::Review => this
                    .child(self.render_review_tab(cx))
                    .child(self.render_footer(cx)),
                ReviewTab::Commits => this.child(self.render_commits_tab(cx)),
            })
    }
}

/// Completes `#tags` with every tag used so far.
struct TagCompletionProvider;

/// Where the tag being typed at `position` starts (at its `#`), and what of
/// it was typed.
fn tag_query_at(buffer: &Buffer, position: language::Anchor) -> Option<(usize, String)> {
    let snapshot = buffer.snapshot();
    let point = position.to_point(&snapshot);
    let line_start = Point::new(point.row, 0);
    let before: String = snapshot.text_for_range(line_start..point).collect();
    let hash_ix = before.rfind('#')?;
    let query = &before[hash_ix + 1..];
    if !query.chars().all(git_review::is_tag_character) {
        return None;
    }
    if before[..hash_ix]
        .chars()
        .next_back()
        .is_some_and(|character| !character.is_whitespace())
    {
        return None;
    }
    let offset = line_start.to_offset(&snapshot) + hash_ix;
    Some((offset, query.to_string()))
}

impl CompletionProvider for TagCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        _trigger: editor::CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let buffer = buffer.read(cx);
        let Some((start, query)) = tag_query_at(buffer, buffer_position) else {
            return Task::ready(Ok(Vec::new()));
        };
        let replace_range = buffer.anchor_before(start)..buffer_position;
        let query = query.to_lowercase();
        let tags = cx
            .try_global::<ReviewStore>()
            .map(|store| store.known_tags.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let (mut prefixed, mut containing): (Vec<_>, Vec<_>) = tags
            .into_iter()
            .filter(|tag| tag.to_lowercase().contains(&query))
            .partition(|tag| tag.to_lowercase().starts_with(&query));
        prefixed.append(&mut containing);
        let completions = prefixed
            .into_iter()
            .map(|tag| project::Completion {
                replace_range: replace_range.clone(),
                label: language::CodeLabel::plain(format!("#{tag}"), None),
                new_text: format!("#{tag} "),
                documentation: None,
                source: project::CompletionSource::Custom,
                icon_path: None,
                icon_color: None,
                match_start: None,
                snippet_deduplication_key: None,
                insert_text_mode: None,
                confirm: None,
                group: None,
            })
            .collect();
        Task::ready(Ok(vec![CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions {
                dynamic_width: true,
            },
            is_incomplete: true,
        }]))
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        text: &str,
        _trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        text.chars()
            .last()
            .is_some_and(|last| last == '#' || git_review::is_tag_character(last))
            && tag_query_at(buffer.read(cx), position).is_some()
    }

    fn sort_completions(&self) -> bool {
        false
    }

    fn filter_completions(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    #[allow(
        clippy::disallowed_methods,
        reason = "the test drives a real repository"
    )]
    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    async fn wait_until(
        cx: &mut gpui::TestAppContext,
        description: &str,
        mut condition: impl FnMut(&mut gpui::TestAppContext) -> bool,
    ) {
        for _ in 0..400 {
            cx.run_until_parked();
            if condition(cx) {
                return;
            }
            cx.background_executor
                .timer(Duration::from_millis(25))
                .await;
        }
        panic!("timed out waiting until {description}");
    }

    #[gpui::test]
    async fn test_comments_follow_their_lines_into_commits(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let root = util::paths::SanitizedPath::new(&root)
            .as_path()
            .to_path_buf();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-m", "initial"]);
        git(&root, &["checkout", "-b", "feature"]);
        std::fs::write(root.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        git(&root, &["commit", "-am", "shout two"]);
        let feature_commit = git(&root, &["rev-parse", "HEAD"]);
        std::fs::write(root.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();

        let fs = fs::RealFs::new(None, cx.executor());
        let project = Project::test(fs, [root.as_path()], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("the repository is found")
        });
        let model = cx
            .update(|cx| ReviewModel::for_repository(&repository, &project, cx))
            .unwrap();
        wait_until(cx, "the base is found", |cx| {
            model.read_with(cx, |model, _| {
                model.base().is_some() && !model.is_loading_commits()
            })
        })
        .await;
        model.read_with(cx, |model, _| {
            assert_eq!(model.base(), Some("main"));
            assert_eq!(
                model
                    .commits()
                    .iter()
                    .map(|commit| commit.sha.as_str())
                    .collect::<Vec<_>>(),
                vec![feature_commit.as_str()]
            );
        });

        // A committed line belongs to the commit that changed it; one that isn't
        // committed yet belongs to none.
        let committed_comment = model
            .update(cx, |model, cx| {
                model.add_line_comment_at(
                    &project,
                    "a.txt".into(),
                    None,
                    1..1,
                    "louder #style".into(),
                    cx,
                )
            })
            .await
            .unwrap();
        let uncommitted_comment = model
            .update(cx, |model, cx| {
                model.add_line_comment_at(
                    &project,
                    "a.txt".into(),
                    None,
                    3..3,
                    "why four".into(),
                    cx,
                )
            })
            .await
            .unwrap();
        model.read_with(cx, |model, cx| {
            let reviews = model.reviews();
            let line_target =
                |comment_id: &str| match &reviews.comment(comment_id).unwrap().1.target {
                    CommentTarget::Line(target) => target.clone(),
                    CommentTarget::General { .. } => panic!("not a line comment"),
                };
            let committed = line_target(&committed_comment);
            assert_eq!(committed.commit.as_deref(), Some(feature_commit.as_str()));
            assert_eq!((committed.start_line, committed.end_line), (2, 2));
            assert_eq!(committed.excerpt, "TWO");
            assert_eq!(
                reviews.comment(&committed_comment).unwrap().1.text,
                "#style louder"
            );
            assert_eq!(line_target(&uncommitted_comment).commit, None);
            assert_eq!(
                model.submit_blocker(false, cx),
                Some("Commit all changes before submitting the review")
            );
        });

        // Once committed, the comment moves to the new commit.
        git(&root, &["commit", "-am", "add four"]);
        let new_commit = git(&root, &["rev-parse", "HEAD"]);
        wait_until(cx, "the comment moves to its commit", |cx| {
            model.read_with(cx, |model, _| {
                model
                    .reviews()
                    .comment(&uncommitted_comment)
                    .unwrap()
                    .1
                    .commit()
                    == Some(new_commit.as_str())
            })
        })
        .await;
        model.read_with(cx, |model, cx| {
            assert_eq!(model.submit_blocker(false, cx), None);
            let (_, comment) = model.reviews().comment(&uncommitted_comment).unwrap();
            let CommentTarget::Line(target) = &comment.target else {
                panic!("not a line comment");
            };
            assert_eq!((target.start_line, target.end_line), (4, 4));
        });

        let review_id = model
            .update(cx, |model, cx| model.submit(cx))
            .await
            .unwrap();
        cx.run_until_parked();
        let git_dir = repository.read_with(cx, |repository, _| {
            repository.repository_dir_abs_path.to_path_buf()
        });
        wait_until(cx, "the review is saved", |_| {
            git_review::load(&git_dir, &root).is_ok_and(|saved| {
                saved
                    .review(&review_id)
                    .is_some_and(|review| !review.is_draft() && review.comments.len() == 2)
            })
        })
        .await;
        let saved = git_review::load(&git_dir, &root).unwrap();
        let review = saved.review(&review_id).unwrap();
        assert_eq!(review.base_branch.as_deref(), Some("main"));
        assert_eq!(review.branch.as_deref(), Some("feature"));
        assert_eq!(review.head_commit.as_deref(), Some(new_commit.as_str()));
    }

    #[gpui::test]
    fn test_tag_query_at(cx: &mut gpui::TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("fix #per", cx));
        buffer.read_with(cx, |buffer, _| {
            let end = buffer.anchor_after(buffer.len());
            assert_eq!(tag_query_at(buffer, end), Some((4, "per".to_string())));
            let after_hash = buffer.anchor_after(5);
            assert_eq!(tag_query_at(buffer, after_hash), Some((4, String::new())));
            let in_word = buffer.anchor_after(2);
            assert_eq!(tag_query_at(buffer, in_word), None);
        });
        let buffer = cx.new(|cx| Buffer::local("a#b", cx));
        buffer.read_with(cx, |buffer, _| {
            let end = buffer.anchor_after(buffer.len());
            assert_eq!(tag_query_at(buffer, end), None);
        });
    }

    #[test]
    fn test_comment_body() {
        let comment = |text: &str| Comment {
            id: "cm-1".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            text: text.into(),
            tags: Vec::new(),
            addressed_at: None,
            target: CommentTarget::General { commit: None },
        };
        assert_eq!(comment_body(&comment("#a #b fix it")), "fix it");
        assert_eq!(comment_body(&comment("#a")), "");
        assert_eq!(comment_body(&comment("plain #a")), "plain #a");
    }
}
