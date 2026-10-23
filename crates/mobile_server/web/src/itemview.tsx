import type { EditorView } from "@codemirror/view";
import type { ComponentChildren } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";
import { BufferEditor, createStaticDiffView, type AccessoryKey } from "./editor";
import { handleMarkdownClick, renderMarkdown } from "./markdown";
import type { CommitFile, DiffFile, FileStatus, Hover, ItemContent, RepositoryId, WorkspaceId } from "./protocol";
import { call, closeOverlay, conn, errorMessage, openOverlay, overlay, reportError, toast, type OverlayTarget } from "./state";
import { TerminalView } from "./terminal";
import {
  CloseButton,
  EmptyState,
  haptic,
  Icon,
  keyboardOpen,
  parseDate,
  Sheet,
  Spinner,
  splitPath,
  Switch,
  useInView,
  type IconName,
} from "./ui";

// ---------------------------------------------------------------------------
// Overlay shell

export function ItemOverlay() {
  const target = overlay.value;
  const [shown, setShown] = useState<OverlayTarget | null>(target);
  const [closing, setClosing] = useState(false);

  useEffect(() => {
    if (target) {
      setShown(target);
      setClosing(false);
      return;
    }
    if (!shown) return;
    setClosing(true);
    const timer = setTimeout(() => {
      setShown(null);
      setClosing(false);
    }, 260);
    return () => clearTimeout(timer);
  }, [target]);

  useEffect(() => {
    if (!target) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !(event.target instanceof HTMLElement && event.target.closest(".cm-editor"))) closeOverlay();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [target]);

  if (!shown) return null;
  const key = `${shown.type}:${shown.workspaceId}:${shown.itemId}`;
  return (
    <div class={`item-overlay ${closing ? "closing" : ""}`}>
      {shown.type === "terminal" ? (
        <TerminalView key={key} target={shown} />
      ) : shown.kind === "file" ? (
        <FileItemView key={key} target={shown} />
      ) : shown.kind === "diff" ? (
        <DiffItemView key={key} target={shown} />
      ) : shown.kind === "commit" ? (
        <CommitItemView key={key} target={shown} />
      ) : (
        <OverlayFrame title={shown.title}>
          <EmptyState icon="layers" title="Not available on mobile">
            This tab can only be viewed in Zed on your desktop.
          </EmptyState>
        </OverlayFrame>
      )}
    </div>
  );
}

function OverlayFrame({
  title,
  subtitle,
  toolbar,
  accessory,
  children,
  dirty,
}: {
  title: ComponentChildren;
  subtitle?: ComponentChildren;
  toolbar?: ComponentChildren;
  accessory?: ComponentChildren;
  children: ComponentChildren;
  dirty?: boolean;
}) {
  return (
    <div class="overlay-frame">
      <header class="overlay-header">
        <div class="overlay-title">
          <span class="overlay-title-text">{title}</span>
          {dirty && <span class="dirty-dot" aria-label="Unsaved changes" />}
        </div>
        {subtitle && (
          <div class="overlay-subtitle">
            <bdi>{subtitle}</bdi>
          </div>
        )}
      </header>
      {toolbar && <div class="overlay-toolbar">{toolbar}</div>}
      <div class="overlay-content">
        {children}
        <CloseButton />
      </div>
      {accessory}
    </div>
  );
}

function ToolButton({
  icon,
  label,
  onClick,
  disabled,
  busy,
  active,
}: {
  icon: IconName;
  label: string;
  onClick: () => void;
  disabled?: boolean;
  busy?: boolean;
  active?: boolean;
}) {
  return (
    <button
      class={`tool-button pressable ${active ? "active" : ""}`}
      disabled={disabled || busy}
      aria-label={label}
      // Keep focus (and the keyboard) in the editor.
      onPointerDown={(event) => event.preventDefault()}
      onClick={onClick}
    >
      {busy ? <Spinner size={16} /> : <Icon name={icon} size={17} />}
      <span>{label}</span>
    </button>
  );
}

function LoadingOrError({ error, onRetry }: { error: string | null; onRetry: () => void }) {
  if (!error) {
    return (
      <div class="center-fill">
        <Spinner size={24} />
      </div>
    );
  }
  return (
    <EmptyState icon="alert" title="Couldn't open this tab">
      {error}
      <button class="button secondary pressable" onClick={onRetry}>
        Try again
      </button>
    </EmptyState>
  );
}

/** Opens the item, re-opening it after reconnects; closes it on unmount. */
function useItemContent(
  target: Extract<OverlayTarget, { type: "item" }>,
  onContent: (content: ItemContent, isReload: boolean) => void,
) {
  const [error, setError] = useState<string | null>(null);
  const onContentRef = useRef(onContent);
  onContentRef.current = onContent;
  const loaded = useRef(false);
  const loading = useRef(false);
  const again = useRef(false);
  const reloadTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const load = async () => {
    if (loading.current) {
      again.current = true;
      return;
    }
    loading.current = true;
    try {
      const content = await conn.request(
        "item.open",
        { workspace_id: target.workspaceId, item_id: target.itemId },
        { timeout: 30_000 },
      );
      setError(null);
      onContentRef.current(content, loaded.current);
      loaded.current = true;
    } catch (reason) {
      if (!loaded.current) setError(errorMessage(reason));
      else reportError(reason);
    } finally {
      loading.current = false;
      if (again.current) {
        again.current = false;
        void load();
      }
    }
  };

  const reload = () => {
    if (reloadTimer.current) clearTimeout(reloadTimer.current);
    reloadTimer.current = setTimeout(() => void load(), 120);
  };

  useEffect(() => {
    void load();
    const offHello = conn.on("hello", () => void load());
    return () => {
      offHello();
      if (reloadTimer.current) clearTimeout(reloadTimer.current);
      if (conn.isReady) {
        conn.request("item.close", { workspace_id: target.workspaceId, item_id: target.itemId }).catch(() => {});
      }
    };
  }, []);

  return { error, retry: () => void load(), reload };
}

// ---------------------------------------------------------------------------
// Keyboard accessory row for editors

const EDITOR_SYMBOLS = ["{", "}", "(", ")", "[", "]", "<", ">", ";", ":", "=", "\"", "'", "`", "/", "\\", "|", "&", "!", "?", "_", "-", "+", "*", "#", "$", "%", "@", "~", "^"];

function EditorAccessory({ editor }: { editor: BufferEditor }) {
  const key = (name: AccessoryKey) => () => editor.runKey(name);
  const keyButton = (label: ComponentChildren, onClick: () => void, aria: string) => (
    <button class="accessory-key" aria-label={aria} onPointerDown={(event) => event.preventDefault()} onClick={onClick}>
      {label}
    </button>
  );
  return (
    <div class="accessory-bar">
      {keyButton(<Icon name="indent" size={18} />, key("tab"), "Indent")}
      {keyButton(<Icon name="outdent" size={18} />, key("outdent"), "Outdent")}
      {keyButton(<Icon name="undo" size={18} />, key("undo"), "Undo")}
      {keyButton(<Icon name="redo" size={18} />, key("redo"), "Redo")}
      {keyButton(<Icon name="arrow-left" size={18} />, key("left"), "Left")}
      {keyButton(<Icon name="arrow-up" size={18} />, key("up"), "Up")}
      {keyButton(<Icon name="arrow-down" size={18} />, key("down"), "Down")}
      {keyButton(<Icon name="arrow-right" size={18} />, key("right"), "Right")}
      {EDITOR_SYMBOLS.map((symbol) => (
        <span key={symbol}>{keyButton(<span class="accessory-symbol">{symbol}</span>, () => editor.insertText(symbol), symbol)}</span>
      ))}
      {keyButton(<Icon name="keyboard" size={18} />, () => editor.view.contentDOM.blur(), "Hide keyboard")}
    </div>
  );
}

function HoverSheet({ hover, onClose }: { hover: Hover | null; onClose: () => void }) {
  const [last, setLast] = useState<Hover | null>(hover);
  useEffect(() => {
    if (hover) setLast(hover);
  }, [hover]);
  return (
    <Sheet open={!!hover} onClose={onClose} title="Hover">
      <div
        class="markdown hover-content"
        onClick={handleMarkdownClick}
        dangerouslySetInnerHTML={{ __html: last ? renderMarkdown(last.markdown) : "" }}
      />
    </Sheet>
  );
}

// ---------------------------------------------------------------------------
// File

function FileItemView({ target }: { target: Extract<OverlayTarget, { type: "item" }> }) {
  const hostRef = useRef<HTMLDivElement>(null);
  const [editor, setEditor] = useState<BufferEditor | null>(null);
  const editorRef = useRef<BufferEditor | null>(null);
  const [title, setTitle] = useState(target.title);
  const [path, setPath] = useState<string | null>(null);
  const [dirty, setDirty] = useState(false);
  const [focused, setFocused] = useState(false);
  const [wrap, setWrap] = useState(false);
  const [hover, setHover] = useState<Hover | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const { error, retry, reload } = useItemContent(target, (content, isReload) => {
    if (content.kind !== "file") {
      toast("This tab changed type; reopen it.", "error");
      return;
    }
    setTitle(content.title);
    setPath(content.buffer.path);
    setDirty(content.buffer.is_dirty);
    if (editorRef.current) {
      editorRef.current.resync(content.buffer);
      return;
    }
    if (!hostRef.current) return;
    const created = new BufferEditor({
      parent: hostRef.current,
      buffer: content.buffer,
      workspaceId: target.workspaceId,
      onDirtyChange: setDirty,
      onFocusChange: setFocused,
      onNeedResync: () => reload(),
    });
    editorRef.current = created;
    setEditor(created);
    if (!isReload && target.offset !== null) created.revealOffset(target.offset);
  });

  useEffect(() => () => editorRef.current?.destroy(), []);

  // Keep the cursor above the keyboard when it opens.
  const keyboard = keyboardOpen.value;
  useEffect(() => {
    if (keyboard && editorRef.current) {
      const timer = setTimeout(() => editorRef.current?.ensureCursorVisible(), 60);
      return () => clearTimeout(timer);
    }
  }, [keyboard]);

  const run = async (name: string, action: () => Promise<void>) => {
    haptic();
    setBusy(name);
    try {
      await action();
    } catch (reason) {
      reportError(reason);
    } finally {
      setBusy(null);
    }
  };

  const showHover = () =>
    run("hover", async () => {
      const result = await editorRef.current?.hover();
      if (result) setHover(result);
      else toast("Nothing to show here");
    });

  const goToDefinition = () =>
    run("definition", async () => {
      const result = await editorRef.current?.definition();
      if (!result) {
        toast("No definition found");
        return;
      }
      if (result.item_id === target.itemId) {
        if (result.offset !== null) editorRef.current?.revealOffset(result.offset);
        return;
      }
      openOverlay({ ...target, itemId: result.item_id, kind: result.kind, offset: result.offset, title: "" });
    });

  const readOnly = editor?.readOnly ?? false;
  const lsp = editor?.hasLsp ?? false;
  const directory = path ? splitPath(path).dir : null;

  return (
    <OverlayFrame
      title={title || "Untitled"}
      subtitle={directory || (readOnly ? "Read-only" : null)}
      dirty={dirty}
      toolbar={
        editor && (
          <>
            {!readOnly && (
              <ToolButton icon="save" label="Save" busy={busy === "save"} active={dirty} onClick={() => run("save", () => editor.save())} />
            )}
            {!readOnly && <ToolButton icon="undo" label="Undo" onClick={() => editor.runKey("undo")} />}
            {!readOnly && <ToolButton icon="redo" label="Redo" onClick={() => editor.runKey("redo")} />}
            <ToolButton icon="search" label="Find" onClick={() => editor.runKey("search")} />
            {lsp && <ToolButton icon="info" label="Hover" busy={busy === "hover"} onClick={() => void showHover()} />}
            {lsp && (
              <ToolButton icon="definition" label="Definition" busy={busy === "definition"} onClick={() => void goToDefinition()} />
            )}
            {!readOnly && (
              <ToolButton icon="wand" label="Format" busy={busy === "format"} onClick={() => run("format", () => editor.format())} />
            )}
            <ToolButton icon="alert" label="Problems" onClick={() => editor.runKey("problems")} />
            <ToolButton icon="list" label="Wrap" active={wrap} onClick={() => setWrap(editor.toggleWrap())} />
          </>
        )
      }
      accessory={editor && focused && !readOnly ? <EditorAccessory editor={editor} /> : null}
    >
      <div class="file-editor-host" ref={hostRef} />
      {!editor && (
        <div class="overlay-placeholder">
          <LoadingOrError error={error} onRetry={retry} />
        </div>
      )}
      <HoverSheet hover={hover} onClose={() => setHover(null)} />
    </OverlayFrame>
  );
}

// ---------------------------------------------------------------------------
// Uncommitted changes

const STATUS_LETTER: Record<FileStatus, string> = {
  added: "A",
  modified: "M",
  deleted: "D",
  renamed: "R",
  untracked: "U",
  conflict: "!",
};

function StatusBadge({ status }: { status: FileStatus }) {
  return <span class={`file-status git-${status}`}>{STATUS_LETTER[status]}</span>;
}

function FileSectionHeader({ path, status, children }: { path: string; status: FileStatus; children?: ComponentChildren }) {
  const { dir, name } = splitPath(path);
  return (
    <div class="file-section-header">
      <StatusBadge status={status} />
      <span class="file-section-path">
        <span class="file-section-name">{name}</span>
        {dir && (
          <span class="file-section-dir">
            <bdi>{dir}</bdi>
          </span>
        )}
      </span>
      {children}
    </div>
  );
}

function DiffItemView({ target }: { target: Extract<OverlayTarget, { type: "item" }> }) {
  const [content, setContent] = useState<Extract<ItemContent, { kind: "diff" }> | null>(null);
  const [loadId, setLoadId] = useState(0);
  const [focusedEditor, setFocusedEditor] = useState<BufferEditor | null>(null);

  const { error, retry, reload } = useItemContent(target, (next) => {
    if (next.kind !== "diff") return;
    setContent(next);
    setLoadId((id) => id + 1);
  });

  useEffect(
    () =>
      conn.on("diff.files_changed", (event) => {
        if (event.workspace_id === target.workspaceId && event.item_id === target.itemId) reload();
      }),
    [],
  );

  const keyboard = keyboardOpen.value;
  useEffect(() => {
    if (keyboard && focusedEditor) {
      const timer = setTimeout(() => focusedEditor.ensureCursorVisible(), 60);
      return () => clearTimeout(timer);
    }
  }, [keyboard, focusedEditor]);

  const files = content?.files ?? [];
  return (
    <OverlayFrame
      title={content?.title || target.title || "Uncommitted Changes"}
      subtitle={content ? `${files.length} file${files.length === 1 ? "" : "s"} changed` : null}
      accessory={focusedEditor && !focusedEditor.readOnly ? <EditorAccessory editor={focusedEditor} /> : null}
    >
      <div class="multibuffer-scroll">
        {!content ? (
          <LoadingOrError error={error} onRetry={retry} />
        ) : files.length === 0 ? (
          <EmptyState icon="check-circle" title="No uncommitted changes">
            Your working copy is clean.
          </EmptyState>
        ) : (
          files.map((file) => (
            <DiffSection
              key={`${file.path}:${file.buffer ? "buffer" : "deleted"}`}
              file={file}
              loadId={loadId}
              workspaceId={target.workspaceId}
              repositoryId={content.repository_id}
              onNeedResync={reload}
              onFocusChange={(editor, focused) =>
                setFocusedEditor((current) => (focused ? editor : current === editor ? null : current))
              }
            />
          ))
        )}
        <div class="multibuffer-end" />
      </div>
    </OverlayFrame>
  );
}

function estimateHeight(text: string | null | undefined): number {
  if (!text) return 120;
  const lines = Math.min(40, text.split("\n").length);
  return Math.max(80, lines * 22);
}

function DiffSection({
  file,
  loadId,
  workspaceId,
  repositoryId,
  onNeedResync,
  onFocusChange,
}: {
  file: DiffFile;
  loadId: number;
  workspaceId: WorkspaceId;
  repositoryId: RepositoryId;
  onNeedResync: () => void;
  onFocusChange: (editor: BufferEditor, focused: boolean) => void;
}) {
  const [visible, setVisible] = useState(false);
  const hostRef = useRef<HTMLDivElement>(null);
  const editorRef = useRef<BufferEditor | null>(null);
  const staticRef = useRef<EditorView | null>(null);
  const lostSync = useRef(false);
  const [staged, setStaged] = useState({ is_staged: file.is_staged, partial: file.is_partially_staged });
  const placeholderRef = useInView<HTMLDivElement>(() => setVisible(true), { rootMargin: "800px", enabled: !visible });

  useEffect(() => setStaged({ is_staged: file.is_staged, partial: file.is_partially_staged }), [file.is_staged, file.is_partially_staged]);

  useEffect(() => {
    if (!visible || !hostRef.current) return;
    if (!file.buffer) {
      staticRef.current?.destroy();
      staticRef.current = createStaticDiffView(hostRef.current, file.base_text ?? "", "");
      return;
    }
    const editor = editorRef.current;
    if (!editor) {
      const created = new BufferEditor({
        parent: hostRef.current,
        buffer: file.buffer,
        workspaceId,
        baseText: file.base_text,
        onNeedResync: () => {
          lostSync.current = true;
          onNeedResync();
        },
        onFocusChange: (focused) => onFocusChange(created, focused),
      });
      editorRef.current = created;
    } else if (!lostSync.current && editor.handle === file.buffer.buffer && editor.syncGeneration === conn.generation) {
      // Same subscription: the text is already in sync; only the base may differ.
      editor.setBaseText(file.base_text);
    } else {
      lostSync.current = false;
      editor.resync(file.buffer, file.base_text);
    }
  }, [visible, loadId]);

  useEffect(
    () => () => {
      editorRef.current?.destroy();
      staticRef.current?.destroy();
    },
    [],
  );

  const toggleStage = async (value: boolean) => {
    const previous = staged;
    setStaged({ is_staged: value, partial: false });
    const result = await call(value ? "git.stage" : "git.unstage", {
      workspace_id: workspaceId,
      repository_id: repositoryId,
      paths: [file.path],
    });
    if (result === undefined) setStaged(previous);
  };

  return (
    <section class="file-section diff-section">
      <FileSectionHeader path={file.path} status={file.status}>
        <span class={`stage-toggle ${staged.partial ? "partial" : ""}`}>
          <span class="stage-label">{staged.partial ? "Partial" : staged.is_staged ? "Staged" : "Stage"}</span>
          <Switch
            checked={staged.is_staged && !staged.partial}
            label={`Stage ${file.path}`}
            onChange={(value) => void toggleStage(value)}
          />
        </span>
      </FileSectionHeader>
      {visible ? (
        <div class={`file-section-body ${file.buffer ? "" : "deleted"}`} ref={hostRef} />
      ) : (
        <div
          class="file-section-placeholder"
          ref={placeholderRef}
          style={{ height: `${estimateHeight(file.buffer?.text ?? file.base_text)}px` }}
        />
      )}
    </section>
  );
}

// ---------------------------------------------------------------------------
// Commit

function CommitItemView({ target }: { target: Extract<OverlayTarget, { type: "item" }> }) {
  const [content, setContent] = useState<Extract<ItemContent, { kind: "commit" }> | null>(null);
  const { error, retry } = useItemContent(target, (next) => {
    if (next.kind === "commit") setContent(next);
  });
  const date = content ? parseDate(content.timestamp) : null;
  const [subject, ...bodyLines] = (content?.message ?? "").split("\n");
  const body = bodyLines.join("\n").trim();
  return (
    <OverlayFrame title={content?.title || target.title || "Commit"} subtitle={content ? content.sha.slice(0, 10) : null}>
      <div class="multibuffer-scroll">
        {!content ? (
          <LoadingOrError error={error} onRetry={retry} />
        ) : (
          <>
            <div class="commit-header">
              <div class="commit-subject">{subject}</div>
              {body && <div class="commit-body">{body}</div>}
              <div class="commit-meta">
                <span class="commit-author">{content.author}</span>
                {date && <span>{date.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" })}</span>}
                <span class="sha">{content.sha.slice(0, 10)}</span>
              </div>
              <div class="commit-stats">
                {content.files.length} file{content.files.length === 1 ? "" : "s"} changed
              </div>
            </div>
            {content.files.map((file) => (
              <CommitSection key={file.path} file={file} />
            ))}
          </>
        )}
        <div class="multibuffer-end" />
      </div>
    </OverlayFrame>
  );
}

function CommitSection({ file }: { file: CommitFile }) {
  const [visible, setVisible] = useState(false);
  const hostRef = useRef<HTMLDivElement>(null);
  const placeholderRef = useInView<HTMLDivElement>(() => setVisible(true), { rootMargin: "800px", enabled: !visible && !file.is_binary });

  useEffect(() => {
    if (!visible || !hostRef.current || file.is_binary) return;
    const view = createStaticDiffView(hostRef.current, file.old_text ?? "", file.new_text ?? "");
    return () => view.destroy();
  }, [visible]);

  return (
    <section class="file-section">
      <FileSectionHeader path={file.path} status={file.status} />
      {file.is_binary ? (
        <div class="binary-placeholder">
          <Icon name="file" size={18} />
          Binary file not shown
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
