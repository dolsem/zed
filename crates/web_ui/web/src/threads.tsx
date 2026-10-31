import { useEffect, useState } from "preact/hooks";
import type { NewThreadOptions, ProjectThreads, ThreadStatus, ThreadSummary, WorktreeChoice } from "./protocol";
import { SettingsSheet } from "./settings";
import { call, conn, currentThreadId, openThread, refreshThreads, threadsList } from "./state";
import {
  EmptyState,
  haptic,
  Icon,
  Modal,
  parseDate,
  PickerRow,
  relativeTime,
  Spinner,
  type PickerOption,
} from "./ui";

export function ThreadsView() {
  const list = threadsList.value;
  const [creating, setCreating] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const status = conn.status.value;

  return (
    <div class="panel-inner">
      <header class="panel-header">
        <div class="panel-header-title large">Threads</div>
        <div class="panel-header-actions">
          <button class="header-button" onClick={() => setSettingsOpen(true)} aria-label="Settings">
            <Icon name="gear" size={22} />
          </button>
        </div>
      </header>
      <div class="panel-scroll threads-scroll">
        {list === null ? (
          status === "ready" ? (
            <div class="center-fill">
              <Spinner size={22} />
            </div>
          ) : (
            <ThreadsSkeleton />
          )
        ) : list.projects.length === 0 ? (
          <EmptyState icon="message" title="No threads yet">
            Tap <b>+</b> to start a new thread in one of your projects.
          </EmptyState>
        ) : (
          list.projects.map((project) => <ProjectGroup key={project.project_id} project={project} />)
        )}
        <div class="fab-spacer" />
      </div>
      <button
        class="fab pressable"
        aria-label="New thread"
        onClick={() => {
          haptic();
          setCreating(true);
        }}
      >
        <Icon name="plus" size={26} />
      </button>
      <NewThreadModal open={creating} onClose={() => setCreating(false)} />
      <SettingsSheet open={settingsOpen} onClose={() => setSettingsOpen(false)} />
    </div>
  );
}

function ThreadsSkeleton() {
  return (
    <div class="skeleton-list" aria-hidden="true">
      {[0, 1].map((group) => (
        <div class="project-group" key={group}>
          <div class="skeleton skeleton-heading" />
          {[0, 1, 2].map((row) => (
            <div class="skeleton skeleton-row" key={row} />
          ))}
        </div>
      ))}
    </div>
  );
}

function ProjectGroup({ project }: { project: ProjectThreads }) {
  return (
    <section class="project-group">
      <div class={`project-header ${project.is_active ? "active" : ""}`}>
        <span class="project-name">{project.name}</span>
        {project.remote_host && (
          <span class="remote-badge">
            <Icon name="server" size={12} />
            {project.remote_host}
          </span>
        )}
        <span class="project-count">{project.threads.length || ""}</span>
      </div>
      {project.threads.length === 0 ? (
        <div class="project-empty">No threads</div>
      ) : (
        <div class="thread-list">
          {project.threads.map((thread) => (
            <ThreadRow key={thread.thread_id} thread={thread} />
          ))}
        </div>
      )}
    </section>
  );
}

function StatusIndicator({ status, terminal }: { status: ThreadStatus; terminal: boolean }) {
  switch (status) {
    case "running":
      return (
        <span class="status-indicator running">
          <Spinner size={18} />
        </span>
      );
    case "waiting_for_confirmation":
      return (
        <span class="status-indicator waiting">
          <Icon name="alert" size={18} />
        </span>
      );
    case "error":
      return (
        <span class="status-indicator error">
          <Icon name="x-circle" size={18} />
        </span>
      );
    default:
      return (
        <span class="status-indicator idle">
          <Icon name={terminal ? "terminal" : "message"} size={18} />
        </span>
      );
  }
}

function ThreadRow({ thread }: { thread: ThreadSummary }) {
  const terminal = thread.kind === "terminal";
  const current = currentThreadId.value === thread.thread_id;
  const updated = parseDate(thread.updated_at);
  return (
    <button
      class={`thread-row ${terminal ? "disabled" : "pressable"} ${thread.is_active ? "active" : ""} ${current ? "current" : ""}`}
      disabled={terminal}
      onClick={() => {
        if (terminal) return;
        haptic();
        openThread(thread.thread_id);
      }}
    >
      <StatusIndicator status={thread.status} terminal={terminal} />
      <div class="thread-main">
        <div class="thread-title-row">
          <span class="thread-title">{thread.title || "New Thread"}</span>
          {updated && <span class="thread-time">{relativeTime(updated)}</span>}
        </div>
        <div class="thread-meta">
          {thread.status === "waiting_for_confirmation" && <span class="badge warning">Needs approval</span>}
          {thread.status === "error" && <span class="badge error">Error</span>}
          {terminal && <span class="badge">Terminal</span>}
          {thread.agent_name && <span class="thread-agent">{thread.agent_name}</span>}
          {thread.worktrees.map((worktree) => (
            <span class="worktree-chip" key={worktree}>
              <Icon name="branch" size={11} />
              {worktree}
            </span>
          ))}
          {(thread.lines_added > 0 || thread.lines_removed > 0) && (
            <span class="line-stats">
              <span class="added">+{thread.lines_added}</span>
              <span class="removed">−{thread.lines_removed}</span>
            </span>
          )}
        </div>
      </div>
      {thread.has_notification && <span class="notification-dot" aria-label="Unread" />}
    </button>
  );
}

// ---------------------------------------------------------------------------
// New thread form

const NEW_WORKTREE = "\u0000new";

function NewThreadModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [options, setOptions] = useState<NewThreadOptions | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [projectId, setProjectId] = useState<string | null>(null);
  const [worktree, setWorktree] = useState<string | null>(null);
  const [worktreeName, setWorktreeName] = useState("");
  const [branch, setBranch] = useState<string | null>(null);
  const [agentId, setAgentId] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const project = options?.projects.find((candidate) => candidate.project_id === projectId) ?? null;

  const selectProject = (id: string, source: NewThreadOptions | null = options) => {
    const next = source?.projects.find((candidate) => candidate.project_id === id);
    setProjectId(id);
    setWorktree(next?.worktrees[0]?.path ?? null);
    setBranch(next?.branches[0] ?? null);
    setWorktreeName("");
  };

  useEffect(() => {
    if (!open) return;
    setLoadError(null);
    setSubmitting(false);
    conn.request("threads.new_thread_options", {}).then(
      (result) => {
        setOptions(result);
        const activeProject = threadsList.peek()?.projects.find((candidate) => candidate.is_active);
        const initial =
          result.projects.find((candidate) => candidate.project_id === activeProject?.project_id) ?? result.projects[0];
        if (initial) selectProject(initial.project_id, result);
        setAgentId(result.default_agent_id || result.agents[0]?.id || null);
      },
      (error: unknown) => setLoadError(error instanceof Error ? error.message : String(error)),
    );
  }, [open]);

  const projectOptions: PickerOption[] = (options?.projects ?? []).map((candidate) => ({
    value: candidate.project_id,
    label: candidate.name,
    description: candidate.remote_host ? `on ${candidate.remote_host}` : null,
    icon: candidate.remote_host ? "server" : "folder",
  }));

  const worktreeOptions: PickerOption[] = (project?.worktrees ?? []).map((candidate) => ({
    value: candidate.path,
    label: candidate.name,
    description: [candidate.branch, candidate.path].filter(Boolean).join(" · "),
    icon: "branch",
    badge: candidate.is_main ? "main" : null,
  }));
  if (project?.can_create_worktree) {
    worktreeOptions.push({ value: NEW_WORKTREE, label: "New worktree…", icon: "plus", group: "Create" });
  }

  const branchOptions: PickerOption[] = (project?.branches ?? []).map((candidate, index) => ({
    value: candidate,
    label: candidate,
    badge: index === 0 ? "current" : null,
  }));

  const agentOptions: PickerOption[] = (options?.agents ?? []).map((agent) => ({
    value: agent.id,
    label: agent.name,
    group: agent.is_external ? "External agents" : "Zed",
    icon: agent.is_external ? "bot" : "sparkle",
    badge: agent.id === options?.default_agent_id ? "default" : null,
  }));

  const canSubmit = !!project && !!agentId && !submitting;

  const submit = async () => {
    if (!project || !agentId) return;
    let choice: WorktreeChoice;
    if (worktree === NEW_WORKTREE) {
      choice = { kind: "new", name: worktreeName.trim() || null, branch };
    } else {
      // Non-git projects have no worktrees; Zed uses the project's root then.
      choice = { kind: "existing", path: worktree ?? "" };
    }
    haptic();
    setSubmitting(true);
    const created = await call(
      "threads.create",
      { project_id: project.project_id, worktree: choice, agent_id: agentId },
      { timeout: 120_000 },
    );
    setSubmitting(false);
    if (!created) return;
    onClose();
    openThread(created.thread_id);
    void refreshThreads();
  };

  return (
    <Modal open={open} onClose={onClose} title="New Thread">
      {!options ? (
        <div class="center-fill">
          {loadError ? (
            <EmptyState icon="alert" title="Couldn't load options">
              {loadError}
            </EmptyState>
          ) : (
            <Spinner size={22} />
          )}
        </div>
      ) : options.projects.length === 0 ? (
        <EmptyState icon="folder" title="No projects open">
          Open a project in Zed on your desktop first.
        </EmptyState>
      ) : (
        <form
          class="form"
          onSubmit={(event) => {
            event.preventDefault();
            void submit();
          }}
        >
          <div class="form-section-title">Project</div>
          <div class="form-group">
            <PickerRow label="Project" options={projectOptions} value={projectId} onChange={(id) => selectProject(id)} />
          </div>

          {worktreeOptions.length > 0 && (
            <>
              <div class="form-section-title">Worktree</div>
              <div class="form-group">
                <PickerRow label="Worktree" options={worktreeOptions} value={worktree} onChange={setWorktree} />
                {worktree === NEW_WORKTREE && (
                  <>
                    <label class="form-row">
                      <span class="form-row-label">Name</span>
                      <input
                        class="form-input"
                        type="text"
                        placeholder="Optional"
                        value={worktreeName}
                        autocapitalize="off"
                        autocorrect="off"
                        spellcheck={false}
                        onInput={(event) => setWorktreeName(event.currentTarget.value)}
                      />
                    </label>
                    {branchOptions.length > 0 && (
                      <PickerRow label="Based on" title="Base branch" options={branchOptions} value={branch} onChange={setBranch} />
                    )}
                  </>
                )}
              </div>
            </>
          )}

          <div class="form-section-title">Agent</div>
          <div class="form-group">
            <PickerRow label="Agent" options={agentOptions} value={agentId} onChange={setAgentId} />
          </div>

          <div class="form-footer">
            <button type="submit" class="button primary large pressable" disabled={!canSubmit}>
              {submitting ? <Spinner size={18} /> : <Icon name="plus" size={18} />}
              {submitting ? (worktree === NEW_WORKTREE ? "Creating worktree…" : "Creating…") : "Start Thread"}
            </button>
          </div>
        </form>
      )}
    </Modal>
  );
}
