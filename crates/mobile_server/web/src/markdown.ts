import DOMPurify from "dompurify";
import { Marked, type Tokens } from "marked";

// ---------------------------------------------------------------------------
// Code highlighting: a small language-agnostic tokenizer (comments, strings,
// numbers, keywords). Good enough for chat code blocks; editors get real
// highlighting from Zed.

const KEYWORDS = new Set(
  (
    "fn let mut pub use mod impl struct enum trait type where match if else for while loop return break " +
    "continue async await move ref self Self super crate const static unsafe extern dyn in as true false " +
    "function var class extends new this import export from default interface implements public private " +
    "protected readonly void null undefined typeof instanceof try catch finally throw yield delete def " +
    "lambda pass None True False and or not is elif with global nonlocal package func go chan defer select " +
    "switch case struct map range nil echo then fi do done esac local"
  ).split(" "),
);

const TOKEN_PATTERN =
  /(\/\/[^\n]*|#(?![\[!])[^\n]*|\/\*[\s\S]*?\*\/|--[^\n]*)|("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\\n])*'|`(?:\\.|[^`\\])*`)|(\b\d[\d_]*(?:\.\d+)?(?:e[+-]?\d+)?\b|\b0x[\da-f]+\b)|([A-Za-z_][\w$]*)/gi;

const COMMENT_LANGS_WITH_HASH = /^(py|python|sh|bash|zsh|shell|rb|ruby|toml|yaml|yml|perl|r|make|makefile|dockerfile|conf|ini|fish|nix)$/i;
const COMMENT_LANGS_WITH_DASHES = /^(sql|lua|haskell|hs|elm)$/i;

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}

export function highlightCode(code: string, lang: string): string {
  if (code.length > 50_000) return escapeHtml(code);
  const hashComments = COMMENT_LANGS_WITH_HASH.test(lang);
  const dashComments = COMMENT_LANGS_WITH_DASHES.test(lang);
  let html = "";
  let last = 0;
  TOKEN_PATTERN.lastIndex = 0;
  for (let match = TOKEN_PATTERN.exec(code); match; match = TOKEN_PATTERN.exec(code)) {
    const [text, comment, string, number, word] = match;
    let className: string | null = null;
    if (comment) {
      if (comment.startsWith("#") && !hashComments) {
        // `#` isn't a comment here (e.g. Rust attributes, CSS colors).
        TOKEN_PATTERN.lastIndex = match.index + 1;
        continue;
      }
      if (comment.startsWith("--") && !dashComments) {
        TOKEN_PATTERN.lastIndex = match.index + 2;
        continue;
      }
      className = "tk-comment";
    } else if (string) className = "tk-string";
    else if (number) className = "tk-number";
    else if (word) {
      if (KEYWORDS.has(word)) className = "tk-keyword";
      else if (code[match.index + word.length] === "(") className = "tk-function";
      else if (/^[A-Z][a-z]/.test(word)) className = "tk-type";
    }
    if (!className) continue;
    html += escapeHtml(code.slice(last, match.index));
    html += `<span class="${className}">${escapeHtml(text)}</span>`;
    last = match.index + text.length;
  }
  return html + escapeHtml(code.slice(last));
}

// ---------------------------------------------------------------------------
// Markdown

const marked = new Marked({
  gfm: true,
  breaks: false,
  renderer: {
    code({ text, lang }: Tokens.Code) {
      const language = (lang ?? "").trim().split(/\s+/)[0] ?? "";
      const label = language ? escapeHtml(language) : "";
      return (
        `<div class="code-block"><div class="code-head"><span>${label}</span>` +
        `<button type="button" class="code-copy" data-copy>Copy</button></div>` +
        `<pre><code>${highlightCode(text, language)}</code></pre></div>`
      );
    },
  },
});

DOMPurify.addHook("afterSanitizeAttributes", (node) => {
  if (node.tagName === "A") {
    node.setAttribute("target", "_blank");
    node.setAttribute("rel", "noopener noreferrer");
  }
});

const SANITIZE_OPTIONS = {
  ADD_ATTR: ["target", "data-copy"],
  FORBID_TAGS: ["style", "form", "input", "textarea", "select", "iframe"],
};

class LruCache {
  private map = new Map<string, string>();
  constructor(private readonly capacity: number) {}
  get(key: string): string | undefined {
    const value = this.map.get(key);
    if (value !== undefined) {
      this.map.delete(key);
      this.map.set(key, value);
    }
    return value;
  }
  set(key: string, value: string) {
    this.map.set(key, value);
    if (this.map.size > this.capacity) {
      const oldest = this.map.keys().next().value;
      if (oldest !== undefined) this.map.delete(oldest);
    }
  }
}

const blockCache = new LruCache(400);
const inlineCache = new LruCache(400);

export function renderMarkdown(source: string): string {
  const cached = blockCache.get(source);
  if (cached !== undefined) return cached;
  let html: string;
  try {
    html = DOMPurify.sanitize(marked.parse(source, { async: false }) as string, SANITIZE_OPTIONS);
  } catch {
    html = `<p>${escapeHtml(source)}</p>`;
  }
  blockCache.set(source, html);
  return html;
}

export function renderInlineMarkdown(source: string): string {
  const cached = inlineCache.get(source);
  if (cached !== undefined) return cached;
  let html: string;
  try {
    html = DOMPurify.sanitize(marked.parseInline(source, { async: false }) as string, SANITIZE_OPTIONS);
  } catch {
    html = escapeHtml(source);
  }
  inlineCache.set(source, html);
  return html;
}

/** Click handler for rendered markdown (copy buttons on code blocks). */
export function handleMarkdownClick(event: MouseEvent) {
  const target = event.target as HTMLElement | null;
  const button = target?.closest<HTMLElement>("[data-copy]");
  if (!button) return;
  event.preventDefault();
  const code = button.closest(".code-block")?.querySelector("code")?.textContent ?? "";
  navigator.clipboard?.writeText(code).then(
    () => {
      button.textContent = "Copied";
      setTimeout(() => (button.textContent = "Copy"), 1500);
    },
    () => {
      button.textContent = "Failed";
    },
  );
}

// ---------------------------------------------------------------------------
// Line diff for tool call cards

export type DiffLine = { type: "context" | "added" | "removed"; text: string; oldLine?: number; newLine?: number };
export type DiffRow = DiffLine | { type: "gap"; count: number };

function splitLines(text: string): string[] {
  if (text === "") return [];
  const lines = text.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  return lines;
}

/** Line-level diff (LCS on the region between the common prefix and suffix). */
export function diffLines(oldText: string, newText: string): DiffLine[] {
  const a = splitLines(oldText);
  const b = splitLines(newText);
  let prefix = 0;
  while (prefix < a.length && prefix < b.length && a[prefix] === b[prefix]) prefix++;
  let suffix = 0;
  while (
    suffix < a.length - prefix &&
    suffix < b.length - prefix &&
    a[a.length - 1 - suffix] === b[b.length - 1 - suffix]
  ) {
    suffix++;
  }
  const middleA = a.slice(prefix, a.length - suffix);
  const middleB = b.slice(prefix, b.length - suffix);
  const lines: DiffLine[] = [];
  let oldLine = 1;
  let newLine = 1;
  const context = (text: string) => lines.push({ type: "context", text, oldLine: oldLine++, newLine: newLine++ });
  const removed = (text: string) => lines.push({ type: "removed", text, oldLine: oldLine++ });
  const added = (text: string) => lines.push({ type: "added", text, newLine: newLine++ });

  a.slice(0, prefix).forEach(context);
  const n = middleA.length;
  const m = middleB.length;
  if (n * m > 4_000_000) {
    middleA.forEach(removed);
    middleB.forEach(added);
  } else if (n > 0 || m > 0) {
    // lengths[i][j] = LCS length of middleA[i..] and middleB[j..]
    const width = m + 1;
    const lengths = new Uint32Array((n + 1) * width);
    for (let i = n - 1; i >= 0; i--) {
      for (let j = m - 1; j >= 0; j--) {
        lengths[i * width + j] =
          middleA[i] === middleB[j]
            ? lengths[(i + 1) * width + j + 1] + 1
            : Math.max(lengths[(i + 1) * width + j], lengths[i * width + j + 1]);
      }
    }
    let i = 0;
    let j = 0;
    while (i < n && j < m) {
      if (middleA[i] === middleB[j]) {
        context(middleA[i]);
        i++;
        j++;
      } else if (lengths[(i + 1) * width + j] >= lengths[i * width + j + 1]) {
        removed(middleA[i++]);
      } else {
        added(middleB[j++]);
      }
    }
    while (i < n) removed(middleA[i++]);
    while (j < m) added(middleB[j++]);
  }
  a.slice(a.length - suffix).forEach(context);
  return lines;
}

/** Collapses unchanged runs to `context` lines around each change. */
export function compactDiff(lines: DiffLine[], context = 3): DiffRow[] {
  const keep = new Uint8Array(lines.length);
  lines.forEach((line, index) => {
    if (line.type === "context") return;
    for (let k = Math.max(0, index - context); k <= Math.min(lines.length - 1, index + context); k++) keep[k] = 1;
  });
  const rows: DiffRow[] = [];
  let skipped = 0;
  lines.forEach((line, index) => {
    if (keep[index]) {
      if (skipped) rows.push({ type: "gap", count: skipped });
      skipped = 0;
      rows.push(line);
    } else {
      skipped++;
    }
  });
  if (skipped) rows.push({ type: "gap", count: skipped });
  return rows;
}
