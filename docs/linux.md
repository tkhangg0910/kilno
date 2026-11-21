# Kilno on Linux (preview)

Kilno runs on Linux x86_64 too, from the same code as on Windows. It is a preview: built and
tested on Ubuntu 22.04 by CI, but with less use behind it than the Windows build. Report what
you find in [the issues](https://github.com/tkhangg0910/kilno/issues).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/tkhangg0910/kilno/main/scripts/install.sh | sh
```

The script checks the download's SHA-256, installs into `~/.local/lib/kilno`, links
`~/.local/bin/kilno`, adds Kilno to your applications menu and starts it. No `sudo`, no change
to your shell's rc files. Run it again, or press Update in Kilno, to update. Settings live in
`~/.local/share/kilno` (or `$XDG_DATA_HOME/kilno`) and are kept. You can read
[the script](../scripts/install.sh) first.

It needs glibc 2.35 or later (Ubuntu 22.04, Debian 12, Fedora 36 and newer), an X11 or
Wayland desktop with OpenGL, and `curl`. The agents themselves: see [AGENTS.md](AGENTS.md);
Claude Code installs with `curl -fsSL https://claude.ai/install.sh | bash`, Codex and OpenCode
through npm.

Build it yourself: `cargo build --release`, then run `target/release/kilno`. `scripts/publish.sh`
makes the release tar.gz.

## What is the same

Sessions of Claude Code, Codex and OpenCode in one window, the sidebar (search, resume, branch,
rename, hide, drag), status in the pane header and limits in the footer, live usage, the
Ctrl+click path links, the doc viewer, Settings and the setup window, Update, and start at login
(off until you turn it on; an XDG autostart entry in `~/.config/autostart`).

## What is different

| | Windows | Linux |
|---|---|---|
| Pane's shell | PowerShell | Your login shell (`$SHELL -l`), so the agents on your profile's PATH are found |
| The island | A bar at the top of the screen | Desktop notifications for "needs you", "done" and errors (`notify-send`); the bar itself is not ported yet |
| Tray icon | In the notification area | In the tray where the desktop has one (KDE; GNOME with the AppIndicator extension); else run `kilno` again to bring a hidden window back |
| Ctrl+Alt+S screenshot shortcut | Yes | Not yet |
| Icons in the chrome | Segoe Fluent / MDL2 | The nearest Unicode symbols (those fonts are Windows-only) |
| Paste of copied files | From Explorer | From any file manager that copies `file://` URIs |
| Setup's Install / Sign in | A PowerShell window | Your terminal (x-terminal-emulator, GNOME Terminal, Konsole, Xfce Terminal or xterm) |

Paths that could reach the network are never looked up for a Ctrl+click link: anything under
`/net`, `/smb` or `/afs` and anything inside a mounted NFS, SMB, AFS or sshfs share.

The plan behind the port, with what is left (the island bar on X11 and Wayland, the shortcut): [PORTING-LINUX.md](../PORTING-LINUX.md).
