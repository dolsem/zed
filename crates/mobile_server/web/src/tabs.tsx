import { useEffect, useState } from "preact/hooks";
import type { CommitSummary, DirectoryEntry, EditorTab, EditorTabKind, OpenedItem, TerminalTab, WorkspaceId } from "./protocol";
import {
  activeWorkspaceId,
  call,
  conn,
  goToPanel,
  openOverlay,
  PANEL_THREAD,
  refreshWorkspace,
  reportError,
  workspaceItems,
} from "./state";
import { EmptyState, haptic, Icon, Modal, parseDate, relativeTime, Spinner, splitPath, useInView, type IconName } from "./ui";

const TAB_ICONS: Record<EditorTabKind, IconName> = {
  file: "file",
  diff: "diff",
  commit: "commit",
  other: "layers",
};

function openItem(workspaceId: WorkspaceId, opened: OpenedItem, title: string) {
  openOverlay({
    type: "item",
    workspaceId,
    itemId: opened.item_id,
    kind: opened.kind,
    offset: opened.offset,
    title,
  });
}

export function TabsView() {
  const items = workspaceItems.value;
  const workspaceId = activeWorkspaceId.value;
  const [adding, setAdding] = useState(false);
  const [hidden, setHidden] = useState<Set<number>>(new Set());

  // Forget optimistic removals once the list refreshes.
  useEffect(() => setHidden(new Set()), [items]);

  const editors = (items?.editors ?? []).filter((tab) => !hidden.has(tab.item_id));
  const terminals = (items?.terminals ?? []).filter((tab) => !hidden.has(tab.item_id));

  const closeItem = async (itemId: number) => {
    if (workspaceId === null) return;
    haptic();
    setHidden((previous) => new Set(previous).add(itemId));
    const result = await call("workspace.close_item", { workspace_id: workspaceId, item_id: itemId });
    if (result === undefined) {
      setHidden((previous) => {
        const next = new Set(previous);
        next.delete(itemId);
        return next;
      });
    }
  };

  return (
    <div class="panel-inner tabs-view">
      <header class="panel-header">
        <button class="header-button" onClick={() => goToPanel(PANEL_THREAD)} aria-label="Thread">
          <Icon name="chevron-left" size={24} />
        </button>
        <div class="panel-header-title">
          <div class="thread-header-title">{items?.name ?? "Tabs"}</div>
          {items && <div class="thread-header-sub muted">Open tabs</div>}
        </div>
        <button
          class="header-button"
          aria-label="Open a file or commit"
          disabled={workspaceId === null}
          onClick={() => {
            haptic();
            setAdding(true);
          }}
        >
          <Icon name="plus" size={24} />
        </button>
      </header>

      <div class="panel-scroll tabs-scroll">
        {items === null ? (
          <div class="center-fill">
            <Spinner size={22} />
          </div>
        ) : editors.length === 0 ? (
          <EmptyState icon="file" title="No open tabs">
            <button class="button secondary pressable" onClick={() => setAdding(true)}>
              <Icon name="plus" size={16} />
              Open a file
            </button>
          </EmptyState>
        ) : (
          <div class="tab-list">
            {editors.map((tab) => (
              <EditorTabRow
                key={tab.item_id}
                tab={tab}
                active={items.active_item_id === tab.item_id}
                onOpen={() => workspaceId !== null && openOverlay({
                  type: "item",
                  workspaceId,
                  itemId: tab.item_id,
                  kind: tab.kind,
                  offset: null,
                  title: tab.title,
                })}
                onClose={() => void closeItem(tab.item_id)}
              />
            ))}
          </div>
        )}
        <button class="add-tab-row pressable" disabled={workspaceId === null} onClick={() => setAdding(true)}>
          <Icon name="plus" size={18} />
          Open file or commit…
        </button>
      </div>

      <TerminalDock workspaceId={workspaceId} terminals={terminals} />

      {workspaceId !== null && (
        <AddTabModal open={adding} workspaceId={workspaceId} onClose={() => setAdding(false)} />
      )}
    </div>
  );
}

function EditorTabRow({
  tab,
  active,
  onOpen,
  onClose,
}: {
  tab: EditorTab;
  active: boolean;
  onOpen: () => void;
  onClose: () => void;
}) {
  const disabled = tab.kind === "other";
  const path = tab.path ? splitPath(tab.path).dir : null;
  return (
    <div class={`tab-row ${disabled ? "disabled" : ""} ${active ? "active" : ""}`}>
      <button class="tab-row-main pressable" disabled={disabled} onClick={onOpen}>
        <span class={`tab-icon kind-${tab.kind}`}>
          <Icon name={TAB_ICONS[tab.kind]} size={18} />
        </span>
        <span class="tab-text">
          <span class="tab-title">
            {tab.title}
            {tab.is_dirty && <span class="dirty-dot" aria-label="Unsaved changes" />}
          </span>
          {path && (
            <span class="tab-path">
              <bdi>{path}</bdi>
            </span>
          )}
          {disabled && <span class="tab-path muted-note">Not available on mobile</span>}
        </span>
      </button>
      <button class="icon-button tab-close" onClick={onClose} aria-label={`Close ${tab.title}`}>
        <Icon name="x" size={16} />
      </button>
    </div>
  );
}

function TerminalDock({ workspaceId, terminals }: { workspaceId: WorkspaceId | null; terminals: TerminalTab[] }) {
  const [creating, setCreating] = useState(false);
  const create = async () => {
    if (workspaceId === null || creating) return;
    haptic();
    setCreating(true);
    const result = await call("workspace.new_terminal", { workspace_id: workspaceId });
    setCreating(false);
    // Zed may answer with the new terminal's item id; open it right away.
    if (result && typeof result === "object" && typeof (result as { item_id?: unknown }).item_id === "number") {
      openOverlay({
        type: "terminal",
        workspaceId,
        itemId: (result as { item_id: number }).item_id,
        title: "Terminal",
      });
    }
    void refreshWorkspace();
  };
  return (
    <div class="terminal-dock">
      <div class="dock-title">Terminals</div>
      <div class="dock-row">
        {terminals.map((terminal) => (
          <button
            key={terminal.item_id}
            class="dock-tile pressable"
            onClick={() => {
              if (workspaceId === null) return;
              haptic();
              openOverlay({ type: "terminal", workspaceId, itemId: terminal.item_id, title: terminal.title });
            }}
          >
            <span class="dock-tile-square">
              <Icon name="terminal" size={24} />
              {terminal.has_bell && (
                <span class="dock-bell">
                  <Icon name="bell" size={11} />
                </span>
              )}
            </span>
            <span class="dock-caption">{terminal.title}</span>
          </button>
        ))}
        <button class="dock-tile pressable" disabled={workspaceId === null} onClick={() => void create()}>
          <span class="dock-tile-square add">{creating ? <Spinner size={22} /> : <Icon name="plus" size={24} />}</span>
          <span class="dock-caption">New</span>
        </button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Add tab modal

function AddTabModal({ open, workspaceId, onClose }: { open: boolean; workspaceId: WorkspaceId; onClose: () => void }) {
  const [tab, setTab] = useState<"files" | "history">("files");
  const [opening, setOpening] = useState(false);

  const finish = (opened: OpenedItem | undefined, title: string) => {
    setOpening(false);
    if (!opened) return;
    onClose();
    openItem(workspaceId, opened, title);
  };

  return (
    <Modal
      open={open}
      onClose={onClose}
      title={
        <div class="segmented">
          <button class={tab === "files" ? "selected" : ""} onClick={() => setTab("files")}>
            Files
          </button>
          <button class={tab === "history" ? "selected" : ""} onClick={() => setTab("history")}>
            History
          </button>
        </div>
      }
      headerRight={opening ? <Spinner size={18} /> : null}
      class="add-tab-modal"
    >
      <div class={`add-tab-pane ${tab === "files" ? "" : "hidden"}`}>
        {open && (
          <FileTree
            workspaceId={workspaceId}
            onOpen={async (entry) => {
              haptic();
              setOpening(true);
              const opened = await call(
                "workspace.open_file",
                { workspace_id: workspaceId, worktree_id: entry.worktree_id, path: entry.path },
                { timeout: 30_000 },
              );
              finish(opened, entry.name);
            }}
          />
        )}
      </div>
      <div class={`add-tab-pane ${tab === "history" ? "" : "hidden"}`}>
        {open && tab === "history" && (
          <HistoryList
            workspaceId={workspaceId}
            onOpenUncommitted={async () => {
              haptic();
              setOpening(true);
              finish(await call("git.open_uncommitted", { workspace_id: workspaceId }, { timeout: 30_000 }), "Uncommitted Changes");
            }}
            onOpenCommit={async (commit) => {
              haptic();
              setOpening(true);
              finish(
                await call("git.open_commit", { workspace_id: workspaceId, sha: commit.sha }, { timeout: 30_000 }),
                commit.subject,
              );
            }}
          />
        )}
      </div>
    </Modal>
  );
}

function FileTree({ workspaceId, onOpen }: { workspaceId: WorkspaceId; onOpen: (entry: DirectoryEntry) => void }) {
  const [roots, setRoots] = useState<DirectoryEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    conn.request("workspace.list_directory", { workspace_id: workspaceId, path: "" }).then(
      (listing) => setRoots(listing.entries),
      (reason: unknown) => setError(reason instanceof Error ? reason.message : String(reason)),
    );
  }, [workspaceId]);
  if (error) return <EmptyState icon="alert" title="Couldn't list files">{error}</EmptyState>;
  if (!roots) {
    return (
      <div class="center-fill">
        <Spinner size={22} />
      </div>
    );
  }
  if (roots.length === 0) return <EmptyState icon="folder" title="No folders in this workspace" />;
  return (
    <div class="file-tree">
      {roots.map((entry) => (
        <TreeNode
          key={`${entry.worktree_id}:${entry.path}`}
          entry={entry}
          depth={0}
          workspaceId={workspaceId}
          onOpen={onOpen}
          initiallyOpen={roots.length === 1}
        />
      ))}
    </div>
  );
}

function TreeNode({
  entry,
  depth,
  workspaceId,
  onOpen,
  initiallyOpen = false,
}: {
  entry: DirectoryEntry;
  depth: number;
  workspaceId: WorkspaceId;
  onOpen: (entry: DirectoryEntry) => void;
  initiallyOpen?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [children, setChildren] = useState<DirectoryEntry[] | null>(null);
  const [loading, setLoading] = useState(false);

  const load = async () => {
    setLoading(true);
    try {
      const listing = await conn.request("workspace.list_directory", {
        workspace_id: workspaceId,
        worktree_id: entry.worktree_id,
        path: entry.path,
      });
      setChildren(listing.entries);
    } catch (error) {
      reportError(error);
      setOpen(false);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    if (initiallyOpen && entry.is_dir) {
      setOpen(true);
      void load();
    }
  }, []);

  const toggle = () => {
    if (!entry.is_dir) {
      onOpen(entry);
      return;
    }
    const next = !open;
    setOpen(next);
    if (next && children === null && !loading) void load();
  };

  const statusClass = entry.git_status ? `git-${entry.git_status}` : "";
  return (
    <>
      <button
        class={`tree-row pressable ${statusClass} ${entry.is_ignored ? "ignored" : ""}`}
        style={{ paddingLeft: `${12 + depth * 16}px` }}
        onClick={toggle}
      >
        {entry.is_dir ? (
          <Icon name={open ? "chevron-down" : "chevron-right"} size={14} class="tree-chevron" />
        ) : (
          <span class="tree-chevron-space" />
        )}
        <Icon name={entry.is_dir ? "folder" : "file"} size={16} class="tree-icon" />
        <span class="tree-name">{entry.name}</span>
        {entry.git_status && <span class="tree-status">{entry.git_status[0].toUpperCase()}</span>}
        {loading && <Spinner size={14} />}
      </button>
      {open &&
        children?.map((child) => (
          <TreeNode
            key={`${child.worktree_id}:${child.path}`}
            entry={child}
            depth={depth + 1}
            workspaceId={workspaceId}
            onOpen={onOpen}
          />
        ))}
      {open && children?.length === 0 && (
        <div class="tree-empty" style={{ paddingLeft: `${44 + depth * 16}px` }}>
          Empty folder
        </div>
      )}
    </>
  );
}

const PAGE_SIZE = 50;

function HistoryList({
  workspaceId,
  onOpenUncommitted,
  onOpenCommit,
}: {
  workspaceId: WorkspaceId;
  onOpenUncommitted: () => void;
  onOpenCommit: (commit: CommitSummary) => void;
}) {
  const [commits, setCommits] = useState<CommitSummary[]>([]);
  const [hasMore, setHasMore] = useState(true);
  const [loading, setLoading] = useState(false);
  const [branch, setBranch] = useState<string | null>(null);
  const [noRepository, setNoRepository] = useState(false);

  const loadMore = async () => {
    if (loading || !hasMore) return;
    setLoading(true);
    const log = await call("git.log", { workspace_id: workspaceId, offset: commits.length, limit: PAGE_SIZE });
    setLoading(false);
    if (!log) {
      setHasMore(false);
      return;
    }
    setBranch(log.branch);
    setNoRepository(log.repository_id === null);
    setCommits((previous) => [...previous, ...log.commits]);
    setHasMore(log.has_more);
  };

  const sentinelRef = useInView<HTMLDivElement>(() => void loadMore(), { enabled: hasMore && !loading });

  return (
    <div class="history-list">
      <button class="history-row uncommitted pressable" onClick={onOpenUncommitted}>
        <span class="history-icon">
          <Icon name="pencil" size={16} />
        </span>
        <span class="history-text">
          <span class="history-subject">Uncommitted changes</span>
          <span class="history-meta">{branch ? `Working copy · ${branch}` : "Working copy"}</span>
        </span>
        <Icon name="chevron-right" size={16} class="muted" />
      </button>
      {commits.map((commit) => {
        const date = parseDate(commit.timestamp);
        return (
          <button key={commit.sha} class="history-row pressable" onClick={() => onOpenCommit(commit)}>
            <span class="history-icon">
              <Icon name="commit" size={16} />
            </span>
            <span class="history-text">
              <span class="history-subject">{commit.subject}</span>
              <span class="history-meta">
                <span class="sha">{commit.sha.slice(0, 7)}</span>
                {commit.author}
                {date && ` · ${relativeTime(date)}`}
              </span>
              {commit.refs.length > 0 && (
                <span class="history-refs">
                  {commit.refs.map((ref) => (
                    <span class="badge accent" key={ref}>
                      {ref}
                    </span>
                  ))}
                </span>
              )}
            </span>
          </button>
        );
      })}
      {noRepository && commits.length === 0 && !loading && <div class="list-note">No git repository in this workspace.</div>}
      {hasMore && (
        <div class="history-sentinel" ref={sentinelRef}>
          {loading && <Spinner size={18} />}
        </div>
      )}
    </div>
  );
}
