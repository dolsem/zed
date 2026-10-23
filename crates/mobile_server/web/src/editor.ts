import {
  autocompletion,
  closeBrackets,
  closeBracketsKeymap,
  completionKeymap,
  type Completion as CmCompletion,
  type CompletionContext,
  type CompletionResult,
} from "@codemirror/autocomplete";
import { collab, getSyncedVersion, receiveUpdates, sendableUpdates, type Update } from "@codemirror/collab";
import {
  cursorCharLeft,
  cursorCharRight,
  cursorLineDown,
  cursorLineUp,
  defaultKeymap,
  history,
  historyKeymap,
  indentLess,
  indentMore,
  indentWithTab,
  redo,
  undo,
} from "@codemirror/commands";
import { bracketMatching, indentUnit } from "@codemirror/language";
import { lintGutter, lintKeymap, openLintPanel, setDiagnostics, type Diagnostic as CmDiagnostic } from "@codemirror/lint";
import { getOriginalDoc, unifiedMergeView, updateOriginalDoc } from "@codemirror/merge";
import { highlightSelectionMatches, openSearchPanel, search, searchKeymap } from "@codemirror/search";
import {
  ChangeSet,
  Compartment,
  EditorState,
  StateEffect,
  StateField,
  Text,
  Transaction,
  type Extension,
  type Range,
} from "@codemirror/state";
import {
  Decoration,
  drawSelection,
  EditorView,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  keymap,
  lineNumbers,
  type DecorationSet,
  type ViewUpdate,
} from "@codemirror/view";
import { RequestError } from "./connection";
import type {
  BufferHandle,
  BufferState,
  BufferUpdatesEvent,
  ChangeSetJson,
  Diagnostic,
  Highlights,
  Hover,
  OpenedItem,
  WorkspaceId,
} from "./protocol";
import { conn, reportError, theme, toast } from "./state";

// ---------------------------------------------------------------------------
// Theme (driven by CSS variables so Zed theme changes apply live)

const editorTheme = EditorView.theme({
  "&": {
    color: "var(--c-editor_foreground)",
    backgroundColor: "var(--c-editor_background)",
    fontSize: "var(--editor-font-size, 14px)",
  },
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": {
    fontFamily: "var(--font-mono)",
    lineHeight: "1.6",
    overscrollBehavior: "contain",
  },
  ".cm-content": {
    caretColor: "var(--c-editor_cursor)",
    padding: "6px 0 140px",
  },
  ".cm-line": { padding: "0 12px 0 6px" },
  ".cm-cursor, .cm-dropCursor": { borderLeftColor: "var(--c-editor_cursor)", borderLeftWidth: "2px" },
  "&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground, .cm-selectionBackground, .cm-content ::selection":
    { backgroundColor: "var(--c-editor_selection) !important" },
  ".cm-activeLine": { backgroundColor: "var(--c-editor_active_line)" },
  ".cm-gutters": {
    backgroundColor: "var(--c-editor_background)",
    color: "var(--c-editor_line_number)",
    border: "none",
    fontFamily: "var(--font-mono)",
  },
  ".cm-lineNumbers .cm-gutterElement": { padding: "0 4px 0 10px", minWidth: "28px" },
  ".cm-activeLineGutter": { backgroundColor: "var(--c-editor_active_line)", color: "var(--c-editor_foreground)" },
  ".cm-panels": {
    backgroundColor: "var(--c-elevated_surface)",
    color: "var(--c-text)",
    fontFamily: "var(--font-ui)",
  },
  ".cm-panels.cm-panels-top": { borderBottom: "1px solid var(--c-border)" },
  ".cm-panels.cm-panels-bottom": { borderTop: "1px solid var(--c-border)" },
  ".cm-search": { display: "flex", flexWrap: "wrap", gap: "6px", alignItems: "center", padding: "8px" },
  ".cm-search label": { fontSize: "13px", display: "flex", alignItems: "center", gap: "4px" },
  ".cm-textfield": {
    fontSize: "16px",
    background: "var(--c-surface)",
    color: "var(--c-text)",
    border: "1px solid var(--c-border)",
    borderRadius: "8px",
    padding: "6px 8px",
  },
  ".cm-button": {
    fontSize: "13px",
    backgroundImage: "none",
    background: "var(--c-surface)",
    color: "var(--c-text)",
    border: "1px solid var(--c-border)",
    borderRadius: "8px",
    padding: "6px 10px",
  },
  ".cm-searchMatch": { backgroundColor: "color-mix(in srgb, var(--c-warning) 30%, transparent)" },
  ".cm-searchMatch-selected": { backgroundColor: "color-mix(in srgb, var(--c-accent) 45%, transparent)" },
  ".cm-selectionMatch": { backgroundColor: "color-mix(in srgb, var(--c-accent) 18%, transparent)" },
  ".cm-matchingBracket, &.cm-focused .cm-matchingBracket": {
    backgroundColor: "color-mix(in srgb, var(--c-accent) 25%, transparent)",
    outline: "none",
  },
  ".cm-tooltip": {
    backgroundColor: "var(--c-elevated_surface)",
    color: "var(--c-text)",
    border: "1px solid var(--c-border)",
    borderRadius: "10px",
    boxShadow: "0 10px 30px rgba(0,0,0,.35)",
    overflow: "hidden",
  },
  ".cm-tooltip.cm-tooltip-autocomplete > ul": {
    fontFamily: "var(--font-mono)",
    maxHeight: "min(40vh, 280px)",
  },
  ".cm-tooltip.cm-tooltip-autocomplete > ul > li": { padding: "6px 10px", lineHeight: "1.4" },
  ".cm-tooltip-autocomplete ul li[aria-selected]": {
    backgroundColor: "var(--c-accent)",
    color: "var(--on-accent)",
  },
  ".cm-completionDetail": { opacity: "0.7", fontStyle: "normal", marginLeft: "10px" },
  ".cm-diagnostic": { padding: "6px 10px", fontFamily: "var(--font-ui)", fontSize: "13px" },
  ".cm-panel.cm-panel-lint ul": { maxHeight: "30vh" },
  ".cm-lintRange-error": { backgroundImage: "none", textDecoration: "underline wavy var(--c-error)", textUnderlineOffset: "3px" },
  ".cm-lintRange-warning": { backgroundImage: "none", textDecoration: "underline wavy var(--c-warning)", textUnderlineOffset: "3px" },
  ".cm-lintRange-info, .cm-lintRange-hint": { backgroundImage: "none", textDecoration: "underline dotted var(--c-info)", textUnderlineOffset: "3px" },
  ".cm-changedLine, .cm-insertedLine": { backgroundColor: "var(--c-diff_added_background) !important" },
  ".cm-deletedChunk": { backgroundColor: "var(--c-diff_deleted_background) !important" },
  ".cm-changedText, .cm-insertedText": {
    background: "color-mix(in srgb, var(--c-diff_added) 30%, transparent) !important",
  },
  ".cm-deletedText, .cm-deletedChunk del": {
    background: "color-mix(in srgb, var(--c-diff_deleted) 30%, transparent) !important",
    textDecoration: "none",
  },
  ".cm-changeGutter": { width: "3px", paddingLeft: "0" },
  ".cm-changedLineGutter, .cm-insertedLineGutter": { background: "var(--c-diff_added)" },
  ".cm-deletedLineGutter": { background: "var(--c-diff_deleted)" },
  ".cm-collapsedLines": {
    background: "var(--c-surface)",
    color: "var(--c-text_muted)",
    fontFamily: "var(--font-ui)",
    fontSize: "12px",
    padding: "6px 12px",
  },
  ".cm-chunkButtons button": {
    fontFamily: "var(--font-ui)",
    fontSize: "12px",
    padding: "4px 10px",
    margin: "2px 4px",
    borderRadius: "6px",
    border: "1px solid var(--c-border)",
    background: "var(--c-elevated_surface)",
    color: "var(--c-text)",
  },
  ".cm-chunkButtons button[name=accept]": { color: "var(--c-success)" },
  ".cm-chunkButtons button[name=reject]": { color: "var(--c-error)" },
});

const touchFriendly = EditorView.contentAttributes.of({
  autocapitalize: "off",
  autocorrect: "off",
  spellcheck: "false",
  autocomplete: "off",
});

function darkFlag(): Extension {
  return EditorView.darkTheme.of(theme.peek().appearance === "dark");
}

// ---------------------------------------------------------------------------
// Highlights from Zed

const setHighlights = StateEffect.define<{ from: number; to: number; ranges: Range<Decoration>[] }>();

const highlightField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(decorations, transaction) {
    let next = decorations.map(transaction.changes);
    for (const effect of transaction.effects) {
      if (!effect.is(setHighlights)) continue;
      next = next.update({
        filterFrom: effect.value.from,
        filterTo: effect.value.to,
        filter: (from, to) => to <= effect.value.from || from >= effect.value.to,
        add: effect.value.ranges,
        sort: true,
      });
    }
    return next;
  },
  provide: (field) => EditorView.decorations.from(field),
});

const markCache = new Map<number, Decoration>();
function syntaxMark(index: number): Decoration {
  let mark = markCache.get(index);
  if (!mark) {
    mark = Decoration.mark({ class: `zs-${index}` });
    markCache.set(index, mark);
  }
  return mark;
}

const COMPLETION_TYPES: Record<string, string> = {
  function: "function",
  method: "method",
  constructor: "class",
  field: "property",
  property: "property",
  variable: "variable",
  class: "class",
  struct: "class",
  interface: "interface",
  module: "namespace",
  enum: "enum",
  enum_member: "enum",
  enummember: "enum",
  keyword: "keyword",
  snippet: "text",
  constant: "constant",
  type_parameter: "type",
  typeparameter: "type",
  text: "text",
};

// ---------------------------------------------------------------------------
// Event routing: one listener per event type, dispatching by buffer handle.

const editors = new Map<BufferHandle, BufferEditor>();

conn.on("buffer.updates", (event) => editors.get(event.buffer)?.receive(event));
conn.on("buffer.metadata", (event) => editors.get(event.buffer)?.options.onDirtyChange?.(event.is_dirty));
conn.on("buffer.diagnostics", (event) => editors.get(event.buffer)?.applyDiagnostics(event.version, event.diagnostics));
conn.on("buffer.reparsed", (event) => editors.get(event.buffer)?.scheduleHighlights(60));
conn.on("diff.base_changed", (event) => editors.get(event.buffer)?.setBaseText(event.base_text));

type Mapping = { kind: "ok"; map: (position: number, assoc?: number) => number } | { kind: "future" } | { kind: "stale" };

export interface BufferEditorOptions {
  parent: HTMLElement;
  buffer: BufferState;
  workspaceId: WorkspaceId;
  /** Shows the buffer as a unified diff against this base (diff items). */
  baseText?: string | null;
  onDirtyChange?: (dirty: boolean) => void;
  onFocusChange?: (focused: boolean) => void;
  /** The editor lost sync and needs a fresh `item.open`. */
  onNeedResync: () => void;
}

const HISTORY_LIMIT = 500;

/**
 * A CodeMirror editor kept in sync with a Zed buffer through
 * `@codemirror/collab`, with Zed's highlights, diagnostics and completions.
 */
export class BufferEditor {
  readonly view: EditorView;
  handle: BufferHandle;
  private buffer: BufferState;
  private generation: number;
  private epoch = 0;
  private syncedDoc: Text;
  /** Confirmed changes by the version they apply to. */
  private changeHistory = new Map<number, ChangeSet>();
  private pushing = false;
  private waitingFor: { target: number; timer: ReturnType<typeof setTimeout> } | null = null;
  private rejections = 0;
  private settleWaiters = new Set<() => void>();
  private highlightTimer: ReturnType<typeof setTimeout> | null = null;
  private highlightSequence = 0;
  private pendingHighlights: { highlights: Highlights; from: number; to: number } | null = null;
  private pendingDiagnostics: { version: number; diagnostics: Diagnostic[] } | null = null;
  private baseText: string | null;
  private readonly hasMerge: boolean;
  private readonly wrapCompartment = new Compartment();
  private wrapping = false;
  private disposed = false;

  constructor(readonly options: BufferEditorOptions) {
    this.buffer = options.buffer;
    this.handle = options.buffer.buffer;
    this.generation = conn.generation;
    this.syncedDoc = Text.of(options.buffer.text.split("\n"));
    this.baseText = options.baseText ?? null;
    this.hasMerge = options.baseText !== undefined;
    this.view = new EditorView({
      state: this.createState(options.buffer),
      parent: options.parent,
      dispatchTransactions: (transactions, view) => {
        view.update(transactions);
        this.afterTransactions(transactions);
      },
    });
    editors.set(this.handle, this);
    this.applyDiagnostics(options.buffer.version, options.buffer.diagnostics);
    this.scheduleHighlights(0);
  }

  get readOnly(): boolean {
    return this.buffer.read_only;
  }

  /** The connection generation the editor is synced on. */
  get syncGeneration(): number {
    return this.generation;
  }

  get hasLsp(): boolean {
    return this.buffer.has_lsp;
  }

  private createState(buffer: BufferState, doc?: Text): EditorState {
    const readOnly = buffer.read_only;
    const extensions: Extension[] = [
      lineNumbers(),
      highlightActiveLineGutter(),
      highlightActiveLine(),
      highlightSpecialChars(),
      drawSelection({ cursorBlinkRate: 1000 }),
      history(),
      bracketMatching(),
      closeBrackets(),
      highlightSelectionMatches(),
      search({ top: true }),
      lintGutter(),
      keymap.of([
        ...closeBracketsKeymap,
        ...defaultKeymap,
        ...searchKeymap,
        ...historyKeymap,
        ...completionKeymap,
        ...lintKeymap,
        indentWithTab,
      ]),
      EditorState.tabSize.of(buffer.tab_size || 4),
      indentUnit.of(buffer.hard_tabs ? "\t" : " ".repeat(buffer.tab_size || 4)),
      EditorState.readOnly.of(readOnly),
      EditorView.editable.of(!readOnly),
      collab({ startVersion: buffer.version, clientID: conn.hello.peek()?.client_id ?? "mobile" }),
      highlightField,
      this.wrapCompartment.of(this.wrapping ? EditorView.lineWrapping : []),
      editorTheme,
      darkFlag(),
      touchFriendly,
      EditorView.updateListener.of((update) => this.onViewUpdate(update)),
    ];
    if (buffer.has_lsp && !readOnly) {
      extensions.push(
        autocompletion({
          override: [(context) => this.completionSource(context)],
          activateOnTyping: true,
          icons: true,
          maxRenderedOptions: 80,
        }),
      );
    }
    if (this.hasMerge) {
      extensions.push(
        unifiedMergeView({
          original: this.baseText ?? "",
          mergeControls: !readOnly,
          highlightChanges: true,
          gutter: true,
          syntaxHighlightDeletions: false,
          collapseUnchanged: { margin: 3, minSize: 4 },
        }),
      );
    }
    return EditorState.create({ doc: doc ?? this.syncedDoc, extensions });
  }

  private onViewUpdate(update: ViewUpdate) {
    if (update.viewportChanged && !update.docChanged) this.scheduleHighlights(160);
    if (update.focusChanged) this.options.onFocusChange?.(update.view.hasFocus);
  }

  private afterTransactions(transactions: readonly Transaction[]) {
    const localEdit = transactions.some((transaction) => transaction.docChanged && !transaction.annotation(Transaction.remote));
    if (localEdit) {
      queueMicrotask(() => void this.push());
      this.scheduleHighlights(450);
    }
  }

  // -------------------------------------------------------------------------
  // Sync

  private async push() {
    if (this.disposed || this.pushing || this.waitingFor || this.buffer.read_only) return;
    const updates = sendableUpdates(this.view.state);
    if (updates.length === 0) {
      this.checkSettled();
      return;
    }
    const version = getSyncedVersion(this.view.state);
    const epoch = this.epoch;
    this.pushing = true;
    try {
      const result = await conn.request(
        "buffer.push",
        {
          buffer: this.handle,
          version,
          updates: updates.map((update) => ({ changes: update.changes.toJSON() as ChangeSetJson })),
        },
        { generation: this.generation, timeout: 15_000 },
      );
      if (epoch !== this.epoch || this.disposed) return;
      if (result.accepted) {
        this.rejections = 0;
        this.waitFor(Math.max(result.version, version + updates.length));
      } else {
        this.rejections++;
        if (result.version <= getSyncedVersion(this.view.state)) {
          if (this.rejections > 3) {
            this.options.onNeedResync();
            return;
          }
          // Nothing to wait for; retry shortly.
          this.waitFor(Number.MAX_SAFE_INTEGER, 400 * this.rejections, () => void this.push());
        } else {
          this.waitFor(result.version);
        }
      }
    } catch (error) {
      if (epoch !== this.epoch || this.disposed) return;
      if (error instanceof RequestError && error.kind === "connection") return; // Re-synced on reconnect.
      reportError(error);
      this.options.onNeedResync();
    } finally {
      if (epoch === this.epoch) this.pushing = false;
    }
    if (epoch === this.epoch && !this.waitingFor) void this.push();
  }

  /** Waits for the incoming updates that bring us to `target` before pushing again. */
  private waitFor(target: number, timeout = 8000, onTimeout?: () => void) {
    if (getSyncedVersion(this.view.state) >= target) return;
    const epoch = this.epoch;
    const timer = setTimeout(() => {
      if (epoch !== this.epoch || this.waitingFor?.timer !== timer) return;
      this.waitingFor = null;
      if (onTimeout) onTimeout();
      else this.options.onNeedResync();
    }, timeout);
    this.waitingFor = { target, timer };
  }

  receive(event: BufferUpdatesEvent) {
    if (this.disposed) return;
    const state = this.view.state;
    const synced = getSyncedVersion(state);
    const end = event.version + event.updates.length;
    if (end <= synced) return;
    if (event.version > synced) {
      // We missed some updates.
      this.options.onNeedResync();
      return;
    }
    const updates: Update[] = [];
    let doc = this.syncedDoc;
    try {
      event.updates.slice(synced - event.version).forEach((update, index) => {
        const changes = ChangeSet.fromJSON(update.changes);
        doc = changes.apply(doc);
        this.changeHistory.set(synced + index, changes);
        updates.push({ changes, clientID: update.client_id });
      });
    } catch (error) {
      console.error("Malformed buffer update", error);
      this.options.onNeedResync();
      return;
    }
    this.syncedDoc = doc;
    for (const version of this.changeHistory.keys()) {
      if (version >= end - HISTORY_LIMIT) break;
      this.changeHistory.delete(version);
    }
    this.view.dispatch(receiveUpdates(state, updates));

    const now = getSyncedVersion(this.view.state);
    if (this.waitingFor && now >= this.waitingFor.target) {
      clearTimeout(this.waitingFor.timer);
      this.waitingFor = null;
    }
    if (this.pendingHighlights && this.pendingHighlights.highlights.version <= now) {
      const pending = this.pendingHighlights;
      this.pendingHighlights = null;
      this.applyHighlights(pending.highlights, pending.from, pending.to);
    }
    if (this.pendingDiagnostics && this.pendingDiagnostics.version <= now) {
      const pending = this.pendingDiagnostics;
      this.pendingDiagnostics = null;
      this.applyDiagnostics(pending.version, pending.diagnostics);
    }
    if (!this.waitingFor) void this.push();
    this.checkSettled();
  }

  private isSettled(): boolean {
    return !this.pushing && !this.waitingFor && sendableUpdates(this.view.state).length === 0;
  }

  private checkSettled() {
    if (!this.isSettled()) return;
    for (const waiter of Array.from(this.settleWaiters)) waiter();
  }

  /** Resolves once local edits are confirmed (or after `timeout`). */
  private flush(timeout = 1500): Promise<void> {
    if (this.isSettled()) return Promise.resolve();
    return new Promise((resolve) => {
      const done = () => {
        clearTimeout(timer);
        this.settleWaiters.delete(done);
        resolve();
      };
      const timer = setTimeout(done, timeout);
      this.settleWaiters.add(done);
      void this.push();
    });
  }

  /** Maps a position at `version` to the current document. */
  private mappingFrom(version: number): Mapping {
    const state = this.view.state;
    const synced = getSyncedVersion(state);
    if (version > synced) return { kind: "future" };
    const sets: ChangeSet[] = [];
    for (let v = version; v < synced; v++) {
      const changes = this.changeHistory.get(v);
      if (!changes) return { kind: "stale" };
      sets.push(changes);
    }
    for (const update of sendableUpdates(state)) sets.push(update.changes);
    return {
      kind: "ok",
      map: (position, assoc = 1) => sets.reduce((current, changes) => changes.mapPos(current, assoc), position),
    };
  }

  /** Re-syncs after a reconnect or lost update, keeping unconfirmed edits if possible. */
  resync(buffer: BufferState, baseText?: string | null) {
    if (this.disposed) return;
    const state = this.view.state;
    const unconfirmed = sendableUpdates(state);
    const incoming = Text.of(buffer.text.split("\n"));
    let carried: ChangeSet | null = null;
    if (unconfirmed.length > 0) {
      if (this.syncedDoc.eq(incoming)) {
        carried = unconfirmed.reduce((composed, update) => composed.compose(update.changes), ChangeSet.empty(incoming.length));
      } else {
        toast("Some edits couldn't be synced because the file changed in Zed.", "error");
      }
    }
    const selection = state.selection;
    const scrollTop = this.view.scrollDOM.scrollTop;

    this.epoch++;
    if (this.waitingFor) clearTimeout(this.waitingFor.timer);
    this.waitingFor = null;
    this.pushing = false;
    this.rejections = 0;
    this.pendingHighlights = null;
    this.pendingDiagnostics = null;
    this.changeHistory.clear();
    editors.delete(this.handle);
    this.handle = buffer.buffer;
    editors.set(this.handle, this);
    this.buffer = buffer;
    this.generation = conn.generation;
    this.syncedDoc = incoming;
    if (baseText !== undefined) this.baseText = baseText;

    this.view.setState(this.createState(buffer));
    if (carried) {
      this.view.dispatch({ changes: carried, userEvent: "input" });
    }
    const length = this.view.state.doc.length;
    const anchor = Math.min(selection.main.anchor, length);
    const head = Math.min(selection.main.head, length);
    this.view.dispatch({ selection: { anchor, head } });
    this.view.scrollDOM.scrollTop = scrollTop;
    this.options.onDirtyChange?.(buffer.is_dirty);
    this.applyDiagnostics(buffer.version, buffer.diagnostics);
    this.scheduleHighlights(0);
  }

  // -------------------------------------------------------------------------
  // Highlights

  scheduleHighlights(delay: number) {
    if (this.disposed) return;
    if (this.highlightTimer) clearTimeout(this.highlightTimer);
    this.highlightTimer = setTimeout(() => void this.requestHighlights(), delay);
  }

  private async requestHighlights() {
    if (this.disposed || !conn.isReady) return;
    const sequence = ++this.highlightSequence;
    const { from, to } = this.view.viewport;
    const length = this.view.state.doc.length;
    const margin = 4000;
    const requestFrom = Math.max(0, from - margin);
    const requestTo = Math.min(length, to + margin);
    const epoch = this.epoch;
    try {
      const highlights = await conn.request(
        "buffer.highlights",
        { buffer: this.handle, from: requestFrom, to: requestTo },
        { generation: this.generation, timeout: 10_000 },
      );
      if (sequence !== this.highlightSequence || epoch !== this.epoch || this.disposed) return;
      this.applyHighlights(highlights, requestFrom, requestTo);
    } catch {
      // Highlighting is best-effort.
    }
  }

  private applyHighlights(highlights: Highlights, from: number, to: number) {
    const mapping = this.mappingFrom(highlights.version);
    if (mapping.kind === "future") {
      this.pendingHighlights = { highlights, from, to };
      return;
    }
    if (mapping.kind === "stale") {
      this.scheduleHighlights(200);
      return;
    }
    const length = this.view.state.doc.length;
    const clamp = (position: number) => Math.max(0, Math.min(length, position));
    const ranges: Range<Decoration>[] = [];
    for (const [tokenFrom, tokenTo, index] of highlights.tokens) {
      const start = clamp(mapping.map(tokenFrom, 1));
      const end = clamp(mapping.map(tokenTo, -1));
      if (end > start) ranges.push(syntaxMark(index).range(start, end));
    }
    this.view.dispatch({
      effects: setHighlights.of({ from: clamp(mapping.map(from, -1)), to: clamp(mapping.map(to, 1)), ranges }),
    });
  }

  // -------------------------------------------------------------------------
  // Diagnostics

  applyDiagnostics(version: number, diagnostics: Diagnostic[]) {
    if (this.disposed) return;
    const mapping = this.mappingFrom(version);
    if (mapping.kind === "future") {
      this.pendingDiagnostics = { version, diagnostics };
      return;
    }
    const map = mapping.kind === "ok" ? mapping.map : (position: number) => position;
    const length = this.view.state.doc.length;
    const clamp = (position: number) => Math.max(0, Math.min(length, position));
    const converted: CmDiagnostic[] = diagnostics.map((diagnostic) => {
      const from = clamp(map(diagnostic.from, 1));
      const to = Math.max(from, clamp(map(diagnostic.to, -1)));
      return {
        from,
        to,
        severity: diagnostic.severity,
        message: diagnostic.message,
        source: diagnostic.source ?? undefined,
      };
    });
    this.view.dispatch(setDiagnostics(this.view.state, converted));
  }

  // -------------------------------------------------------------------------
  // Diff base

  setBaseText(baseText: string | null) {
    if (!this.hasMerge || this.disposed) return;
    this.baseText = baseText;
    const original = getOriginalDoc(this.view.state);
    const next = Text.of((baseText ?? "").split("\n"));
    if (original.eq(next)) return;
    this.view.dispatch({
      effects: updateOriginalDoc.of({
        doc: next,
        changes: ChangeSet.of([{ from: 0, to: original.length, insert: next }], original.length),
      }),
    });
  }

  // -------------------------------------------------------------------------
  // Language features

  private async completionSource(context: CompletionContext): Promise<CompletionResult | null> {
    const word = context.matchBefore(/[\w$]+/);
    const before = context.state.sliceDoc(Math.max(0, context.pos - 2), context.pos);
    const triggered = /(\.|::|->)$/.test(before);
    if (!context.explicit && !word && !triggered) return null;
    await this.flush();
    if (context.aborted || !this.isSettled()) return null;
    const state = this.view.state;
    const version = getSyncedVersion(state);
    const offset = state.selection.main.head;
    let items;
    try {
      items = await conn.request(
        "buffer.completions",
        { buffer: this.handle, version, offset },
        { generation: this.generation, timeout: 8000 },
      );
    } catch {
      return null;
    }
    if (context.aborted || items.length === 0) return null;
    const options: CmCompletion[] = items.map((item, index) => ({
      label: item.label,
      detail: item.detail ?? undefined,
      type: item.kind ? COMPLETION_TYPES[item.kind.toLowerCase()] ?? undefined : undefined,
      boost: Math.max(-99, 99 - index),
      // Zed applies the completion (including auto-imports); the edits come
      // back as buffer updates, so nothing is inserted locally.
      apply: () => void this.applyCompletion(item.completion_id),
    }));
    return {
      from: word ? word.from : context.pos,
      options,
      validFor: /^[\w$]*$/,
    };
  }

  private async applyCompletion(completionId: number) {
    await this.flush();
    try {
      await conn.request(
        "buffer.apply_completion",
        { buffer: this.handle, completion_id: completionId },
        { generation: this.generation },
      );
    } catch (error) {
      reportError(error);
    }
  }

  async hover(): Promise<Hover | null> {
    await this.flush();
    const state = this.view.state;
    return conn.request(
      "buffer.hover",
      { buffer: this.handle, version: getSyncedVersion(state), offset: state.selection.main.head },
      { generation: this.generation, timeout: 10_000 },
    );
  }

  async definition(): Promise<OpenedItem | null> {
    await this.flush();
    const state = this.view.state;
    return conn.request(
      "buffer.definition",
      {
        workspace_id: this.options.workspaceId,
        buffer: this.handle,
        version: getSyncedVersion(state),
        offset: state.selection.main.head,
      },
      { generation: this.generation, timeout: 15_000 },
    );
  }

  async format() {
    await this.flush();
    await conn.request("buffer.format", { buffer: this.handle }, { generation: this.generation, timeout: 30_000 });
  }

  async save() {
    await this.flush();
    await conn.request("buffer.save", { buffer: this.handle }, { generation: this.generation, timeout: 30_000 });
  }

  // -------------------------------------------------------------------------
  // Editing helpers for toolbars

  revealOffset(offset: number) {
    const position = Math.max(0, Math.min(this.view.state.doc.length, offset));
    this.view.dispatch({
      selection: { anchor: position },
      effects: EditorView.scrollIntoView(position, { y: "center" }),
    });
  }

  ensureCursorVisible() {
    this.view.dispatch({ effects: EditorView.scrollIntoView(this.view.state.selection.main.head, { y: "nearest", yMargin: 48 }) });
  }

  toggleWrap(): boolean {
    this.wrapping = !this.wrapping;
    this.view.dispatch({ effects: this.wrapCompartment.reconfigure(this.wrapping ? EditorView.lineWrapping : []) });
    return this.wrapping;
  }

  get isWrapping(): boolean {
    return this.wrapping;
  }

  runKey(key: AccessoryKey) {
    const view = this.view;
    switch (key) {
      case "tab":
        if (view.state.selection.ranges.some((range) => !range.empty)) indentMore(view);
        else view.dispatch(view.state.replaceSelection(view.state.facet(indentUnit)), { userEvent: "input.type", scrollIntoView: true });
        break;
      case "outdent":
        indentLess(view);
        break;
      case "undo":
        undo(view);
        break;
      case "redo":
        redo(view);
        break;
      case "left":
        cursorCharLeft(view);
        break;
      case "right":
        cursorCharRight(view);
        break;
      case "up":
        cursorLineUp(view);
        break;
      case "down":
        cursorLineDown(view);
        break;
      case "search":
        openSearchPanel(view);
        break;
      case "problems":
        openLintPanel(view);
        break;
    }
  }

  insertText(text: string) {
    if (this.buffer.read_only) return;
    this.view.dispatch(this.view.state.replaceSelection(text), { userEvent: "input.type", scrollIntoView: true });
  }

  focus() {
    this.view.focus();
  }

  destroy() {
    this.disposed = true;
    this.epoch++;
    if (this.waitingFor) clearTimeout(this.waitingFor.timer);
    if (this.highlightTimer) clearTimeout(this.highlightTimer);
    for (const waiter of Array.from(this.settleWaiters)) waiter();
    if (editors.get(this.handle) === this) editors.delete(this.handle);
    this.view.destroy();
  }
}

export type AccessoryKey = "tab" | "outdent" | "undo" | "redo" | "left" | "right" | "up" | "down" | "search" | "problems";

/** A read-only unified diff of two texts (commit views, deleted files). */
export function createStaticDiffView(parent: HTMLElement, original: string, current: string): EditorView {
  return new EditorView({
    parent,
    state: EditorState.create({
      doc: current,
      extensions: [
        lineNumbers(),
        highlightSpecialChars(),
        EditorState.readOnly.of(true),
        EditorView.editable.of(false),
        editorTheme,
        darkFlag(),
        unifiedMergeView({
          original,
          mergeControls: false,
          highlightChanges: true,
          gutter: true,
          syntaxHighlightDeletions: false,
          collapseUnchanged: { margin: 3, minSize: 4 },
        }),
      ],
    }),
  });
}
