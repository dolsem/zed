import type { TargetedKeyboardEvent } from "preact";
import { memo } from "preact/compat";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "preact/hooks";
import { compactDiff, diffLines, handleMarkdownClick, renderInlineMarkdown, renderMarkdown } from "./markdown";
import type {
  AssistantChunk,
  PlanEntry,
  ThreadControl,
  ThreadEntry,
  ThreadState,
  ThreadStatus,
  TokenUsage,
  ToolCall,
  ToolCallContent,
  ToolCallStatus,
} from "./protocol";
import {
  call,
  currentThreadId,
  getDraft,
  goToPanel,
  PANEL_TABS,
  PANEL_THREADS,
  patchThreadState,
  setDraft,
  threadEntries,
  threadLoading,
  threadState,
  toast,
  workspaceItems,
} from "./state";
import { EmptyState, formatTokens, haptic, Icon, PickerSheet, Spinner, type IconName } from "./ui";

export function ThreadView() {
  const threadId = currentThreadId.value;
  const state = threadState.value;
  const entries = threadEntries.value;
  const loading = threadLoading.value;

  if (!threadId) {
    return (
      <div class="panel-inner">
        <ThreadHeader state={null} />
        <div class="panel-scroll">
          <EmptyState icon="message" title="No thread open">
            Pick a thread from the list, or start a new one.
            <button class="button secondary pressable" onClick={() => goToPanel(PANEL_THREADS)}>
              <Icon name="chevron-left" size={16} />
              Threads
            </button>
          </EmptyState>
        </div>
      </div>
    );
  }

  return (
    <div class="panel-inner thread-view">
      <ThreadHeader state={state} />
      <ThreadEntries threadId={threadId} entries={entries} state={state} loading={loading} />
      <ThreadFooter threadId={threadId} state={state} />
    </div>
  );
}

const STATUS_LABEL: Record<ThreadStatus, string | null> = {
  idle: null,
  running: "Generating",
  waiting_for_confirmation: "Needs approval",
  error: "Error",
};

function ThreadHeader({ state }: { state: ThreadState | null }) {
  const tabsCount = (workspaceItems.value?.editors.length ?? 0) + (workspaceItems.value?.terminals.length ?? 0);
  const status = state?.status ?? "idle";
  const label = STATUS_LABEL[status];
  return (
    <header class="panel-header">
      <button class="header-button" onClick={() => goToPanel(PANEL_THREADS)} aria-label="Threads">
        <Icon name="chevron-left" size={24} />
      </button>
      <div class="panel-header-title">
        <div class="thread-header-title">{state?.title || (state ? "New Thread" : "")}</div>
        {state && (
          <div class="thread-header-sub">
            {label && (
              <span class={`status-pill ${status}`}>
                {status === "running" && <Spinner size={11} />}
                {label}
              </span>
            )}
            <span class="muted">{state.agent_name}</span>
          </div>
        )}
      </div>
      <button class="header-button tabs-button" onClick={() => goToPanel(PANEL_TABS)} aria-label="Tabs">
        <Icon name="layers" size={22} />
        {tabsCount > 0 && <span class="header-count">{tabsCount}</span>}
      </button>
    </header>
  );
}

// ---------------------------------------------------------------------------
// Entries

function ThreadEntries({
  threadId,
  entries,
  state,
  loading,
}: {
  threadId: string;
  entries: ThreadEntry[];
  state: ThreadState | null;
  loading: boolean;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  const [showJump, setShowJump] = useState(false);
  const [pinnedState, setPinnedState] = useState(true);
  const running = state?.status === "running";
  const waiting = state?.status === "waiting_for_confirmation";

  const scrollToBottom = (smooth = false) => {
    const element = scrollRef.current;
    if (!element) return;
    element.scrollTo({ top: element.scrollHeight, behavior: smooth ? "smooth" : "auto" });
  };

  useLayoutEffect(() => {
    pinned.current = true;
    setPinnedState(true);
    setShowJump(false);
    scrollToBottom();
  }, [threadId]);

  useLayoutEffect(() => {
    if (pinned.current) scrollToBottom();
  }, [entries]);

  // Stay pinned while content streams in or the viewport shrinks (keyboard).
  useEffect(() => {
    const scroller = scrollRef.current;
    const content = contentRef.current;
    if (!scroller || !content) return;
    const observer = new ResizeObserver(() => {
      if (pinned.current) scroller.scrollTop = scroller.scrollHeight;
    });
    observer.observe(scroller);
    observer.observe(content);
    return () => observer.disconnect();
  }, []);

  const onScroll = () => {
    const element = scrollRef.current;
    if (!element) return;
    const distance = element.scrollHeight - element.scrollTop - element.clientHeight;
    pinned.current = distance < 56;
    setPinnedState(pinned.current);
    setShowJump(distance > 240);
  };

  return (
    <div class="thread-scroll-wrap">
      <div class="panel-scroll thread-scroll" ref={scrollRef} onScroll={onScroll} onClick={handleMarkdownClick}>
        <div class="thread-entries" ref={contentRef}>
          {entries.length === 0 &&
            (loading ? (
              <div class="center-fill">
                <Spinner size={22} />
              </div>
            ) : (
              <EmptyState icon="sparkle" title="Start the conversation">
                Ask {state?.agent_name || "the agent"} anything about your project.
              </EmptyState>
            ))}
          {entries.map((entry, index) => (
            <EntryView
              key={index}
              entry={entry}
              threadId={threadId}
              streaming={running && index === entries.length - 1}
            />
          ))}
          {running && entries[entries.length - 1]?.kind === "user" && (
            <div class="typing-indicator">
              <span />
              <span />
              <span />
            </div>
          )}
        </div>
      </div>
      {waiting && !pinnedState ? (
        <button
          class="jump-latest attention pressable"
          onClick={() => {
            const card = scrollRef.current?.querySelector(".tool-card.waiting");
            if (card) card.scrollIntoView({ block: "center", behavior: "smooth" });
            else scrollToBottom(true);
          }}
        >
          <Icon name="alert" size={16} />
          Needs approval
        </button>
      ) : (
        showJump && (
          <button
            class="jump-latest pressable"
            onClick={() => {
              pinned.current = true;
              scrollToBottom(true);
            }}
          >
            <Icon name="arrow-down" size={16} />
            Latest
          </button>
        )
      )}
    </div>
  );
}

const EntryView = memo(function EntryView({
  entry,
  threadId,
  streaming,
}: {
  entry: ThreadEntry;
  threadId: string;
  streaming: boolean;
}) {
  switch (entry.kind) {
    case "user":
      return (
        <div class="entry entry-user">
          <div class="user-bubble markdown" dangerouslySetInnerHTML={{ __html: renderMarkdown(entry.text) }} />
        </div>
      );
    case "assistant":
      return <AssistantEntry chunks={entry.chunks} streaming={streaming} />;
    case "tool_call":
      return <ToolCallCard call={entry} threadId={threadId} />;
    case "compaction":
      return <CompactionEntry summary={entry.summary} />;
    case "elicitation":
      return (
        <div class="entry callout info">
          <Icon name="monitor" size={18} />
          <div>
            <div class="markdown" dangerouslySetInnerHTML={{ __html: renderMarkdown(entry.message) }} />
            <div class="callout-hint">Answer this in Zed on your desktop.</div>
          </div>
        </div>
      );
  }
});

function AssistantEntry({ chunks, streaming }: { chunks: AssistantChunk[]; streaming: boolean }) {
  return (
    <div class="entry entry-assistant">
      {chunks.map((chunk, index) =>
        chunk.kind === "thought" ? (
          <ThinkingSection
            key={index}
            markdown={chunk.markdown}
            streaming={streaming && index === chunks.length - 1}
          />
        ) : (
          <div key={index} class="markdown" dangerouslySetInnerHTML={{ __html: renderMarkdown(chunk.markdown) }} />
        ),
      )}
    </div>
  );
}

function ThinkingSection({ markdown, streaming }: { markdown: string; streaming: boolean }) {
  const [userOpen, setUserOpen] = useState<boolean | null>(null);
  const open = userOpen ?? streaming;
  return (
    <div class={`thinking ${open ? "open" : ""}`}>
      <button class="thinking-toggle" onClick={() => setUserOpen(!open)}>
        <Icon name="bulb" size={15} />
        <span>{streaming ? "Thinking…" : "Thought process"}</span>
        <Icon name="chevron-down" size={15} class="chevron" />
      </button>
      {open && <div class="thinking-body markdown" dangerouslySetInnerHTML={{ __html: renderMarkdown(markdown) }} />}
    </div>
  );
}

function CompactionEntry({ summary }: { summary: string | null }) {
  const [open, setOpen] = useState(false);
  return (
    <div class="entry compaction">
      <button class="compaction-divider" disabled={!summary} onClick={() => setOpen(!open)}>
        <span class="line" />
        <Icon name="archive" size={14} />
        <span>Conversation compacted</span>
        {summary && <Icon name={open ? "chevron-up" : "chevron-down"} size={14} />}
        <span class="line" />
      </button>
      {open && summary && <div class="compaction-summary markdown" dangerouslySetInnerHTML={{ __html: renderMarkdown(summary) }} />}
    </div>
  );
}

const TOOL_ICONS: Record<string, IconName> = {
  read: "eye",
  edit: "pencil",
  delete: "trash",
  move: "move",
  search: "search",
  execute: "terminal",
  think: "bulb",
  fetch: "globe",
  switch_mode: "shuffle",
  other: "wrench",
};

function ToolStatusIcon({ status }: { status: ToolCallStatus }) {
  switch (status) {
    case "pending":
    case "in_progress":
      return (
        <span class={`tool-status ${status}`}>
          <Spinner size={15} />
        </span>
      );
    case "waiting_for_confirmation":
      return (
        <span class="tool-status waiting">
          <Icon name="alert" size={15} />
        </span>
      );
    case "completed":
      return (
        <span class="tool-status completed">
          <Icon name="check" size={15} />
        </span>
      );
    case "failed":
      return (
        <span class="tool-status failed">
          <Icon name="x-circle" size={15} />
        </span>
      );
    default:
      return (
        <span class="tool-status canceled">
          <Icon name="slash" size={15} />
        </span>
      );
  }
}

function ToolCallCard({ call: toolCall, threadId }: { call: ToolCall; threadId: string }) {
  const waiting = toolCall.status === "waiting_for_confirmation" || !!toolCall.permission_options?.length;
  const [userOpen, setUserOpen] = useState<boolean | null>(null);
  const hasContent = toolCall.content.length > 0;
  const open = hasContent && (userOpen ?? waiting);
  return (
    <div class={`entry tool-card ${waiting ? "waiting" : ""} status-${toolCall.status}`}>
      <button
        class="tool-header"
        disabled={!hasContent}
        onClick={() => setUserOpen(!open)}
        aria-expanded={open}
      >
        <Icon name={TOOL_ICONS[toolCall.tool_kind] ?? "wrench"} size={16} class="tool-kind-icon" />
        <span class="tool-title" dangerouslySetInnerHTML={{ __html: renderInlineMarkdown(toolCall.title) }} />
        <ToolStatusIcon status={toolCall.status} />
        {hasContent && <Icon name="chevron-down" size={15} class={`chevron ${open ? "open" : ""}`} />}
      </button>
      {open && (
        <div class="tool-body">
          {toolCall.content.map((content, index) => (
            <ToolContent key={index} content={content} />
          ))}
        </div>
      )}
      {toolCall.permission_options && toolCall.permission_options.length > 0 && (
        <PermissionButtons threadId={threadId} toolCall={toolCall} />
      )}
    </div>
  );
}

function PermissionButtons({ threadId, toolCall }: { threadId: string; toolCall: ToolCall }) {
  const [chosen, setChosen] = useState<string | null>(null);
  const options = toolCall.permission_options ?? [];
  return (
    <div class="permission-buttons">
      {options.map((option) => {
        const allow = option.kind.startsWith("allow");
        return (
          <button
            key={option.id}
            class={`button ${allow ? (option.kind === "allow_once" ? "primary" : "secondary") : "danger-outline"} pressable`}
            disabled={chosen !== null}
            onClick={async () => {
              haptic();
              setChosen(option.id);
              const result = await call("thread.authorize", {
                thread_id: threadId,
                tool_call_id: toolCall.id,
                option_id: option.id,
              });
              if (result === undefined) setChosen(null);
            }}
          >
            {chosen === option.id ? <Spinner size={15} /> : <Icon name={allow ? "check" : "x"} size={15} />}
            {option.label}
          </button>
        );
      })}
    </div>
  );
}

// Strips ANSI escape sequences from terminal output shown in cards.
const ANSI_PATTERN = /\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]/g;

function ToolContent({ content }: { content: ToolCallContent }) {
  switch (content.kind) {
    case "markdown":
      return <div class="tool-markdown markdown" dangerouslySetInnerHTML={{ __html: renderMarkdown(content.markdown) }} />;
    case "diff":
      return <DiffBlock path={content.path} oldText={content.old_text} newText={content.new_text} />;
    case "terminal":
      return <TerminalBlock command={content.command} output={content.output} exitCode={content.exit_code} />;
  }
}

const MAX_DIFF_ROWS = 300;

function DiffBlock({ path, oldText, newText }: { path: string; oldText: string | null; newText: string }) {
  const [expanded, setExpanded] = useState(false);
  const { rows, added, removed } = useMemo(() => {
    const lines = diffLines(oldText ?? "", newText);
    let addedCount = 0;
    let removedCount = 0;
    for (const line of lines) {
      if (line.type === "added") addedCount++;
      else if (line.type === "removed") removedCount++;
    }
    return { rows: compactDiff(lines, 3), added: addedCount, removed: removedCount };
  }, [oldText, newText]);
  const visible = expanded ? rows : rows.slice(0, MAX_DIFF_ROWS);
  return (
    <div class="diff-block">
      <div class="diff-block-header">
        <span class="diff-path">
          <bdi>{path}</bdi>
        </span>
        {oldText === null && <span class="badge success">new</span>}
        <span class="line-stats">
          <span class="added">+{added}</span>
          <span class="removed">−{removed}</span>
        </span>
      </div>
      <div class="diff-lines">
        {visible.map((row, index) =>
          row.type === "gap" ? (
            <div key={index} class="diff-gap">
              ⋯ {row.count} unchanged line{row.count === 1 ? "" : "s"}
            </div>
          ) : (
            <div key={index} class={`diff-line ${row.type}`}>
              <span class="diff-gutter">{row.type === "added" ? "+" : row.type === "removed" ? "−" : " "}</span>
              <span class="diff-text">{row.text || " "}</span>
            </div>
          ),
        )}
      </div>
      {rows.length > MAX_DIFF_ROWS && !expanded && (
        <button class="show-more" onClick={() => setExpanded(true)}>
          Show {rows.length - MAX_DIFF_ROWS} more lines
        </button>
      )}
    </div>
  );
}

const MAX_TERMINAL_LINES = 120;

function TerminalBlock({ command, output, exitCode }: { command: string | null; output: string; exitCode: number | null }) {
  const [expanded, setExpanded] = useState(false);
  const lines = useMemo(() => output.replace(ANSI_PATTERN, "").replace(/\r\n?/g, "\n").replace(/\n+$/, "").split("\n"), [output]);
  const hidden = expanded ? 0 : Math.max(0, lines.length - MAX_TERMINAL_LINES);
  return (
    <div class="terminal-block">
      {command && (
        <div class="terminal-command">
          <span class="prompt">$</span> {command}
        </div>
      )}
      {hidden > 0 && (
        <button class="show-more" onClick={() => setExpanded(true)}>
          Show {hidden} earlier lines
        </button>
      )}
      {output && <pre class="terminal-output">{lines.slice(hidden).join("\n")}</pre>}
      {exitCode !== null && (
        <div class={`exit-code ${exitCode === 0 ? "ok" : "failed"}`}>
          {exitCode === 0 ? <Icon name="check" size={12} /> : <Icon name="x" size={12} />}
          exit {exitCode}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Footer: plan, queue, errors and the composer

function ThreadFooter({ threadId, state }: { threadId: string; state: ThreadState | null }) {
  return (
    <div class="thread-footer">
      {state?.error && (
        <div class="callout error">
          <Icon name="alert" size={18} />
          <div class="callout-text">{state.error}</div>
        </div>
      )}
      {state?.needs_desktop_input && (
        <div class="callout info compact">
          <Icon name="monitor" size={16} />
          <div class="callout-text">The agent is waiting for input you can only give in Zed on your desktop.</div>
        </div>
      )}
      {state && state.plan.length > 0 && <PlanView plan={state.plan} />}
      {state && state.queued_messages.length > 0 && <QueuedMessages threadId={threadId} messages={state.queued_messages} />}
      <Composer threadId={threadId} state={state} />
    </div>
  );
}

function PlanView({ plan }: { plan: PlanEntry[] }) {
  const [open, setOpen] = useState(false);
  const completed = plan.filter((entry) => entry.status === "completed").length;
  const current = plan.find((entry) => entry.status === "in_progress") ?? plan.find((entry) => entry.status === "pending");
  return (
    <div class={`plan ${open ? "open" : ""}`}>
      <button class="plan-header" onClick={() => setOpen(!open)}>
        <Icon name="list" size={16} />
        <span class="plan-label">Plan</span>
        <span class="plan-progress">
          {completed}/{plan.length}
        </span>
        {!open && current && <span class="plan-current">{current.content}</span>}
        <Icon name={open ? "chevron-down" : "chevron-up"} size={16} class="plan-chevron" />
      </button>
      {open && (
        <ul class="plan-list">
          {plan.map((entry, index) => (
            <li key={index} class={`plan-item ${entry.status}`}>
              {entry.status === "completed" ? (
                <Icon name="check-circle" size={16} />
              ) : entry.status === "in_progress" ? (
                <Spinner size={16} />
              ) : (
                <Icon name="circle" size={16} />
              )}
              <span>{entry.content}</span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function QueuedMessages({ threadId, messages }: { threadId: string; messages: string[] }) {
  return (
    <div class="queued">
      <div class="queued-title">Queued</div>
      {messages.map((message, index) => (
        <div class="queued-item" key={`${index}:${message}`}>
          <span class="queued-text">{message}</span>
          <button
            class="icon-button small"
            aria-label="Remove queued message"
            onClick={() => {
              haptic();
              patchThreadState((state) => ({
                ...state,
                queued_messages: state.queued_messages.filter((_, i) => i !== index),
              }));
              void call("thread.remove_queued", { thread_id: threadId, index });
            }}
          >
            <Icon name="x" size={14} />
          </button>
        </div>
      ))}
    </div>
  );
}

function Composer({ threadId, state }: { threadId: string; state: ThreadState | null }) {
  const [text, setText] = useState(() => getDraft(threadId));
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const running = state?.status === "running" || state?.status === "waiting_for_confirmation";

  useEffect(() => {
    setText(getDraft(threadId));
  }, [threadId]);

  useLayoutEffect(() => {
    const textarea = textareaRef.current;
    if (!textarea) return;
    textarea.style.height = "auto";
    const max = Math.max(120, window.innerHeight * 0.35);
    textarea.style.height = `${Math.min(textarea.scrollHeight, max)}px`;
  }, [text]);

  const update = (value: string) => {
    setText(value);
    setDraft(threadId, value);
  };

  const send = async () => {
    const message = text.trim();
    if (!message) return;
    haptic();
    update("");
    const result = await call("thread.send", { thread_id: threadId, text: message });
    if (result === undefined && !getDraft(threadId)) update(message);
  };

  const stop = () => {
    haptic();
    void call("thread.cancel", { thread_id: threadId });
  };

  const onKeyDown = (event: TargetedKeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      void send();
    }
  };

  return (
    <div class="composer">
      <textarea
        ref={textareaRef}
        class="composer-input"
        rows={1}
        placeholder={running ? "Queue a message…" : `Message ${state?.agent_name || "the agent"}…`}
        value={text}
        onInput={(event) => update(event.currentTarget.value)}
        onKeyDown={onKeyDown}
        enterkeyhint="enter"
      />
      <div class="composer-bar">
        <div class="composer-controls">
          {state?.controls.map((control) => <ControlChip key={control.id} control={control} threadId={threadId} />)}
        </div>
        {state?.token_usage && <TokenMeter usage={state.token_usage} />}
        {running && (
          <button class="composer-button stop pressable" onClick={stop} aria-label="Stop">
            <Icon name="stop" size={16} />
          </button>
        )}
        <button
          class="composer-button send pressable"
          disabled={!text.trim()}
          onClick={() => void send()}
          aria-label={running ? "Queue message" : "Send"}
        >
          <Icon name="arrow-up" size={18} />
        </button>
      </div>
    </div>
  );
}

function setControl(threadId: string, control: ThreadControl, value: string | boolean) {
  const previous = control;
  patchThreadState((state) => ({
    ...state,
    controls: state.controls.map((candidate) =>
      candidate.id === control.id ? ({ ...candidate, value } as ThreadControl) : candidate,
    ),
  }));
  void call("thread.set_control", { thread_id: threadId, control_id: control.id, value }).then((result) => {
    if (result !== undefined) return;
    patchThreadState((state) => ({
      ...state,
      controls: state.controls.map((candidate) => (candidate.id === control.id ? previous : candidate)),
    }));
  });
}

function ControlChip({ control, threadId }: { control: ThreadControl; threadId: string }) {
  const [open, setOpen] = useState(false);
  if (control.kind === "toggle") {
    return (
      <button
        class={`control-chip toggle pressable ${control.value ? "on" : ""}`}
        aria-pressed={control.value}
        onClick={() => {
          haptic();
          setControl(threadId, control, !control.value);
        }}
      >
        {control.id === "fast" ? <Icon name="zap" size={13} /> : control.id === "thinking" ? <Icon name="bulb" size={13} /> : null}
        {control.label}
      </button>
    );
  }
  const selected = control.options.find((option) => option.value === control.value);
  return (
    <>
      <button class="control-chip pressable" onClick={() => setOpen(true)}>
        <span class="control-chip-value">{selected?.label ?? control.label}</span>
        <Icon name="chevron-down" size={13} />
      </button>
      <PickerSheet
        open={open}
        title={control.label}
        options={control.options}
        value={control.value}
        onSelect={(value) => {
          if (value !== control.value) setControl(threadId, control, value);
        }}
        onClose={() => setOpen(false)}
      />
    </>
  );
}

function TokenMeter({ usage }: { usage: TokenUsage }) {
  const ratio = usage.max > 0 ? Math.min(1, usage.used / usage.max) : 0;
  const radius = 7;
  const circumference = 2 * Math.PI * radius;
  const level = ratio > 0.9 ? "error" : ratio > 0.75 ? "warning" : "";
  return (
    <button
      class={`token-meter ${level}`}
      onClick={() =>
        toast(`${usage.used.toLocaleString()} of ${usage.max.toLocaleString()} tokens (${Math.round(ratio * 100)}%)`)
      }
      aria-label="Token usage"
    >
      <svg width="18" height="18" viewBox="0 0 18 18">
        <circle cx="9" cy="9" r={radius} fill="none" stroke="currentColor" stroke-width="2.2" opacity="0.2" />
        <circle
          cx="9"
          cy="9"
          r={radius}
          fill="none"
          stroke="currentColor"
          stroke-width="2.2"
          stroke-linecap="round"
          stroke-dasharray={`${circumference * ratio} ${circumference}`}
          transform="rotate(-90 9 9)"
        />
      </svg>
      <span>{formatTokens(usage.used)}</span>
    </button>
  );
}
