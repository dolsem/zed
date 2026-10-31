import { batch, computed, effect, signal } from "@preact/signals";
import { Connection, RequestError, type RequestOptions } from "./connection";
import type {
  EditorTabKind,
  ItemId,
  MethodName,
  Params,
  Result,
  Theme,
  ThreadEntry,
  ThreadState,
  ThreadsList,
  WorkspaceId,
  WorkspaceItems,
} from "./protocol";
import { applyTheme, DEFAULT_THEME, loadCachedTheme } from "./theme";

export const conn = new Connection();

// ---------------------------------------------------------------------------
// Toasts

export interface Toast {
  id: number;
  level: "info" | "error";
  message: string;
}

export const toasts = signal<Toast[]>([]);
let nextToastId = 1;

export function toast(message: string, level: Toast["level"] = "info", duration = level === "error" ? 5000 : 3000) {
  const id = nextToastId++;
  // Collapse repeats of the same message (e.g. several failing requests).
  const existing = toasts.peek().filter((item) => item.message !== message);
  toasts.value = [...existing.slice(-3), { id, level, message }];
  setTimeout(() => dismissToast(id), duration);
}

export function dismissToast(id: number) {
  toasts.value = toasts.peek().filter((item) => item.id !== id);
}

export function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  return String(error);
}

/** Sends a request and shows a toast if it fails, resolving to `undefined`. */
export async function call<M extends MethodName>(
  method: M,
  params: Params<M>,
  options?: RequestOptions,
): Promise<Result<M> | undefined> {
  try {
    return await conn.request(method, params, options);
  } catch (error) {
    reportError(error);
    return undefined;
  }
}

export function reportError(error: unknown) {
  if (error instanceof RequestError && error.kind === "connection" && !conn.isReady) {
    // The reconnecting banner already says so.
    return;
  }
  toast(errorMessage(error), "error");
}

conn.on("toast", (event) => toast(event.message, event.level));

// ---------------------------------------------------------------------------
// Theme

export const theme = signal<Theme>(loadCachedTheme() ?? DEFAULT_THEME);
effect(() => applyTheme(theme.value));
conn.on("hello", (hello) => (theme.value = hello.theme));
conn.on("theme", (value) => (theme.value = value));

// ---------------------------------------------------------------------------
// Navigation

export const PANEL_THREADS = 0;
export const PANEL_THREAD = 1;
export const PANEL_TABS = 2;

/** The panel currently snapped into view. */
export const panel = signal(PANEL_THREADS);

type PanelNavigator = (index: number, smooth?: boolean) => void;
let panelNavigator: PanelNavigator = (index) => (panel.value = index);

export function setPanelNavigator(navigator: PanelNavigator) {
  panelNavigator = navigator;
}

export function goToPanel(index: number, smooth = true) {
  panelNavigator(index, smooth);
}

// ---------------------------------------------------------------------------
// Threads list

export const threadsList = signal<ThreadsList | null>(null);

export async function refreshThreads() {
  const list = await call("threads.list", {});
  if (list) threadsList.value = list;
}

conn.on("threads.changed", (list) => (threadsList.value = list));

// ---------------------------------------------------------------------------
// The open thread

export const currentThreadId = signal<string | null>(null);
export const threadEntries = signal<ThreadEntry[]>([]);
export const threadState = signal<ThreadState | null>(null);
export const threadWorkspaceId = signal<WorkspaceId | null>(null);
export const threadLoading = signal(false);

let threadLoadToken = 0;

export function openThread(threadId: string, { navigate = true } = {}) {
  const previous = currentThreadId.peek();
  if (previous !== threadId) {
    if (previous && conn.isReady) {
      conn.request("thread.close", { thread_id: previous }).catch(() => {});
    }
    batch(() => {
      currentThreadId.value = threadId;
      threadEntries.value = [];
      threadState.value = null;
    });
    void loadThread(threadId);
  }
  if (navigate) goToPanel(PANEL_THREAD);
}

async function loadThread(threadId: string) {
  const loadToken = ++threadLoadToken;
  threadLoading.value = true;
  try {
    const snapshot = await conn.request("thread.open", { thread_id: threadId }, { timeout: 30_000 });
    if (loadToken !== threadLoadToken || currentThreadId.peek() !== threadId) return;
    batch(() => {
      threadEntries.value = snapshot.entries;
      threadState.value = snapshot.state;
      threadWorkspaceId.value = snapshot.workspace_id;
    });
  } catch (error) {
    if (loadToken === threadLoadToken) reportError(error);
  } finally {
    if (loadToken === threadLoadToken) threadLoading.value = false;
  }
}

conn.on("thread.entry", (event) => {
  if (event.thread_id !== currentThreadId.peek()) return;
  const entries = threadEntries.peek().slice();
  if (event.index < entries.length) entries[event.index] = event.entry;
  else {
    entries.length = event.index;
    entries[event.index] = event.entry;
  }
  threadEntries.value = entries;
});

conn.on("thread.entries_removed", (event) => {
  if (event.thread_id !== currentThreadId.peek()) return;
  const entries = threadEntries.peek().slice();
  entries.splice(event.start, Math.max(0, event.end - event.start));
  threadEntries.value = entries;
});

conn.on("thread.state", (state) => {
  if (state.thread_id !== currentThreadId.peek()) return;
  threadState.value = state;
});

/** Replaces the open thread's state locally until Zed confirms it. */
export function patchThreadState(patch: (state: ThreadState) => ThreadState) {
  const state = threadState.peek();
  if (state) threadState.value = patch(state);
}

// Per-thread composer drafts survive switching threads and relaunching.
const DRAFTS_KEY = "zed-mobile.drafts";
function loadDrafts(): Record<string, string> {
  try {
    return JSON.parse(localStorage.getItem(DRAFTS_KEY) ?? "{}") as Record<string, string>;
  } catch {
    return {};
  }
}
const drafts = loadDrafts();
let draftTimer: ReturnType<typeof setTimeout> | null = null;

export function getDraft(threadId: string): string {
  return drafts[threadId] ?? "";
}

export function setDraft(threadId: string, text: string) {
  if (text) drafts[threadId] = text;
  else delete drafts[threadId];
  if (draftTimer) clearTimeout(draftTimer);
  draftTimer = setTimeout(() => {
    try {
      localStorage.setItem(DRAFTS_KEY, JSON.stringify(drafts));
    } catch {
      // Ignore storage failures; drafts are a convenience.
    }
  }, 400);
}

// ---------------------------------------------------------------------------
// Workspace items (tabs screen)

export const workspaceItems = signal<WorkspaceItems | null>(null);
const fallbackWorkspaceId = signal<WorkspaceId | null>(null);
export const activeWorkspaceId = computed(() => threadWorkspaceId.value ?? fallbackWorkspaceId.value);

let workspaceRefreshToken = 0;

export async function refreshWorkspace() {
  const refreshToken = ++workspaceRefreshToken;
  const requested = threadWorkspaceId.peek();
  const items = await call("workspace.items", requested === null ? {} : { workspace_id: requested });
  if (!items || refreshToken !== workspaceRefreshToken) return;
  batch(() => {
    if (requested === null) fallbackWorkspaceId.value = items.workspace_id;
    workspaceItems.value = items;
  });
}

let itemsChangedTimer: ReturnType<typeof setTimeout> | null = null;
conn.on("workspace.items_changed", (event) => {
  if (event.workspace_id !== activeWorkspaceId.peek()) return;
  if (itemsChangedTimer) clearTimeout(itemsChangedTimer);
  itemsChangedTimer = setTimeout(() => void refreshWorkspace(), 80);
});

// Reload the tabs whenever the open thread's workspace changes.
let lastWorkspaceKey: string | null = null;
effect(() => {
  const workspaceId = threadWorkspaceId.value;
  const generation = conn.generationSignal.value;
  if (conn.status.value !== "ready") return;
  const key = `${generation}:${workspaceId}`;
  if (key === lastWorkspaceKey) return;
  lastWorkspaceKey = key;
  void refreshWorkspace();
});

// ---------------------------------------------------------------------------
// Full-screen overlay

export type OverlayTarget =
  | {
      type: "item";
      workspaceId: WorkspaceId;
      itemId: ItemId;
      kind: EditorTabKind;
      /** Cursor offset to reveal, e.g. after go-to-definition. */
      offset: number | null;
      title: string;
    }
  | { type: "terminal"; workspaceId: WorkspaceId; itemId: ItemId; title: string };

export const overlay = signal<OverlayTarget | null>(null);

export function openOverlay(target: OverlayTarget) {
  overlay.value = target;
}

export function closeOverlay() {
  overlay.value = null;
}

// ---------------------------------------------------------------------------
// Reconnection: every `hello` (first or after a reconnect) re-syncs the lists
// and the open thread. Item and terminal views re-sync themselves.

conn.on("hello", () => {
  void refreshThreads();
  const threadId = currentThreadId.peek();
  if (threadId) void loadThread(threadId);
});

// ---------------------------------------------------------------------------
// Deep links (`#/thread/<id>`, `#pair=CODE`)

export const pendingPairCode = signal<string | null>(null);

export function handleLocation(url: string = location.hash) {
  const hashIndex = url.indexOf("#");
  const hash = hashIndex >= 0 ? url.slice(hashIndex + 1) : url;
  if (!hash) return;
  const threadMatch = /^\/?thread\/(.+)$/.exec(hash);
  const pairMatch = /^pair=([\w-]+)/i.exec(hash);
  if (threadMatch) {
    closeOverlay();
    openThread(decodeURIComponent(threadMatch[1]));
  } else if (pairMatch) {
    pendingPairCode.value = pairMatch[1];
  } else {
    return;
  }
  // Clear the hash so tapping the same notification again still navigates.
  history.replaceState(null, "", location.pathname + location.search);
}
