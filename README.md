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
