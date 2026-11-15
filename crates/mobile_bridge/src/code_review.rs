//! Answers the web UI's `review.*` requests with the review mode of Zed's git
//! panel, which keeps the reviews (see `git_ui::review_mode`).

use std::time::Duration;

use anyhow::{Context as _, Result};
use collections::HashMap;
use git_review::{Comment, CommentTarget, Review, Side};
use git_ui::review_mode::{self, ReviewFileStatus, ReviewModel};
use gpui::{App, AppContext as _, Context, Entity, Subscription, Task, WeakEntity};
use project::Project;

use crate::api::{
    AddedReviewComment, FileStatus, ReviewChangedEvent, ReviewCommentSnapshot,
    ReviewCommentTargetParams, ReviewCommitSummary, ReviewDiff, ReviewDiffFile, ReviewLineComment,
    ReviewSnapshot, ReviewState, SubmittedReview, WorkspaceId, events,
};
use crate::{MobileBridge, find_workspace, to_json};

/// Reviews change in bursts (e.g. a status scan after each save), so clients
/// hear about them once things settle.
const REVIEW_CHANGED_DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Default)]
pub(crate) struct ReviewWatches {
    watches: HashMap<WorkspaceId, Watch>,
}

struct Watch {
    model: WeakEntity<ReviewModel>,
    pending_event: Option<Task<()>>,
    _subscription: Subscription,
}

impl MobileBridge {
    /// The review of the workspace's active repository, telling clients when
    /// it changes from now on.
    fn review_model(
        &mut self,
        workspace_id: WorkspaceId,
        cx: &mut Context<Self>,
    ) -> Result<(Entity<Project>, Entity<ReviewModel>)> {
        let (_, workspace) = find_workspace(workspace_id, cx)?;
        let project = workspace.read(cx).project().clone();
        let repository = project
            .read(cx)
            .active_repository(cx)
            .context("the workspace has no git repository")?;
        let model = ReviewModel::for_repository(&repository, &project, cx)?;
        let is_watched = self
            .reviews
            .watches
            .get(&workspace_id)
            .is_some_and(|watch| watch.model == model.downgrade());
        if !is_watched {
            let subscription = cx.observe(&model, move |this, _, cx| {
                let Some(watch) = this.reviews.watches.get_mut(&workspace_id) else {
                    return;
                };
                if watch.pending_event.is_some() {
                    return;
                }
                watch.pending_event = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(REVIEW_CHANGED_DEBOUNCE)
                        .await;
                    this.update(cx, |this, _| {
                        if let Some(watch) = this.reviews.watches.get_mut(&workspace_id) {
                            watch.pending_event = None;
                        }
                        this.broadcast_event(
                            events::REVIEW_CHANGED,
                            &ReviewChangedEvent { workspace_id },
                        );
                    })
                    .ok();
                }));
            });
            self.reviews.watches.insert(
                workspace_id,
                Watch {
                    model: model.downgrade(),
                    pending_event: None,
                    _subscription: subscription,
                },
            );
        }
        Ok((project, model))
    }

    pub(crate) fn review_state(
        &mut self,
        workspace_id: WorkspaceId,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let model = match self.review_model(workspace_id, cx) {
            Ok((_, model)) => model,
            Err(error) => {
                return Task::ready(to_json(unavailable_state(workspace_id, error, cx)));
            }
        };
        let repository = model.read(cx).repository().clone();
        let branches = repository.update(cx, |repository, _| repository.branches());
        cx.spawn(async move |_, cx| {
            let branches = branches
                .await
                .ok()
                .and_then(|branches| branches.ok())
                .map(|branches| {
                    let mut names = branches
                        .branches
                        .iter()
                        .filter(|branch| !branch.is_remote())
                        .map(|branch| branch.name().to_string())
                        .collect::<Vec<_>>();
                    names.sort();
                    names
                })
                .unwrap_or_default();
            let state = cx.update(|cx| review_state(workspace_id, &model, branches, cx));
            to_json(state)
        })
    }

    pub(crate) fn review_diff(
        &mut self,
        workspace_id: WorkspaceId,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let (project, model) = match self.review_model(workspace_id, cx) {
            Ok(result) => result,
            Err(error) => return Task::ready(Err(error)),
        };
        let files = model.update(cx, |model, cx| model.diff_files(&project, cx));
        cx.spawn(async move |_, cx| {
            let files = files.await?;
            let diff = cx.update(|cx| {
                let model = model.read(cx);
                let line_comments = model
                    .reviews()
                    .reviews
                    .iter()
                    .flat_map(|review| review.comments.iter())
                    .filter(|comment| !comment.is_addressed())
                    .filter_map(|comment| match &comment.target {
                        CommentTarget::Line(target) => Some((comment.id.clone(), target)),
                        CommentTarget::General { .. } => None,
                    })
                    .collect::<Vec<_>>();
                ReviewDiff {
                    base: model.base().map(str::to_string),
                    files: files
                        .into_iter()
                        .map(|file| {
                            let comments = file
                                .new_text
                                .as_ref()
                                .map(|new_text| {
                                    line_comments
                                        .iter()
                                        .filter(|(_, target)| target.path == file.path)
                                        .filter_map(|(comment_id, target)| {
                                            let location =
                                                model.current_location(target, new_text)?;
                                            Some(ReviewLineComment {
                                                comment_id: comment_id.clone(),
                                                start_row: location.start_row,
                                                end_row: location.end_row,
                                            })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            ReviewDiffFile {
                                path: file.path,
                                status: match file.status {
                                    ReviewFileStatus::Added => FileStatus::Added,
                                    ReviewFileStatus::Modified => FileStatus::Modified,
                                    ReviewFileStatus::Deleted => FileStatus::Deleted,
                                },
                                old_text: file.old_text,
                                new_text: file.new_text,
                                is_binary: file.is_binary,
                                comments,
                            }
                        })
                        .collect(),
                }
            });
            to_json(diff)
        })
    }

    pub(crate) fn set_review_base(
        &mut self,
        workspace_id: WorkspaceId,
        base: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        Task::ready(self.review_model(workspace_id, cx).map(|(_, model)| {
            model.update(cx, |model, cx| model.set_base(base, cx));
            serde_json::Value::Null
        }))
    }

    pub(crate) fn add_review_comment(
        &mut self,
        workspace_id: WorkspaceId,
        text: String,
        target: ReviewCommentTargetParams,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let (project, model) = match self.review_model(workspace_id, cx) {
            Ok(result) => result,
            Err(error) => return Task::ready(Err(error)),
        };
        match target {
            ReviewCommentTargetParams::General => Task::ready(
                model
                    .update(cx, |model, cx| model.add_general_comment(&text, cx))
                    .context("the comment is empty")
                    .and_then(|comment_id| to_json(AddedReviewComment { comment_id })),
            ),
            ReviewCommentTargetParams::Line {
                path,
                start_row,
                end_row,
                commit,
            } => {
                if text.trim().is_empty() {
                    return Task::ready(Err(anyhow::anyhow!("the comment is empty")));
                }
                let add = model.update(cx, |model, cx| {
                    model.add_line_comment_at(&project, path, commit, start_row..end_row, text, cx)
                });
                cx.background_spawn(async move {
                    let comment_id = add.await?;
                    to_json(AddedReviewComment { comment_id })
                })
            }
        }
    }

    pub(crate) fn edit_review_comment(
        &mut self,
        workspace_id: WorkspaceId,
        comment_id: String,
        text: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        if text.trim().is_empty() {
            return Task::ready(Err(anyhow::anyhow!("the comment is empty")));
        }
        Task::ready(self.review_model(workspace_id, cx).map(|(_, model)| {
            model.update(cx, |model, cx| model.edit_comment(&comment_id, &text, cx));
            serde_json::Value::Null
        }))
    }

    pub(crate) fn delete_review_comment(
        &mut self,
        workspace_id: WorkspaceId,
        comment_id: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        Task::ready(self.review_model(workspace_id, cx).map(|(_, model)| {
            model.update(cx, |model, cx| model.delete_comment(&comment_id, cx));
            serde_json::Value::Null
        }))
    }

    pub(crate) fn set_review_comment_addressed(
        &mut self,
        workspace_id: WorkspaceId,
        comment_id: String,
        addressed: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        Task::ready(self.review_model(workspace_id, cx).map(|(_, model)| {
            model.update(cx, |model, cx| {
                model.set_addressed(&comment_id, addressed, cx)
            });
            serde_json::Value::Null
        }))
    }

    pub(crate) fn submit_review(
        &mut self,
        workspace_id: WorkspaceId,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let model = match self.review_model(workspace_id, cx) {
            Ok((_, model)) => model,
            Err(error) => return Task::ready(Err(error)),
        };
        let submit = model.update(cx, |model, cx| model.submit(cx));
        cx.background_spawn(async move {
            let review_id = submit.await?;
            to_json(SubmittedReview { review_id })
        })
    }
}

fn unavailable_state(workspace_id: WorkspaceId, error: anyhow::Error, cx: &App) -> ReviewState {
    ReviewState {
        workspace_id,
        unavailable_reason: Some(format!("{error:#}")),
        is_loaded: true,
        worktree_name: None,
        branch: None,
        base: None,
        is_base_picked: false,
        branches: Vec::new(),
        commits: Vec::new(),
        commits_error: None,
        uncommitted_files: 0,
        submit_blocker: None,
        is_submitting: false,
        reviews: Vec::new(),
        tags: review_mode::known_tags(cx),
        error: None,
    }
}

fn review_state(
    workspace_id: WorkspaceId,
    model: &Entity<ReviewModel>,
    branches: Vec<String>,
    cx: &App,
) -> ReviewState {
    let model = model.read(cx);
    let repository = model.repository().read(cx);
    let reviews = model.reviews();
    let mut comment_counts: HashMap<&str, usize> = HashMap::default();
    for comment in reviews
        .reviews
        .iter()
        .flat_map(|review| review.comments.iter())
    {
        if let Some(commit) = comment.commit() {
            *comment_counts.entry(commit).or_default() += 1;
        }
    }
    ReviewState {
        workspace_id,
        unavailable_reason: None,
        is_loaded: model.is_loaded(),
        worktree_name: Some(repository.display_name().to_string()),
        branch: model.branch().map(str::to_string),
        base: model.base().map(str::to_string),
        is_base_picked: model.is_base_picked(),
        branches,
        commits: model
            .commits()
            .iter()
            .map(|commit| ReviewCommitSummary {
                sha: commit.sha.clone(),
                subject: commit.subject.to_string(),
                author: commit.author.to_string(),
                timestamp: commit.timestamp,
                comment_count: comment_counts
                    .get(commit.sha.as_str())
                    .copied()
                    .unwrap_or(0),
            })
            .collect(),
        commits_error: model.commits_error().map(ToString::to_string),
        uncommitted_files: repository.cached_status().count(),
        submit_blocker: model.submit_blocker(false, cx).map(str::to_string),
        is_submitting: model.is_submitting(),
        reviews: reviews
            .draft()
            .into_iter()
            .chain(reviews.submitted().rev())
            .map(review_snapshot)
            .collect(),
        tags: review_mode::known_tags(cx),
        error: model.error().map(ToString::to_string),
    }
}

fn review_snapshot(review: &Review) -> ReviewSnapshot {
    ReviewSnapshot {
        review_id: review.id.clone(),
        is_draft: review.is_draft(),
        created_at: review.created_at.to_rfc3339(),
        submitted_at: review.submitted_at.map(|time| time.to_rfc3339()),
        unaddressed_count: review.unaddressed_count(),
        as_text: git_review::review_as_text(review),
        comments: review.comments.iter().map(comment_snapshot).collect(),
    }
}

fn comment_snapshot(comment: &Comment) -> ReviewCommentSnapshot {
    let line = match &comment.target {
        CommentTarget::Line(target) => Some(target),
        CommentTarget::General { .. } => None,
    };
    ReviewCommentSnapshot {
        comment_id: comment.id.clone(),
        created_at: comment.created_at.to_rfc3339(),
        updated_at: comment.updated_at.to_rfc3339(),
        addressed: comment.is_addressed(),
        text: comment.text.clone(),
        tags: comment.tags.clone(),
        location: git_review::comment_location(comment),
        commit: comment.commit().map(str::to_string),
        path: line.map(|target| target.path.clone()),
        start_line: line.map(|target| target.start_line),
        end_line: line.map(|target| target.end_line),
        is_deleted_side: line.is_some_and(|target| target.side == Side::Old),
        excerpt: line.map(|target| target.excerpt.clone()),
        as_text: git_review::comment_as_text(comment),
    }
}
