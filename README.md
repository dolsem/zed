> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.
# Zed

[![Zed](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/zed-industries/zed/main/assets/badge/v0.json)](https://zed.dev)
[![CI](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml/badge.svg)](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml)

Welcome to Zed, a high-performance, multiplayer code editor from the creators of [Atom](https://github.com/atom/atom) and [Tree-sitter](https://github.com/tree-sitter/tree-sitter).

---

### About this fork

This is a personal fork of Zed, currently based on upstream **v1.23.2**. It adds the following on top of upstream:

#### Git worktree navigation actions

New actions for switching between the git worktrees of the active repository. They have no default bindings, so map them in your `keymap.json`:

| Action                       | Switches to                                                    |
| ---------------------------- | -------------------------------------------------------------- |
| `git::OpenMainWorktree`      | The main worktree                                              |
| `git::OpenLastWorktree`      | The worktree you were in before the last switch (like `cd -`)  |
| `git::OpenNextWorktree`      | The next worktree, ordered by name (wraps around)              |
| `git::OpenPreviousWorktree`  | The previous worktree, ordered by name (wraps around)          |

#### Control over untracked files in the Git Panel

The `git_panel.untracked_changes` setting (also in the Settings UI under **Panels**) controls how untracked files are shown and staged:

| Value       | Untracked files are…                                              | Stage All / Commit include them? |
| ----------- | ----------------------------------------------------------------- | -------------------------------- |
| `classic`   | Shown in their own section (upstream behavior, the default)       | Stage All: yes; commit without staging: no |
| `mixed`     | Shown alongside tracked changes                                   | Yes                              |
| `separate`  | Shown in their own section                                        | No                               |
| `hidden`    | Hidden from the Git Panel and the uncommitted-changes diff        | No                               |

When untracked files are excluded, the bulk actions are labeled **Stage Tracked** / **Unstage Tracked**. In `separate` mode a **Stage Untracked** item (the `git::StageUntracked` action) is added to the changes menu.

#### `ZED_THREAD_ID` in Terminal Threads

Every Terminal Thread's shell gets a `ZED_THREAD_ID` environment variable holding the thread's stable id. The id is kept when the thread is restored after a restart or from Thread History, so scripts and the `agent.terminal_init_command` can use it to resume the same session (for example, to map a Terminal Thread to a CLI agent's conversation).

#### Archiving Terminal Threads

Terminal Threads are archived like agent threads instead of being deleted:

- Hover a Terminal Thread in the Threads Sidebar to get **Rename** (pencil) and **Archive** buttons, and right-click it for **Rename Title** and **Archive Terminal**. `agent::ArchiveSelectedThread` archives the selected terminal.
- A Terminal Thread is also archived when its shell exits.
- Archived terminals are listed in Thread History. Clicking one restores it: a new shell starts with the same working directory, title and `ZED_THREAD_ID`, and the init command runs again. The trash icon deletes it permanently.
- Archiving a Terminal Thread never removes its git worktree, so it can always be restored.

#### `remove_worktree` task hook

Tasks with `"hooks": ["remove_worktree"]` run before Zed removes a linked git worktree, either when it's deleted from the worktree picker or when the last thread using a Zed-created worktree is archived. They get `ZED_WORKTREE_ROOT` (the worktree being removed) and `ZED_MAIN_GIT_WORKTREE` (the main repository), and run from inside the worktree. The tasks run one after another. If any of them fails or is cancelled, the rest are skipped and the worktree is kept. An archived thread is un-archived in that case.

```json
[
  {
    "label": "tear down worktree",
    "command": "docker compose down",
    "hooks": ["remove_worktree"]
  }
]
```

#### Threads of remote projects are stored on the remote host

Agent threads of a project opened over a remote connection (SSH, WSL, Docker) are stored on the remote host, in the same databases the Zed installed there uses. A thread started remotely shows up in the sidebar of the host's own Zed, and threads started on the host show up when the project is opened remotely. Changes made on either side appear on the other while both are open.

- This covers native agent threads (contents and sidebar entries), the sidebar entries of external agents, and Terminal Threads. External agents already keep their sessions on the host.
- A Terminal Thread's shell belongs to the Zed that started it. Opening one from the other machine starts a new shell on the host with the same `ZED_THREAD_ID` and runs the init command again, the same way an archived terminal is restored. The init command is the one configured on the host (its `agent.terminal_init_command`), since that's where the shell runs. To share one live session between both machines instead, run the init command inside `tmux new-session -A -s "zed-$ZED_THREAD_ID"`.
- Both sides must run the same release channel, and Zed must have been run on the host at least once.
- Threads of remote projects created before this change stay on the machine they were created on.
- When Zed starts, it reconnects to the remote projects in the sidebar in the background. That refreshes their threads and restores their unsaved changes. Until a host is reconnected, the client lists the threads it last saw there.
- Changes made while a host is unreachable are kept on the client, also across restarts, and sent once the host is reachable again.
- If the same thread is open on both machines at once, the last save wins.

#### HTTP API

Zed can serve an HTTP API that lets other programs control it: create worktrees (including running their `create_worktree` hooks), start threads with any agent (Claude and other external agents included), follow them, send messages, answer permission prompts and agent questions, and switch models and modes. Enable it with a secret, which clients send as `Authorization: Bearer <secret>`:

```json
"http_api": { "enabled": true, "address": "127.0.0.1", "port": 7878, "secret": "<a long random string>" }
```

Zed shows an error if the API is enabled without a secret. Changing the secret revokes every client that had the old one. The endpoints are documented in `crates/mobile_protocol/src/rest.rs`. In short:

- `GET /api/projects` and `GET /api/agents` list what threads can be started in and with.
- `POST /api/worktrees` and `POST /api/threads` start an **operation** and answer `202` right away. `GET /api/operations/{id}/events` streams its progress as server-sent events: every step, each worktree hook as it starts and finishes (with the end of its output), and any error. A new worktree only counts as created once its hooks finished. `POST /api/threads` can also set controls (e.g. `{"mode": "plan"}`) and send the first message.
- A thread is split into **runs**, one per user message, each holding everything the agent did in response. `GET /api/threads/{id}` lists them and `GET /api/threads/{id}/runs/{run_id}` returns one. By default a run only shows its outcome: the agent's final message once it finished, or what it's waiting on, such as a permission prompt (which is also how Claude asks to leave plan mode) or a question. Add `?detail=full` for every message, thought and tool call. Command output is left out of those, and is fetched separately from `GET /api/threads/{id}/tool_calls/{tool_call_id}/output`.
- `GET /api/threads/{id}/events` subscribes to a thread: a snapshot, then an event whenever a run starts, changes status or finishes, and whenever the thread's state changes. With `?detail=full`, every item as it streams in too.
- `POST /api/threads/{id}/messages`, `/cancel`, `/permissions/{tool_call_id}` and `/questions/{question_id}`, plus `PUT /api/threads/{id}/controls/{control_id}`, act on the thread.

The API is also an MCP server, at `/api/mcp` (Streamable HTTP), so agents can drive Zed. Its tools cover the same ground, and the ones that start something slow wait for it, e.g. `send_message` returns the agent's answer, or the permission prompt it stopped at. `POST /api/pairing` (or the `start_pairing` tool) returns a web UI pairing code. To add it to Claude Code:

```sh
claude mcp add --transport http zed https://<your host>/api/mcp --header "Authorization: Bearer <secret>"
```

API calls run in the background, so they don't get in the way of whoever is using Zed: threads are loaded and created without switching what Zed shows, projects and new worktrees open as background workspaces, and their hooks and queued messages don't take the focus.

The server is the separate `http_api` executable, which Zed starts and stops.

#### Web UI

The HTTP API can also serve a web app that lets you control Zed from your phone. It works like an app when added to the home screen, including on iOS, and it can send push notifications when an agent finishes or needs permission.
- **Threads:** the active threads of every project, including remote ones. Terminal Threads are listed but can't be opened yet. The **+** button starts a thread in a chosen project, worktree (or a new worktree) and agent.
- **Thread:** the full chat, with tool calls, permission prompts, the plan, the message queue and the model/mode/profile/thinking/effort/fast-mode controls.
- **Tabs:** the thread's workspace's editor tabs, plus its terminals along the bottom. **+** opens a file tree, and a git history whose first entry is the uncommitted changes. That view always includes untracked files, whatever `git_panel.untracked_changes` says.
- **Terminals** open full screen too. While one is open on the phone, Zed resizes it to the phone's screen, so the desktop shows the terminal at that size until the phone closes it. Scrolling the history on the phone doesn't move the desktop's view.
- **Editors** open full screen. They stay in sync with Zed's buffers, with syntax highlighting, diagnostics, completions, hover, go-to-definition, formatting and saving through Zed. Diffs and terminals also open full screen.

To set it up:

1. Enable the HTTP API (with a secret, see above) and the web UI. Zed shows an error if the web UI is enabled without the HTTP API:
   ```json
   "web_ui": { "enabled": true }
   ```
2. Expose the server over HTTPS with a reverse proxy, e.g. `cloudflared tunnel --url http://127.0.0.1:7878`. Installing the app and receiving push notifications both need HTTPS.
3. Open the URL on your phone and add it to the home screen. Run `web ui: pair device` in Zed and enter the code it shows in the app, which then receives the HTTP API's secret. Do this from the installed app, because on iOS it doesn't share storage with Safari.
4. Turn on notifications in the app's settings (the gear icon).

Pushes follow the same rule as Zed's own popups: nothing is sent while the thread is on screen in an active Zed window. On top of that, they only go out when the phone was the last thing used to interact with Zed, unless the bell in the status bar is switched on (`web_ui.always_notify`), which pushes whenever Zed would notify.

To unpair every device, change `http_api.secret`. That also drops their push subscriptions. The web UI's state (push keys and subscriptions) lives in Zed's data directory under `web_ui/`.
#### `dolsem` release channel

Builds from this fork use a separate `dolsem` release channel (`crates/zed/RELEASE_CHANNEL`). It runs as **Zed Dolsem**, with its own app icon, app identifier (`dev.zed.Zed-Dolsem`) and single-instance port. That lets it run side by side with an official Zed install. It never checks for or installs updates.

---

### Installation

On macOS, Linux, and Windows you can [download Zed directly](https://zed.dev/download) or install Zed via your local package manager ([macOS](https://zed.dev/docs/installation#macos)/[Linux](https://zed.dev/docs/linux#installing-via-a-package-manager)/[Windows](https://zed.dev/docs/windows#package-managers)).

Other platforms are not yet available:

- Web ([tracking discussion](https://github.com/zed-industries/zed/discussions/26195))

### Developing Zed

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

### Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

Also... we're hiring! Check out our [jobs](https://zed.dev/jobs) page for open roles.

### Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

## Sponsorship

Zed is developed by **Zed Industries, Inc.**, a for-profit company.

If you’d like to financially support the project, you can do so via GitHub Sponsors.
Sponsorships go directly to Zed Industries and are used as general company revenue.
There are no perks or entitlements associated with sponsorship.
