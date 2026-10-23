import { computed, signal } from "@preact/signals";
import type { ComponentChildren, JSX } from "preact";
import { createPortal } from "preact/compat";
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { closeOverlay } from "./state";

// ---------------------------------------------------------------------------
// Icons (24×24 stroke icons)

function circle(cx: number, cy: number, r: number): string {
  return `M${cx - r} ${cy}a${r} ${r} 0 1 0 ${2 * r} 0a${r} ${r} 0 1 0 ${-2 * r} 0`;
}

function gear(): string {
  const teeth = 8;
  const outer = 10;
  const inner = 7.6;
  const points: string[] = [];
  for (let i = 0; i < teeth * 4; i++) {
    const angle = (i / (teeth * 4)) * Math.PI * 2 - Math.PI / (teeth * 4);
    const radius = i % 4 < 2 ? outer : inner;
    points.push(`${(12 + radius * Math.cos(angle)).toFixed(2)} ${(12 + radius * Math.sin(angle)).toFixed(2)}`);
  }
  return `M${points.join("L")}Z${circle(12, 12, 3)}`;
}

const ICONS = {
  "chevron-left": "M15 18l-6-6 6-6",
  "chevron-right": "M9 18l6-6-6-6",
  "chevron-down": "M6 9l6 6 6-6",
  "chevron-up": "M18 15l-6-6-6 6",
  plus: "M12 5v14M5 12h14",
  x: "M18 6L6 18M6 6l12 12",
  check: "M20 6L9 17l-5-5",
  gear: gear(),
  "arrow-up": "M12 19V5M5 12l7-7 7 7",
  "arrow-down": "M12 5v14M19 12l-7 7-7-7",
  "arrow-left": "M19 12H5M12 19l-7-7 7-7",
  "arrow-right": "M5 12h14M12 5l7 7-7 7",
  terminal: "M4 17l6-6-6-6M12 19h8",
  file: "M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8zM14 2v6h6",
  folder: "M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z",
  diff: "M12 3v12M6 9h12M6 21h12",
  commit: `${circle(12, 12, 3.5)}M2 12h6.5M15.5 12H22`,
  branch: `M6 3v12${circle(18, 6, 3)}${circle(6, 18, 3)}M18 9a9 9 0 0 1-9 9`,
  server: "M4 4h16v6H4zM4 14h16v6H4zM8 7h.01M8 17h.01",
  bell: "M18 8a6 6 0 0 0-12 0c0 7-3 9-3 9h18s-3-2-3-9M13.73 21a2 2 0 0 1-3.46 0",
  search: `${circle(11, 11, 7)}M21 21l-5-5`,
  undo: "M3 7v6h6M21 17a9 9 0 0 0-15-6.7L3 13",
  redo: "M21 7v6h-6M3 17a9 9 0 0 1 15-6.7l3 2.7",
  save: "M19 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11l5 5v11a2 2 0 0 1-2 2zM17 21v-8H7v8M7 3v5h8",
  wand: "M15 4V2M15 16v-2M8 9h2M20 9h2M17.8 11.8L19 13M17.8 6.2L19 5M3 21l9-9M12.2 6.2L11 5",
  info: `${circle(12, 12, 10)}M12 16v-4M12 8h.01`,
  definition: "M15 10l5 5-5 5M4 4v7a4 4 0 0 0 4 4h12",
  indent: "M3 8l4 4-4 4M21 12H11M21 6H11M21 18H11",
  outdent: "M7 8l-4 4 4 4M21 12H11M21 6H11M21 18H11",
  keyboard: "M4 6h16a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2zM6 10h.01M10 10h.01M14 10h.01M18 10h.01M8 14h8",
  copy: "M11 9h9a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2h-9a2 2 0 0 1-2-2v-9a2 2 0 0 1 2-2zM5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1",
  bulb: "M9 18h6M10 22h4M12 2a7 7 0 0 0-4 12.7V17h8v-2.3A7 7 0 0 0 12 2z",
  eye: `M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z${circle(12, 12, 3)}`,
  pencil: "M17 3a2.83 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5z",
  trash: "M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2",
  move: "M5 9l-3 3 3 3M9 5l3-3 3 3M15 19l-3 3-3-3M19 9l3 3-3 3M2 12h20M12 2v20",
  globe: `${circle(12, 12, 10)}M2 12h20M12 2a15 15 0 0 1 4 10 15 15 0 0 1-4 10 15 15 0 0 1-4-10 15 15 0 0 1 4-10z`,
  shuffle: "M16 3h5v5M4 20L21 3M21 16v5h-5M15 15l6 6M4 4l5 5",
  wrench:
    "M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z",
  alert: "M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0zM12 9v4M12 17h.01",
  list: "M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01",
  history: "M3 3v5h5M3.05 13A9 9 0 1 0 6 5.3L3 8M12 7v5l4 2",
  layers: "M12 2l10 6.5-10 6.5L2 8.5zM2 15.5l10 6.5 10-6.5",
  slash: `${circle(12, 12, 10)}M4.93 4.93l14.14 14.14`,
  circle: circle(12, 12, 9),
  "x-circle": `${circle(12, 12, 10)}M15 9l-6 6M9 9l6 6`,
  "check-circle": `${circle(12, 12, 10)}M8 12l3 3 5-6`,
  message: "M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z",
  monitor: "M4 3h16a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2zM8 21h8M12 17v4",
  logout: "M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4M16 17l5-5-5-5M21 12H9",
  archive: "M21 8v13H3V8M1 3h22v5H1zM10 12h4",
  clipboard: "M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2M9 2h6v4H9z",
  sparkle: "M12 3l1.9 5.8L20 11l-6.1 2.2L12 19l-1.9-5.8L4 11l6.1-2.2z",
  bot: "M12 8V4H8M4 8h16a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2v-8a2 2 0 0 1 2-2zM2 14h0M22 14h0M9 13v2M15 13v2",
  zap: "M13 2L3 14h9l-1 8 10-12h-9z",
  stop: "",
  spinner: "",
} as const;

export type IconName = keyof typeof ICONS;

export function Icon({ name, size = 20, class: className }: { name: IconName; size?: number; class?: string }) {
  if (name === "stop") {
    return (
      <svg class={`icon ${className ?? ""}`} width={size} height={size} viewBox="0 0 24 24" aria-hidden="true">
        <rect x="6" y="6" width="12" height="12" rx="2.5" fill="currentColor" />
      </svg>
    );
  }
  if (name === "spinner") return <Spinner size={size} />;
  return (
    <svg
      class={`icon ${className ?? ""}`}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
    >
      <path d={ICONS[name]} />
    </svg>
  );
}

export function Spinner({ size = 16 }: { size?: number }) {
  return (
    <svg class="spinner" width={size} height={size} viewBox="0 0 24 24" aria-label="Loading">
      <circle cx="12" cy="12" r="9" fill="none" stroke="currentColor" stroke-width="2.5" opacity="0.25" />
      <path d="M21 12a9 9 0 0 0-9-9" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" />
    </svg>
  );
}

// ---------------------------------------------------------------------------
// Device & viewport

export const isIOS =
  /iPad|iPhone|iPod/.test(navigator.userAgent) ||
  (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);

export function isStandalone(): boolean {
  return (
    (navigator as Navigator & { standalone?: boolean }).standalone === true ||
    window.matchMedia("(display-mode: standalone)").matches
  );
}

export function defaultDeviceName(): string {
  const ua = navigator.userAgent;
  if (/iPad/.test(ua) || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1)) return "iPad";
  if (/iPhone/.test(ua)) return "iPhone";
  if (/Android/.test(ua)) return /Mobile/.test(ua) ? "Android phone" : "Android tablet";
  if (/Mac/.test(ua)) return "Mac";
  if (/Windows/.test(ua)) return "Windows PC";
  return "Phone";
}

/** Height of the on-screen keyboard (0 when hidden). */
export const keyboardHeight = signal(0);
export const keyboardOpen = computed(() => keyboardHeight.value > 80);

// iOS doesn't resize the layout viewport for the keyboard; size the app to the
// visual viewport instead so composers and editors stay above it.
export function initViewport() {
  const root = document.documentElement;
  const viewport = window.visualViewport;
  const update = () => {
    const height = viewport ? viewport.height : window.innerHeight;
    const offset = viewport ? viewport.offsetTop : 0;
    const zoomed = viewport ? viewport.scale > 1.01 : false;
    if (!zoomed) {
      root.style.setProperty("--app-height", `${Math.round(height)}px`);
      root.style.setProperty("--app-offset", `${Math.round(offset)}px`);
    }
    const keyboard = zoomed ? keyboardHeight.peek() : Math.max(0, Math.round(window.innerHeight - height));
    keyboardHeight.value = keyboard;
    root.classList.toggle("keyboard-open", keyboard > 80);
  };
  update();
  viewport?.addEventListener("resize", update);
  viewport?.addEventListener("scroll", update);
  window.addEventListener("resize", update);
  // Lets `:active` styles apply on iOS.
  document.addEventListener("touchstart", () => {}, { passive: true });
}

// iOS 18+ plays a haptic tick when a `<input switch>` is toggled through its
// label; elsewhere use the Vibration API when available.
let hapticLabel: HTMLLabelElement | null = null;
export function haptic() {
  try {
    if (typeof navigator.vibrate === "function" && !isIOS) {
      navigator.vibrate(8);
      return;
    }
    if (!isIOS) return;
    if (!hapticLabel) {
      const input = document.createElement("input");
      input.type = "checkbox";
      input.setAttribute("switch", "");
      input.id = "haptic-switch";
      input.style.display = "none";
      hapticLabel = document.createElement("label");
      hapticLabel.htmlFor = input.id;
      hapticLabel.style.display = "none";
      document.body.append(input, hapticLabel);
    }
    hapticLabel.click();
  } catch {
    // Purely cosmetic.
  }
}

// ---------------------------------------------------------------------------
// Formatting

export function relativeTime(date: Date): string {
  const seconds = Math.round((Date.now() - date.getTime()) / 1000);
  if (Number.isNaN(seconds)) return "";
  if (seconds < 45) return "now";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.round(hours / 24);
  if (days < 7) return `${days}d`;
  if (days < 365) return date.toLocaleDateString(undefined, { month: "short", day: "numeric" });
  return date.toLocaleDateString(undefined, { year: "numeric", month: "short" });
}

export function parseDate(value: string | number | null | undefined): Date | null {
  if (value === null || value === undefined || value === "") return null;
  const date = typeof value === "number" ? new Date(value * 1000) : new Date(/^\d+$/.test(value) ? Number(value) * 1000 : value);
  return Number.isNaN(date.getTime()) ? null : date;
}

export function formatTokens(count: number): string {
  if (count >= 1_000_000) return `${(count / 1_000_000).toFixed(1).replace(/\.0$/, "")}M`;
  if (count >= 1000) return `${Math.round(count / 1000)}k`;
  return String(count);
}

export function splitPath(path: string): { dir: string; name: string } {
  const index = path.lastIndexOf("/");
  return index < 0 ? { dir: "", name: path } : { dir: path.slice(0, index), name: path.slice(index + 1) };
}

// ---------------------------------------------------------------------------
// Layers: sheets and modals render into `#layers` so they sit above every
// panel regardless of ancestors' transforms.

function Layer({ children }: { children: ComponentChildren }) {
  const host = document.getElementById("layers") ?? document.body;
  return createPortal(children as JSX.Element, host);
}

/** Keeps children mounted during the exit animation. */
function usePresence(open: boolean, duration = 240): { mounted: boolean; closing: boolean } {
  const [mounted, setMounted] = useState(open);
  const [closing, setClosing] = useState(false);
  useEffect(() => {
    if (open) {
      setMounted(true);
      setClosing(false);
      return;
    }
    if (!mounted) return;
    setClosing(true);
    const timer = setTimeout(() => {
      setMounted(false);
      setClosing(false);
    }, duration);
    return () => clearTimeout(timer);
  }, [open]);
  return { mounted, closing };
}

export function Sheet({
  open,
  onClose,
  title,
  children,
  class: className,
}: {
  open: boolean;
  onClose: () => void;
  title?: ComponentChildren;
  children: ComponentChildren;
  class?: string;
}) {
  const { mounted, closing } = usePresence(open);
  const panelRef = useRef<HTMLDivElement>(null);
  const drag = useRef<{ startY: number; dy: number } | null>(null);

  useEffect(() => {
    if (!open) return;
    const onKey = (event: KeyboardEvent) => event.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!mounted) return null;

  // Drag the grabber/header down to dismiss.
  const onTouchStart = (event: TouchEvent) => {
    drag.current = { startY: event.touches[0].clientY, dy: 0 };
  };
  const onTouchMove = (event: TouchEvent) => {
    if (!drag.current || !panelRef.current) return;
    const dy = Math.max(0, event.touches[0].clientY - drag.current.startY);
    drag.current.dy = dy;
    panelRef.current.style.transition = "none";
    panelRef.current.style.transform = `translateY(${dy}px)`;
  };
  const onTouchEnd = () => {
    const panel = panelRef.current;
    const dy = drag.current?.dy ?? 0;
    drag.current = null;
    if (!panel) return;
    panel.style.transition = "";
    panel.style.transform = "";
    if (dy > 90) onClose();
  };

  return (
    <Layer>
      <div class={`sheet-root ${closing ? "closing" : ""}`}>
        <div class="sheet-backdrop" onClick={onClose} />
        <div class={`sheet ${className ?? ""}`} ref={panelRef} role="dialog" aria-modal="true">
          <div class="sheet-grab" onTouchStart={onTouchStart} onTouchMove={onTouchMove} onTouchEnd={onTouchEnd}>
            <div class="sheet-handle" />
            {title && <div class="sheet-title">{title}</div>}
          </div>
          <div class="sheet-body">{children}</div>
        </div>
      </div>
    </Layer>
  );
}

export function Modal({
  open,
  onClose,
  title,
  children,
  headerRight,
  class: className,
}: {
  open: boolean;
  onClose: () => void;
  title: ComponentChildren;
  children: ComponentChildren;
  headerRight?: ComponentChildren;
  class?: string;
}) {
  const { mounted, closing } = usePresence(open, 280);
  if (!mounted) return null;
  return (
    <Layer>
      <div class={`modal ${closing ? "closing" : ""} ${className ?? ""}`} role="dialog" aria-modal="true">
        <header class="modal-header">
          <button class="header-button" onClick={onClose} aria-label="Close">
            <Icon name="x" size={22} />
          </button>
          <div class="modal-title">{title}</div>
          <div class="modal-header-right">{headerRight}</div>
        </header>
        <div class="modal-body">{children}</div>
      </div>
    </Layer>
  );
}

// ---------------------------------------------------------------------------
// Picker

export interface PickerOption {
  value: string;
  label: string;
  group?: string | null;
  description?: string | null;
  icon?: IconName;
  badge?: string | null;
}

export function PickerSheet({
  open,
  title,
  options,
  value,
  onSelect,
  onClose,
}: {
  open: boolean;
  title: string;
  options: PickerOption[];
  value: string | null;
  onSelect: (value: string) => void;
  onClose: () => void;
}) {
  const groups: { name: string | null; options: PickerOption[] }[] = [];
  for (const option of options) {
    const name = option.group ?? null;
    let group = groups.find((candidate) => candidate.name === name);
    if (!group) {
      group = { name, options: [] };
      groups.push(group);
    }
    group.options.push(option);
  }
  return (
    <Sheet open={open} onClose={onClose} title={title}>
      <div class="picker">
        {groups.map((group) => (
          <div class="picker-group" key={group.name ?? ""}>
            {group.name && <div class="picker-group-name">{group.name}</div>}
            <div class="picker-options">
              {group.options.map((option) => {
                const selected = option.value === value;
                return (
                  <button
                    type="button"
                    key={option.value}
                    class={`picker-option pressable ${selected ? "selected" : ""}`}
                    onClick={() => {
                      haptic();
                      onSelect(option.value);
                      onClose();
                    }}
                  >
                    {option.icon && <Icon name={option.icon} size={18} class="picker-option-icon" />}
                    <div class="picker-option-text">
                      <div class="picker-option-label">
                        {option.label}
                        {option.badge && <span class="badge">{option.badge}</span>}
                      </div>
                      {option.description && <div class="picker-option-description">{option.description}</div>}
                    </div>
                    {selected && <Icon name="check" size={18} class="picker-check" />}
                  </button>
                );
              })}
            </div>
          </div>
        ))}
      </div>
    </Sheet>
  );
}

export function PickerRow({
  label,
  title,
  options,
  value,
  onChange,
  disabled,
  placeholder = "Choose…",
}: {
  label: string;
  title?: string;
  options: PickerOption[];
  value: string | null;
  onChange: (value: string) => void;
  disabled?: boolean;
  placeholder?: string;
}) {
  const [open, setOpen] = useState(false);
  const selected = options.find((option) => option.value === value);
  return (
    <>
      <button type="button" class="form-row pressable" disabled={disabled} onClick={() => setOpen(true)}>
        <span class="form-row-label">{label}</span>
        <span class={`form-row-value ${selected ? "" : "placeholder"}`}>
          {selected?.label ?? placeholder}
          {selected?.badge && <span class="badge">{selected.badge}</span>}
        </span>
        <Icon name="chevron-right" size={18} class="form-row-chevron" />
      </button>
      <PickerSheet
        open={open}
        title={title ?? label}
        options={options}
        value={value}
        onSelect={onChange}
        onClose={() => setOpen(false)}
      />
    </>
  );
}

export function Switch({ checked, onChange, label }: { checked: boolean; onChange: (value: boolean) => void; label?: string }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      class={`switch ${checked ? "on" : ""}`}
      onClick={() => {
        haptic();
        onChange(!checked);
      }}
    >
      <span class="switch-thumb" />
    </button>
  );
}

/** Calls `callback` once the element scrolls into view (for lazy loading). */
export function useInView<T extends Element>(
  callback: () => void,
  { rootMargin = "400px", enabled = true }: { rootMargin?: string; enabled?: boolean } = {},
) {
  const ref = useRef<T>(null);
  const callbackRef = useRef(callback);
  callbackRef.current = callback;
  useLayoutEffect(() => {
    const element = ref.current;
    if (!element || !enabled) return;
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) callbackRef.current();
      },
      { rootMargin },
    );
    observer.observe(element);
    return () => observer.disconnect();
  }, [enabled, rootMargin]);
  return ref;
}

/** The floating exit button of full-screen item views. */
export function CloseButton() {
  return (
    <button
      class="overlay-close pressable"
      aria-label="Close"
      onPointerDown={(event) => event.preventDefault()}
      onClick={() => {
        haptic();
        closeOverlay();
      }}
    >
      <Icon name="x" size={24} />
    </button>
  );
}

export function EmptyState({ icon, title, children }: { icon: IconName; title: string; children?: ComponentChildren }) {
  return (
    <div class="empty-state">
      <div class="empty-icon">
        <Icon name={icon} size={28} />
      </div>
      <div class="empty-title">{title}</div>
      {children && <div class="empty-body">{children}</div>}
    </div>
  );
}
