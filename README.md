<p align="center">
  <img src="docs/assets/logo.png" width="96" alt="Kilno mascot">
</p>

<h1 align="center">Kilno</h1>

<p align="center"><b>Never miss an agent waiting on you.</b><br>
Every Claude Code, Codex and OpenCode session in one window on Windows (Linux in preview), plus the <i>island</i>: a small bar at
the top of the screen that tells you when a session asks for a permission or a decision, or is done.</p>

<p align="center">
  <a href="https://github.com/tkhangg0910/kilno/releases/latest"><img src="https://img.shields.io/github/v/release/tkhangg0910/kilno" alt="Latest release"></a>
  <a href="https://github.com/tkhangg0910/kilno/actions/workflows/ci.yml"><img src="https://github.com/tkhangg0910/kilno/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/tkhangg0910/kilno" alt="MIT licence"></a>
  <img src="https://img.shields.io/badge/platform-Windows%2010%20%7C%2011%20%7C%20Linux%20(preview)-0078D4" alt="Windows 10 and 11, Linux in preview">
</p>

<p align="center">
  <img src="docs/assets/demo.gif" width="860" alt="Kilno: four sessions in one window, the island flags the one that needs you">
  <br>
  <a href="https://github.com/tkhangg0910/kilno/blob/main/docs/demo/demo-full.mp4"><b>▶ Watch the full demo</b></a>
</p>

## Why Kilno

| | |
|---|---|
| <img src="docs/assets/island.png" width="420" alt="The island announcing a session that needs you"> | **Know who is waiting.** The island at the top of the screen lights up when a session asks you something or finishes, even while you work in another app. |
| <img src="docs/assets/sessions.png" width="420" alt="Sessions grouped by project in the sidebar"> | **One window, every session.** Claude Code, Codex and OpenCode sessions from all your projects, grouped in one sidebar; start, resume or branch any of them. |
| <img src="docs/assets/status.png" width="420" alt="Model, branch, context and usage limits in the pane header"> | **Limits in view.** Model, branch, context use and your 5-hour and 7-day limits sit in the pane header, so a session never stalls on a surprise. |
| <img src="docs/assets/picker.png" width="420" alt="The island offering a new screenshot to a session"> | **Screenshots straight to the prompt.** Take a screenshot with Ctrl+Alt+S or any tool, and the island hands it to the session you pick. |

Kilno is one small native `kilno.exe`. It reads your agents' local session files and sends nothing
out; its only network call is the update check. It is an early release (v0.2.6), forked from
the Rust port of [Clayo](https://github.com/YogiOnCode/Clayo) (see [Credits](#credits)).

## Lighter than Clayo

Kilno keeps Clayo's design but drops .NET and WebView2: the terminal is drawn natively instead of
by xterm.js in an embedded browser.

| | Kilno (Rust) | Clayo 1.0.6 (C#/WPF + WebView2) | Difference |
|---|---|---|---|
| Start → first frame | **0.15–0.18 s** | 1.0–1.4 s (8.8 s first run) | **5–10× faster** |
| RAM, idle with 1 session | **33 MB** | 223 MB (80 MB app + 144 MB WebView2) | **6.8× less** |
| RAM, 5 sessions | **48 MB** | 410 MB (65 MB app + 345 MB WebView2) | **8.6× less** |
| On disk | **one 18 MB exe** (7.2 MB download) | 135 MB folder, 260 files | **7.4× smaller** |

Same laptop, release builds, throwaway config folders. Measured on clayo-rust, the codebase Kilno is
forked from; method and raw numbers in [`BENCH.md`](BENCH.md).

## Quick start

Needs Windows 10 or 11 (64-bit) and [Claude Code](https://docs.claude.com/en/docs/claude-code)
and/or [Codex](https://github.com/openai/codex) and/or [OpenCode](https://opencode.ai). In PowerShell:

```powershell
irm https://raw.githubusercontent.com/tkhangg0910/kilno/main/scripts/install.ps1 | iex
```

This installs the latest release to `%LOCALAPPDATA%\Programs\Kilno` and starts it; the setup
window finds your agents. Run the same line again to update. You can read
[the script](scripts/install.ps1) first. Building from source and verifying a release:
[docs/installation.md](docs/installation.md).

On Linux (preview):
`curl -fsSL https://raw.githubusercontent.com/tkhangg0910/kilno/main/scripts/install.sh | sh`;
see [docs/linux.md](docs/linux.md).

## Features

| Feature | What it does |
|---|---|
| Real terminals | Each session runs in a real Windows console inside the window, keys and mouse included |
| Search and resume | Find any past session by text and pick up where it stopped |
| Branch from here | Fork a session at any point and keep the original |
| Doc viewer | Read the Markdown docs and HTML pages a session writes, rendered offline |
| Files | The session's folder beside the terminal: read Markdown and pictures, fix a `.env` or a config in a plain editor |
| Status themes | Numbers, bars, rings or chips for context and limits |
| Ask-before guard | Sessions Kilno starts must ask you before they push, tag, release or merge ([details](docs/configuration.md#settings-pages)) |

## Architecture

<img src="docs/assets/architecture.svg" width="860" alt="Agents run in kilno-pty (ConPTY), drawn by kilno-term in kilno-app; kilno-core reads transcripts read-only and the status line relay, and drives kilno-app and kilno-island">

## Documentation

| | |
|---|---|
| [Usage](docs/usage.md) | Command line, main flows, data locations |
| [Architecture](docs/architecture.md) · [Structure](docs/structure.md) · [Decisions](docs/decisions.md) | How it is built and why |
| [Configuration](docs/configuration.md) | Settings pages, `settings.json` keys, environment variables |
| [FAQ](docs/faq.md) | Privacy, accounts, running next to Clayo |
| [Troubleshooting](docs/troubleshooting.md) | Shortcut conflicts, missing agents or sessions, crash logs |
| [Claude Code, Codex and OpenCode](docs/AGENTS.md) | What works with each agent |
| [Linux](docs/linux.md) | Installing on Linux and what differs from Windows |
| [Testing](docs/testing.md) | How to run the tests and what they cover |

## Contributing

Issues and pull requests are welcome. To build, you need Rust stable (1.85+) with the MSVC
toolchain and the Visual Studio C++ build tools:

```powershell
cargo build --release          # target\release\kilno.exe
cargo test
cargo clippy --all-targets -- -D warnings
```

CI runs the same checks on every push. Where things live: [docs/structure.md](docs/structure.md).

## License

MIT, see [`LICENSE`](LICENSE): the original Clayo copyright, then the Rust port's and Kilno's.
Third-party crates and the embedded Chart.js / Mermaid files:
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md). Citation metadata: [`CITATION.cff`](CITATION.cff).

## Credits

- **[Clayo](https://github.com/YogiOnCode/Clayo)** by **Yogeswaran Amsavalli** is the original app
  (C#/WPF, MIT). Its design, behaviour, texts, mascot, icon and island are his work; Kilno starts
  from them. Kilno's mascot, a small brick kiln, is redrawn on top of Clayo's: the same grid,
  limbs, sunglasses and animation.
- **[clayo-rust](https://github.com/tkhangg0910/clayo-rust)** is the native Rust port of Clayo
  that Kilno is forked from; its full history, including every original Clayo commit, is kept here.
  Background: [`PORTING.md`](PORTING.md), [`BENCH.md`](BENCH.md).
- Kilno is not affiliated with or endorsed by the Clayo project, Anthropic or OpenAI.
