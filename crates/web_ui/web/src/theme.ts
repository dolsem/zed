import type { Theme } from "./protocol";

const THEME_CACHE_KEY = "zed-mobile.theme";

const MONO_FALLBACK = `ui-monospace, "SF Mono", SFMono-Regular, Menlo, Consolas, monospace`;
const UI_FALLBACK = `-apple-system, BlinkMacSystemFont, "SF Pro Text", system-ui, "Segoe UI", Roboto, sans-serif`;

/** Used until Zed sends its theme (One Dark-like). */
export const DEFAULT_THEME: Theme = {
  appearance: "dark",
  colors: {
    background: "#1e2127ff",
    surface: "#21252bff",
    elevated_surface: "#2c313aff",
    border: "#3a3f4bff",
    text: "#dce0e5ff",
    text_muted: "#8b929eff",
    text_accent: "#74ade8ff",
    accent: "#74ade8ff",
    error: "#e06c75ff",
    warning: "#dec184ff",
    success: "#98c379ff",
    info: "#74ade8ff",
    editor_background: "#282c33ff",
    editor_foreground: "#dce0e5ff",
    editor_line_number: "#5c6370ff",
    editor_active_line: "#2f343eff",
    editor_selection: "#74ade83d",
    editor_cursor: "#74ade8ff",
    diff_added: "#98c379ff",
    diff_deleted: "#e06c75ff",
    diff_modified: "#dec184ff",
    diff_added_background: "#98c37926",
    diff_deleted_background: "#e06c7526",
    terminal_background: "#1e2127ff",
    terminal_foreground: "#dce0e5ff",
    terminal_cursor: "#74ade8ff",
  },
  syntax: [],
  terminal_ansi: [
    "#282c34", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
    "#5c6370", "#ea858b", "#aad581", "#ffd885", "#85c1ff", "#d398eb", "#6ed5de", "#ffffff",
  ],
  buffer_font_family: "",
  ui_font_family: "",
};

export function loadCachedTheme(): Theme | null {
  try {
    const raw = localStorage.getItem(THEME_CACHE_KEY);
    return raw ? (JSON.parse(raw) as Theme) : null;
  } catch {
    return null;
  }
}

function cacheTheme(theme: Theme) {
  try {
    localStorage.setItem(THEME_CACHE_KEY, JSON.stringify(theme));
  } catch {
    // Storage full or disabled.
  }
}

/** Parses `#rgb`, `#rrggbb` or `#rrggbbaa` into 0..1 channels. */
export function parseColor(color: string): [number, number, number, number] | null {
  const match = /^#([0-9a-f]{3,8})$/i.exec(color.trim());
  if (!match) return null;
  let hex = match[1];
  if (hex.length === 3 || hex.length === 4) hex = hex.replace(/./g, (c) => c + c);
  if (hex.length !== 6 && hex.length !== 8) return null;
  const channel = (index: number) => parseInt(hex.slice(index, index + 2), 16) / 255;
  return [channel(0), channel(2), channel(4), hex.length === 8 ? channel(6) : 1];
}

function luminance(color: string): number | null {
  const parsed = parseColor(color);
  if (!parsed) return null;
  const linear = parsed.slice(0, 3).map((value) =>
    value <= 0.03928 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4,
  );
  return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
}

function quoteFamily(family: string): string {
  const trimmed = family.trim();
  if (!trimmed) return "";
  return /^[\w-]+$/.test(trimmed) ? trimmed : `"${trimmed.replace(/"/g, "")}"`;
}

let appliedColorKeys: string[] = [];

export function applyTheme(theme: Theme, { cache = true } = {}) {
  const root = document.documentElement;
  const style = root.style;
  for (const key of appliedColorKeys) style.removeProperty(`--c-${key}`);
  appliedColorKeys = [];
  for (const [key, value] of Object.entries(theme.colors)) {
    if (!/^[\w-]+$/.test(key) || typeof value !== "string") continue;
    style.setProperty(`--c-${key}`, value);
    appliedColorKeys.push(key);
  }

  const accent = theme.colors.accent ?? DEFAULT_THEME.colors.accent;
  const accentLuminance = luminance(accent);
  style.setProperty("--on-accent", accentLuminance !== null && accentLuminance > 0.45 ? "#111318" : "#ffffff");

  const bufferFont = quoteFamily(theme.buffer_font_family);
  const uiFont = quoteFamily(theme.ui_font_family);
  style.setProperty("--font-mono", bufferFont ? `${bufferFont}, ${MONO_FALLBACK}` : MONO_FALLBACK);
  style.setProperty("--font-ui", uiFont ? `${uiFont}, ${UI_FALLBACK}` : UI_FALLBACK);

  theme.terminal_ansi.forEach((color, index) => style.setProperty(`--ansi-${index}`, color));

  root.dataset.appearance = theme.appearance;
  style.colorScheme = theme.appearance;
  const background = theme.colors.background ?? DEFAULT_THEME.colors.background;
  document.querySelector('meta[name="theme-color"]')?.setAttribute("content", background.slice(0, 7));

  applySyntaxStyles(theme);
  if (cache) cacheTheme(theme);
}

function applySyntaxStyles(theme: Theme) {
  let element = document.getElementById("syntax-theme") as HTMLStyleElement | null;
  if (!element) {
    element = document.createElement("style");
    element.id = "syntax-theme";
    document.head.appendChild(element);
  }
  const safe = (value: string | null) => (value && /^[#\w(),.%\s-]+$/.test(value) ? value : null);
  const rules = theme.syntax.map((syntax, index) => {
    const declarations: string[] = [];
    const color = safe(syntax.color);
    const background = safe(syntax.background);
    if (color) declarations.push(`color:${color}`);
    if (background) declarations.push(`background-color:${background}`);
    if (syntax.italic) declarations.push("font-style:italic");
    if (syntax.bold) declarations.push("font-weight:bold");
    const decorations = [syntax.underline && "underline", syntax.strikethrough && "line-through"].filter(Boolean);
    if (decorations.length) declarations.push(`text-decoration:${decorations.join(" ")}`);
    return `.zs-${index}{${declarations.join(";")}}`;
  });
  element.textContent = rules.join("\n");
}

/** Resolves a terminal color: a CSS color, or an ANSI/256-color index. */
export function terminalColor(value: string | null, ansi: string[]): string | null {
  if (!value) return null;
  const indexMatch = /^(?:ansi:)?(\d{1,3})$/.exec(value);
  if (!indexMatch) return value;
  const index = Number(indexMatch[1]);
  if (index < 16) return ansi[index] ?? DEFAULT_THEME.terminal_ansi[index] ?? null;
  if (index < 232) {
    const cube = index - 16;
    const level = (n: number) => (n === 0 ? 0 : 55 + n * 40);
    const r = level(Math.floor(cube / 36));
    const g = level(Math.floor(cube / 6) % 6);
    const b = level(cube % 6);
    return `rgb(${r},${g},${b})`;
  }
  if (index < 256) {
    const gray = 8 + (index - 232) * 10;
    return `rgb(${gray},${gray},${gray})`;
  }
  return null;
}
