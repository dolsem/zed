// A mock of the mobile server + Zed bridge for developing the PWA without a
// backend. Serves the app (Vite dev middleware, or `dist/` with --dist) and
// implements the HTTP + WebSocket API with fake data on a single port.
//
//   npm run mock          # http://localhost:8787 (pairing code ABCD-2345)
//   npm run mock:dist     # serves the production build instead
//
// Options: --port <n>, --dist, --latency <ms> (artificial response delay)

import { ChangeSet, Text } from "@codemirror/state";
import { createECDH, createHash } from "node:crypto";
import { createReadStream, existsSync, statSync } from "node:fs";
import { createServer as createHttpServer } from "node:http";
import { dirname, extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { WebSocketServer } from "ws";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const flag = (name) => args.includes(name);
const option = (name, fallback) => {
  const index = args.indexOf(name);
  return index >= 0 && args[index + 1] ? args[index + 1] : fallback;
};
const PORT = Number(option("--port", process.env.PORT ?? 8787));
const SERVE_DIST = flag("--dist");
const LATENCY = Number(option("--latency", 0));
const PAIRING_CODE = "ABCD-2345";
const TOKEN = "mock-token-1";
const VAPID_PUBLIC_KEY = (() => {
  const ecdh = createECDH("prime256v1");
  ecdh.generateKeys();
  return ecdh.getPublicKey().toString("base64url");
})();

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const now = () => new Date().toISOString();
const log = (...parts) => console.log(`[mock ${new Date().toLocaleTimeString()}]`, ...parts);

// ---------------------------------------------------------------------------
// Theme

const SYNTAX = [
  { color: "#c678ddff" }, // 0 keyword
  { color: "#98c379ff" }, // 1 string
  { color: "#7f848eff", italic: true }, // 2 comment
  { color: "#d19a66ff" }, // 3 number
  { color: "#61afefff" }, // 4 function
  { color: "#e5c07bff" }, // 5 type
  { color: "#56b6c2ff" }, // 6 operator / punctuation
  { color: "#e06c75ff" }, // 7 property / macro
  { color: "#d19a66ff", bold: true }, // 8 heading
].map((style) => ({ color: null, background: null, italic: false, bold: false, underline: false, strikethrough: false, ...style }));

const THEME = {
  appearance: "dark",
  colors: {
    background: "#1b1d23ff",
    surface: "#21242bff",
    elevated_surface: "#2a2e37ff",
    border: "#363b46ff",
    text: "#dce0e5ff",
    text_muted: "#8b929eff",
    text_accent: "#74ade8ff",
    accent: "#74ade8ff",
    error: "#e06c75ff",
    warning: "#dec184ff",
    success: "#a1c181ff",
    info: "#74ade8ff",
    editor_background: "#23262dff",
    editor_foreground: "#dce0e5ff",
    editor_line_number: "#5d636fff",
    editor_active_line: "#2b2f37ff",
    editor_selection: "#74ade83d",
    editor_cursor: "#74ade8ff",
    diff_added: "#a1c181ff",
    diff_deleted: "#e06c75ff",
    diff_modified: "#dec184ff",
    diff_added_background: "#a1c18122",
    diff_deleted_background: "#e06c7522",
    terminal_background: "#1b1d23ff",
    terminal_foreground: "#dce0e5ff",
    terminal_cursor: "#74ade8ff",
  },
  syntax: SYNTAX,
  terminal_ansi: [
    "#282c34", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
    "#5c6370", "#ea858b", "#aad581", "#ffd885", "#85c1ff", "#d398eb", "#6ed5de", "#ffffff",
  ],
  buffer_font_family: "Zed Plex Mono",
  ui_font_family: "Zed Plex Sans",
};

// ---------------------------------------------------------------------------
// Clients

let nextClient = 1;
const clients = new Set();

function send(client, message) {
  if (client.ws.readyState === 1) client.ws.send(JSON.stringify(message));
}

function emit(client, event, data) {
  send(client, { event, data });
}

function broadcast(event, data, filter = () => true) {
  for (const client of clients) if (client.authed && filter(client)) emit(client, event, data);
}

class MethodError extends Error {}

// ---------------------------------------------------------------------------
// Threads

const WORKSPACE_ZED = 1;
const WORKSPACE_API = 2;

const projects = [
  { project_id: "p-zed", name: "zed", remote_host: null, workspace_id: WORKSPACE_ZED },
  { project_id: "p-api", name: "api-server", remote_host: "devbox", workspace_id: WORKSPACE_API },
];

const CONTROLS = () => [
  {
    id: "model",
    label: "Model",
    kind: "select",
    value: "claude-sonnet",
    options: [
      { value: "claude-opus", label: "Claude Opus", group: "Anthropic", description: "Most capable, slower" },
      { value: "claude-sonnet", label: "Claude Sonnet", group: "Anthropic", description: "Balanced speed and quality" },
      { value: "claude-haiku", label: "Claude Haiku", group: "Anthropic", description: "Fastest" },
      { value: "gpt-5", label: "GPT-5", group: "OpenAI", description: null },
      { value: "qwen", label: "Qwen 3 Coder", group: "Ollama", description: "Local" },
    ],
  },
  {
    id: "profile",
    label: "Profile",
    kind: "select",
    value: "write",
    options: [
      { value: "write", label: "Write", group: null, description: "All tools enabled" },
      { value: "ask", label: "Ask", group: null, description: "Read-only tools" },
      { value: "minimal", label: "Minimal", group: null, description: "No tools" },
    ],
  },
  { id: "thinking", label: "Thinking", kind: "toggle", value: true },
  { id: "fast", label: "Fast", kind: "toggle", value: false },
];

const SAMPLE_DIFF_OLD = `fn main() {\n    let config = load_config().unwrap();\n    run(config);\n}\n`;
const SAMPLE_DIFF_NEW = `fn main() -> anyhow::Result<()> {\n    let config = load_config()?;\n    run(config)?;\n    Ok(())\n}\n`;

function initialEntries(title) {
  return [
    { kind: "user", text: `Can you look into **${title.toLowerCase()}**? Start with \`crates/zed/src/main.rs\`.` },
    {
      kind: "assistant",
      chunks: [
        { kind: "thought", markdown: "The user wants me to investigate. I should read `main.rs` first, then check how config loading handles errors." },
        { kind: "message", markdown: "I'll start by reading the entry point to understand how startup works." },
      ],
    },
    {
      kind: "tool_call",
      id: "tc-read-1",
      title: "Read `crates/zed/src/main.rs`",
      tool_kind: "read",
      status: "completed",
      content: [{ kind: "markdown", markdown: "```rust\nfn main() {\n    let config = load_config().unwrap();\n    run(config);\n}\n```" }],
      permission_options: null,
    },
    {
      kind: "tool_call",
      id: "tc-run-1",
      title: "Run `cargo test -p zed`",
      tool_kind: "execute",
      status: "completed",
      content: [
        {
          kind: "terminal",
          command: "cargo test -p zed",
          output: "\u001b[32m   Compiling\u001b[0m zed v0.1.0\n\u001b[32m    Finished\u001b[0m test profile in 4.21s\n     Running unittests src/main.rs\n\nrunning 12 tests\ntest config::tests::parses_defaults ... ok\ntest config::tests::rejects_bad_port ... ok\n\ntest result: ok. 12 passed; 0 failed",
          exit_code: 0,
        },
      ],
      permission_options: null,
    },
    {
      kind: "tool_call",
      id: "tc-edit-1",
      title: "Edit `crates/zed/src/main.rs`",
      tool_kind: "edit",
      status: "completed",
      content: [{ kind: "diff", path: "crates/zed/src/main.rs", old_text: SAMPLE_DIFF_OLD, new_text: SAMPLE_DIFF_NEW }],
      permission_options: null,
    },
    { kind: "compaction", summary: "Earlier we discussed the startup sequence and agreed to replace `unwrap()` calls with `?`." },
    {
      kind: "assistant",
      chunks: [
        {
          kind: "message",
          markdown:
            "Here's what I found:\n\n1. `load_config()` **panics** when the file is missing.\n2. The error isn't surfaced to the user.\n\nI changed `main` to return a `Result`:\n\n```rust\nfn main() -> anyhow::Result<()> {\n    let config = load_config()?;\n    run(config)?;\n    Ok(())\n}\n```\n\n| Check | Result |\n| --- | --- |\n| Tests | ✅ 12 passed |\n| Clippy | ⚠️ 1 warning |",
        },
      ],
    },
  ];
}

let threadCounter = 0;
const threads = new Map();

function createThread({ project_id, title, status = "idle", agent = "Zed Agent", worktrees = [], kind = "agent", added = 0, removed = 0, entries }) {
  const thread_id = `t${++threadCounter}`;
  const project = projects.find((candidate) => candidate.project_id === project_id);
  const thread = {
    thread_id,
    project_id,
    kind,
    workspace_id: project.workspace_id,
    worktrees,
    lines_added: added,
    lines_removed: removed,
    has_notification: false,
    is_active: false,
    updated_at: now(),
    entries: entries ?? (kind === "agent" ? initialEntries(title) : []),
    state: {
      thread_id,
      title,
      agent_name: agent,
      status,
      controls: CONTROLS(),
      plan: [],
      queued_messages: [],
      token_usage: { used: 18_400, max: 200_000 },
      error: null,
      needs_desktop_input: false,
    },
    turn: null,
    pendingPermission: null,
  };
  threads.set(thread_id, thread);
  return thread;
}

function seedThreads() {
  const running = createThread({ project_id: "p-zed", title: "Fix startup panic when config is missing", added: 42, removed: 7 });
  running.is_active = true;
  const waiting = createThread({ project_id: "p-zed", title: "Refactor workspace serialization", status: "waiting_for_confirmation", agent: "Claude Code", added: 120, removed: 88 });
  waiting.entries.push({
    kind: "tool_call",
    id: "tc-perm-seed",
    title: "Run `rm -rf target/debug/incremental`",
    tool_kind: "execute",
    status: "waiting_for_confirmation",
    content: [{ kind: "terminal", command: "rm -rf target/debug/incremental", output: "", exit_code: null }],
    permission_options: [
      { id: "allow-once", label: "Allow", kind: "allow_once" },
      { id: "allow-always", label: "Always allow", kind: "allow_always" },
      { id: "reject", label: "Reject", kind: "reject_once" },
    ],
  });
  waiting.pendingPermission = { toolCallId: "tc-perm-seed", resolve: null };
  waiting.has_notification = true;
  createThread({ project_id: "p-zed", title: "Add mobile remote control", worktrees: ["feature/mobile"], added: 1834, removed: 12 });
  createThread({ project_id: "p-zed", title: "cargo watch", kind: "terminal" });
  const errored = createThread({ project_id: "p-api", title: "Investigate flaky integration test", status: "error", agent: "Gemini CLI" });
  errored.state.error = "Request failed: 529 Overloaded. The provider is temporarily unavailable.";
  createThread({ project_id: "p-api", title: "Write migration for users table", agent: "Zed Agent" });
  const elicit = createThread({ project_id: "p-api", title: "Configure deployment", agent: "Codex" });
  elicit.entries.push({ kind: "elicitation", message: "Which **environment** should I deploy to: `staging` or `production`?" });
  elicit.state.needs_desktop_input = true;
}
seedThreads();

function threadsList() {
  return {
    projects: projects.map((project) => ({
      project_id: project.project_id,
      name: project.name,
      remote_host: project.remote_host,
      is_active: project.project_id === "p-zed",
      threads: [...threads.values()]
        .filter((thread) => thread.project_id === project.project_id)
        .sort((a, b) => (a.updated_at < b.updated_at ? 1 : -1))
        .map((thread) => ({
          thread_id: thread.thread_id,
          kind: thread.kind,
          title: thread.state.title,
          status: thread.state.status,
          agent_name: thread.kind === "agent" ? thread.state.agent_name : null,
          worktrees: thread.worktrees,
          lines_added: thread.lines_added,
          lines_removed: thread.lines_removed,
          has_notification: thread.has_notification,
          is_active: thread.is_active,
          updated_at: thread.updated_at,
        })),
    })),
  };
}

let threadsChangedTimer = null;
function threadsChanged() {
  clearTimeout(threadsChangedTimer);
  threadsChangedTimer = setTimeout(() => broadcast("threads.changed", threadsList()), 50);
}

const watching = (thread) => (client) => client.threads.has(thread.thread_id);

function emitEntry(thread, index) {
  thread.updated_at = now();
  broadcast("thread.entry", { thread_id: thread.thread_id, index, entry: thread.entries[index] }, watching(thread));
}

function emitState(thread) {
  broadcast("thread.state", thread.state, watching(thread));
}

function setStatus(thread, status) {
  thread.state.status = status;
  emitState(thread);
  threadsChanged();
}

async function streamText(thread, index, chunkIndex, text, turn, wordsPerTick = 3, delay = 45) {
  const words = text.split(/(\s+)/);
  let shown = "";
  for (let i = 0; i < words.length; i += wordsPerTick * 2) {
    if (turn.cancelled) return;
    shown += words.slice(i, i + wordsPerTick * 2).join("");
    thread.entries[index].chunks[chunkIndex].markdown = shown;
    emitEntry(thread, index);
    await sleep(delay);
  }
}

const REPLY = `Got it. Here's my plan:\n\n- Read the relevant modules\n- Make the change behind a small helper\n- Run the tests\n\nThe key change is in \`load_config\`:\n\n\`\`\`rust\npub fn load_config(path: &Path) -> Result<Config> {\n    let text = std::fs::read_to_string(path)\n        .with_context(|| format!("reading {}", path.display()))?;\n    Ok(toml::from_str(&text)?)\n}\n\`\`\`\n\nThis way a missing file produces a readable error instead of a panic.`;

async function runTurn(thread, text) {
  const turn = { cancelled: false };
  thread.turn = turn;
  thread.state.error = null;
  setStatus(thread, "running");
  const wantsPermission = /edit|change|fix|rm/i.test(text);
  const wantsError = /error|fail/i.test(text);
  try {
    await sleep(600);
    if (turn.cancelled) return;
    const assistantIndex = thread.entries.push({ kind: "assistant", chunks: [{ kind: "thought", markdown: "" }] }) - 1;
    emitEntry(thread, assistantIndex);
    await streamText(thread, assistantIndex, 0, `The user asked: "${text.slice(0, 80)}". I should inspect the code before changing anything, then verify with tests. Let me think about edge cases: missing files, permissions, and invalid TOML.`, turn, 2, 60);
    if (turn.cancelled) return;
    thread.state.plan = [
      { content: "Read the relevant modules", status: "in_progress" },
      { content: "Implement the change", status: "pending" },
      { content: "Run the test suite", status: "pending" },
    ];
    emitState(thread);
    thread.entries[assistantIndex].chunks.push({ kind: "message", markdown: "" });
    await streamText(thread, assistantIndex, 1, REPLY, turn);
    if (turn.cancelled) return;

    const runIndex = thread.entries.push({
      kind: "tool_call",
      id: `tc-${Date.now()}`,
      title: "Run `cargo test -p config`",
      tool_kind: "execute",
      status: "in_progress",
      content: [{ kind: "terminal", command: "cargo test -p config", output: "   Compiling config v0.1.0\n", exit_code: null }],
      permission_options: null,
    }) - 1;
    emitEntry(thread, runIndex);
    await sleep(1200);
    if (turn.cancelled) return;
    Object.assign(thread.entries[runIndex], {
      status: "completed",
      content: [{ kind: "terminal", command: "cargo test -p config", output: "   Compiling config v0.1.0\n    Finished test profile in 2.3s\n\nrunning 4 tests\n....\ntest result: ok. 4 passed", exit_code: 0 }],
    });
    emitEntry(thread, runIndex);
    thread.state.plan[0].status = "completed";
    thread.state.plan[1].status = "in_progress";
    emitState(thread);

    if (wantsPermission) {
      const toolCallId = `tc-perm-${Date.now()}`;
      const editIndex = thread.entries.push({
        kind: "tool_call",
        id: toolCallId,
        title: "Edit `crates/config/src/lib.rs`",
        tool_kind: "edit",
        status: "waiting_for_confirmation",
        content: [{ kind: "diff", path: "crates/config/src/lib.rs", old_text: SAMPLE_DIFF_OLD, new_text: SAMPLE_DIFF_NEW }],
        permission_options: [
          { id: "allow-once", label: "Allow", kind: "allow_once" },
          { id: "allow-always", label: "Always allow edits", kind: "allow_always" },
          { id: "reject", label: "Reject", kind: "reject_once" },
        ],
      }) - 1;
      emitEntry(thread, editIndex);
      setStatus(thread, "waiting_for_confirmation");
      thread.has_notification = true;
      const optionId = await new Promise((resolve) => (thread.pendingPermission = { toolCallId, resolve }));
      thread.pendingPermission = null;
      if (turn.cancelled) return;
      Object.assign(thread.entries[editIndex], {
        status: optionId.startsWith("allow") ? "completed" : "rejected",
        permission_options: null,
      });
      emitEntry(thread, editIndex);
      setStatus(thread, "running");
      thread.lines_added += 4;
      thread.lines_removed += 2;
    }

    thread.state.plan.forEach((entry) => (entry.status = "completed"));
    emitState(thread);
    const finalIndex = thread.entries.push({ kind: "assistant", chunks: [{ kind: "message", markdown: "" }] }) - 1;
    await streamText(thread, finalIndex, 0, "All done. The tests pass and the startup path now reports a clear error. Let me know if you'd like me to add a regression test.", turn);
    if (turn.cancelled) return;
    if (wantsError) {
      thread.state.error = "Request failed: 529 Overloaded. The provider is temporarily unavailable.";
      thread.state.status = "error";
    } else {
      thread.state.status = "idle";
    }
    thread.state.token_usage.used = Math.min(thread.state.token_usage.max, thread.state.token_usage.used + 9_500);
    thread.has_notification = true;
    emitState(thread);
    threadsChanged();
  } finally {
    if (thread.turn === turn) thread.turn = null;
  }
  const next = thread.state.queued_messages.shift();
  if (next && !turn.cancelled) {
    emitState(thread);
    sendMessage(thread, next);
  }
}

function sendMessage(thread, text) {
  if (thread.turn) {
    thread.state.queued_messages.push(text);
    emitState(thread);
    return;
  }
  const index = thread.entries.push({ kind: "user", text }) - 1;
  emitEntry(thread, index);
  void runTurn(thread, text);
}

// ---------------------------------------------------------------------------
// Buffers

const MAIN_RS = `use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

/// Application configuration loaded from \`zed.toml\`.
#[derive(Debug, Default)]
pub struct Config {
    pub port: u16,
    pub theme: String,
    pub root: PathBuf,
}

fn load_config(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path).context("reading config")?;
    let port = text.lines().count() as u16 + 8080;
    Ok(Config { port, theme: "One Dark".into(), root: path.to_path_buf() })
}

fn run(config: Config) -> Result<()> {
    println!("listening on {}", config.port);
    let value = std::env::var("ZED_DEBUG").ok().unwrap();
    todo!("start the app with {value}")
}

fn main() -> Result<()> {
    let config = load_config(Path::new("zed.toml"))?;
    run(config)
}
`;

const MAIN_RS_BASE = MAIN_RS.replace("fn main() -> Result<()> {\n    let config = load_config(Path::new(\"zed.toml\"))?;\n    run(config)\n}", "fn main() {\n    let config = load_config(Path::new(\"zed.toml\")).unwrap();\n    run(config).unwrap();\n}").replace("/// Application configuration loaded from `zed.toml`.\n", "");

const README = `# Zed\n\nCode at the speed of thought.\n\n## Building\n\n\`\`\`sh\ncargo run --release\n\`\`\`\n\n- Fast\n- Collaborative\n- Extensible\n`;

const LIB_RS = `//! Mobile remote control.\n\npub mod bridge;\npub mod server;\n\npub fn init() {\n    bridge::start();\n}\n`;

const CARGO_TOML = `[workspace]\nmembers = ["crates/*"]\nresolver = "2"\n\n[workspace.dependencies]\nanyhow = "1.0"\nserde = { version = "1.0", features = ["derive"] }\n`;

const buffers = new Map();
let nextBuffer = 1001;

function createBuffer({ text, path, language, readOnly = false, handle }) {
  const buffer = {
    handle: handle ?? nextBuffer++,
    version: 0,
    doc: Text.of(text.split("\n")),
    path,
    language,
    read_only: readOnly,
    is_dirty: false,
    subscribers: new Set(),
    reparseTimer: null,
  };
  buffers.set(buffer.handle, buffer);
  return buffer;
}

function bufferState(buffer) {
  return {
    buffer: buffer.handle,
    version: buffer.version,
    text: buffer.doc.toString(),
    path: buffer.path,
    language: buffer.language,
    read_only: buffer.read_only,
    is_dirty: buffer.is_dirty,
    has_lsp: buffer.language === "Rust" || buffer.language === "Markdown",
    diagnostics: diagnosticsFor(buffer),
    tab_size: 4,
    hard_tabs: false,
  };
}

function diagnosticsFor(buffer) {
  if (buffer.language !== "Rust") return [];
  const text = buffer.doc.toString();
  const diagnostics = [];
  for (const match of text.matchAll(/\.unwrap\(\)/g)) {
    diagnostics.push({ from: match.index + 1, to: match.index + match[0].length, severity: "warning", message: "Avoid `unwrap()`; propagate the error with `?` instead.", source: "clippy" });
  }
  for (const match of text.matchAll(/todo!/g)) {
    diagnostics.push({ from: match.index, to: match.index + 5, severity: "error", message: "not yet implemented", source: "rustc" });
  }
  return diagnostics;
}

/** Applies changes made by Zed (or another collaborator) and broadcasts them. */
function applyServerChange(buffer, spec, clientId = "zed") {
  const changes = ChangeSet.of(spec, buffer.doc.length);
  commitUpdates(buffer, [{ client_id: clientId, changes: changes.toJSON() }]);
}

function commitUpdates(buffer, updates, { defer = false } = {}) {
  const startVersion = buffer.version;
  for (const update of updates) {
    const changes = ChangeSet.fromJSON(update.changes);
    if (changes.length !== buffer.doc.length) throw new MethodError(`change length ${changes.length} != doc length ${buffer.doc.length}`);
    buffer.doc = changes.apply(buffer.doc);
    buffer.version++;
  }
  const wasDirty = buffer.is_dirty;
  buffer.is_dirty = true;
  const subscribed = (client) => client.buffers.has(buffer.handle);
  const announce = () => {
    broadcast("buffer.updates", { buffer: buffer.handle, version: startVersion, updates }, subscribed);
    if (!wasDirty) broadcast("buffer.metadata", { buffer: buffer.handle, is_dirty: true, language: buffer.language, path: buffer.path }, subscribed);
  };
  // Always deferred (so push responses go out first) and in commit order.
  void defer;
  setImmediate(announce);
  clearTimeout(buffer.reparseTimer);
  buffer.reparseTimer = setTimeout(() => {
    broadcast("buffer.reparsed", { buffer: buffer.handle }, subscribed);
    broadcast("buffer.diagnostics", { buffer: buffer.handle, version: buffer.version, diagnostics: diagnosticsFor(buffer) }, subscribed);
  }, 250);
}

const KEYWORDS = new Set("use pub fn let mut struct enum impl trait for in if else match return as mod crate self Self super where async await move ref const static type loop while break continue true false".split(" "));
const TOKEN_PATTERN = /(\/\/[^\n]*)|("(?:\\.|[^"\\])*")|(\b\d+\b)|(#\[[^\]]*\]|[a-z_]+!)|([A-Za-z_]\w*)(\s*\()?|([{}()[\];:,.<>=&|?+\-*/]+)|(^#+ [^\n]*)/gm;

function highlight(buffer, from, to) {
  const text = buffer.doc.toString();
  from = Math.max(0, Math.min(from, text.length));
  to = Math.max(from, Math.min(to, text.length));
  // Extend to line boundaries so tokens aren't cut.
  const start = text.lastIndexOf("\n", from - 1) + 1;
  let end = text.indexOf("\n", to);
  if (end < 0) end = text.length;
  const slice = text.slice(start, end);
  const tokens = [];
  const markdown = buffer.language === "Markdown";
  TOKEN_PATTERN.lastIndex = 0;
  for (let match = TOKEN_PATTERN.exec(slice); match; match = TOKEN_PATTERN.exec(slice)) {
    const [whole, comment, string, number, macro, word, call, punct, heading] = match;
    const at = start + match.index;
    if (markdown) {
      if (heading) tokens.push([at, at + heading.length, 8]);
      else if (string) tokens.push([at, at + string.length, 1]);
      continue;
    }
    if (comment) tokens.push([at, at + comment.length, 2]);
    else if (string) tokens.push([at, at + string.length, 1]);
    else if (number) tokens.push([at, at + number.length, 3]);
    else if (macro) tokens.push([at, at + macro.length, 7]);
    else if (word) {
      if (KEYWORDS.has(word)) tokens.push([at, at + word.length, 0]);
      else if (call) tokens.push([at, at + word.length, 4]);
      else if (/^[A-Z]/.test(word)) tokens.push([at, at + word.length, 5]);
    } else if (punct) tokens.push([at, at + punct.length, 6]);
    if (whole.length === 0) TOKEN_PATTERN.lastIndex++;
  }
  return { version: buffer.version, tokens };
}

function wordAt(text, offset) {
  let start = offset;
  while (start > 0 && /[\w$]/.test(text[start - 1])) start--;
  let end = offset;
  while (end < text.length && /[\w$]/.test(text[end])) end++;
  return { start, end, word: text.slice(start, end) };
}

// ---------------------------------------------------------------------------
// Workspaces, items and git

const mainBuffer = createBuffer({ text: MAIN_RS, path: "crates/zed/src/main.rs", language: "Rust" });
const readmeBuffer = createBuffer({ text: README, path: "README.md", language: "Markdown" });
const notesBuffer = createBuffer({ text: "# Notes\n\nThis file receives a remote edit every 15 seconds.\n", path: "notes.md", language: "Markdown" });
const libBuffer = createBuffer({ text: LIB_RS, path: "crates/mobile/src/lib.rs", language: "Rust" });
const cargoBuffer = createBuffer({ text: CARGO_TOML, path: "Cargo.toml", language: "TOML" });
cargoBuffer.is_dirty = false;

let nextItem = 300;
const workspaces = new Map([
  [WORKSPACE_ZED, { workspace_id: WORKSPACE_ZED, name: "zed", items: [], terminals: [], active_item_id: 101 }],
  [WORKSPACE_API, { workspace_id: WORKSPACE_API, name: "api-server (devbox)", items: [], terminals: [], active_item_id: null }],
]);
const zed = workspaces.get(WORKSPACE_ZED);
zed.items.push(
  { item_id: 101, kind: "file", title: "main.rs", path: "crates/zed/src/main.rs", buffer: mainBuffer },
  { item_id: 102, kind: "file", title: "README.md", path: "README.md", buffer: readmeBuffer },
  { item_id: 103, kind: "file", title: "notes.md", path: "notes.md", buffer: notesBuffer },
  { item_id: 104, kind: "diff", title: "Uncommitted Changes", path: null },
  { item_id: 105, kind: "commit", title: "a1b2c3d Fix panic on startup", path: null, sha: "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678" },
  { item_id: 106, kind: "other", title: "Settings", path: null },
);

const diffFiles = [
  { path: "crates/zed/src/main.rs", status: "modified", is_staged: false, is_partially_staged: true, buffer: mainBuffer, base_text: MAIN_RS_BASE },
  { path: "crates/mobile/src/lib.rs", status: "untracked", is_staged: false, is_partially_staged: false, buffer: libBuffer, base_text: null },
  { path: "docs/legacy.md", status: "deleted", is_staged: true, is_partially_staged: false, buffer: null, base_text: "# Legacy\n\nThis document is obsolete.\nIt describes the old plugin system.\n" },
  { path: "Cargo.toml", status: "modified", is_staged: true, is_partially_staged: false, buffer: cargoBuffer, base_text: CARGO_TOML.replace('resolver = "2"\n', "") },
];

const commits = Array.from({ length: 237 }, (_, index) => {
  const subjects = ["Fix panic on startup", "Add mobile remote control", "Improve terminal rendering", "Refactor workspace serialization", "Bump dependencies", "Speed up project search", "Polish the agent panel", "Handle remote reconnects"];
  const authors = ["Denis Olsem", "Ada Lovelace", "Linus T.", "Grace Hopper"];
  const sha = createHash("sha1").update(`commit-${index}`).digest("hex");
  return {
    sha: index === 0 ? "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678" : sha,
    subject: `${subjects[index % subjects.length]}${index > 7 ? ` (#${3200 - index})` : ""}`,
    author: authors[index % authors.length],
    timestamp: Math.floor(Date.now() / 1000) - index * 5400 - 300,
    refs: index === 0 ? ["HEAD", "main", "origin/main"] : index === 12 ? ["v0.200.0"] : [],
  };
});

function workspaceItems(workspace) {
  return {
    workspace_id: workspace.workspace_id,
    name: workspace.name,
    editors: workspace.items.map((item) => ({
      item_id: item.item_id,
      kind: item.kind,
      title: item.title,
      path: item.path,
      is_dirty: item.buffer ? item.buffer.is_dirty : false,
    })),
    terminals: workspace.terminals.map((terminal) => ({ item_id: terminal.item_id, title: terminal.title, has_bell: terminal.has_bell })),
    active_item_id: workspace.active_item_id,
  };
}

function itemsChanged(workspace) {
  broadcast("workspace.items_changed", { workspace_id: workspace.workspace_id }, (client) => client.workspaces.has(workspace.workspace_id));
}

function findItem(workspaceId, itemId) {
  const workspace = workspaces.get(workspaceId);
  if (!workspace) throw new MethodError(`No workspace ${workspaceId}`);
  const item = workspace.items.find((candidate) => candidate.item_id === itemId);
  if (!item) throw new MethodError(`No item ${itemId} in workspace ${workspaceId}`);
  return { workspace, item };
}

const TREE = {
  "": [
    { name: "crates", dir: true, status: "modified" },
    { name: "docs", dir: true, status: "deleted" },
    { name: "target", dir: true, ignored: true },
    { name: "Cargo.toml", status: "modified" },
    { name: "README.md" },
    { name: "notes.md" },
  ],
  crates: [{ name: "zed", dir: true, status: "modified" }, { name: "mobile", dir: true, status: "untracked" }, { name: "editor", dir: true }],
  "crates/zed": [{ name: "src", dir: true, status: "modified" }, { name: "Cargo.toml" }],
  "crates/zed/src": [{ name: "main.rs", status: "modified" }, { name: "app.rs" }],
  "crates/mobile": [{ name: "src", dir: true, status: "untracked" }],
  "crates/mobile/src": [{ name: "lib.rs", status: "untracked" }],
  "crates/editor": [{ name: "src", dir: true }],
  "crates/editor/src": Array.from({ length: 30 }, (_, index) => ({ name: `module_${String(index).padStart(2, "0")}.rs` })),
  docs: [],
  target: [{ name: "debug", dir: true, ignored: true }],
  "target/debug": [],
};

function listDirectory(worktreeId, path) {
  if (worktreeId === undefined || worktreeId === null) {
    return { entries: [{ worktree_id: 1, path: "", name: "zed", is_dir: true, is_ignored: false, git_status: null }] };
  }
  const children = TREE[path];
  if (!children) throw new MethodError(`No such directory: ${path}`);
  return {
    entries: children.map((child) => ({
      worktree_id: worktreeId,
      path: path ? `${path}/${child.name}` : child.name,
      name: child.name,
      is_dir: !!child.dir,
      is_ignored: !!child.ignored,
      git_status: child.status ?? null,
    })),
  };
}

function generatedFile(path) {
  const name = path.split("/").pop();
  if (name.endsWith(".rs")) return `//! ${name}\n\npub fn ${name.replace(/\W/g, "_").replace(/_rs$/, "")}() -> usize {\n    let items = vec![1, 2, 3];\n    items.iter().sum()\n}\n`;
  return `# ${name}\n\nGenerated content for ${path}.\n`;
}

// ---------------------------------------------------------------------------
// Terminals

let nextTerminal = 201;
const terminals = new Map();

const PROMPT = () => [
  { text: "~/zed", fg: "4", bg: null, bold: true },
  { text: " main", fg: "#c678dd", bg: null },
  { text: " ❯ ", fg: "2", bg: null, bold: true },
];

function createTerminal(workspace, title) {
  const terminal = {
    item_id: nextTerminal++,
    workspace_id: workspace.workspace_id,
    title,
    has_bell: false,
    columns: 80,
    rows: 24,
    lines: [],
    input: "",
    history: [],
    display_offset: 0,
  };
  terminal.lines.push([{ text: "Last login: today on ttys001", fg: "8", bg: null }]);
  terminal.lines.push([...PROMPT()]);
  terminals.set(terminal.item_id, terminal);
  workspace.terminals.push(terminal);
  return terminal;
}

function terminalScreen(terminal) {
  const total = terminal.lines.length;
  const end = total - terminal.display_offset;
  const start = Math.max(0, end - terminal.rows);
  const visible = terminal.lines.slice(start, end);
  const promptLine = terminal.lines[total - 1];
  const promptLength = promptLine.reduce((sum, run) => sum + run.text.length, 0);
  return {
    item_id: terminal.item_id,
    title: terminal.title,
    columns: terminal.columns,
    rows: terminal.rows,
    lines: visible,
    cursor: terminal.display_offset === 0 ? { row: visible.length - 1, column: promptLength, shape: "block" } : null,
    display_offset: terminal.display_offset,
    history_size: Math.max(0, total - terminal.rows),
    bracketed_paste: true,
    application_cursor: false,
  };
}

function emitScreen(terminal) {
  broadcast("terminal.screen", terminalScreen(terminal), (client) => client.terminals.has(terminal.item_id));
}

function setPromptInput(terminal) {
  terminal.lines[terminal.lines.length - 1] = [...PROMPT(), { text: terminal.input, fg: null, bg: null }];
}

function runCommand(terminal, command) {
  const out = (text, fg = null, extra = {}) => terminal.lines.push([{ text, fg, bg: null, ...extra }]);
  const trimmed = command.trim();
  if (trimmed) terminal.history.push(trimmed);
  if (trimmed === "clear") {
    terminal.lines = [];
  } else if (trimmed === "ls") {
    terminal.lines.push([
      { text: "Cargo.toml  ", fg: null, bg: null },
      { text: "README.md  ", fg: null, bg: null },
      { text: "crates/  ", fg: "4", bg: null, bold: true },
      { text: "docs/  ", fg: "4", bg: null, bold: true },
      { text: "script/  ", fg: "4", bg: null, bold: true },
      { text: "target/", fg: "8", bg: null },
    ]);
  } else if (trimmed === "git status") {
    out("On branch main");
    out("Changes not staged for commit:");
    out("\tmodified:   crates/zed/src/main.rs", "1");
    out("Untracked files:");
    out("\tcrates/mobile/", "1");
  } else if (trimmed.startsWith("cargo")) {
    out("   Compiling zed v0.1.0 (/Users/me/zed/crates/zed)", "2", { bold: true });
    out("warning: unused variable: `value`", "3", { bold: true });
    out("    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.42s", "2", { bold: true });
  } else if (trimmed === "colors") {
    for (let row = 0; row < 2; row++) {
      terminal.lines.push(Array.from({ length: 8 }, (_, index) => ({ text: `  ${row * 8 + index}  `.slice(0, 6), fg: "0", bg: String(row * 8 + index) })));
    }
    terminal.lines.push([{ text: "bold", fg: null, bg: null, bold: true }, { text: " italic", fg: null, bg: null, italic: true }, { text: " underline", fg: null, bg: null, underline: true }, { text: " dim", fg: null, bg: null, dim: true }, { text: " strike", fg: null, bg: null, strikethrough: true }, { text: " 256:208", fg: "208", bg: null }]);
  } else if (trimmed === "seq") {
    for (let index = 1; index <= 100; index++) out(String(index));
  } else if (trimmed) {
    out(`zsh: command not found: ${trimmed.split(" ")[0]}`, "1");
  }
  terminal.input = "";
  terminal.lines.push([...PROMPT()]);
  if (terminal.lines.length > 2000) terminal.lines.splice(0, terminal.lines.length - 2000);
}

function terminalInput(terminal, data) {
  terminal.display_offset = 0;
  // Bracketed paste
  data = data.replace(/\x1b\[200~([\s\S]*?)\x1b\[201~/g, (_, text) => text);
  for (let i = 0; i < data.length; i++) {
    const char = data[i];
    if (char === "\x1b") {
      const rest = data.slice(i);
      const match = /^\x1b(\[[\d;]*[A-Za-z~]|O[A-D]|.)?/.exec(rest);
      const sequence = match?.[1] ?? "";
      if (sequence === "[A" || sequence === "OA") {
        terminal.input = terminal.history[terminal.history.length - 1] ?? "";
      }
      i += (match?.[0].length ?? 1) - 1;
      continue;
    }
    if (char === "\r") {
      setPromptInput(terminal);
      runCommand(terminal, terminal.input);
    } else if (char === "\x7f") {
      terminal.input = terminal.input.slice(0, -1);
    } else if (char === "\x03") {
      setPromptInput(terminal);
      terminal.lines[terminal.lines.length - 1].push({ text: "^C", fg: null, bg: null });
      terminal.input = "";
      terminal.lines.push([...PROMPT()]);
    } else if (char === "\x0c") {
      terminal.lines = [[...PROMPT()]];
    } else if (char === "\x17") {
      terminal.input = terminal.input.replace(/\S+\s*$/, "");
    } else if (char === "\t") {
      terminal.input += "  ";
    } else if (char >= " ") {
      terminal.input += char;
    }
  }
  setPromptInput(terminal);
  emitScreen(terminal);
}

createTerminal(zed, "zsh");
const watchTerminal = createTerminal(zed, "cargo watch");
watchTerminal.has_bell = true;
createTerminal(workspaces.get(WORKSPACE_API), "ssh devbox");

// ---------------------------------------------------------------------------
// Methods

const handlers = {
  "threads.list": () => threadsList(),
  "threads.new_thread_options": () => ({
    projects: [
      {
        project_id: "p-zed",
        name: "zed",
        remote_host: null,
        worktrees: [
          { path: "/Users/me/zed", name: "zed", branch: "main", is_main: true },
          { path: "/Users/me/.worktrees/zed/mobile", name: "mobile", branch: "feature/mobile", is_main: false },
        ],
        can_create_worktree: true,
        branches: ["main", "feature/mobile", "release/0.200"],
      },
      { project_id: "p-api", name: "api-server", remote_host: "devbox", worktrees: [], can_create_worktree: false, branches: [] },
    ],
    agents: [
      { id: "zed", name: "Zed Agent", is_external: false },
      { id: "claude-code", name: "Claude Code", is_external: true },
      { id: "gemini", name: "Gemini CLI", is_external: true },
      { id: "codex", name: "Codex", is_external: true },
    ],
    default_agent_id: "zed",
  }),
  "threads.create": async ({ project_id, worktree, agent_id }) => {
    if (!worktree || (worktree.kind !== "existing" && worktree.kind !== "new")) throw new MethodError("invalid worktree choice");
    if (worktree.kind === "new") await sleep(1500);
    const agentNames = { zed: "Zed Agent", "claude-code": "Claude Code", gemini: "Gemini CLI", codex: "Codex" };
    const thread = createThread({
      project_id,
      title: "New Thread",
      agent: agentNames[agent_id] ?? agent_id,
      worktrees: worktree.kind === "new" ? [worktree.name || "worktree-" + threadCounter] : worktree.path?.includes(".worktrees") ? ["feature/mobile"] : [],
      entries: [],
    });
    threadsChanged();
    return { thread_id: thread.thread_id };
  },
  "thread.open": ({ thread_id }, client) => {
    const thread = threads.get(thread_id);
    if (!thread) throw new MethodError(`No thread ${thread_id}`);
    if (thread.kind !== "agent") throw new MethodError("Terminal threads can't be opened on mobile");
    client.threads.add(thread_id);
    for (const other of threads.values()) other.is_active = other === thread;
    thread.has_notification = false;
    threadsChanged();
    return { thread_id, workspace_id: thread.workspace_id, entries: thread.entries, state: thread.state };
  },
  "thread.close": ({ thread_id }, client) => {
    client.threads.delete(thread_id);
    return null;
  },
  "thread.send": ({ thread_id, text }) => {
    const thread = threads.get(thread_id);
    if (!thread) throw new MethodError(`No thread ${thread_id}`);
    if (thread.state.title === "New Thread") {
      thread.state.title = text.slice(0, 48);
      threadsChanged();
    }
    sendMessage(thread, text);
    return null;
  },
  "thread.cancel": ({ thread_id }) => {
    const thread = threads.get(thread_id);
    if (!thread) throw new MethodError(`No thread ${thread_id}`);
    if (thread.turn) thread.turn.cancelled = true;
    if (thread.pendingPermission?.resolve) thread.pendingPermission.resolve("reject");
    for (const entry of thread.entries) {
      if (entry.kind === "tool_call" && (entry.status === "in_progress" || entry.status === "waiting_for_confirmation")) {
        entry.status = "canceled";
        entry.permission_options = null;
        emitEntry(thread, thread.entries.indexOf(entry));
      }
    }
    setStatus(thread, "idle");
    return null;
  },
  "thread.authorize": ({ thread_id, tool_call_id, option_id }) => {
    const thread = threads.get(thread_id);
    if (!thread) throw new MethodError(`No thread ${thread_id}`);
    const pending = thread.pendingPermission;
    if (!pending || pending.toolCallId !== tool_call_id) throw new MethodError("That tool call isn't waiting for permission");
    if (pending.resolve) {
      pending.resolve(option_id);
    } else {
      // Seeded permission request without a running turn.
      const index = thread.entries.findIndex((entry) => entry.id === tool_call_id);
      Object.assign(thread.entries[index], { status: option_id.startsWith("allow") ? "completed" : "rejected", permission_options: null });
      if (option_id.startsWith("allow")) thread.entries[index].content = [{ kind: "terminal", command: "rm -rf target/debug/incremental", output: "", exit_code: 0 }];
      emitEntry(thread, index);
      thread.pendingPermission = null;
      setStatus(thread, "idle");
    }
    return null;
  },
  "thread.set_control": ({ thread_id, control_id, value }) => {
    const thread = threads.get(thread_id);
    if (!thread) throw new MethodError(`No thread ${thread_id}`);
    const control = thread.state.controls.find((candidate) => candidate.id === control_id);
    if (!control) throw new MethodError(`No control ${control_id}`);
    if (control.kind === "toggle" && typeof value !== "boolean") throw new MethodError("toggle needs a bool");
    if (control.kind === "select" && typeof value !== "string") throw new MethodError("select needs a string");
    control.value = value;
    setTimeout(() => emitState(thread), 150);
    return null;
  },
  "thread.remove_queued": ({ thread_id, index }) => {
    const thread = threads.get(thread_id);
    if (!thread) throw new MethodError(`No thread ${thread_id}`);
    thread.state.queued_messages.splice(index, 1);
    emitState(thread);
    return null;
  },

  "workspace.items": ({ workspace_id }, client) => {
    const workspace = workspaces.get(workspace_id ?? WORKSPACE_ZED);
    if (!workspace) throw new MethodError(`No workspace ${workspace_id}`);
    client.workspaces.add(workspace.workspace_id);
    return workspaceItems(workspace);
  },
  "workspace.close_item": ({ workspace_id, item_id }) => {
    const workspace = workspaces.get(workspace_id);
    if (!workspace) throw new MethodError(`No workspace ${workspace_id}`);
    const terminalIndex = workspace.terminals.findIndex((terminal) => terminal.item_id === item_id);
    if (terminalIndex >= 0) {
      workspace.terminals.splice(terminalIndex, 1);
      terminals.delete(item_id);
      broadcast("terminal.closed", { workspace_id, item_id });
    } else {
      const index = workspace.items.findIndex((item) => item.item_id === item_id);
      if (index < 0) throw new MethodError(`No item ${item_id}`);
      workspace.items.splice(index, 1);
    }
    itemsChanged(workspace);
    return null;
  },
  "workspace.new_terminal": ({ workspace_id }) => {
    const workspace = workspaces.get(workspace_id);
    if (!workspace) throw new MethodError(`No workspace ${workspace_id}`);
    createTerminal(workspace, "zsh");
    itemsChanged(workspace);
    return null;
  },
  "workspace.list_directory": ({ worktree_id, path }) => listDirectory(worktree_id, path),
  "workspace.open_file": ({ workspace_id, path }) => {
    const workspace = workspaces.get(workspace_id);
    if (!workspace) throw new MethodError(`No workspace ${workspace_id}`);
    let item = workspace.items.find((candidate) => candidate.kind === "file" && candidate.path === path);
    if (!item) {
      const existing = [...buffers.values()].find((buffer) => buffer.path === path);
      const language = path.endsWith(".rs") ? "Rust" : path.endsWith(".md") ? "Markdown" : path.endsWith(".toml") ? "TOML" : null;
      const buffer = existing ?? createBuffer({ text: generatedFile(path), path, language });
      item = { item_id: nextItem++, kind: "file", title: path.split("/").pop(), path, buffer };
      workspace.items.push(item);
    }
    workspace.active_item_id = item.item_id;
    itemsChanged(workspace);
    return { item_id: item.item_id, kind: "file", offset: null };
  },

  "git.log": ({ offset, limit }) => ({
    repository_id: 1,
    branch: "main",
    commits: commits.slice(offset, offset + limit),
    has_more: offset + limit < commits.length,
  }),
  "git.open_uncommitted": ({ workspace_id }) => {
    const workspace = workspaces.get(workspace_id);
    let item = workspace.items.find((candidate) => candidate.kind === "diff");
    if (!item) {
      item = { item_id: 104, kind: "diff", title: "Uncommitted Changes", path: null };
      workspace.items.push(item);
      itemsChanged(workspace);
    }
    return { item_id: item.item_id, kind: "diff", offset: null };
  },
  "git.open_commit": ({ workspace_id, sha }) => {
    const workspace = workspaces.get(workspace_id);
    let item = workspace.items.find((candidate) => candidate.kind === "commit" && candidate.sha === sha);
    if (!item) {
      const commit = commits.find((candidate) => candidate.sha === sha);
      item = { item_id: nextItem++, kind: "commit", title: `${sha.slice(0, 7)} ${commit?.subject ?? ""}`, path: null, sha };
      workspace.items.push(item);
      itemsChanged(workspace);
    }
    return { item_id: item.item_id, kind: "commit", offset: null };
  },
  "git.stage": ({ paths }) => setStaged(paths, true),
  "git.unstage": ({ paths }) => setStaged(paths, false),

  "item.open": ({ workspace_id, item_id }, client) => {
    const { item } = findItem(workspace_id, item_id);
    if (item.kind === "file") {
      client.buffers.add(item.buffer.handle);
      return { kind: "file", title: item.title, buffer: bufferState(item.buffer) };
    }
    if (item.kind === "diff") {
      for (const file of diffFiles) if (file.buffer) client.buffers.add(file.buffer.handle);
      return {
        kind: "diff",
        title: item.title,
        repository_id: 1,
        files: diffFiles.map((file) => ({ ...file, buffer: file.buffer ? bufferState(file.buffer) : null })),
      };
    }
    if (item.kind === "commit") {
      const commit = commits.find((candidate) => candidate.sha === item.sha) ?? commits[0];
      return {
        kind: "commit",
        title: item.title,
        sha: commit.sha,
        message: `${commit.subject}\n\nThe config loader used to panic when the file was missing.\nNow it returns an error that main() reports.\n\nCo-authored-by: Zed Agent <agent@zed.dev>`,
        author: commit.author,
        timestamp: commit.timestamp,
        files: [
          { path: "crates/zed/src/main.rs", status: "modified", old_text: MAIN_RS_BASE, new_text: MAIN_RS, is_binary: false },
          { path: "crates/mobile/src/lib.rs", status: "added", old_text: null, new_text: LIB_RS, is_binary: false },
          { path: "assets/icons/zed.png", status: "added", old_text: null, new_text: null, is_binary: true },
          { path: "docs/legacy.md", status: "deleted", old_text: "# Legacy\n\nObsolete.\n", new_text: null, is_binary: false },
        ],
      };
    }
    throw new MethodError("This item can't be shown on mobile");
  },
  "item.close": ({ workspace_id, item_id }, client) => {
    const { item } = findItem(workspace_id, item_id);
    if (item.kind === "file") client.buffers.delete(item.buffer.handle);
    if (item.kind === "diff") for (const file of diffFiles) if (file.buffer) client.buffers.delete(file.buffer.handle);
    return null;
  },

  "buffer.push": ({ buffer: handle, version, updates }, client) => {
    const buffer = buffers.get(handle);
    if (!buffer) throw new MethodError(`No buffer ${handle}`);
    if (buffer.read_only) throw new MethodError("Buffer is read-only");
    if (version !== buffer.version) return { accepted: false, version: buffer.version };
    if (!Array.isArray(updates)) throw new MethodError("updates must be an array");
    // Validate synchronously so bad input is reported to the caller.
    let doc = buffer.doc;
    for (const update of updates) {
      const changes = ChangeSet.fromJSON(update.changes);
      if (changes.length !== doc.length) throw new MethodError(`change length ${changes.length} != doc length ${doc.length}`);
      doc = changes.apply(doc);
    }
    // Commit now, but broadcast after responding, like a real server racing the two.
    const startVersion = buffer.version;
    const serverUpdates = updates.map((update) => ({ client_id: client.clientId, changes: update.changes }));
    commitUpdates(buffer, serverUpdates, { defer: true });
    return { accepted: true, version: startVersion + updates.length };
  },
  "buffer.highlights": ({ buffer: handle, from, to }) => {
    const buffer = buffers.get(handle);
    if (!buffer) throw new MethodError(`No buffer ${handle}`);
    return highlight(buffer, from, to);
  },
  "buffer.completions": ({ buffer: handle, version, offset }, client) => {
    const buffer = buffers.get(handle);
    if (!buffer) throw new MethodError(`No buffer ${handle}`);
    if (version !== buffer.version) throw new MethodError("stale version");
    const text = buffer.doc.toString();
    const { start, word } = wordAt(text, offset);
    const prefix = text.slice(start, offset);
    const words = new Set([...text.matchAll(/[A-Za-z_]\w{2,}/g)].map((match) => match[0]));
    for (const keyword of ["println", "format", "Result", "Option", "Vec", "String", "unwrap_or_default", "iter", "collect"]) words.add(keyword);
    const candidates = [...words].filter((candidate) => candidate.startsWith(prefix) && candidate !== word).slice(0, 40);
    client.completions = new Map();
    return candidates.map((label, index) => {
      const completion = {
        completion_id: index + 1,
        label,
        detail: /^[A-Z]/.test(label) ? "struct" : "fn() -> ()",
        kind: /^[A-Z]/.test(label) ? "struct" : index % 3 === 0 ? "function" : "variable",
        from: start,
        to: offset,
        insert_text: label,
      };
      client.completions.set(completion.completion_id, { ...completion, buffer: handle, version });
      return completion;
    });
  },
  "buffer.apply_completion": ({ buffer: handle, completion_id }, client) => {
    const buffer = buffers.get(handle);
    const completion = client.completions?.get(completion_id);
    if (!buffer || !completion) throw new MethodError("Unknown completion");
    // Re-resolve the word at the cursor in case the user kept typing.
    const text = buffer.doc.toString();
    const { start, end } = wordAt(text, Math.min(completion.to, text.length));
    applyServerChange(buffer, [{ from: start, to: end, insert: completion.insert_text }]);
    return null;
  },
  "buffer.hover": ({ buffer: handle, offset }) => {
    const buffer = buffers.get(handle);
    if (!buffer) throw new MethodError(`No buffer ${handle}`);
    const { start, end, word } = wordAt(buffer.doc.toString(), offset);
    if (!word) return null;
    return { markdown: `\`\`\`rust\nfn ${word}(path: &Path) -> Result<Config>\n\`\`\`\n\nDocumentation for **${word}**. Loads things and returns a [Result](https://doc.rust-lang.org/std/result/).`, from: start, to: end };
  },
  "buffer.definition": ({ workspace_id, buffer: handle, offset }) => {
    const buffer = buffers.get(handle);
    if (!buffer) throw new MethodError(`No buffer ${handle}`);
    const text = buffer.doc.toString();
    const { word } = wordAt(text, offset);
    if (!word) return null;
    if (word === "README") {
      return { item_id: 102, kind: "file", offset: 0 };
    }
    const match = new RegExp(`(fn|struct|let|enum)\\s+${word}\\b`).exec(text);
    if (!match) return null;
    const workspace = workspaces.get(workspace_id);
    const item = workspace.items.find((candidate) => candidate.buffer === buffer);
    return { item_id: item?.item_id ?? 101, kind: "file", offset: match.index + match[1].length + 1 };
  },
  "buffer.format": ({ buffer: handle }) => {
    const buffer = buffers.get(handle);
    if (!buffer) throw new MethodError(`No buffer ${handle}`);
    const text = buffer.doc.toString();
    const changes = [];
    for (const match of text.matchAll(/[ \t]+$/gm)) changes.push({ from: match.index, to: match.index + match[0].length });
    if (changes.length) applyServerChange(buffer, changes);
    return null;
  },
  "buffer.save": ({ buffer: handle }) => {
    const buffer = buffers.get(handle);
    if (!buffer) throw new MethodError(`No buffer ${handle}`);
    buffer.is_dirty = false;
    broadcast("buffer.metadata", { buffer: handle, is_dirty: false, language: buffer.language, path: buffer.path }, (client) => client.buffers.has(handle));
    itemsChanged(zed);
    return null;
  },

  "terminal.open": ({ item_id }, client) => {
    const terminal = terminals.get(item_id);
    if (!terminal) throw new MethodError(`No terminal ${item_id}`);
    client.terminals.add(item_id);
    if (terminal.has_bell) {
      terminal.has_bell = false;
      itemsChanged(workspaces.get(terminal.workspace_id));
    }
    return terminalScreen(terminal);
  },
  "terminal.close": ({ item_id }, client) => {
    client.terminals.delete(item_id);
    return null;
  },
  "terminal.input": ({ item_id, data }) => {
    const terminal = terminals.get(item_id);
    if (!terminal) throw new MethodError(`No terminal ${item_id}`);
    terminalInput(terminal, data);
    return null;
  },
  "terminal.scroll": ({ item_id, lines }) => {
    const terminal = terminals.get(item_id);
    if (!terminal) throw new MethodError(`No terminal ${item_id}`);
    const max = Math.max(0, terminal.lines.length - terminal.rows);
    terminal.display_offset = Math.max(0, Math.min(max, terminal.display_offset + lines));
    emitScreen(terminal);
    return null;
  },
};

function setStaged(paths, staged) {
  for (const path of paths) {
    const file = diffFiles.find((candidate) => candidate.path === path);
    if (!file) throw new MethodError(`Not changed: ${path}`);
    file.is_staged = staged;
    file.is_partially_staged = false;
  }
  setTimeout(() => broadcast("diff.files_changed", { workspace_id: WORKSPACE_ZED, item_id: 104 }), 100);
  return null;
}

// ---------------------------------------------------------------------------
// Background activity

// A "collaborator" appends to notes.md every 15s to exercise remote updates.
setInterval(() => {
  const line = `- Remote edit at ${new Date().toLocaleTimeString()}\n`;
  applyServerChange(notesBuffer, [{ from: notesBuffer.doc.length, insert: line }]);
}, 15_000);

// `cargo watch` prints periodically and rings the bell.
setInterval(() => {
  const stamp = new Date().toLocaleTimeString();
  watchTerminal.lines.splice(watchTerminal.lines.length - 1, 0, [
    { text: "[Running 'cargo check']", fg: "6", bg: null },
  ], [{ text: `    Finished dev profile at ${stamp}`, fg: "2", bg: null, bold: true }]);
  emitScreen(watchTerminal);
}, 6_000);

// ---------------------------------------------------------------------------
// HTTP

function readJson(request) {
  return new Promise((resolve, reject) => {
    let body = "";
    request.on("data", (chunk) => (body += chunk));
    request.on("end", () => {
      try {
        resolve(body ? JSON.parse(body) : {});
      } catch (error) {
        reject(error);
      }
    });
  });
}

function json(response, status, body) {
  response.writeHead(status, { "Content-Type": "application/json", "Cache-Control": "no-store" });
  response.end(JSON.stringify(body));
}

const authorized = (request) => request.headers.authorization === `Bearer ${TOKEN}`;

async function handleApi(request, response) {
  const url = new URL(request.url, "http://localhost");
  if (LATENCY) await sleep(LATENCY);
  if (url.pathname === "/api/pair" && request.method === "POST") {
    const body = await readJson(request).catch(() => ({}));
    const normalized = String(body.code ?? "").toUpperCase().replace(/[^A-Z0-9]/g, "");
    if (typeof body.device_name !== "string") return json(response, 400, { error: "device_name is required" });
    if (normalized !== PAIRING_CODE.replace("-", "")) {
      log(`pairing rejected: ${body.code}`);
      return json(response, 401, { error: "Invalid or expired pairing code" });
    }
    log(`paired "${body.device_name}" (code sent as ${JSON.stringify(body.code)})`);
    return json(response, 200, { token: TOKEN });
  }
  if (url.pathname === "/api/session" && request.method === "GET") {
    if (!authorized(request)) return json(response, 401, { error: "Unauthorized" });
    return json(response, 200, { device_name: "iPhone (mock)" });
  }
  if (url.pathname === "/api/push/key" && request.method === "GET") {
    if (!authorized(request)) return json(response, 401, { error: "Unauthorized" });
    return json(response, 200, { public_key: VAPID_PUBLIC_KEY });
  }
  // Test hooks: drop every socket (optionally refusing reconnects for a while),
  // or revoke the token (close code 4001).
  if (url.pathname === "/api/mock/drop" && request.method === "POST") {
    refuseUntil = Date.now() + Number(url.searchParams.get("pause") ?? 0);
    for (const client of clients) client.ws.terminate();
    log(`dropped all sockets, refusing reconnects for ${url.searchParams.get("pause") ?? 0}ms`);
    return json(response, 200, {});
  }
  if (url.pathname === "/api/mock/revoke" && request.method === "POST") {
    for (const client of clients) client.ws.close(4001, "revoked");
    return json(response, 200, {});
  }
  if ((url.pathname === "/api/push/subscribe" || url.pathname === "/api/push/unsubscribe") && request.method === "POST") {
    if (!authorized(request)) return json(response, 401, { error: "Unauthorized" });
    const body = await readJson(request).catch(() => ({}));
    log(`${url.pathname}: ${body.endpoint ?? "(no endpoint)"}`);
    return json(response, 200, {});
  }
  return json(response, 404, { error: "Not found" });
}

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".png": "image/png",
  ".svg": "image/svg+xml",
  ".json": "application/json",
  ".webmanifest": "application/manifest+json",
};

function serveDist(request, response) {
  const dist = join(root, "dist");
  const pathname = decodeURIComponent(new URL(request.url, "http://localhost").pathname);
  let file = normalize(join(dist, pathname));
  if (!file.startsWith(dist)) {
    response.writeHead(403).end();
    return;
  }
  if (!existsSync(file) || statSync(file).isDirectory()) file = join(dist, "index.html");
  const headers = { "Content-Type": MIME[extname(file)] ?? "application/octet-stream" };
  if (file.endsWith("sw.js") || file.endsWith("index.html")) headers["Cache-Control"] = "no-cache";
  else if (file.includes(`${join("dist", "assets")}`)) headers["Cache-Control"] = "public, max-age=31536000, immutable";
  response.writeHead(200, headers);
  createReadStream(file).pipe(response);
}

const server = createHttpServer();
let vite = null;
if (SERVE_DIST) {
  if (!existsSync(join(root, "dist", "index.html"))) {
    console.error("dist/ is missing; run `npm run build` first.");
    process.exit(1);
  }
} else {
  const { createServer } = await import("vite");
  vite = await createServer({
    root,
    configFile: join(root, "vite.config.ts"),
    server: { middlewareMode: true, ws: { server } },
    appType: "spa",
  });
}

server.on("request", (request, response) => {
  if (request.url.startsWith("/api/")) {
    handleApi(request, response).catch((error) => json(response, 500, { error: String(error) }));
  } else if (vite) {
    vite.middlewares(request, response);
  } else {
    serveDist(request, response);
  }
});

// ---------------------------------------------------------------------------
// WebSocket

const wss = new WebSocketServer({ noServer: true });
let refuseUntil = 0;
server.on("upgrade", (request, socket, head) => {
  if (!request.url.startsWith("/api/ws")) return; // Vite's HMR socket handles its own path.
  if (Date.now() < refuseUntil) {
    socket.destroy();
    return;
  }
  wss.handleUpgrade(request, socket, head, (ws) => wss.emit("connection", ws, request));
});

wss.on("connection", (ws) => {
  const client = {
    ws,
    id: nextClient++,
    clientId: `mobile-${nextClient - 1}`,
    authed: false,
    threads: new Set(),
    workspaces: new Set(),
    buffers: new Set(),
    terminals: new Set(),
    completions: null,
  };
  clients.add(client);
  log(`client ${client.id} connected`);
  const authTimer = setTimeout(() => ws.close(4000, "auth timeout"), 10_000);

  ws.on("message", async (raw) => {
    let frame;
    try {
      frame = JSON.parse(String(raw));
    } catch {
      log(`client ${client.id}: malformed frame`);
      return;
    }
    if (!client.authed) {
      if (frame.type !== "auth") {
        log(`client ${client.id}: first frame wasn't auth`, frame);
        ws.close(4001, "unauthorized");
        return;
      }
      if (frame.token !== TOKEN) {
        log(`client ${client.id}: bad token`);
        ws.close(4001, "unauthorized");
        return;
      }
      clearTimeout(authTimer);
      client.authed = true;
      send(client, { type: "auth_ok", client_id: client.id });
      emit(client, "hello", { client_id: client.clientId, zed_version: "0.200.0-mock", theme: THEME });
      return;
    }
    const { id, method, params } = frame;
    if (typeof id !== "number" || typeof method !== "string") {
      log(`client ${client.id}: invalid request`, frame);
      return;
    }
    if (params === undefined || params === null || typeof params !== "object" || Array.isArray(params)) {
      log(`CONTRACT VIOLATION: ${method} sent without a params object`);
      send(client, { id, error: "missing params object" });
      return;
    }
    const handler = handlers[method];
    if (!handler) {
      log(`unknown method ${method}`);
      send(client, { id, error: `unknown variant \`${method}\`` });
      return;
    }
    try {
      if (LATENCY) await sleep(LATENCY);
      const result = await handler(params, client);
      send(client, { id, result: result === undefined ? null : result });
    } catch (error) {
      if (!(error instanceof MethodError)) console.error(error);
      send(client, { id, error: error instanceof Error ? error.message : String(error) });
    }
  });

  ws.on("close", () => {
    clearTimeout(authTimer);
    clients.delete(client);
    log(`client ${client.id} disconnected`);
  });
});

server.listen(PORT, "0.0.0.0", () => {
  log(`serving ${SERVE_DIST ? "dist/" : "Vite dev"} + mock API on http://localhost:${PORT}`);
  log(`pairing code: ${PAIRING_CODE}  (or open http://localhost:${PORT}/#pair=${PAIRING_CODE})`);
});
