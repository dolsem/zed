import type { JSX, TargetedKeyboardEvent } from "preact";
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import type { TerminalRun, TerminalScreen } from "./protocol";
import { closeOverlay, conn, errorMessage, reportError, theme, toast, type OverlayTarget } from "./state";
import { terminalColor } from "./theme";
import { CloseButton, EmptyState, haptic, Icon, keyboardOpen, Spinner } from "./ui";

// Below this the grid is unreadable on a phone; wider terminals scroll sideways instead.
const MIN_FONT_SIZE = 11;
const MAX_FONT_SIZE = 15;
const LINE_HEIGHT = 1.25;

let cachedRatio: { font: string; ratio: number } | null = null;

/** Width of one monospace cell relative to the font size. */
function cellRatio(): number {
  const font = getComputedStyle(document.documentElement).getPropertyValue("--font-mono").trim() || "monospace";
  if (cachedRatio?.font === font) return cachedRatio.ratio;
  const context = document.createElement("canvas").getContext("2d");
  let ratio = 0.6;
  if (context) {
    context.font = `100px ${font}`;
    const width = context.measureText("MMMMMMMMMM").width / 10;
    if (width > 0) ratio = width / 100;
  }
  cachedRatio = { font, ratio };
  return ratio;
}

function controlCharacter(character: string): string | null {
  if (character === " " || character === "@" || character === "2") return "\x00";
  if (character === "?") return "\x7f";
  const code = character.toUpperCase().charCodeAt(0);
  if (code >= 64 && code <= 95) return String.fromCharCode(code - 64);
  return null;
}

const ARROWS: Record<string, string> = { ArrowUp: "A", ArrowDown: "B", ArrowRight: "C", ArrowLeft: "D" };
const SPECIAL_KEYS: Record<string, string> = {
  Enter: "\r",
  Backspace: "\x7f",
  Escape: "\x1b",
  Tab: "\t",
  Home: "\x1b[H",
  End: "\x1b[F",
  PageUp: "\x1b[5~",
  PageDown: "\x1b[6~",
  Delete: "\x1b[3~",
  Insert: "\x1b[2~",
};

export function TerminalView({ target }: { target: Extract<OverlayTarget, { type: "terminal" }> }) {
  const [screen, setScreen] = useState<TerminalScreen | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [ctrl, setCtrl] = useState(false);
  const [focused, setFocused] = useState(false);
  const [width, setWidth] = useState(0);
  const screenRef = useRef<TerminalScreen | null>(null);
  const ctrlRef = useRef(false);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  screenRef.current = screen;
  ctrlRef.current = ctrl;

  // Subscribe on show, re-subscribe after reconnects, unsubscribe on exit.
  useEffect(() => {
    let disposed = false;
    const open = async () => {
      try {
        const result = await conn.request("terminal.open", { workspace_id: target.workspaceId, item_id: target.itemId });
        if (!disposed) {
          setScreen(result);
          setError(null);
        }
      } catch (reason) {
        if (!disposed && !screenRef.current) setError(errorMessage(reason));
        else reportError(reason);
      }
    };
    void open();
    const offs = [
      conn.on("hello", () => void open()),
      conn.on("terminal.screen", (next) => {
        if (next.item_id === target.itemId) setScreen(next);
      }),
      conn.on("terminal.closed", (event) => {
        if (event.item_id !== target.itemId || event.workspace_id !== target.workspaceId) return;
        toast("Terminal closed");
        closeOverlay();
      }),
    ];
    return () => {
      disposed = true;
      offs.forEach((off) => off());
      if (conn.isReady) {
        conn.request("terminal.close", { workspace_id: target.workspaceId, item_id: target.itemId }).catch(() => {});
      }
    };
  }, []);

  useLayoutEffect(() => {
    const element = wrapRef.current;
    if (!element) return;
    const observer = new ResizeObserver(() => setWidth(element.clientWidth));
    observer.observe(element);
    setWidth(element.clientWidth);
    return () => observer.disconnect();
  }, [screen === null]);

  const send = (data: string) => {
    if (!data) return;
    conn
      .request("terminal.input", { workspace_id: target.workspaceId, item_id: target.itemId, data }, { timeout: 8000 })
      .catch(reportError);
  };

  const sendText = (text: string) => {
    if (ctrlRef.current && text.length === 1) {
      const control = controlCharacter(text);
      setCtrl(false);
      if (control) {
        send(control);
        return;
      }
    }
    send(text.replace(/\r?\n/g, "\r"));
  };

  const arrow = (key: string) => {
    const letter = ARROWS[key];
    send((screenRef.current?.application_cursor ? "\x1bO" : "\x1b[") + letter);
  };

  const paste = (text: string) => {
    if (!text) return;
    const normalized = text.replace(/\r?\n/g, "\r");
    send(screenRef.current?.bracketed_paste ? `\x1b[200~${normalized}\x1b[201~` : normalized);
  };

  const onKeyDown = (event: TargetedKeyboardEvent<HTMLTextAreaElement>) => {
    if (event.isComposing) return;
    if (ARROWS[event.key]) {
      event.preventDefault();
      arrow(event.key);
      return;
    }
    if (event.key === "Tab" && event.shiftKey) {
      event.preventDefault();
      send("\x1b[Z");
      return;
    }
    const special = SPECIAL_KEYS[event.key];
    if (special) {
      event.preventDefault();
      setCtrl(false);
      send(special);
      return;
    }
    if ((event.ctrlKey || ctrlRef.current) && event.key.length === 1 && !event.metaKey) {
      const control = controlCharacter(event.key);
      if (control) {
        event.preventDefault();
        setCtrl(false);
        send(control);
      }
      return;
    }
    if (event.altKey && event.key.length === 1) {
      event.preventDefault();
      send(`\x1b${event.key}`);
    }
  };

  const onBeforeInput = (event: InputEvent) => {
    switch (event.inputType) {
      case "insertText":
        event.preventDefault();
        sendText(event.data ?? "");
        break;
      case "insertLineBreak":
      case "insertParagraph":
        event.preventDefault();
        send("\r");
        break;
      case "deleteContentBackward":
        event.preventDefault();
        send("\x7f");
        break;
      case "deleteWordBackward":
        event.preventDefault();
        send("\x17");
        break;
      default:
        // Composition and autocorrect replacements are handled on input.
        break;
    }
  };

  // Attach `beforeinput` natively (Preact's typings don't cover it well).
  useEffect(() => {
    const input = inputRef.current;
    if (!input) return;
    const handler = (event: Event) => onBeforeInput(event as InputEvent);
    input.addEventListener("beforeinput", handler);
    return () => input.removeEventListener("beforeinput", handler);
  });

  const onCompositionEnd = (event: CompositionEvent) => {
    sendText(event.data ?? "");
    if (inputRef.current) inputRef.current.value = "";
  };

  const onInput = () => {
    // Anything that slipped through (e.g. composition) has been sent already.
    const input = inputRef.current;
    if (input && !(input as HTMLTextAreaElement & { isComposing?: boolean }).isComposing) input.value = "";
  };

  const focusInput = () => inputRef.current?.focus({ preventScroll: true });

  // Vertical swipes scroll the terminal's history in Zed.
  const scrollState = useRef({ pending: 0, timer: null as ReturnType<typeof setTimeout> | null });
  const queueScroll = (lines: number) => {
    const state = scrollState.current;
    state.pending += lines;
    if (state.timer) return;
    state.timer = setTimeout(() => {
      state.timer = null;
      const whole = Math.trunc(state.pending);
      if (whole === 0) return;
      state.pending -= whole;
      conn
        .request("terminal.scroll", { workspace_id: target.workspaceId, item_id: target.itemId, lines: whole })
        .catch(reportError);
    }, 40);
  };

  const ratio = cellRatio();
  const columns = Math.max(1, screen?.columns ?? 80);
  const fittedSize = width > 0 ? (width - 8) / (columns * ratio) : 12;
  const fontSize = Math.max(MIN_FONT_SIZE, Math.min(MAX_FONT_SIZE, fittedSize));
  const charWidth = fontSize * ratio;
  const rowHeight = Math.round(fontSize * LINE_HEIGHT * 100) / 100;

  useEffect(() => {
    const element = wrapRef.current;
    if (!element) return;
    let start: { x: number; y: number; lastY: number; mode: "undecided" | "vertical" | "horizontal" } | null = null;
    let moved = false;
    const onStart = (event: TouchEvent) => {
      const touch = event.touches[0];
      start = { x: touch.clientX, y: touch.clientY, lastY: touch.clientY, mode: "undecided" };
      moved = false;
    };
    const onMove = (event: TouchEvent) => {
      if (!start) return;
      const touch = event.touches[0];
      const dx = touch.clientX - start.x;
      const dy = touch.clientY - start.y;
      if (start.mode === "undecided" && Math.hypot(dx, dy) > 8) {
        start.mode = Math.abs(dy) > Math.abs(dx) ? "vertical" : "horizontal";
        moved = true;
      }
      if (start.mode !== "vertical") return;
      event.preventDefault();
      const delta = touch.clientY - start.lastY;
      start.lastY = touch.clientY;
      // Finger down reveals older output (positive = into history).
      queueScroll(delta / rowHeight);
    };
    const onEnd = () => {
      start = null;
    };
    const onClick = () => {
      if (!moved) focusInput();
    };
    const onWheel = (event: WheelEvent) => {
      if (Math.abs(event.deltaY) < Math.abs(event.deltaX)) return;
      event.preventDefault();
      queueScroll(-event.deltaY / rowHeight);
    };
    element.addEventListener("touchstart", onStart, { passive: true });
    element.addEventListener("touchmove", onMove, { passive: false });
    element.addEventListener("touchend", onEnd);
    element.addEventListener("click", onClick);
    element.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      element.removeEventListener("touchstart", onStart);
      element.removeEventListener("touchmove", onMove);
      element.removeEventListener("touchend", onEnd);
      element.removeEventListener("click", onClick);
      element.removeEventListener("wheel", onWheel);
    };
  }, [rowHeight, screen === null]);

  const keyboard = keyboardOpen.value;
  const ansi = theme.value.terminal_ansi;

  const accessoryKey = (label: JSX.Element | string, onPress: () => void, aria: string, extraClass = "") => (
    <button
      class={`accessory-key ${extraClass}`}
      aria-label={aria}
      onPointerDown={(event) => event.preventDefault()}
      onClick={() => {
        haptic();
        onPress();
      }}
    >
      {label}
    </button>
  );

  return (
    <div class="overlay-frame terminal-frame">
      <header class="overlay-header">
        <div class="overlay-title">
          <Icon name="terminal" size={16} />
          <span class="overlay-title-text">{screen?.title || target.title || "Terminal"}</span>
        </div>
        {screen && (
          <div class="overlay-subtitle">
            {screen.columns}×{screen.rows}
          </div>
        )}
      </header>
      <div class="overlay-content terminal-content">
        {!screen ? (
          error ? (
            <EmptyState icon="alert" title="Couldn't open the terminal">
              {error}
            </EmptyState>
          ) : (
            <div class="center-fill">
              <Spinner size={24} />
            </div>
          )
        ) : (
          <div class="terminal-wrap" ref={wrapRef}>
            <div
              class="terminal-grid"
              style={{
                fontSize: `${fontSize}px`,
                lineHeight: `${rowHeight}px`,
                width: `${columns * charWidth}px`,
              }}
            >
              {screen.lines.map((runs, row) => (
                <div class="terminal-row" key={row} style={{ height: `${rowHeight}px` }}>
                  {runs.map((run, index) => (
                    <TerminalSpan key={index} run={run} ansi={ansi} />
                  ))}
                </div>
              ))}
              {screen.cursor && screen.display_offset === 0 && (
                <div
                  class={`terminal-cursor ${screen.cursor.shape} ${focused ? "focused" : ""}`}
                  style={{
                    top: `${screen.cursor.row * rowHeight}px`,
                    left: `${screen.cursor.column * charWidth}px`,
                    width: `${charWidth}px`,
                    height: `${rowHeight}px`,
                  }}
                />
              )}
              <textarea
                ref={inputRef}
                class="terminal-input"
                autocapitalize="off"
                autocomplete="off"
                autocorrect="off"
                spellcheck={false}
                aria-label="Terminal input"
                style={
                  screen.cursor
                    ? { top: `${screen.cursor.row * rowHeight}px`, left: `${screen.cursor.column * charWidth}px` }
                    : undefined
                }
                onKeyDown={onKeyDown}
                onInput={onInput}
                onCompositionEnd={onCompositionEnd}
                onPaste={(event) => {
                  event.preventDefault();
                  paste(event.clipboardData?.getData("text") ?? "");
                }}
                onFocus={() => setFocused(true)}
                onBlur={() => setFocused(false)}
              />
            </div>
          </div>
        )}
        {screen && screen.display_offset > 0 && (
          <button
            class="jump-latest pressable"
            onClick={() =>
              conn
                .request("terminal.scroll", {
                  workspace_id: target.workspaceId,
                  item_id: target.itemId,
                  lines: -screen.display_offset,
                })
                .catch(reportError)
            }
          >
            <Icon name="arrow-down" size={16} />
            Live
          </button>
        )}
        {!focused && !keyboard && screen && (
          <button class="terminal-keyboard-hint pressable" onClick={focusInput}>
            <Icon name="keyboard" size={18} />
            Type
          </button>
        )}
        <CloseButton />
      </div>
      <div class="accessory-bar terminal-accessory">
        {accessoryKey("esc", () => send("\x1b"), "Escape")}
        {accessoryKey("tab", () => send("\t"), "Tab")}
        {accessoryKey("ctrl", () => setCtrl(!ctrl), "Control", ctrl ? "sticky-on" : "")}
        {accessoryKey(<Icon name="arrow-left" size={18} />, () => arrow("ArrowLeft"), "Left")}
        {accessoryKey(<Icon name="arrow-up" size={18} />, () => arrow("ArrowUp"), "Up")}
        {accessoryKey(<Icon name="arrow-down" size={18} />, () => arrow("ArrowDown"), "Down")}
        {accessoryKey(<Icon name="arrow-right" size={18} />, () => arrow("ArrowRight"), "Right")}
        {["|", "~", "/", "-"].map((symbol) => (
          <span key={symbol}>{accessoryKey(<span class="accessory-symbol">{symbol}</span>, () => sendText(symbol), symbol)}</span>
        ))}
        {accessoryKey(
          <Icon name="clipboard" size={18} />,
          () => {
            navigator.clipboard?.readText().then(paste, () => toast("Couldn't read the clipboard", "error"));
          },
          "Paste",
        )}
        {accessoryKey(
          <Icon name="keyboard" size={18} />,
          () => (focused ? inputRef.current?.blur() : focusInput()),
          focused ? "Hide keyboard" : "Show keyboard",
        )}
      </div>
    </div>
  );
}

function TerminalSpan({ run, ansi }: { run: TerminalRun; ansi: string[] }) {
  const style: Record<string, string> = {};
  const fg = terminalColor(run.fg, ansi);
  const bg = terminalColor(run.bg, ansi);
  if (fg) style.color = fg;
  if (bg) style.backgroundColor = bg;
  if (run.bold) style.fontWeight = "bold";
  if (run.italic) style.fontStyle = "italic";
  if (run.dim) style.opacity = "0.6";
  const decorations = [run.underline && "underline", run.strikethrough && "line-through"].filter(Boolean);
  if (decorations.length) style.textDecoration = decorations.join(" ");
  return <span style={style}>{run.text}</span>;
}
