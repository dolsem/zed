//! Answers the review endpoints from the reviews saved in worktrees' git
//! directories, so they work whether or not the worktree is open.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use git_review::{Comment, CommentTarget, IndexedWorktree, Review, Side};
use mobile_protocol::rest::{
    CommentStatusFilter, ReviewComment, ReviewCommentLocation, ReviewCommentSide,
    ReviewCommentsQuery, ReviewCommentsResponse, ReviewSummary, ReviewsQuery, ReviewsResponse,
};

use crate::rest::not_found;

pub(crate) fn list_reviews(query: &ReviewsQuery) -> Result<ReviewsResponse> {
    let mut reviews = git_review::load_all()?
        .into_iter()
        .filter(|(worktree, _)| {
            query
                .worktree_path
                .as_deref()
                .is_none_or(|path| same_path(&worktree.worktree_path, Path::new(path)))
        })
        .flat_map(|(worktree, worktree_reviews)| {
            worktree_reviews
                .submitted()
                .filter(|review| query.all || review.unaddressed_count() > 0)
                .map(|review| review_summary(&worktree, review))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    reviews.sort_by(|a, b| b.submitted_at.cmp(&a.submitted_at));
    Ok(ReviewsResponse { reviews })
}

pub(crate) fn review_comments(
    review_id: &str,
    query: &ReviewCommentsQuery,
) -> Result<ReviewCommentsResponse> {
    let (worktree, worktree_reviews) = git_review::load_all()?
        .into_iter()
        .find(|(_, worktree_reviews)| {
            worktree_reviews
                .review(review_id)
                .is_some_and(|review| !review.is_draft())
        })
        .ok_or_else(|| not_found(format!("no review {review_id:?}")))?;
    let review = worktree_reviews
        .review(review_id)
        .ok_or_else(|| not_found(format!("no review {review_id:?}")))?;
    let mut current_files = HashMap::new();
    let comments = review
        .comments
        .iter()
        .filter(|comment| match query.status {
            CommentStatusFilter::Unaddressed => !comment.is_addressed(),
            CommentStatusFilter::Addressed => comment.is_addressed(),
            CommentStatusFilter::All => true,
        })
        .filter(|comment| query.tag.as_deref().is_none_or(|tag| comment.has_tag(tag)))
        .map(|comment| api_comment(&worktree, comment, &mut current_files))
        .collect();
    Ok(ReviewCommentsResponse {
        review: review_summary(&worktree, review),
        comments,
    })
}

fn same_path(a: &Path, b: &Path) -> bool {
    let normalize = |path: &Path| {
        let path = path.to_string_lossy().replace('\\', "/");
        let path = path.trim_end_matches('/').to_string();
        if cfg!(windows) {
            path.to_lowercase()
        } else {
            path
        }
    };
    normalize(a) == normalize(b)
}

fn review_summary(worktree: &IndexedWorktree, review: &Review) -> ReviewSummary {
    ReviewSummary {
        review_id: review.id.clone(),
        worktree_path: worktree.worktree_path.to_string_lossy().into_owned(),
        host: worktree.host.clone(),
        branch: review.branch.clone(),
        base_branch: review.base_branch.clone(),
        head_commit: review.head_commit.clone(),
        created_at: review.created_at.to_rfc3339(),
        submitted_at: review.submitted_at.map(|time| time.to_rfc3339()),
        comment_count: review.comments.len(),
        unaddressed_count: review.unaddressed_count(),
        tags: review.tags().into_iter().collect(),
    }
}

fn api_comment(
    worktree: &IndexedWorktree,
    comment: &Comment,
    current_files: &mut HashMap<String, Option<String>>,
) -> ReviewComment {
    let location = match &comment.target {
        CommentTarget::General { .. } => None,
        CommentTarget::Line(target) => {
            // Remote worktrees' files can't be read from here.
            let current_text = current_files
                .entry(target.path.clone())
                .or_insert_with(|| {
                    worktree.host.is_none().then(|| {
                        std::fs::read_to_string(worktree.worktree_path.join(&target.path)).ok()
                    })?
                })
                .clone();
            let current = current_text.and_then(|current_text| {
                let snapshot =
                    git_review::read_snapshot(&worktree.git_dir, &target.anchor.snapshot).ok()?;
                Some(git_review::relocate_rows(
                    &snapshot,
                    &current_text,
                    target.anchor.start_row,
                    target.anchor.end_row,
                ))
            });
            Some(ReviewCommentLocation {
                path: target.path.clone(),
                side: match target.side {
                    Side::New => ReviewCommentSide::New,
                    Side::Old => ReviewCommentSide::Old,
                },
                start_line: target.start_line,
                end_line: target.end_line,
                excerpt: target.excerpt.clone(),
                current_start_line: current.map(|relocation| relocation.start_row + 1),
                current_end_line: current.map(|relocation| relocation.end_row + 1),
            })
        }
    };
    ReviewComment {
        comment_id: comment.id.clone(),
        created_at: comment.created_at.to_rfc3339(),
        updated_at: comment.updated_at.to_rfc3339(),
        addressed_at: comment.addressed_at.map(|time| time.to_rfc3339()),
        text: comment.text.clone(),
        tags: comment.tags.clone(),
        commit: comment.commit().map(str::to_string),
        location,
        as_text: git_review::comment_as_text(comment),
    }
}
