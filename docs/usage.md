# Usage

## Command line

```powershell
kilno.exe [folder] [--background]
```

- `folder`: the first argument that is an existing folder opens a session there. With none, Kilno uses the working directory, unless that is Kilno's own folder or System32 (shortcut/Start menu launches); then it opens on the home folder with no session.
- `--background`: used by start at login: no window, only the island, until asked for.
- Running `kilno.exe <folder>` while Kilno is already running hands the folder to the running instance and exits.
- `kilno.exe --statusline [--pass]` is internal (the status line relay), not meant to be run by hand.

## Main flows

1. First launch: the setup window finds Claude Code, Codex and OpenCode, can install or sign in, and lets you choose the default. Reopen it with Settings > Agents.
2. The sidebar lists sessions by project for every agent in use (each row marked with its agent's logo when more than one is used). Start a new session, resume one, or branch from one. Right-click a row to rename it, close its pane, or hide it (Del): a hidden session and its branches move to the folded Hidden section at the bottom, and Unhide (or opening it) brings it back. Hiding keeps the transcript, so `claude --resume` still lists it. Drag a row to put its session (with its branches) somewhere else in its section; the sort switches to Custom and the order is kept.
3. In a terminal, hold Ctrl over a local path (`C:\work\app.rs`, `src/main.rs:42`) and click it to show it in the Files panel beside the terminal: a file opens there (markdown, a picture, or text in the editor), a folder opens in the tree. A path outside the session's folder makes its folder the panel's. Relative paths are looked up in the pane's folder; only paths that exist are links.
   - Copying from a terminal: select with the mouse and the text is copied when you let go. Dragging past the top or bottom scrolls. Right-click copies the selection, or pastes when nothing is selected. Ctrl+C copies a selection and otherwise interrupts the agent, except within 2 seconds of a copy. Hold Shift to select while the app uses the mouse.
   - The Files editor: Ctrl+C, Ctrl+X, Ctrl+V; Ctrl+F finds (Enter or F3 for the next); Ctrl+S saves. Paste a full path into the box at the top of the tree and press Enter to open it.
4. Each pane header shows model, branch, context and effort; a status light shows working, done or needs you.
5. The island at the top of the screen announces when a session needs you or is done, offers a picker, and accepts dropped files.
6. Closing the window only hides it; quit from the gear menu.
7. Ctrl+Alt+S is the screenshot shortcut. Only one of Kilno or Clayo can own it; the other shows it as taken in Settings > Island.

Per-agent differences and manual agent installs: [AGENTS.md](AGENTS.md).

## Pages

Agents can write HTML pages to `<project>\.kilno\pages\*.html`. Kilno seals them with a Content-Security-Policy (no connections, no form posts) before showing them, and keeps that folder out of git.

## Data and diagnostics

- Data: `%LOCALAPPDATA%\Kilno` (`settings.json`, `names.json`, `parents.json`, `hidden.json`, `order.json`, `status\`, `turns\`, `crash.log`).
- Moving from Clayo: copy `%LOCALAPPDATA%\Clayo\settings.json`, `names.json` and `parents.json` into `%LOCALAPPDATA%\Kilno`.
- `KILNO_STARTUP_LOG=<file>` appends startup timing marks to that file.
- Network: the GitHub releases check of `tkhangg0910/kilno`, and Anthropic's usage endpoint if you turn on live usage.
