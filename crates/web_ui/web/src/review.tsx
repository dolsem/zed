// Code review, as in the git panel's review mode in Zed: comments on the
// branch's lines and commits, gathered into reviews that are submitted once
// everything is committed.

import { signal } from "@preact/signals";
import type { ComponentChildren, TargetedKeyboardEvent } from "preact";
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { createReviewDiffView, type ReviewDiffView, type ReviewMark } from "./editor";
import type {
  ReviewCommentSnapshot,
  ReviewCommentTarget,
  ReviewDiff,
  ReviewDiffFile,
  ReviewSnapshot,
  ReviewState,
  WorkspaceId,
} from "./protocol";
import { call, conn, errorMessage, pushOverlay, toast, type OverlayTarget } from "./state";
import { CloseButton, EmptyState, haptic, Icon, parseDate, PickerSheet, relativeTime, Sheet, Spinner, splitPath, useInView } from "./ui";

// ---------------------------------------------------------------------------
// State

const reviewStates = signal<Map<WorkspaceId, ReviewState>>(new Map());

async function loadReviewState(workspaceId: WorkspaceId) {
  const state = await call("review.state", { workspace_id: workspaceId });
  if (!state) return;
  const next = new Map(reviewStates.peek());
  next.set(workspaceId, state);
  reviewStates.value = next;
}

conn.on("review.changed", (event) => {
  if (reviewStates.peek().has(event.workspace_id)) void loadReviewState(event.workspace_id);
});

conn.on("hello", () => {
  for (const workspaceId of reviewStates.peek().keys()) void loadReviewState(workspaceId);
});

/** The review of the workspace's repository, kept up to date while mounted. */
export function useReviewState(workspaceId: WorkspaceId): ReviewState | null {
  useEffect(() => {
    void loadReviewState(workspaceId);
  }, [workspaceId]);
  return reviewStates.value.get(workspaceId) ?? null;
}

/** Runs a review request, then refreshes the state. Resolves to whether it worked. */
async function mutate(workspaceId: WorkspaceId, request: () => Promise<unknown>): Promise<boolean> {
  haptic();
  const result = await request();
  void loadReviewState(workspaceId);
  return result !== undefined;
}

export async function addReviewComment(
  workspaceId: WorkspaceId,
  text: string,
  target: ReviewCommentTarget,
): Promise<boolean> {
  return mutate(workspaceId, () => call("review.add_comment", { workspace_id: workspaceId, text, target }));
}

// ---------------------------------------------------------------------------
// Helpers

async function copyText(text: string, what: string) {
  haptic();
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // The clipboard API needs a secure context; fall back to a selection.
    const textarea = document.createElement("textarea");
    textarea.value = text;
    textarea.setAttribute("readonly", "");
    textarea.style.position = "fixed";
    textarea.style.opacity = "0";
    document.body.append(textarea);
    textarea.select();
    const copied = document.execCommand("copy");
    textarea.remove();
    if (!copied) {
      toast("Couldn't copy", "error");
      return;
    }
  }
  toast(`Copied ${what}`);
}

const TAG_PATTERN = /^#[\p{L}\p{N}_\-/.]*[\p{L}_\-/.][\p{L}\p{N}_\-/.]*$/u;

/** The comment's text without the tags it starts with. */
function commentBody(text: string): string {
  let body = text;
  for (;;) {
    const trimmed = body.replace(/^ +/, "");
    const end = trimmed.search(/[ \n]/);
    const word = end < 0 ? trimmed : trimmed.slice(0, end);
    if (!TAG_PATTERN.test(word)) return trimmed;
    body = end < 0 ? "" : trimmed.slice(end);
  }
}

function formatTime(value: string | null | undefined): string {
  const date = parseDate(value ?? null);
  return date ? relativeTime(date) : "";
}

function formatFullTime(value: string | null | undefined): string {
  const date = parseDate(value ?? null);
  return date ? date.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" }) : "";
}

// ---------------------------------------------------------------------------
// Comment input: a growing textarea that suggests tags while typing `#…`

/** The tag being typed before the caret, if any. */
function tagQuery(text: string, caret: number): { start: number; query: string } | null {
  const match = /(^|\s)#([\p{L}\p{N}_\-/.]*)$/u.exec(text.slice(0, caret));
  if (!match) return null;
  return { start: caret - match[2].length - 1, query: match[2] };
}

function CommentInput({
  value,
  onChange,
  placeholder,
  tags,
  autoFocus,
  onSubmit,
}: {
  value: string;
  onChange: (value: string) => void;
  placeholder: string;
  tags: string[];
  autoFocus?: boolean;
  onSubmit?: () => void;
}) {
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const [caret, setCaret] = useState(value.length);

  useLayoutEffect(() => {
    const textarea = textareaRef.current;
    if (!textarea) return;
    textarea.style.height = "auto";
    const max = Math.max(140, window.innerHeight * 0.35);
    textarea.style.height = `${Math.min(textarea.scrollHeight, max)}px`;
  }, [value]);

  useEffect(() => {
    if (!autoFocus) return;
    const timer = setTimeout(() => {
      const textarea = textareaRef.current;
      if (!textarea) return;
      textarea.focus();
      textarea.setSelectionRange(textarea.value.length, textarea.value.length);
    }, 120);
    return () => clearTimeout(timer);
  }, [autoFocus]);

  const query = tagQuery(value, caret);
  const suggestions = query
    ? tags
        .filter((tag) => tag.toLowerCase().includes(query.query.toLowerCase()) && tag !== query.query)
        .sort(
          (a, b) =>
            Number(!a.toLowerCase().startsWith(query.query.toLowerCase())) -
            Number(!b.toLowerCase().startsWith(query.query.toLowerCase())),
        )
        .slice(0, 12)
    : [];

  const pick = (tag: string) => {
    if (!query) return;
    haptic();
    const insertion = `#${tag} `;
    const next = value.slice(0, query.start) + insertion + value.slice(caret);
    const nextCaret = query.start + insertion.length;
    onChange(next);
    setCaret(nextCaret);
    requestAnimationFrame(() => {
      const textarea = textareaRef.current;
      if (!textarea) return;
      textarea.focus();
      textarea.setSelectionRange(nextCaret, nextCaret);
    });
  };

  const onKeyDown = (event: TargetedKeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey) && onSubmit) {
      event.preventDefault();
      onSubmit();
    }
  };

  const syncCaret = () => setCaret(textareaRef.current?.selectionStart ?? value.length);

  return (
    <div class="review-input">
      {suggestions.length > 0 && (
        <div class="tag-suggestions" role="listbox" aria-label="Tags">
          {suggestions.map((tag) => (
            <button
              key={tag}
              type="button"
              class="tag-chip pressable"
              onPointerDown={(event) => event.preventDefault()}
              onClick={() => pick(tag)}
            >
              #{tag}
            </button>
          ))}
        </div>
      )}
      <textarea
        ref={textareaRef}
        class="review-textarea"
        rows={2}
        placeholder={placeholder}
        value={value}
        onInput={(event) => {
          onChange(event.currentTarget.value);
          setCaret(event.currentTarget.selectionStart);
        }}
        onKeyUp={syncCaret}
        onClick={syncCaret}
        onSelect={syncCaret}
        onKeyDown={onKeyDown}
        enterkeyhint="enter"
      />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Sheets for writing comments

export function CommentSheet({
  open,
  title,
  quote,
  initialText,
  submitLabel,
  tags,
  onSubmit,
  onClose,
  children,
}: {
  open: boolean;
  title: string;
  quote?: string | null;
  initialText: string;
  submitLabel: string;
  tags: string[];
  onSubmit: (text: string) => Promise<boolean>;
  onClose: () => void;
  children?: ComponentChildren;
}) {
  const [text, setText] = useState(initialText);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (open) setText(initialText);
  }, [open, initialText]);

  const submit = async () => {
    if (!text.trim() || busy) return;
    setBusy(true);
    const succeeded = await onSubmit(text.trim());
    setBusy(false);
    if (succeeded) onClose();
  };

  return (
    <Sheet open={open} onClose={onClose} title={title} class="comment-sheet">
      {children}
      {quote && <pre class="review-quote">{quote}</pre>}
      <CommentInput
        value={text}
        onChange={setText}
        placeholder="Write a comment… Type # for tags."
        tags={tags}
        autoFocus={open}
        onSubmit={() => void submit()}
      />
      <div class="comment-sheet-actions">
        <button type="button" class="button secondary pressable" onClick={onClose}>
          Cancel
        </button>
        <button type="button" class="button primary pressable" disabled={!text.trim() || busy} onClick={() => void submit()}>
          {busy ? <Spinner size={16} /> : null}
          {submitLabel}
        </button>
      </div>
    </Sheet>
  );
}

export interface LineCommentRequest {
  path: string;
  /** The file's lines, as the rows refer to them. */
  lines: string[];
  row: number;
  /** Comments on a commit's version of the file; otherwise the working tree's. */
  commit: string | null;
}

/** Comments on one or more lines, starting at the line that was tapped. */
export function LineCommentSheet({
  workspaceId,
  request,
  tags,
  onClose,
  onAdded,
}: {
  workspaceId: WorkspaceId;
  request: LineCommentRequest | null;
  tags: string[];
  onClose: () => void;
  onAdded?: () => void;
}) {
  const [last, setLast] = useState<LineCommentRequest | null>(request);
  const [range, setRange] = useState({ start: request?.row ?? 0, end: request?.row ?? 0 });

  useEffect(() => {
    if (!request) return;
    setLast(request);
    setRange({ start: request.row, end: request.row });
  }, [request]);

  const shown = request ?? last;
  const lineCount = shown?.lines.length ?? 1;
  const quote = shown ? shown.lines.slice(range.start, range.end + 1).join("\n") : "";
  const label = range.start === range.end ? `Line ${range.start + 1}` : `Lines ${range.start + 1}–${range.end + 1}`;
  const step = (edge: "start" | "end", delta: number) => {
    haptic();
    setRange((current) => {
      const next = { ...current, [edge]: current[edge] + delta };
      next.start = Math.max(0, Math.min(next.start, current.end));
      next.end = Math.min(lineCount - 1, Math.max(next.end, next.start));
      return next;
    });
  };

  return (
    <CommentSheet
      open={!!request}
      title={shown ? splitPath(shown.path).name : "Comment"}
      quote={quote}
      initialText=""
      submitLabel="Add comment"
      tags={tags}
      onClose={onClose}
      onSubmit={async (text) => {
        if (!shown) return false;
        const added = await addReviewComment(workspaceId, text, {
          kind: "line",
          path: shown.path,
          start_row: range.start,
          end_row: range.end,
          commit: shown.commit,
        });
        if (added) {
          toast("Comment added");
          onAdded?.();
        }
        return added;
      }}
    >
      <div class="line-range">
        <button type="button" class="icon-button pressable" aria-label="Start one line earlier" disabled={range.start === 0} onClick={() => step("start", -1)}>
          <Icon name="arrow-up" size={16} />
        </button>
        <button type="button" class="icon-button pressable" aria-label="Start one line later" disabled={range.start === range.end} onClick={() => step("start", 1)}>
          <Icon name="arrow-down" size={16} />
        </button>
        <span class="line-range-label">{label}</span>
        <button type="button" class="icon-button pressable" aria-label="End one line earlier" disabled={range.start === range.end} onClick={() => step("end", -1)}>
          <Icon name="arrow-up" size={16} />
        </button>
        <button type="button" class="icon-button pressable" aria-label="End one line later" disabled={range.end >= lineCount - 1} onClick={() => step("end", 1)}>
          <Icon name="arrow-down" size={16} />
        </button>
      </div>
    </CommentSheet>
  );
}

/** Edits a comment. */
export function EditCommentSheet({
  workspaceId,
  comment,
  tags,
  onClose,
}: {
  workspaceId: WorkspaceId;
  comment: ReviewCommentSnapshot | null;
  tags: string[];
  onClose: () => void;
}) {
  const [last, setLast] = useState(comment);
  useEffect(() => {
    if (comment) setLast(comment);
  }, [comment]);
  const shown = comment ?? last;
  return (
    <CommentSheet
      open={!!comment}
      title="Edit comment"
      quote={shown?.excerpt}
      initialText={shown?.text ?? ""}
      submitLabel="Save"
      tags={tags}
      onClose={onClose}
      onSubmit={(text) =>
        shown
          ? mutate(workspaceId, () =>
              call("review.edit_comment", { workspace_id: workspaceId, comment_id: shown.comment_id, text }),
            )
          : Promise.resolve(false)
      }
    >
      {shown && <div class="comment-sheet-location">{shown.location}</div>}
    </CommentSheet>
  );
}

// ---------------------------------------------------------------------------
// The review overlay

type ReviewSegment = "comments" | "changes" | "commits";

interface FocusRequest {
  path: string;
  commentId: string;
  /** Opened if the comment can't be found in the changes. */
  commit: string | null;
  nonce: number;
}

export function ReviewOverlay({ target }: { target: Extract<OverlayTarget, { type: "review" }> }) {
  const workspaceId = target.workspaceId;
  const state = useReviewState(workspaceId);
  const [segment, setSegment] = useState<ReviewSegment>("comments");
  const [focus, setFocus] = useState<FocusRequest | null>(null);
  const [pickingBase, setPickingBase] = useState(false);
  const draftCount = state?.reviews.find((review) => review.is_draft)?.comments.length ?? 0;

  const openCommit = async (sha: string, title: string) => {
    haptic();
    const opened = await call("git.open_commit", { workspace_id: workspaceId, sha }, { timeout: 30_000 });
    if (!opened) return;
    pushOverlay({ type: "item", workspaceId, itemId: opened.item_id, kind: opened.kind, offset: null, title });
  };

  const jump = (comment: ReviewCommentSnapshot) => {
    if (comment.path && !comment.is_deleted_side) {
      setFocus({ path: comment.path, commentId: comment.comment_id, commit: comment.commit, nonce: Date.now() });
      setSegment("changes");
    } else if (comment.commit) {
      void openCommit(comment.commit, comment.location);
    }
  };

  const subtitle = state
    ? [state.worktree_name, state.branch && state.base ? `${state.branch} → ${state.base}` : state.branch]
        .filter(Boolean)
        .join(" · ")
    : null;

  return (
    <div class="overlay-frame review-overlay">
      <header class="overlay-header">
        <div class="overlay-title">
          <span class="overlay-title-text">Review</span>
        </div>
        {subtitle && (
          <div class="overlay-subtitle">
            <bdi>{subtitle}</bdi>
          </div>
        )}
      </header>
      <div class="overlay-toolbar review-toolbar">
        <div class="segmented">
          <button class={segment === "comments" ? "selected" : ""} onClick={() => setSegment("comments")}>
            Comments{draftCount > 0 ? ` (${draftCount})` : ""}
          </button>
          <button class={segment === "changes" ? "selected" : ""} onClick={() => setSegment("changes")}>
            Changes
          </button>
          <button class={segment === "commits" ? "selected" : ""} onClick={() => setSegment("commits")}>
            Commits{state && state.commits.length > 0 ? ` (${state.commits.length})` : ""}
          </button>
        </div>
        {state && !state.unavailable_reason && (
          <button class="review-base pressable" onClick={() => setPickingBase(true)} aria-label="Pick the base branch">
            <Icon name="branch" size={15} />
            <span>{state.base ?? "Pick a base"}</span>
            <Icon name="chevron-down" size={14} />
          </button>
        )}
      </div>
      <div class="overlay-content">
        {!state ? (
          <div class="center-fill">
            <Spinner size={24} />
          </div>
        ) : state.unavailable_reason ? (
          <EmptyState icon="alert" title="Can't review this workspace">
            {state.unavailable_reason}
          </EmptyState>
        ) : segment === "comments" ? (
          <CommentsView state={state} onJump={jump} />
        ) : segment === "changes" ? (
          <ChangesView state={state} focus={focus} onOpenCommit={openCommit} />
        ) : (
          <CommitsView state={state} onOpenCommit={openCommit} />
        )}
        <CloseButton />
      </div>
      {state && !state.unavailable_reason && segment === "comments" && <ReviewComposer state={state} />}
      {state && (
        <PickerSheet
          open={pickingBase}
          title="Base branch"
          options={state.branches
            .filter((branch) => branch !== state.branch)
            .map((branch) => ({ value: branch, label: branch, icon: "branch" as const }))}
          value={state.base}
          onSelect={(base) =>
            void mutate(workspaceId, () => call("review.set_base", { workspace_id: workspaceId, base }))
          }
          onClose={() => setPickingBase(false)}
        />
      )}
    </div>
  );
}

function ReviewComposer({ state }: { state: ReviewState }) {
  const workspaceId = state.workspace_id;
  const [text, setText] = useState("");
  const [busy, setBusy] = useState<"comment" | "submit" | null>(null);
  const hasText = text.trim().length > 0;
  const hasDraft = state.reviews.some((review) => review.is_draft && review.comments.length > 0);
  // The server can't see the text that's not added yet.
  const blocker = state.submit_blocker === "Add comments to submit a review" && hasText ? null : state.submit_blocker;

  const comment = async () => {
    if (!hasText || busy) return false;
    setBusy("comment");
    const added = await addReviewComment(workspaceId, text.trim(), { kind: "general" });
    setBusy(null);
    if (added) setText("");
    return added;
  };

  const submit = async () => {
    if (blocker || busy) return;
    if (hasText && !(await comment())) return;
    setBusy("submit");
    const submitted = await mutate(workspaceId, () => call("review.submit", { workspace_id: workspaceId }));
    setBusy(null);
    if (submitted) toast("Review submitted");
  };

  return (
    <div class="review-composer">
      <CommentInput
        value={text}
        onChange={setText}
        placeholder="Comment on the latest commit… Type # for tags."
        tags={state.tags}
        onSubmit={() => void comment()}
      />
      <div class="review-composer-bar">
        {blocker && (hasDraft || hasText) ? <span class="review-blocker">{blocker}</span> : <span class="review-blocker" />}
        <button type="button" class="button secondary pressable" disabled={!hasText || !!busy} onClick={() => void comment()}>
          {busy === "comment" ? <Spinner size={14} /> : null}
          Comment
        </button>
        <button
          type="button"
          class="button primary pressable"
          disabled={!!blocker || !!busy || state.is_submitting}
          onClick={() => void submit()}
        >
          {busy === "submit" || state.is_submitting ? <Spinner size={14} /> : null}
          Submit review
        </button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Comments

function CommentsView({ state, onJump }: { state: ReviewState; onJump: (comment: ReviewCommentSnapshot) => void }) {
  const [editing, setEditing] = useState<ReviewCommentSnapshot | null>(null);
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const reviews = state.reviews;

  return (
    <div class="review-scroll">
      {state.error && (
        <div class="callout error compact">
          <Icon name="alert" size={16} />
          <div class="callout-text">{state.error}</div>
        </div>
      )}
      {!state.is_loaded ? (
        <div class="center-fill">
          <Spinner size={22} />
        </div>
      ) : reviews.length === 0 ? (
        <EmptyState icon="message" title="No comments yet">
          Tap a line number in Changes or a commit to comment on it, or comment on the latest commit below.
        </EmptyState>
      ) : (
        reviews.map((review) => (
          <ReviewSection
            key={review.review_id}
            review={review}
            workspaceId={state.workspace_id}
            collapsed={collapsed.has(review.review_id)}
            onToggle={() =>
              setCollapsed((previous) => {
                const next = new Set(previous);
                if (!next.delete(review.review_id)) next.add(review.review_id);
                return next;
              })
            }
            onEdit={setEditing}
            onJump={onJump}
          />
        ))
      )}
      <EditCommentSheet workspaceId={state.workspace_id} comment={editing} tags={state.tags} onClose={() => setEditing(null)} />
    </div>
  );
}

function ReviewSection({
  review,
  workspaceId,
  collapsed,
  onToggle,
  onEdit,
  onJump,
}: {
  review: ReviewSnapshot;
  workspaceId: WorkspaceId;
  collapsed: boolean;
  onToggle: () => void;
  onEdit: (comment: ReviewCommentSnapshot) => void;
  onJump: (comment: ReviewCommentSnapshot) => void;
}) {
  const time = review.submitted_at ?? review.created_at;
  const count = review.comments.length;
  return (
    <section class="review-section">
      <div class="review-section-header">
        <button class="review-section-toggle pressable" onClick={onToggle} aria-expanded={!collapsed}>
          <Icon name={collapsed ? "chevron-right" : "chevron-down"} size={16} />
          <span class="review-section-title">{review.is_draft ? "Pending review" : "Review"}</span>
          <span class="review-section-meta" title={formatFullTime(time)}>
            {formatTime(time)} · {count} comment{count === 1 ? "" : "s"}
            {count > 0 ? ` · ${review.unaddressed_count} open` : ""}
          </span>
        </button>
        <button class="icon-button pressable" aria-label="Copy review ID" onClick={() => void copyText(review.review_id, "review ID")}>
          <span class="id-glyph">#</span>
        </button>
        <button class="icon-button pressable" aria-label="Copy review as text" onClick={() => void copyText(review.as_text, "review")}>
          <Icon name="copy" size={16} />
        </button>
      </div>
      {!collapsed &&
        review.comments.map((comment) => (
          <CommentCard key={comment.comment_id} comment={comment} workspaceId={workspaceId} onEdit={onEdit} onJump={onJump} />
        ))}
    </section>
  );
}

function CommentCard({
  comment,
  workspaceId,
  onEdit,
  onJump,
}: {
  comment: ReviewCommentSnapshot;
  workspaceId: WorkspaceId;
  onEdit: (comment: ReviewCommentSnapshot) => void;
  onJump: (comment: ReviewCommentSnapshot) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const collapsed = comment.addressed && !expanded;
  const body = commentBody(comment.text);
  const edited = comment.updated_at !== comment.created_at;
  const canJump = !!comment.commit || (!!comment.path && !comment.is_deleted_side);

  const setAddressed = (addressed: boolean) =>
    void mutate(workspaceId, () =>
      call("review.set_addressed", { workspace_id: workspaceId, comment_id: comment.comment_id, addressed }),
    );
  const remove = () => {
    if (!confirm("Delete this comment?")) return;
    void mutate(workspaceId, () =>
      call("review.delete_comment", { workspace_id: workspaceId, comment_id: comment.comment_id }),
    );
  };

  return (
    <article class={`review-comment ${comment.addressed ? "addressed" : ""}`}>
      <div class="review-comment-header">
        <button
          class={`address-toggle pressable ${comment.addressed ? "on" : ""}`}
          aria-label={comment.addressed ? "Mark as unaddressed" : "Mark as addressed"}
          onClick={() => setAddressed(!comment.addressed)}
        >
          <Icon name={comment.addressed ? "check-circle" : "circle"} size={20} />
        </button>
        <button class="review-comment-location pressable" disabled={!canJump} onClick={() => onJump(comment)}>
          <Icon name={comment.path ? "file" : "message"} size={14} />
          <span>{comment.location}</span>
        </button>
        <span class="review-comment-time" title={`Created ${formatFullTime(comment.created_at)}${edited ? `, edited ${formatFullTime(comment.updated_at)}` : ""}`}>
          {formatTime(comment.created_at)}
          {edited ? " ✎" : ""}
        </span>
        {comment.addressed && (
          <button class="icon-button pressable" aria-label={collapsed ? "Expand" : "Collapse"} onClick={() => setExpanded(!expanded)}>
            <Icon name={collapsed ? "chevron-right" : "chevron-down"} size={16} />
          </button>
        )}
      </div>
      {collapsed ? (
        <button class="review-comment-collapsed pressable" onClick={() => setExpanded(true)}>
          {body.split("\n")[0]}
        </button>
      ) : (
        <>
          {comment.tags.length > 0 && (
            <div class="review-comment-tags">
              {comment.tags.map((tag) => (
                <span key={tag} class="tag-chip static">
                  #{tag}
                </span>
              ))}
            </div>
          )}
          {body && <div class="review-comment-body">{body}</div>}
          <div class="review-comment-actions">
            <button class="icon-button pressable" aria-label="Edit" onClick={() => onEdit(comment)}>
              <Icon name="pencil" size={16} />
            </button>
            <button class="icon-button pressable" aria-label="Delete" onClick={remove}>
              <Icon name="trash" size={16} />
            </button>
            <button class="icon-button pressable" aria-label="Copy comment ID" onClick={() => void copyText(comment.comment_id, "comment ID")}>
              <span class="id-glyph">#</span>
            </button>
            <button class="icon-button pressable" aria-label="Copy comment as text" onClick={() => void copyText(comment.as_text, "comment")}>
              <Icon name="copy" size={16} />
            </button>
          </div>
        </>
      )}
    </article>
  );
}

// ---------------------------------------------------------------------------
// Changes since the base

function ChangesView({
  state,
  focus,
  onOpenCommit,
}: {
  state: ReviewState;
  focus: FocusRequest | null;
  onOpenCommit: (sha: string, title: string) => void;
}) {
  const workspaceId = state.workspace_id;
  const [diff, setDiff] = useState<ReviewDiff | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [lineRequest, setLineRequest] = useState<LineCommentRequest | null>(null);
  const [editing, setEditing] = useState<ReviewCommentSnapshot | null>(null);
  const loadToken = useRef(0);

  const load = async () => {
    const token = ++loadToken.current;
    try {
      const next = await conn.request("review.diff", { workspace_id: workspaceId }, { timeout: 60_000 });
      if (token !== loadToken.current) return;
      setDiff(next);
      setError(null);
    } catch (reason) {
      if (token === loadToken.current) setError(errorMessage(reason));
    }
  };

  // Comments and the base change with the state; the files with each save.
  const stateKey = `${state.base}:${state.reviews.map((review) => review.comments.length).join(",")}:${state.uncommitted_files}:${state.commits[0]?.sha ?? ""}`;
  useEffect(() => {
    void load();
  }, [stateKey]);

  const comments = new Map(
    state.reviews.flatMap((review) => review.comments).map((comment) => [comment.comment_id, comment] as const),
  );

  // A focused comment that isn't in the changes is shown in its commit.
  useEffect(() => {
    if (!focus || !diff) return;
    const found = diff.files.some(
      (file) => file.path === focus.path && file.comments.some((comment) => comment.comment_id === focus.commentId),
    );
    if (!found && focus.commit) onOpenCommit(focus.commit, focus.path);
  }, [focus?.nonce, diff]);

  if (error && !diff) {
    return (
      <EmptyState icon="alert" title="Couldn't load the changes">
        {error}
        <button class="button secondary pressable" onClick={() => void load()}>
          Try again
        </button>
      </EmptyState>
    );
  }
  if (!diff) {
    return (
      <div class="center-fill">
        <Spinner size={24} />
      </div>
    );
  }
  return (
    <div class="multibuffer-scroll">
      {diff.files.length === 0 ? (
        <EmptyState icon="check-circle" title="No changes">
          Nothing changed since {diff.base ?? "the base"}.
        </EmptyState>
      ) : (
        <>
          <div class="list-note">Tap a line number to comment on it.</div>
          {diff.files.map((file) => (
            <ReviewFileSection
              key={file.path}
              file={file}
              comments={comments}
              focusRow={
                focus && focus.path === file.path
                  ? (file.comments.find((comment) => comment.comment_id === focus.commentId)?.start_row ?? null)
                  : null
              }
              focusNonce={focus?.nonce ?? 0}
              onLineTap={(row) =>
                setLineRequest({ path: file.path, lines: (file.new_text ?? "").split("\n"), row, commit: null })
              }
              onCommentTap={(id) => {
                const comment = comments.get(id);
                if (comment) setEditing(comment);
              }}
            />
          ))}
        </>
      )}
      <div class="multibuffer-end" />
      <LineCommentSheet
        workspaceId={workspaceId}
        request={lineRequest}
        tags={state.tags}
        onClose={() => setLineRequest(null)}
      />
      <EditCommentSheet workspaceId={workspaceId} comment={editing} tags={state.tags} onClose={() => setEditing(null)} />
    </div>
  );
}

const STATUS_LETTER: Record<string, string> = {
  added: "A",
  modified: "M",
  deleted: "D",
  renamed: "R",
  untracked: "U",
  conflict: "!",
};

function estimateHeight(text: string | null | undefined): number {
  if (!text) return 120;
  const lines = Math.min(40, text.split("\n").length);
  return Math.max(80, lines * 22);
}

/** Shows the comments as marks in a diff view, updating them as they change. */
export function useReviewMarks(view: ReviewDiffView | null, marks: ReviewMark[]) {
  const key = marks.map((mark) => `${mark.id}:${mark.startRow}:${mark.endRow}:${mark.text}`).join("|");
  useEffect(() => {
    view?.setMarks(marks);
  }, [view, key]);
}

function ReviewFileSection({
  file,
  comments,
  focusRow,
  focusNonce,
  onLineTap,
  onCommentTap,
}: {
  file: ReviewDiffFile;
  comments: Map<string, ReviewCommentSnapshot>;
  focusRow: number | null;
  focusNonce: number;
  onLineTap: (row: number) => void;
  onCommentTap: (id: string) => void;
}) {
  const [visible, setVisible] = useState(focusRow !== null);
  const hostRef = useRef<HTMLDivElement>(null);
  const sectionRef = useRef<HTMLElement>(null);
  const [view, setView] = useState<ReviewDiffView | null>(null);
  const handlers = useRef({ onLineTap, onCommentTap });
  handlers.current = { onLineTap, onCommentTap };
  const placeholderRef = useInView<HTMLDivElement>(() => setVisible(true), { rootMargin: "800px", enabled: !visible && !file.is_binary });

  useEffect(() => {
    if (focusRow !== null) setVisible(true);
  }, [focusNonce]);

  useEffect(() => {
    if (!visible || !hostRef.current || file.is_binary) return;
    const created = createReviewDiffView(hostRef.current, file.old_text ?? "", file.new_text ?? "", {
      onLineTap: (row) => (file.new_text === null ? undefined : handlers.current.onLineTap(row)),
      onCommentTap: (id) => handlers.current.onCommentTap(id),
    });
    setView(created);
    return () => {
      created.view.destroy();
      setView(null);
    };
  }, [visible, file.old_text, file.new_text]);

  useReviewMarks(
    view,
    file.comments.map((mark) => ({
      id: mark.comment_id,
      startRow: mark.start_row,
      endRow: mark.end_row,
      text: comments.get(mark.comment_id)?.text ?? "",
    })),
  );

  useEffect(() => {
    if (focusRow === null) return;
    if (!view) {
      sectionRef.current?.scrollIntoView({ block: "start" });
      return;
    }
    const timer = setTimeout(() => view.revealRow(focusRow), 60);
    return () => clearTimeout(timer);
  }, [focusNonce, view]);

  const { dir, name } = splitPath(file.path);
  const count = file.comments.length;
  return (
    <section class="file-section" ref={sectionRef}>
      <div class="file-section-header">
        <span class={`file-status git-${file.status}`}>{STATUS_LETTER[file.status]}</span>
        <span class="file-section-path">
          <span class="file-section-name">{name}</span>
          {dir && (
            <span class="file-section-dir">
              <bdi>{dir}</bdi>
            </span>
          )}
        </span>
        {count > 0 && (
          <span class="review-file-count">
            <Icon name="message" size={13} />
            {count}
          </span>
        )}
      </div>
      {file.is_binary ? (
        <div class="binary-placeholder">
          <Icon name="file" size={18} />
          Binary or very large file not shown
        </div>
      ) : visible ? (
        <div class="file-section-body" ref={hostRef} />
      ) : (
        <div
          class="file-section-placeholder"
          ref={placeholderRef}
          style={{ height: `${estimateHeight(file.new_text ?? file.old_text)}px` }}
        />
      )}
    </section>
  );
}

// ---------------------------------------------------------------------------
// Commits on top of the base

function CommitsView({ state, onOpenCommit }: { state: ReviewState; onOpenCommit: (sha: string, title: string) => void }) {
  const workspaceId = state.workspace_id;
  const uncommittedComments = state.reviews
    .flatMap((review) => review.comments)
    .filter((comment) => comment.path && !comment.commit).length;

  const openUncommitted = async () => {
    haptic();
    const opened = await call("git.open_uncommitted", { workspace_id: workspaceId }, { timeout: 30_000 });
    if (opened) {
      pushOverlay({ type: "item", workspaceId, itemId: opened.item_id, kind: opened.kind, offset: null, title: "Uncommitted Changes" });
    }
  };

  return (
    <div class="history-list review-scroll">
      {state.uncommitted_files > 0 && (
        <button class="history-row uncommitted pressable" onClick={() => void openUncommitted()}>
          <span class="history-icon review-working-copy">@</span>
          <span class="history-text">
            <span class="history-subject">Uncommitted changes</span>
            <span class="history-meta">
              {state.uncommitted_files} file{state.uncommitted_files === 1 ? "" : "s"}
              {uncommittedComments > 0 ? ` · ${uncommittedComments} comment${uncommittedComments === 1 ? "" : "s"}` : ""}
            </span>
          </span>
          <Icon name="chevron-right" size={16} class="muted" />
        </button>
      )}
      {state.commits.map((commit) => {
        const date = parseDate(commit.timestamp);
        return (
          <button key={commit.sha} class="history-row pressable" onClick={() => onOpenCommit(commit.sha, commit.subject)}>
            <span class="history-icon">
              <Icon name="commit" size={16} />
            </span>
            <span class="history-text">
              <span class="history-subject">{commit.subject}</span>
              <span class="history-meta">
                <span class="sha">{commit.sha.slice(0, 7)}</span>
                {commit.author}
                {date && ` · ${relativeTime(date)}`}
                {commit.comment_count > 0 && ` · ${commit.comment_count} comment${commit.comment_count === 1 ? "" : "s"}`}
              </span>
            </span>
            <Icon name="chevron-right" size={16} class="muted" />
          </button>
        );
      })}
      {state.commits.length === 0 && state.uncommitted_files === 0 && (
        <div class="list-note">{state.commits_error ?? `No commits on top of ${state.base ?? "the base"}.`}</div>
      )}
    </div>
  );
}

