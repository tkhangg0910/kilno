//! The main window (MainWindow.xaml): sidebar | splitter | header + panes.
//!
//! Background work never depends on `ui`: panes run on their PTY threads, transcripts on the
//! sessions thread ([`crate::model`]), and pane events are handled in `App::logic`, which
//! eframe keeps calling while the window is hidden. Closing the window only hides it.

use crate::doc_view::{self, DocViewer};
use crate::image_chip::ImageChip;
use crate::island_host::{Host, IslandCmd};
use crate::model::{self, lock, Job, Model, Shared};
use crate::pane::{AppEvent, Pane, Ticker, Waker};
use crate::setup_window::{self, SetupWindow};
use crate::status::Status;
use crate::updater::{UpdateState, Updater};
use kilno_core::system::updater as update;
use crate::status_view;
use crate::theme::{self, *};
use crate::os;
#[cfg(windows)]
use crate::win;
use chrono::{Local, Utc};
use kilno_core::agent::claude::ClaudeAgent;
use kilno_core::agent::codex::CodexAgent;
use kilno_core::agent::opencode::OpenCodeAgent;
use kilno_core::agent::AgentKind;
use kilno_core::launch::{self, plan, Agent, LaunchMode, LaunchPlan};
use kilno_core::pane_status::PaneStatus;
use kilno_core::paths::{claude_dir, data_dir, DataDir, OsEnv};
use kilno_core::security::ask_rules;
use kilno_core::settings::{self, KilnoSettings};
use kilno_core::sidebar::{self, Bucket, PaneRef, SortMode, UnboundPane, ViewOptions, Waiting};
use kilno_core::status::strip::{self, Meter, Strip};
use kilno_core::status::{parse, relay, AccountLimits};
use kilno_core::system::login_startup::LoginStartup;
use kilno_island::{Island, IslandHandle};
use kilno_term::input::{bracketed_path, paste_bytes};
use kilno_term::widget::{self, TermFont};
use kilno_term::GridSize;
use egui::{Popup, PopupCloseBehavior};
use egui::{
    Align2, Color32, CursorIcon, Id, Key, Margin, Pos2, Rect, RectAlign, Sense, SetOpenCommand, Stroke, StrokeKind,
    TextEdit, Ui, UiBuilder, Vec2, ViewportCommand,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

/// Sidebar 310 (240..520) | 7 splitter | terminal (min 360).
const SIDEBAR_W: f32 = 310.0;
const SIDEBAR_MIN: f32 = 240.0;
const SIDEBAR_MAX: f32 = 520.0;
const SPLITTER_W: f32 = 7.0;
/// The RAM and CPU line in the sidebar footer.
const USAGE_H: f32 = 16.0;
const TERMINAL_MIN: f32 = 360.0;
const HEADER_H: f32 = 42.0;
const ROW_H: f32 = 48.0;
const EMPTY_PANES: &str = "Pick a session on the left, or start a new one.";

/// How the app was asked to start (see `kilno_core::cli`).
#[derive(Clone)]
pub struct Startup {
    pub background: bool,
    pub folder: Option<String>,
}

pub fn run(start: Startup) -> i32 {
    // Until setup is done the setup window comes first, in place of the main window.
    let settings = KilnoSettings::load(&settings::default_path(&data_dir(&OsEnv)));
    let setup_first = !start.background && needs_setup(&settings);
    // OpenGL first (fast to start, light); a machine without a usable GL driver gets Direct3D 12
    // (or WARP) through wgpu; failing both, a message box and crash.log (PORTING.md D10).
    let mut failures = Vec::new();
    for renderer in [eframe::Renderer::Glow, eframe::Renderer::Wgpu] {
        // Debug builds can try the fallback on its own.
        if cfg!(debug_assertions) && renderer == eframe::Renderer::Glow && std::env::var("KILNO_DEV_RENDERER").as_deref() == Ok("wgpu") {
            continue;
        }
        crate::mark("run_native");
        let visible = start_visible(&start, setup_first);
        let start = start.clone();
        let app = Box::new(move |cc: &eframe::CreationContext<'_>| -> Result<Box<dyn eframe::App>, _> {
            Ok(Box::new(KilnoApp::new(cc, start, setup_first)))
        });
        match eframe::run_native("Kilno", native_options(renderer, visible), app) {
            Ok(()) => return 0,
            Err(e) => {
                crate::log_fatal(&format!("Kilno could not open its window with {renderer}: {e}"));
                failures.push(format!("{renderer}: {e}"));
            }
        }
    }
    #[cfg(windows)]
    win::shell::error_box(
        "Kilno",
        &format!(
            "Kilno could not open its window.\n\n{}\n\nThe details are in %LOCALAPPDATA%\\Kilno\\crash.log.",
            failures.join("\n")
        ),
    );
    1
}

fn start_visible(start: &Startup, setup_first: bool) -> bool {
    !start.background && !setup_first
}

fn window_icon() -> egui::IconData {
    if cfg!(windows) {
        return egui::IconData::default();
    }
    eframe::icon_data::from_png_bytes(include_bytes!("../../../docs/assets/logo.png")).unwrap_or_default()
}

fn native_options(renderer: eframe::Renderer, visible: bool) -> eframe::NativeOptions {
    #[cfg_attr(not(windows), allow(unused_mut))] // moved onto the work area on Windows only
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("K I L N O")
        // Linux desktops match the window to kilno.desktop (StartupWMClass) by this.
        .with_app_id("kilno")
        .with_inner_size([1280.0, 820.0])
        .with_min_inner_size([760.0, 420.0])
        // Windows: the exe's own icon goes on by handle (win::shell::set_window_icon), not egui's.
        // Elsewhere the window carries the logo itself.
        .with_icon(Arc::new(window_icon()))
        .with_visible(visible);
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut centered = true;
    #[cfg(windows)]
    if let Some((x, y, w, h)) = win::primary_work_area() {
        // Leave room for the caption (~32) and borders inside the work area.
        let (iw, ih) = (1280f32.min(w - 16.0), 820f32.min(h - 48.0));
        viewport = viewport.with_inner_size([iw, ih]).with_position([x + (w - iw) / 2.0, y + (h - ih - 32.0) / 2.0]);
        centered = false;
    }
    let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration::default();
    // Key echo must not wait for the next vblank (PORTING.md §4.6, < 5 ms).
    wgpu_options.surface.present_mode = wgpu::PresentMode::AutoNoVsync;
    eframe::NativeOptions {
        viewport,
        renderer,
        centered,
        glow_options: eframe::egui_glow::GlowConfiguration { vsync: false, ..Default::default() },
        wgpu_options,
        clipboard_shortcuts: false,
        persist_window: false,
        // The event loop outlives a failed renderer, so the next one can be tried.
        run_and_return: true,
        ..Default::default()
    }
}

/// NeedsSetup: never set up, or none of the agents in use is where it was (a file check only).
fn needs_setup(s: &KilnoSettings) -> bool {
    kilno_core::setup::needs_setup(s, |kind, saved| kilno_core::agent::detect::locate(&OsEnv, kind, saved))
}

/// Brings the main window forward from any thread (the island's click, a second instance):
/// shown if hidden, restored if minimized (a maximized window comes back maximized), focused.
#[derive(Clone)]
pub struct Revealer {
    flag: Arc<AtomicBool>,
    ctx: egui::Context,
}

impl Revealer {
    pub fn reveal(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.ctx.request_repaint();
    }
}

/// The agent choice menu over New session is open for this request.
struct AskAgent {
    folder: String,
    typed: Typed,
}

/// What a new session gets typed at its prompt, unsent.
#[derive(Default)]
enum Typed {
    #[default]
    Nothing,
    /// On its first Done (Open a file).
    OnDone(String),
    /// The pane's prefill, once the agent's input box is up (the island's file drop and
    /// screenshots, AdoptFolder).
    Prefill(String),
}

/// What the header strip and the footer show this frame.
#[derive(Default)]
struct StatusFrame {
    strip: Option<Strip>,
    meters: Vec<Meter>,
}

struct Rename {
    id: String,
    text: String,
    started: bool,
}

/// One line of the sidebar list.
enum Item {
    Section(Bucket, usize),
    Folder(String, usize),
    Row(String, Bucket),
}

/// A drawn session row: where it is and what it can be dropped among (same section, and same
/// folder when grouped by folder).
struct RowHit {
    id: String,
    rect: Rect,
    /// Its top-level session: a thread moves as one.
    root: String,
    group: (Bucket, String),
}

#[derive(Default)]
struct ListCache {
    key: Option<(u64, ViewOptions, i64)>,
    items: Vec<Item>,
    signature: String,
}

/// Things the UI asked for this frame, run after drawing (dialogs block).
enum Action {
    Select(String),
    Open(String),
    StartRename(String),
    CommitRename(String, String),
    ResetName(String),
    /// Close the session's pane (the agent is asked to quit first).
    Close(String),
    /// Move a session (and its branches) to Hidden; its pane closes. Nothing is deleted.
    Hide(String),
    Unhide(String),
    NewIn(String),
    Browse,
    StartWith(AgentKind),
    OpenFile,
    Attach,
    Branch,
    CloseActive,
    OpenSettings,
    CloseSettings,
    /// The Settings page changed something: saved and applied.
    ApplySettings(Box<KilnoSettings>),
    SetLogin(bool),
    OpenSetup,
    Update,
    UpdateConfirmed,
    /// The doc chip: the viewer opens on the session's newest doc, or closes.
    ToggleDoc,
    /// The Files panel (the session's folder) beside the terminal, or back.
    ToggleFiles,
    /// A link clicked in the terminal or a doc: asked about first.
    AskLink(String),
    /// A page the session wrote, in the default browser (PORTING.md D3).
    OpenPage(std::path::PathBuf),
    Quit,
}

pub struct KilnoApp {
    settings: KilnoSettings,
    data: DataDir,
    model: Shared,
    jobs: Sender<Job>,
    panes: Vec<Pane>,
    active: Option<u64>,
    /// Open panes, the one used last first (the screenshot picker's order).
    used: Vec<u64>,
    next_id: u64,
    waker: Waker,
    rx: Receiver<AppEvent>,
    ticker: Ticker,
    font: TermFont,
    /// Where New session starts and the sidebar header points.
    folder: String,
    view: ViewOptions,
    sections: HashMap<Bucket, bool>,
    selected: Option<String>,
    /// The session row being dragged to a new place, if one is.
    drag: Option<String>,
    /// This frame's drawn rows, for dropping a dragged one.
    row_hits: Vec<RowHit>,
    /// Follows Claude's hook events ([`kilno_core::status::hooks`]); dropped, it stops.
    _turns: Option<Box<dyn std::any::Any + Send>>,
    list_focus: bool,
    rename: Option<Rename>,
    cache: ListCache,
    scroll_top: bool,
    sidebar_w: f32,
    settings_open: bool,
    focus_terminal: bool,
    ask_agent: Option<AskAgent>,
    open_agent_menu: bool,
    /// Start at login (on, can change), read fresh when the gear menu opens.
    login: (bool, bool),
    quitting: bool,
    shown: bool,
    reveal: Arc<AtomicBool>,
    revealer: Revealer,
    actions: Vec<Action>,
    first_frame_done: bool,
    island: IslandHandle,
    /// Another app holds Ctrl+Alt+S (the Settings page says so).
    screenshot_key_taken: bool,
    status: Status,
    status_frame: StatusFrame,
    /// Codex's account limits, recomputed when the rows change (model generation).
    codex_limits: (u64, Option<AccountLimits>),
    /// The setup window while it is open (one at a time).
    setup: Option<setup_window::Shared>,
    /// Opening Kilno in a folder while setup came first: the session starts once the main
    /// window shows.
    pending_start: Option<String>,
    updater: Updater,
    update: UpdateState,
    /// The doc viewers' render cache (one for every pane).
    md_cache: egui_commonmark::CommonMarkCache,
    /// Numbers each doc a viewer shows, so a different one starts at the top.
    doc_seq: u64,
    /// "Open this link in your browser?" for this link.
    confirm_link: Option<String>,
    /// The pane whose page chip is flashing, since when.
    page_flash: Option<(u64, std::time::Instant)>,
    /// The Settings page's own state (which page, open cards, own rules read).
    settings_page: crate::settings_page::SettingsPage,
    background: crate::background::Background,
    title_bar: crate::title_bar::TitleBar,
    /// KILNO_DEV_FILES was applied (debug builds).
    dev_files_done: bool,
    usage: crate::usage::Sampler,
    /// This frame's RAM and CPU, read once so the footer's height and drawing agree.
    usage_now: Option<crate::usage::Usage>,
    /// The main window (0 off Windows), for its caption colours.
    hwnd: isize,
    /// The image chip's thumbnails and full-size view.
    image_chip: ImageChip,
    /// The grid the terminal area fits, as last drawn: a new pane starts its console at it, as
    /// the C# pane waited for its view to report one (TP:264-278).
    grid: GridSize,
}

impl KilnoApp {
    fn new(cc: &eframe::CreationContext<'_>, start: Startup, setup_first: bool) -> Self {
        crate::mark("app new (window + GL ready)");
        cc.egui_ctx.set_fonts(crate::fonts::definitions());
        crate::mark("fonts");
        theme::install(&cc.egui_ctx);
        // Pictures in Markdown (doc viewer, Files): local files only, made safe by doc::prepare.
        egui_extras::install_image_loaders(&cc.egui_ctx);
        cc.egui_ctx.send_viewport_cmd(ViewportCommand::SetTheme(egui::SystemTheme::Dark));
        let hwnd = main_hwnd(cc);
        #[cfg(windows)]
        if hwnd != 0 {
            win::dark_caption(hwnd);
            win::shell::set_window_icon(hwnd);
        }
        let (tx, rx) = channel();
        let env = OsEnv;
        let data = data_dir(&env);
        let settings = KilnoSettings::load(&settings::default_path(&data));
        let model: Shared =
            Arc::new(std::sync::Mutex::new(Model::new(&data, settings.used_agents())));
        let ctx = cc.egui_ctx.clone();
        let jobs = model::start_worker(model.clone(), move || ctx.request_repaint());
        crate::image_chip::start_poll(model.clone(), cc.egui_ctx.clone());
        let reveal = Arc::new(AtomicBool::new(false));
        let revealer = Revealer { flag: reveal.clone(), ctx: cc.egui_ctx.clone() };
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        let waker = Waker { ctx: cc.egui_ctx.clone(), tx };

        // The island: on its own thread, after the main window (it never keeps Kilno alive).
        let host = Host {
            model: model.clone(),
            revealer: revealer.clone(),
            waker: waker.clone(),
            data: data.clone(),
            hwnd,
        };
        let island = Island::start(Arc::new(host));
        let screenshot_key_taken = island.set_settings(settings.screenshot_hotkey, settings.screenshot_offer);
        if start.background {
            island.greet();
        }
        crate::mark("island");
        // Screenshots and browser copies of pages are kept a week.
        crate::pages_open::prune(kilno_core::pages::copies_dir(&data));
        let shots = kilno_core::screenshots::dir(&data);
        let _ = std::thread::Builder::new().name("prune shots".into()).spawn(move || {
            let before = std::time::SystemTime::now() - kilno_core::screenshots::KEEP_FOR;
            kilno_core::screenshots::prune(&shots, before);
        });
        let status = {
            let ctx = cc.egui_ctx.clone();
            let island = island.clone();
            Status::start(&data, settings.clone(), !start.background, move || ctx.request_repaint(), move |m| {
                island.reserve(&m.label, m.percent.max(0) as u32)
            })
        };

        // A second launch's folder (or "" to just show the window) arrives here.
        #[cfg(unix)]
        {
            let w = waker.clone();
            let _ = os::instance::serve(move |msg| w.send(AppEvent::Handoff(msg)));
            // The tray icon, as on Windows below.
            let (revealer, w) = (revealer.clone(), waker.clone());
            os::tray::start(move |cmd| match cmd {
                os::tray::TrayCmd::Show => revealer.reveal(),
                os::tray::TrayCmd::Quit => w.send(AppEvent::Island(IslandCmd::Quit)),
            });
        }
        #[cfg(windows)]
        {
            // A second launch's folder (or "" to just show the window) arrives here.
            if let Some(name) = win::instance::pipe_name() {
                let w = waker.clone();
                let _ = win::instance::serve(name, move |msg| w.send(AppEvent::Handoff(msg)));
            }
            // The tray icon: Kilno runs on with its window closed. A click shows the window; its
            // menu's Quit goes the island's way.
            {
                let (revealer, w) = (revealer.clone(), waker.clone());
                win::tray::start(move |cmd| match cmd {
                    win::tray::TrayCmd::Show => revealer.reveal(),
                    win::tray::TrayCmd::Quit => w.send(AppEvent::Island(IslandCmd::Quit)),
                });
            }
            // Signing out ends the app as a real quit would, panes closed, rather than meeting
            // a close that only hides.
            if hwnd != 0 {
                let m = model.clone();
                win::shell::on_session_end(hwnd, move || {
                    for p in lock(&m).panes.iter() {
                        p.kill();
                    }
                    win::tray::remove();
                    std::process::exit(0);
                });
            }
        }
        // Start at login, if you turned it on, follows a Kilno that was moved (a release build only:
        // LoginStartup::can_write); it is never added here.
        if let Ok(exe) = std::env::current_exe() {
            let _ = LoginStartup::for_this_user(&data).keep(&exe);
        }
        // A Claude hook's event goes to the pane open on its session.
        let turns = {
            let m = model.clone();
            kilno_core::status::hooks::watch(&kilno_core::status::hooks::turns_dir(&data), move |id, hook| {
                let pane = lock(&m).panes.iter().find(|p| p.session_id().is_some_and(|s| s.eq_ignore_ascii_case(&id))).cloned();
                if let Some(p) = pane {
                    p.show_hook(hook);
                }
            })
        };
        // Once you have dragged sessions, the list opens in that order.
        let sort = if lock(&model).order.is_empty() { SortMode::Newest } else { SortMode::Custom };
        let updater = Updater::start(settings.update_check, waker.clone());

        let mut app = Self {
            settings,
            settings_page: crate::settings_page::SettingsPage::new(data.clone()),
            background: Default::default(),
            title_bar: Default::default(),
            dev_files_done: false,
            usage: crate::usage::Sampler::start(cc.egui_ctx.clone()),
            usage_now: None,
            hwnd,
            data,
            model,
            jobs,
            panes: Vec::new(),
            active: None,
            used: Vec::new(),
            next_id: 1,
            waker,
            rx,
            ticker: Ticker::start(),
            font: TermFont::default(),
            folder: start.folder.clone().unwrap_or(home),
            view: ViewOptions { sort, ..ViewOptions::default() },
            sections: HashMap::from([(Bucket::Open, true), (Bucket::Today, true), (Bucket::Earlier, false)]),
            selected: None,
            drag: None,
            row_hits: Vec::new(),
            _turns: turns,
            list_focus: false,
            rename: None,
            cache: ListCache::default(),
            scroll_top: false,
            sidebar_w: SIDEBAR_W,
            settings_open: false,
            focus_terminal: false,
            ask_agent: None,
            open_agent_menu: false,
            login: (false, false),
            quitting: false,
            shown: !start.background && !setup_first,
            reveal,
            revealer,
            actions: Vec::new(),
            first_frame_done: false,
            island,
            screenshot_key_taken,
            status,
            status_frame: StatusFrame::default(),
            codex_limits: (u64::MAX, None),
            setup: None,
            pending_start: None,
            updater,
            update: UpdateState::default(),
            md_cache: Default::default(),
            doc_seq: 0,
            confirm_link: None,
            page_flash: None,
            image_chip: ImageChip::default(),
            grid: GridSize { cols: 120, rows: 40 },
        };
        // Debug builds: KILNO_DEV_SETTINGS=<General|Status bar|Security|Island> opens Settings
        // on that page at start (rendering checks).
        if cfg!(debug_assertions) {
            if let Ok(page) = std::env::var("KILNO_DEV_SETTINGS") {
                app.settings_page.refresh();
                app.settings_page.dev_page(&page);
                app.settings_open = true;
            }
        }
        // Opening Kilno in a folder lands you in a live session there (once setup is done).
        if let Some(folder) = start.folder {
            if setup_first {
                app.pending_start = Some(folder);
            } else {
                app.start_new(&folder, Typed::Nothing);
                crate::mark("pane started");
            }
        }
        if setup_first {
            app.show_setup(&cc.egui_ctx);
        }
        app
    }

    // ------------------------------------------------------------------ reveal and setup

    /// Brings the main window forward from wherever it is: hidden (closed, or never shown after
    /// a login start), minimized, or behind. While setup is needed and the window is hidden,
    /// the setup window comes instead. True when the main window shows.
    fn reveal(&mut self, ctx: &egui::Context) -> bool {
        if !self.shown && needs_setup(&self.settings) {
            self.show_setup(ctx);
            return false;
        }
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        self.shown = true;
        if let Some(folder) = self.pending_start.take() {
            self.start_new(&folder, Typed::Nothing);
        }
        true
    }

    /// The setup window, on its own or over the main window; asked for again while open, it
    /// comes forward.
    fn show_setup(&mut self, ctx: &egui::Context) {
        if let Some(s) = &self.setup {
            setup_window::lock(s).activate();
            return;
        }
        self.setup = Some(Arc::new(std::sync::Mutex::new(SetupWindow::new(&self.settings, &self.data, ctx))));
        // A hidden main window runs no ui pass that would open it; ask for one.
        ctx.send_viewport_cmd(ViewportCommand::RequestPaintWhileHidden);
        ctx.request_repaint();
    }

    /// Declares the setup viewport for this pass and acts on what came of it.
    fn setup_frame(&mut self, ctx: &egui::Context) {
        let Some(s) = self.setup.clone() else { return };
        let outcome = setup_window::lock(&s).outcome.take();
        match outcome {
            Some(setup_window::Outcome::Closed) => {
                // Nothing changes; the next Reveal asks again.
                self.setup = None;
                return;
            }
            Some(setup_window::Outcome::Done(new)) => {
                self.setup = None;
                self.apply_settings(*new);
                lock(&self.model).generation += 1;
                let _ = self.jobs.send(Job::Refresh);
                self.reveal(ctx);
                return;
            }
            None => {}
        }
        let over = if self.shown { ctx.input(|i| i.viewport().outer_rect) } else { None };
        let builder = setup_window::builder(over);
        ctx.show_viewport_deferred(setup_window::viewport_id(), builder, move |ui, _| {
            setup_window::lock(&s).frame(ui);
        });
    }

    /// New settings from setup (or, later, the Settings page): saved, and passed to everything
    /// that reads them.
    fn apply_settings(&mut self, s: KilnoSettings) {
        let check_changed = s.update_check != self.settings.update_check;
        s.save(&settings::default_path(&self.data));
        {
            let mut m = lock(&self.model);
            m.agents = s.used_agents();
        }
        self.screenshot_key_taken = self.island.set_settings(s.screenshot_hotkey, s.screenshot_offer);
        self.status.set_settings(s.clone());
        if check_changed {
            // Off forgets what an earlier check found.
            if !s.update_check && !self.update.waiting {
                self.update.to = None;
            }
            self.updater.set_enabled(s.update_check);
        }
        self.settings = s;
    }

    // ------------------------------------------------------------------ update

    /// The Update button: with sessions open, ask first; then wait for the busy ones. Pressed
    /// again while waiting, it calls the update off and checks again.
    fn update_clicked(&mut self) {
        if self.update.waiting {
            self.update.waiting = false;
            self.updater.check_now();
            return;
        }
        if !self.panes.is_empty() {
            self.update.confirming = true;
            return;
        }
        self.update_confirmed();
    }

    fn update_confirmed(&mut self) {
        self.update.confirming = false;
        self.update.waiting = true;
        self.update_when_idle();
    }

    /// UpdateWhenIdle (on the click, every pane status change and every close): once no pane is
    /// working or asking, start the install script and quit; it waits for this Kilno to exit,
    /// swaps the folder and starts the new one.
    fn update_when_idle(&mut self) {
        if !self.update.waiting || !update::idle(self.panes.iter().map(|p| p.status)) {
            return;
        }
        if let Some(v) = self.update.to {
            if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(std::path::Path::to_path_buf)) {
                let _ = update::install_command(v, &dir).spawn();
            }
        }
        let ctx = self.revealer.ctx.clone();
        self.quit(&ctx);
    }

    // ------------------------------------------------------------------ launching

    fn agent(&self, kind: AgentKind) -> Box<dyn Agent> {
        let env = OsEnv;
        match kind {
            AgentKind::Codex => Box::new(CodexAgent::for_this_user(&env, self.settings.codex_path.as_deref())),
            AgentKind::OpenCode => Box::new(OpenCodeAgent::for_this_user(&env, self.settings.opencode_path.as_deref())),
            AgentKind::Claude => {
                let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                let relay = relay::write_settings(
                    &self.data,
                    &claude_dir(&env),
                    &exe,
                    &ask_rules::claude_ask(&self.settings),
                );
                Box::new(ClaudeAgent::for_this_user(&env, self.settings.claude_path.as_deref(), relay.as_deref()))
            }
        }
    }

    fn plan(&self, kind: AgentKind, mode: LaunchMode, folder: &str, id: Option<&str>) -> Option<LaunchPlan> {
        let mut p = plan(self.agent(kind).as_ref(), mode, folder, id, &OsEnv).ok()?;
        // Debug builds: run something else in place of the agent (rendering checks, benches);
        // `{id}` in it is the session id the pane expects.
        if cfg!(debug_assertions) {
            if let Ok(cmd) = std::env::var("KILNO_DEV_AGENT_CMD") {
                p.agent_command = cmd.replace("{id}", p.expected_session_id.as_deref().unwrap_or(""));
            }
        }
        // The latency probe (BENCH.md) measures echo through a bare shell, not the agent.
        if crate::latency::enabled() {
            p.agent_command = String::new();
        }
        // BENCH.md runs (release builds too): a fixed command in place of the agent.
        if let Ok(cmd) = std::env::var("KILNO_BENCH_AGENT_CMD") {
            p.agent_command = cmd;
        }
        Some(p)
    }

    /// A new session with the agent settings pick, or the one chosen from the menu over New
    /// session when set to ask (closing that menu starts nothing).
    fn start_new(&mut self, folder: &str, typed: Typed) {
        match self.settings.new_session_kind() {
            Some(kind) => self.start_new_with(kind, folder, typed),
            None => {
                self.ask_agent = Some(AskAgent { folder: folder.to_string(), typed });
                self.open_agent_menu = true;
            }
        }
    }

    fn start_new_with(&mut self, kind: AgentKind, folder: &str, typed: Typed) {
        let Some(p) = self.plan(kind, LaunchMode::New, folder, None) else { return };
        let (on_done, prefill) = match typed {
            Typed::Nothing => (None, None),
            Typed::OnDone(t) => (Some(t), None),
            Typed::Prefill(t) => (None, Some(t)),
        };
        let id = self.start_pane(&p, sidebar::new_title(folder), None, prefill);
        if let (Some(text), Some(pane)) = (on_done, self.pane(id)) {
            pane.shared.insert_on_first_done(text);
        }
    }

    /// Points the window at `folder` and starts a session there (SpawnIn).
    fn spawn_in(&mut self, folder: &str, typed: Typed) {
        if !std::path::Path::new(folder).is_dir() {
            return;
        }
        self.folder = folder.to_string();
        self.start_new(folder, typed);
    }

    /// AdoptFolder: the island's drop or screenshot. The window comes forward on a new session
    /// in `folder`, `prefill` typed into its prompt once it is up, not sent.
    fn adopt_folder(&mut self, folder: &str, prefill: Option<String>) {
        if !std::path::Path::new(folder).is_dir() {
            return;
        }
        self.folder = folder.to_string();
        let ctx = self.revealer.ctx.clone();
        // Setup came up instead: there may be no agent to start yet. New session will use the
        // folder.
        if !self.reveal(&ctx) {
            return;
        }
        self.spawn_in(folder, prefill.map_or(Typed::Nothing, Typed::Prefill));
    }

    /// The island's screenshot: into the picked live pane's prompt (bracketed, not sent), or a
    /// new session in its folder, the last-used pane's, or the current one (MainWindow.SendImage).
    fn send_image(&mut self, pane: Option<u64>, path: std::path::PathBuf) {
        let text = path.to_string_lossy().into_owned();
        let live = pane.and_then(|id| self.pane(id)).filter(|p| p.status != PaneStatus::Exited);
        if let Some(p) = live {
            let id = p.id();
            p.shared.write(&bracketed_path(&text));
            // Claude copies it into the session's paste cache; Codex uses it where it is.
            if p.agent == AgentKind::Codex {
                p.shared.codex_images.lock().unwrap_or_else(|e| e.into_inner()).push((path, Utc::now()));
            }
            self.revealer.reveal();
            self.activate(id);
            return;
        }
        let picked = pane.and_then(|id| self.pane(id)).map(|p| p.cwd.to_string_lossy().into_owned());
        let last = self.used.first().and_then(|id| self.pane(*id)).map(|p| p.cwd.to_string_lossy().into_owned());
        let folder = picked.or(last).unwrap_or_else(|| self.folder.clone());
        let folder = if std::path::Path::new(&folder).is_dir() { folder } else { self.folder.clone() };
        let before = self.active;
        let pasted = String::from_utf8_lossy(&bracketed_path(&text)).into_owned();
        self.adopt_folder(&folder, Some(pasted));
        if let Some(p) = self.active_pane().filter(|p| self.active != before && p.agent == AgentKind::Codex) {
            p.shared.codex_images.lock().unwrap_or_else(|e| e.into_inner()).push((path, Utc::now()));
        }
    }

    /// The row exists at once (status Starting), under Open, before any transcript.
    fn start_pane(&mut self, p: &LaunchPlan, title: String, fork_of: Option<String>, prefill: Option<String>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let pane = Pane::start(
            id,
            p,
            title.clone(),
            self.grid,
            prefill,
            self.waker.clone(),
            self.ticker.clone(),
        );
        {
            let mut m = lock(&self.model);
            let key = p.expected_session_id.clone().unwrap_or_else(kilno_core::text::new_guid);
            let custom = m.names.get(&key).map(String::from);
            let pref = PaneRef { id, agent: p.agent, status: pane.status, working_dir: p.working_directory.clone() };
            m.sidebar.open_pane(&key, pref, &title, custom);
            if p.expected_session_id.is_none() {
                m.waiting.push(Waiting {
                    placeholder: key.clone(),
                    pane: UnboundPane { agent: p.agent, folder: p.working_directory.clone(), parent_id: fork_of, since: Utc::now() },
                });
            }
            m.panes.push(pane.shared.clone());
            m.generation += 1;
            self.selected = Some(key);
        }
        self.panes.push(pane);
        self.panes_changed();
        self.activate(id);
        id
    }

    /// The open panes' folders are what git is polled for.
    fn panes_changed(&self) {
        self.status.set_folders(self.panes.iter().map(|p| p.cwd.to_string_lossy().into_owned()).collect());
    }

    fn pane(&self, id: u64) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id() == id)
    }

    fn active_pane(&self) -> Option<&Pane> {
        self.active.and_then(|id| self.pane(id))
    }

    /// A pane's name as the sidebar shows it (renamed or the session's title).
    fn pane_name(&self, pane: &Pane) -> String {
        lock(&self.model).sidebar.by_pane(pane.id()).map_or(pane.title.clone(), |r| r.name().to_string())
    }

    /// Switches to a pane; picking or starting a session also leaves Settings.
    fn activate(&mut self, id: u64) {
        self.settings_open = false;
        self.active = Some(id);
        self.used.retain(|u| *u != id);
        self.used.insert(0, id);
        lock(&self.model).used = self.used.clone();
        self.focus_terminal = true;
        self.list_focus = false;
        // Its branch at once, not at the next poll.
        if let Some(p) = self.pane(id) {
            self.status.git.refresh(&p.cwd.to_string_lossy());
        }
    }

    /// Ends the session: the process goes and its row drops out of Open. A Codex pane closed
    /// before its transcript appeared has no session to list. The last pane left takes over.
    /// The pane a session is open in, if any.
    fn pane_of(&self, session_id: &str) -> Option<u64> {
        lock(&self.model).sidebar.get(session_id).and_then(|r| r.pane.as_ref()).map(|p| p.id)
    }

    /// Back in the list: the session and any hidden parent above it (a branch under a hidden
    /// parent would stay hidden otherwise).
    fn unhide(&mut self, session_id: &str) {
        let mut m = lock(&self.model);
        for id in m.sidebar.lineage(session_id) {
            m.hidden.unhide(&id);
        }
        m.generation += 1;
    }

    fn close_pane(&mut self, id: u64) {
        let Some(i) = self.panes.iter().position(|p| p.id() == id) else { return };
        // Dropped at the end of this function: Pane::drop asks the agent to quit, then kills it.
        let _pane = self.panes.remove(i);
        {
            let mut m = lock(&self.model);
            let placeholder = m.sidebar.by_pane(id).map(|r| r.session_id.clone());
            let before = m.waiting.len();
            m.waiting.retain(|w| Some(&w.placeholder) != placeholder.as_ref());
            let unbound = m.waiting.len() != before;
            m.sidebar.close_pane(id, unbound);
            m.panes.retain(|p| p.id != id);
            m.generation += 1;
        }
        self.island.pane_closed(id);
        self.used.retain(|u| *u != id);
        lock(&self.model).used = self.used.clone();
        self.panes_changed();
        self.active = self.panes.last().map(Pane::id);
        if let Some(a) = self.active {
            self.activate(a);
        }
        self.update_when_idle();
    }

    /// Resume a row's session in its folder (or the current one if that is gone), or switch to
    /// its pane if it is open.
    fn open_row(&mut self, id: &str) {
        let (pane, folder, agent, name) = {
            let m = lock(&self.model);
            let Some(r) = m.sidebar.get(id) else { return };
            (r.pane.as_ref().map(|p| p.id), r.folder.clone(), r.agent(), r.name().to_string())
        };
        if let Some(p) = pane {
            self.activate(p);
            return;
        }
        let cwd = launch::resume_folder(&folder, &self.folder);
        if let Some(p) = self.plan(agent, LaunchMode::Resume, &cwd, Some(id)) {
            self.start_pane(&p, name, None, None);
        }
    }

    fn branch(&mut self) {
        let Some(pane) = self.active_pane() else { return };
        let Some(parent) = pane.shared.session_id() else { return };
        let (agent, cwd) = (pane.agent, pane.cwd.to_string_lossy().into_owned());
        let Some(p) = self.plan(agent, LaunchMode::Fork, &cwd, Some(&parent)) else { return };
        // Record the link now: the fork rewrites sessionId on every line it copies, so nothing
        // on disk will say the two are related. A Codex fork names its parent itself.
        if let Some(child) = &p.expected_session_id {
            lock(&self.model).parents.set(child, &parent);
        }
        self.start_pane(&p, sidebar::branch_title(&parent), Some(parent), None);
    }

    /// Ends Kilno: every session closes, the watchers stop, the app exits.
    fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        let _ = self.jobs.send(Job::Stop);
        // Each pane's drop asks its agent to quit and kills it CLOSE_TIME later (Pane::close).
        let agents = !self.panes.is_empty();
        self.panes.clear();
        lock(&self.model).panes.clear();
        self.status.stop();
        self.island.shutdown();
        os::tray::remove();
        if !agents {
            // A hidden window gets no egui pass that would carry out the Close (the island's
            // Quit comes while hidden): end here, as main would after the event loop.
            if !self.shown {
                std::process::exit(0);
            }
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        // The window goes at once; the process ends once the agents have had their time (a
        // Close now would end it before them, as main exits right after the event loop).
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        let _ = std::thread::Builder::new().name("quit".into()).spawn(|| {
            std::thread::sleep(crate::pane::CLOSE_TIME + std::time::Duration::from_millis(300));
            std::process::exit(0);
        });
    }

    fn paste(&self, pane: &Pane) {
        let mode = pane.shared.term.mode();
        match os::read_clipboard() {
            os::Clip::Text(t) => pane.shared.type_keys(&paste_bytes(&t, mode)),
            os::Clip::Files(files) => {
                let (bytes, images) = file_paste(&files);
                // Codex reads a pasted image where it is: the image chip remembers it.
                if pane.agent == AgentKind::Codex {
                    let now = Utc::now();
                    pane.shared.codex_images.lock().unwrap_or_else(|e| e.into_inner()).extend(images.into_iter().map(|i| (i, now)));
                }
                pane.shared.type_keys(&bytes);
            }
            os::Clip::Nothing => {}
        }
    }

    // ------------------------------------------------------------------ actions

    fn run_actions(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        for a in std::mem::take(&mut self.actions) {
            match a {
                Action::Select(id) => {
                    if self.rename.as_ref().is_some_and(|r| r.id != id) {
                        self.rename = None; // leaving a row cancels its rename
                    }
                    let pane = lock(&self.model).sidebar.get(&id).and_then(|r| r.pane.as_ref()).map(|p| p.id);
                    self.selected = Some(id);
                    if let Some(p) = pane {
                        self.activate(p);
                    }
                }
                Action::Open(id) => {
                    // Opening a hidden session brings it back to the list.
                    if lock(&self.model).sidebar.get(&id).is_some_and(|r| r.hidden) {
                        self.unhide(&id);
                    }
                    self.open_row(&id);
                }
                Action::Close(id) => {
                    if let Some(p) = self.pane_of(&id) {
                        self.close_pane(p);
                    }
                }
                Action::Hide(id) => {
                    if let Some(p) = self.pane_of(&id) {
                        self.close_pane(p);
                    }
                    let mut m = lock(&self.model);
                    m.hidden.hide(&id, Utc::now());
                    m.generation += 1;
                }
                Action::Unhide(id) => self.unhide(&id),
                Action::StartRename(id) => {
                    let text = lock(&self.model).sidebar.get(&id).map(|r| r.edit_text());
                    if let Some(text) = text {
                        self.rename = Some(Rename { id, text, started: false });
                    }
                }
                Action::CommitRename(id, text) => self.commit_rename(&id, &text),
                Action::ResetName(id) => {
                    let mut m = lock(&self.model);
                    m.names.set(&id, None);
                    if let Some(r) = m.sidebar.get_mut(&id) {
                        r.custom_name = None;
                    }
                    m.generation += 1;
                }
                Action::NewIn(folder) => self.spawn_in(&folder, Typed::Nothing),
                Action::Browse => {
                    let mut d = rfd::FileDialog::new().set_title("Start a session in").set_parent(frame);
                    if std::path::Path::new(&self.folder).is_dir() {
                        d = d.set_directory(&self.folder);
                    }
                    if let Some(f) = d.pick_folder() {
                        self.spawn_in(&f.to_string_lossy(), Typed::Nothing);
                    }
                }
                Action::StartWith(kind) => {
                    if let Some(ask) = self.ask_agent.take() {
                        self.start_new_with(kind, &ask.folder, ask.typed);
                    }
                }
                Action::OpenFile => {
                    let mut d = rfd::FileDialog::new().set_title("Open a file in a new session").set_parent(frame);
                    if std::path::Path::new(&self.folder).is_dir() {
                        d = d.set_directory(&self.folder);
                    }
                    if let Some(file) = d.pick_file() {
                        let text = kilno_core::text::insert_paths(&[file.to_string_lossy()]);
                        if let Some(dir) = file.parent() {
                            self.spawn_in(&dir.to_string_lossy(), Typed::OnDone(text));
                        }
                    }
                }
                Action::Attach => {
                    let Some(pane) = self.active_pane() else { continue };
                    let mut d = rfd::FileDialog::new().set_title("Attach files").set_parent(frame);
                    if pane.cwd.is_dir() {
                        d = d.set_directory(&pane.cwd);
                    }
                    if let Some(files) = d.pick_files() {
                        let files: Vec<String> = files.iter().map(|f| f.to_string_lossy().into_owned()).collect();
                        pane.shared.insert_paths(&files);
                    }
                    self.focus_terminal = true;
                }
                Action::Branch => self.branch(),
                Action::CloseActive => {
                    if let Some(id) = self.active {
                        self.close_pane(id);
                    }
                }
                Action::OpenSettings => {
                    Popup::close_all(ctx);
                    if !self.settings_open {
                        self.settings_page.refresh();
                    }
                    self.settings_open = true;
                }
                Action::ApplySettings(s) => self.apply_settings(*s),
                Action::CloseSettings => {
                    self.settings_open = false;
                    self.focus_terminal = true;
                }
                Action::SetLogin(on) => {
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = LoginStartup::for_this_user(&self.data).set(on, &exe);
                    }
                }
                Action::OpenSetup => self.show_setup(ctx),
                Action::Update => self.update_clicked(),
                Action::UpdateConfirmed => self.update_confirmed(),
                Action::ToggleDoc => self.toggle_doc(),
                Action::ToggleFiles => self.toggle_files(),
                Action::OpenPage(page) => {
                    if let Some(p) = self.active_pane() {
                        p.shared.pages_seen();
                    }
                    self.page_flash = None;
                    crate::pages_open::open(kilno_core::pages::copies_dir(&self.data), &page);
                    self.focus_terminal = true;
                }
                Action::AskLink(uri) => {
                    // Web links only: a file: or other scheme would run something (TP:326-331).
                    self.confirm_link = kilno_core::doc::web_link(&uri).map(str::to_string);
                    if self.confirm_link.is_none() {
                        self.focus_terminal = true;
                    }
                }
                Action::Quit => self.quit(ctx),
            }
        }
    }

    /// "Update Kilno" (OK / Cancel), when sessions are open.
    fn update_confirm(&mut self, ctx: &egui::Context) {
        if !self.update.confirming {
            return;
        }
        let resp = egui::Modal::new(Id::NULL.with("update-confirm")).frame(popup_frame(8, MENU_RIM).inner_margin(20)).show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.label(egui::RichText::new(update::CONFIRM_TITLE).font(semibold(14.0)).color(MENU_HOVER_FG));
            ui.add_space(8.0);
            ui.label(egui::RichText::new(update::CONFIRM_TEXT).font(ui_font(12.5)).color(TEXT_PRIMARY));
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ghost_button(ui, None, "Cancel", false, Some(80.0)).clicked() {
                    self.update.confirming = false;
                }
                ui.add_space(8.0);
                if ghost_button(ui, None, "OK", true, Some(80.0)).clicked() {
                    self.actions.push(Action::UpdateConfirmed);
                }
            });
        });
        if resp.should_close() {
            self.update.confirming = false;
        }
    }

    /// "Open this link in your browser?\n\n<uri>" (OK / Cancel), as the C# page's confirm();
    /// either way the terminal has the focus after.
    fn link_confirm(&mut self, ctx: &egui::Context) {
        let Some(uri) = self.confirm_link.clone() else { return };
        let mut done = false;
        let resp = egui::Modal::new(Id::NULL.with("link-confirm")).frame(popup_frame(8, MENU_RIM).inner_margin(20)).show(ctx, |ui| {
            ui.set_max_width(440.0);
            let text = kilno_core::doc::confirm_text(&uri);
            ui.add(egui::Label::new(egui::RichText::new(text).font(ui_font(12.5)).color(TEXT_PRIMARY)).wrap());
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ghost_button(ui, None, "Cancel", false, Some(80.0)).clicked() {
                    done = true;
                }
                ui.add_space(8.0);
                if ghost_button(ui, None, "OK", true, Some(80.0)).clicked() {
                    os::shell::open(&uri);
                    done = true;
                }
            });
        });
        if done || resp.should_close() {
            self.confirm_link = None;
            self.focus_terminal = true;
        }
    }

    /// The page chip's colour while a page written since its last click waits (PORTING.md D3:
    /// it flashes rather than opening a browser by itself): amber, blinking for 1.5 s.
    fn page_flash(&mut self, ctx: &egui::Context, pane: Option<u64>, new_page: bool) -> Option<Color32> {
        let pane = pane.filter(|_| new_page)?;
        let since = match self.page_flash {
            Some((p, at)) if p == pane => at,
            _ => {
                let now = std::time::Instant::now();
                self.page_flash = Some((pane, now));
                now
            }
        };
        const BLINK: std::time::Duration = std::time::Duration::from_millis(250);
        let t = since.elapsed();
        if t >= BLINK * 6 {
            return Some(WORKING);
        }
        let n = t.as_millis() / BLINK.as_millis();
        ctx.request_repaint_after(BLINK * (n as u32 + 1) - t);
        n.is_multiple_of(2).then_some(WORKING)
    }

    /// The doc chip (MainWindow.DocChip_Click): closes the viewer, or opens it on the
    /// session's newest doc; the terminal keeps the focus.
    fn toggle_doc(&mut self) {
        self.doc_seq += 1;
        let seq = self.doc_seq;
        let Some(pane) = self.active.and_then(|id| self.panes.iter_mut().find(|p| p.id() == id)) else { return };
        if pane.doc.take().is_none() {
            let w = pane.shared.written();
            if let Some(doc) = w.doc {
                // One panel beside the terminal: the doc replaces Files (unless it holds edits).
                if !pane.files.as_ref().is_some_and(|f| f.dirty()) {
                    pane.files = None;
                    pane.doc = Some(DocViewer::open(&doc, &pane.cwd, w.generation, seq));
                }
            }
        }
        self.focus_terminal = true;
    }

    /// The Files button: the session's folder beside the terminal, or closed (not while the
    /// editor holds unsaved edits: its own ✕ asks about them).
    fn toggle_files(&mut self) {
        let Some(pane) = self.active.and_then(|id| self.panes.iter_mut().find(|p| p.id() == id)) else { return };
        match &pane.files {
            Some(f) if f.dirty() => {}
            Some(_) => {
                pane.files = None;
                self.focus_terminal = true;
            }
            None => {
                pane.doc = None;
                pane.files = Some(crate::files_panel::FilesPanel::new(&pane.cwd));
            }
        }
    }

    // ------------------------------------------------------------------ drawing

    fn rebuild_list(&mut self) {
        let now = Utc::now();
        let mut m = lock(&self.model);
        let key = (m.generation, self.view.clone(), now.timestamp() / 60);
        // Never under an open rename box: it would move or vanish mid-word.
        if self.cache.key.as_ref() == Some(&key) || self.rename.is_some() {
            return;
        }
        let m = &mut *m;
        let parents = &m.parents;
        m.sidebar.link_threads(|id| parents.get(id).map(String::from), self.view.sort != SortMode::Oldest, now);
        let hidden = &m.hidden;
        m.sidebar.mark_hidden(|id| hidden.is_hidden(id));
        let order = &m.order;
        m.sidebar.mark_order(|id| order.rank(id));
        let rows = m.sidebar.view(&self.view, now, &Local);
        let signature = sidebar::layout_signature(&rows, self.view.group_by_folder, now, &Local);
        let mut items = Vec::new();
        let mut i = 0;
        while i < rows.len() {
            let b = rows[i].bucket(now, &Local);
            let n = rows[i..].iter().take_while(|r| r.bucket(now, &Local) == b).count();
            items.push(Item::Section(b, n));
            let mut j = i;
            while j < i + n {
                if self.view.group_by_folder {
                    let proj = rows[j].project();
                    let k = proj.to_lowercase();
                    let c = rows[j..i + n].iter().take_while(|r| r.project().to_lowercase() == k).count();
                    items.push(Item::Folder(proj, c));
                    for r in &rows[j..j + c] {
                        items.push(Item::Row(r.session_id.clone(), b));
                    }
                    j += c;
                } else {
                    items.push(Item::Row(rows[j].session_id.clone(), b));
                    j += 1;
                }
            }
            i += n;
        }
        if signature != self.cache.signature {
            self.scroll_top = true;
        }
        self.cache = ListCache { key: Some(key), items, signature };
    }

    fn draw_sidebar(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0, crate::background::veil(SURFACE_BG));
        // The account's limits sit above the actions, 12 px apart; none, and the gap goes too.
        // So does the Update button, when there is a release to offer.
        let limits_h = status_view::limits_height(ui, &self.status_frame.meters, self.settings.theme);
        let limits_gap = if limits_h > 0.0 { limits_h + 12.0 } else { 0.0 };
        let update_gap = if self.update.button().is_some() { 30.0 + 12.0 } else { 0.0 };
        self.usage_now = self.usage.get();
        let usage_gap = if self.usage_now.is_some() { USAGE_H + 12.0 } else { 0.0 };
        let footer_h = 12.0 + limits_gap + usage_gap + update_gap + 30.0 + 14.0;
        let body = Rect::from_min_max(rect.min, Pos2::new(rect.right(), rect.bottom() - footer_h - 1.0));
        let footer = Rect::from_min_max(Pos2::new(rect.left(), body.bottom()), rect.max);
        ui.scope_builder(UiBuilder::new().max_rect(body), |ui| self.draw_sidebar_body(ui));
        ui.scope_builder(UiBuilder::new().max_rect(footer), |ui| self.draw_footer(ui));
    }

    fn draw_sidebar_body(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        let inner = rect.width() - 28.0;
        // Header: current folder leaf over the full path.
        let x = rect.left() + 14.0;
        let mut y = rect.top() + 14.0;
        let leaf = kilno_core::paths::leaf(&self.folder);
        let g = elided(ui, &leaf, semibold(14.0), TEXT_PRIMARY, inner);
        let h = g.size().y;
        ui.painter().galley(Pos2::new(x, y), g, TEXT_PRIMARY);
        y += h + 2.0;
        let g = elided(ui, &self.folder, mono(10.5), TEXT_FAINT, inner);
        let h = g.size().y;
        ui.painter().galley(Pos2::new(x, y), g, TEXT_FAINT);
        y += h + 8.0;

        // Search: the hint goes as soon as the caret lands, and comes back on blur if empty.
        let search_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(inner, 29.0));
        let out = ui.scope_builder(UiBuilder::new().max_rect(search_rect), |ui| {
            TextEdit::singleline(&mut self.view.filter)
                .id(Id::NULL.with("search"))
                .font(ui_font(12.0))
                .text_color(TEXT_PRIMARY)
                .margin(Margin::symmetric(8, 6))
                .desired_width(inner)
                .frame(
                    egui::Frame::new()
                        .fill(TERMINAL_BG)
                        .stroke(Stroke::new(1.0, HAIRLINE))
                        .corner_radius(4)
                        .inner_margin(Margin::symmetric(8, 6)),
                )
                .show(ui)
        });
        let sresp = out.inner.response.response;
        if self.view.filter.is_empty() && !sresp.has_focus() {
            ui.painter().text(
                Pos2::new(sresp.rect.left() + 9.0, sresp.rect.center().y),
                Align2::LEFT_CENTER,
                "Search sessions",
                ui_font(12.0),
                TEXT_FAINT,
            );
        }
        if sresp.has_focus() {
            self.list_focus = false;
        }
        y = sresp.rect.bottom() + 6.0;

        // Group / sort, the Explorer idiom.
        let combos = Rect::from_min_size(Pos2::new(rect.left() + 10.0, y), Vec2::new(rect.width() - 20.0, 24.0));
        ui.scope_builder(UiBuilder::new().max_rect(combos).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
            let group = if self.view.group_by_folder { "Group: Folder" } else { "Group: None" };
            let r = tiny_combo(ui, group);
            tiny_popup(&r, |ui| {
                for (label, v) in [("Group: Folder", true), ("Group: None", false)] {
                    if tiny_item(ui, label, self.view.group_by_folder == v).clicked() {
                        self.view.group_by_folder = v;
                        ui.close();
                    }
                }
            });
            ui.add_space(4.0);
            let sort = match self.view.sort {
                SortMode::Newest => "Newest",
                SortMode::Oldest => "Oldest",
                SortMode::Name => "Name",
                SortMode::Custom => "Custom",
            };
            let r = tiny_combo(ui, sort);
            tiny_popup(&r, |ui| {
                for (label, v) in [("Newest", SortMode::Newest), ("Oldest", SortMode::Oldest), ("Name", SortMode::Name), ("Custom", SortMode::Custom)] {
                    if tiny_item(ui, label, self.view.sort == v).clicked() {
                        self.view.sort = v;
                        ui.close();
                    }
                }
            });
        });
        y = combos.bottom() + 4.0;

        let list = Rect::from_min_max(Pos2::new(rect.left(), y), rect.max);
        self.rebuild_list();
        ui.scope_builder(UiBuilder::new().max_rect(list), |ui| self.draw_list(ui));
    }

    fn draw_list(&mut self, ui: &mut Ui) {
        let now = Utc::now();
        let mut sa = egui::ScrollArea::vertical().id_salt("sessions").auto_shrink(false);
        if std::mem::take(&mut self.scroll_top) {
            sa = sa.vertical_scroll_offset(0.0);
        }
        let model = self.model.clone();
        let items = std::mem::take(&mut self.cache.items);
        self.row_hits.clear();
        let mut dropped = None;
        sa.show(ui, |ui| {
            let m = lock(&model);
            for item in &items {
                match item {
                    Item::Section(b, n) => {
                        let open = self.sections.get(b).copied().unwrap_or(b.expanded_by_default());
                        if section_header(ui, b.label(), *n, open).clicked() {
                            self.sections.insert(*b, !open);
                        }
                    }
                    Item::Folder(name, n) => {
                        if self.section_open(item) {
                            folder_header(ui, name, *n);
                        }
                    }
                    Item::Row(id, b) => {
                        if !self.section_open(item) {
                            continue;
                        }
                        if let Some(row) = m.sidebar.get(id) {
                            let rect = self.draw_row(ui, row, now);
                            let folder = if self.view.group_by_folder { row.project().to_lowercase() } else { String::new() };
                            self.row_hits.push(RowHit { id: id.clone(), rect, root: m.sidebar.root_of(id), group: (*b, folder) });
                        }
                    }
                }
            }
            dropped = self.drag_feedback(ui);
        });
        self.cache.items = items;
        if let Some((moved, target, after)) = dropped {
            self.drop_session(&moved, &target, after);
        }
    }

    /// While a row is dragged: the line where it would land, among the threads of its own
    /// section (and folder). On release, the (moved, target, after) to apply.
    fn drag_feedback(&mut self, ui: &Ui) -> Option<(String, String, bool)> {
        let id = self.drag.clone()?;
        let (pos, down, released) = ui.input(|i| (i.pointer.interact_pos(), i.pointer.primary_down(), i.pointer.any_released()));
        if !down && !released {
            self.drag = None;
            return None;
        }
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        let from = self.row_hits.iter().find(|h| h.id.eq_ignore_ascii_case(&id))?;
        let (moved, group) = (from.root.clone(), from.group.clone());
        // Each other thread of the group as one block: its rows' union.
        let mut blocks: Vec<(String, Rect)> = Vec::new();
        for h in self.row_hits.iter().filter(|h| h.group == group && !h.root.eq_ignore_ascii_case(&moved)) {
            match blocks.iter_mut().find(|(r, _)| r.eq_ignore_ascii_case(&h.root)) {
                Some((_, rect)) => *rect = rect.union(h.rect),
                None => blocks.push((h.root.clone(), h.rect)),
            }
        }
        let pos = pos?;
        let dist = |r: &Rect| if r.y_range().contains(pos.y) { 0.0 } else { (pos.y - r.center().y).abs() };
        let (target, rect) = blocks.iter().min_by(|a, b| dist(&a.1).total_cmp(&dist(&b.1)))?.clone();
        let after = pos.y > rect.center().y;
        let y = if after { rect.bottom() } else { rect.top() };
        ui.painter().hline(rect.x_range(), y, Stroke::new(2.0, WORKING));
        if released {
            self.drag = None;
            return Some((moved, target, after));
        }
        None
    }

    /// A dragged thread dropped by another: the section's threads are ranked in their new order
    /// and the list switches to Custom.
    fn drop_session(&mut self, moved: &str, target: &str, after: bool) {
        let Some(group) = self.row_hits.iter().find(|h| h.root.eq_ignore_ascii_case(moved)).map(|h| h.group.clone()) else { return };
        let mut roots: Vec<String> = Vec::new();
        for h in self.row_hits.iter().filter(|h| h.group == group) {
            if !roots.iter().any(|r| r.eq_ignore_ascii_case(&h.root)) {
                roots.push(h.root.clone());
            }
        }
        let order = kilno_core::store::order::move_to(&roots, moved, target, after);
        if order == roots {
            return;
        }
        let mut m = lock(&self.model);
        m.order.set(&order);
        m.generation += 1;
        drop(m);
        self.view.sort = SortMode::Custom;
    }

    fn section_open(&self, item: &Item) -> bool {
        match item {
            Item::Row(_, b) => self.sections.get(b).copied().unwrap_or(b.expanded_by_default()),
            // A folder header sits in the section of the row after it.
            Item::Folder(..) => true,
            Item::Section(..) => true,
        }
    }

    /// One session row; returns where it was drawn. Dragging it moves its thread.
    fn draw_row(&mut self, ui: &mut Ui, row: &sidebar::SessionRow, now: chrono::DateTime<Utc>) -> Rect {
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, ROW_H), Sense::click_and_drag());
        if resp.drag_started() && self.rename.is_none() {
            self.drag = Some(row.session_id.clone());
        }
        let id = &row.session_id;
        let selected = self.selected.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(id));
        if ui.is_rect_visible(rect) {
            let p = ui.painter();
            if selected {
                p.rect_filled(rect, 0, crate::background::shade(ROW_SELECTED, 0.08));
                p.rect_filled(Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height())), 0, WORKING);
            } else if resp.hovered() {
                p.rect_filled(rect, 0, crate::background::shade(ROW_HOVER, 0.05));
            }
            let x0 = rect.left() + 2.0 + 34.0 + row.depth as f32 * 15.0;
            let top = rect.top() + 7.0;
            dot(p, Pos2::new(x0 + 3.5, top + 7.5), status_color(row.status_key()));
            let mut x = x0 + 14.0;
            let right = rect.right() - 10.0;
            if row.is_branch() {
                let g = p.layout_no_wrap("↳".into(), ui_font(11.0), TEXT_FAINT);
                let gw = g.size().x;
                p.galley(Pos2::new(x, top + 1.0), g, TEXT_FAINT);
                x += gw + 5.0;
            }
            let renaming = self.rename.as_ref().is_some_and(|r| r.id.eq_ignore_ascii_case(id));
            if !renaming {
                let mut name_right = right;
                if row.agent_tag().is_some() {
                    let c = parse_hex(row.agent_color()).gamma_multiply(0.85);
                    let logo = Rect::from_min_size(Pos2::new(right - 12.0, top + 2.0), Vec2::splat(12.0));
                    crate::agent_logo::paint(p, logo, row.agent(), c);
                    name_right -= 12.0 + 6.0;
                }
                let g = elided(ui, row.name(), ui_font(12.5), TEXT_PRIMARY, name_right - x);
                p.galley(Pos2::new(x, top), g, TEXT_PRIMARY);
            }
            let sub = row.subtitle(now, &Local);
            let g = elided(ui, &sub, ui_font(10.5), TEXT_FAINT, right - (x0 + 14.0));
            p.galley(Pos2::new(x0 + 14.0, top + 20.0), g, TEXT_FAINT);
            if renaming {
                self.draw_rename(ui, Rect::from_min_max(Pos2::new(x, top - 3.0), Pos2::new(right, top + 18.0)));
            }
        }
        if resp.clicked() || resp.secondary_clicked() {
            self.actions.push(Action::Select(id.clone()));
            self.list_focus = true;
            resp.request_focus();
        }
        if resp.double_clicked() {
            self.actions.push(Action::Open(id.clone()));
        }
        let resp = if resp.hovered() && self.rename.is_none() { resp.on_hover_text(row.tooltip()) } else { resp };
        resp.context_menu(|ui| {
            if menu_item(ui, "Open", None, None, true).clicked() {
                self.actions.push(Action::Open(id.clone()));
                ui.close();
            }
            if row.is_open() && menu_item(ui, "Close", None, None, true).clicked() {
                self.actions.push(Action::Close(id.clone()));
                ui.close();
            }
            if menu_item(ui, "Rename", Some("F2"), None, true).clicked() {
                self.actions.push(Action::StartRename(id.clone()));
                ui.close();
            }
            menu_separator(ui);
            if menu_item(ui, "Reset name", None, None, true).clicked() {
                self.actions.push(Action::ResetName(id.clone()));
                ui.close();
            }
            menu_separator(ui);
            if row.hidden && !row.is_open() {
                if menu_item(ui, "Unhide", None, None, true).clicked() {
                    self.actions.push(Action::Unhide(id.clone()));
                    ui.close();
                }
            } else {
                let label = if row.is_open() { "Close and hide" } else { "Hide" };
                let r = menu_item(ui, label, Some("Del"), None, true)
                    .on_hover_text("Moves it (and its branches) to Hidden at the bottom of the list. The transcript is kept.");
                if r.clicked() {
                    self.actions.push(Action::Hide(id.clone()));
                    ui.close();
                }
            }
        });
        rect
    }

    /// The in-row rename box: focused with everything selected; Enter or leaving it commits,
    /// Esc cancels.
    fn draw_rename(&mut self, ui: &mut Ui, rect: Rect) {
        let Some(r) = self.rename.as_mut() else { return };
        let edit_id = Id::NULL.with("rename-box");
        let out = ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
            TextEdit::singleline(&mut r.text)
                .id(edit_id)
                .font(ui_font(12.5))
                .text_color(TEXT_PRIMARY)
                .desired_width(rect.width())
                .frame(
                    egui::Frame::new()
                        .fill(TERMINAL_BG)
                        .stroke(Stroke::new(1.0, WORKING))
                        .corner_radius(3)
                        .inner_margin(Margin::symmetric(4, 1)),
                )
                .show(ui)
        });
        let mut out = out.inner;
        let resp = out.response.response.clone();
        if !r.started {
            r.started = true;
            resp.request_focus();
            out.state.cursor.set_char_range(Some(egui::text::CCursorRange::select_all_str(&r.text)));
            out.state.store(ui.ctx(), edit_id);
            return;
        }
        if resp.lost_focus() {
            let cancel = ui.input(|i| i.key_pressed(Key::Escape));
            let r = self.rename.take().expect("checked above");
            if !cancel {
                self.actions.push(Action::CommitRename(r.id, r.text));
            }
        }
    }

    fn commit_rename(&mut self, id: &str, text: &str) {
        let mut m = lock(&self.model);
        let Some(row) = m.sidebar.get(id) else { return };
        let custom = row.rename_to(text);
        m.names.set(id, custom.as_deref());
        if let Some(r) = m.sidebar.get_mut(id) {
            r.custom_name = custom;
        }
        m.generation += 1;
    }

    fn draw_footer(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0, crate::background::shade(FOOTER_BAND, -0.15));
        ui.painter().hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0, HAIRLINE));
        // The account's limits (LimitsView), then the Update button, then the actions.
        let mut top = rect.top() + 1.0 + 12.0;
        let limits_h = status_view::limits_height(ui, &self.status_frame.meters, self.settings.theme);
        if limits_h > 0.0 {
            let r = Rect::from_min_max(Pos2::new(rect.left() + 14.0, top), Pos2::new(rect.right() - 14.0, top + limits_h));
            if status_view::draw_limits(ui, r, &self.status_frame.meters, self.settings.theme) {
                self.status.refresh();
            }
            top += limits_h + 12.0;
        }
        if let Some(u) = &self.usage_now {
            let r = Rect::from_min_max(Pos2::new(rect.left() + 14.0, top), Pos2::new(rect.right() - 14.0, top + USAGE_H));
            usage_line(ui, r, u, self.settings.resource_usage == "all");
            top += USAGE_H + 12.0;
        }
        if let Some((text, tip)) = self.update.button() {
            let r = Rect::from_min_max(Pos2::new(rect.left() + 14.0, top), Pos2::new(rect.right() - 14.0, top + 30.0));
            ui.scope_builder(UiBuilder::new().max_rect(r).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
                if ghost_button(ui, Some('\u{E896}'), &text, false, None).on_hover_text(tip).clicked() {
                    self.actions.push(Action::Update);
                }
            });
            top += 30.0 + 12.0;
        }
        let row = Rect::from_min_max(Pos2::new(rect.left() + 14.0, top), Pos2::new(rect.right() - 14.0, top + 30.0));
        ui.scope_builder(UiBuilder::new().max_rect(row).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
            let new = ghost_button(ui, Some('\u{E710}'), "New session", true, None)
                .on_hover_text("Pick the folder to start in. Recent folders first.");
            self.folder_popup(ui, &new);
            self.agent_menu(&new);
            ui.add_space(8.0);
            let open = ghost_button(ui, Some('\u{E8B7}'), "Open", false, None).on_hover_text(
                "Pick a file: a new session starts in its folder with the file ready at the prompt.",
            );
            if open.clicked() {
                self.actions.push(Action::OpenFile);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let gear = ghost_button(ui, Some('\u{E713}'), "", false, Some(30.0)).on_hover_text("Settings");
                if gear.clicked() {
                    let s = LoginStartup::for_this_user(&self.data);
                    let on = s.is_on();
                    self.login = (on, s.can_write());
                }
                Popup::menu(&gear)
                    .align(RectAlign::TOP_END)
                    .gap(6.0)
                    .frame(popup_frame(8, MENU_RIM))
                    .show(|ui| {
                        ui.set_min_width(176.0);
                        if menu_item(ui, "Settings…", Some("Ctrl+,"), None, true).clicked() {
                            self.actions.push(Action::OpenSettings);
                        }
                        let (on, can) = self.login;
                        if menu_item(ui, "Start at login", None, Some(on), can).clicked() {
                            self.actions.push(Action::SetLogin(!on));
                        }
                        menu_separator(ui);
                        if menu_item(ui, "Quit Kilno", None, None, true).clicked() {
                            self.actions.push(Action::Quit);
                        }
                    });
            });
        });
    }

    /// New session's folder picker, opening upward: recent folders, then Browse.
    fn folder_popup(&mut self, ui: &mut Ui, new: &egui::Response) {
        let choices = if new.clicked() || Popup::is_id_open(ui.ctx(), Popup::default_response_id(new)) {
            let exists = |f: &str| std::path::Path::new(f).is_dir();
            lock(&self.model).sidebar.recent_folders(&self.folder, 8, Utc::now(), exists)
        } else {
            Vec::new()
        };
        Popup::from_toggle_button_response(new)
            .align(RectAlign::TOP_START)
            .gap(6.0)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
            .frame(popup_frame(5, HAIRLINE))
            .show(|ui| {
                ui.set_min_width(272.0);
                ui.set_max_width(420.0);
                let w = ui.available_width().clamp(272.0, 420.0);
                let (r, _) = ui.allocate_exact_size(Vec2::new(w, 24.0), Sense::hover());
                ui.painter().text(
                    Pos2::new(r.left() + 7.0, r.top() + 4.0),
                    Align2::LEFT_TOP,
                    "Start a session in",
                    ui_font(10.5),
                    TEXT_FAINT,
                );
                egui::ScrollArea::vertical().max_height(264.0).id_salt("recent").show(ui, |ui| {
                    for c in &choices {
                        let (r, resp) = ui.allocate_exact_size(Vec2::new(w, 40.0), Sense::click());
                        if resp.hovered() {
                            ui.painter().rect_filled(r, 3, POPUP_HOVER);
                        }
                        let g = elided(ui, &c.name, ui_font(12.0), TEXT_PRIMARY, w - 14.0);
                        ui.painter().galley(Pos2::new(r.left() + 7.0, r.top() + 5.0), g, TEXT_PRIMARY);
                        let g = elided(ui, &c.full_path, mono(10.0), TEXT_FAINT, w - 14.0);
                        ui.painter().galley(Pos2::new(r.left() + 7.0, r.top() + 22.0), g, TEXT_FAINT);
                        if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                            self.actions.push(Action::NewIn(c.full_path.clone()));
                            ui.close();
                        }
                    }
                });
                ui.add_space(5.0);
                let (r, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
                ui.painter().hline(r.x_range().shrink(4.0), r.center().y, Stroke::new(1.0, HAIRLINE));
                ui.add_space(7.0);
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    let b = quiet_button_wide(ui, "Browse for a folder…", w - 8.0);
                    if b.clicked() {
                        self.actions.push(Action::Browse);
                        ui.close();
                    }
                });
                ui.add_space(2.0);
            });
    }

    /// Set to ask each time: "Claude Code" / "Codex" over New session; closing starts nothing.
    fn agent_menu(&mut self, new: &egui::Response) {
        if self.ask_agent.is_none() {
            return;
        }
        let cmd = std::mem::take(&mut self.open_agent_menu).then_some(SetOpenCommand::Bool(true));
        let popup = Popup::from_response(new)
            .id(Id::NULL.with("agent-menu"))
            .open_memory(cmd)
            .align(RectAlign::TOP_START)
            .gap(6.0)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
            .frame(popup_frame(8, MENU_RIM));
        let shown = popup.show(|ui| {
            ui.set_min_width(176.0);
            for kind in self.settings.used_agents() {
                if menu_item(ui, kind.name(), None, None, true).clicked() {
                    self.actions.push(Action::StartWith(kind));
                    ui.close();
                }
            }
        });
        if shown.is_none() && !self.actions.iter().any(|a| matches!(a, Action::StartWith(_))) {
            self.ask_agent = None;
        }
    }

    fn draw_terminal_side(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        if self.settings_open {
            self.draw_settings(ui);
            return;
        }
        let header = top_band(rect, HEADER_H);
        let body = Rect::from_min_max(Pos2::new(rect.left(), header.bottom()), rect.max);
        // The header veils its own band; this is the body's one veil (the terminal adds none).
        ui.painter().rect_filled(body, 0, crate::background::veil(TERMINAL_BG));
        ui.scope_builder(UiBuilder::new().max_rect(header), |ui| self.draw_header(ui));
        ui.scope_builder(UiBuilder::new().max_rect(body), |ui| self.draw_pane(ui));
    }

    fn draw_header(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        let p = ui.painter();
        p.rect_filled(rect, 0, crate::background::veil(SURFACE_BG));
        p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, HAIRLINE));
        let active = self.active_pane();
        let (status, title, has_id) = match active {
            Some(pane) => (Some(pane.status), self.pane_name(pane), pane.shared.session_id().is_some()),
            None => (None, String::new(), false),
        };
        let has_pane = active.is_some();
        let active_id = active.map(Pane::id);
        let written = active.map(|p| p.shared.written()).unwrap_or_default();
        let (images, images_added) = active.map(|p| p.shared.take_images()).unwrap_or_default();
        let front = ui.ctx().input(|i| i.focused);
        let inner = rect.shrink2(Vec2::new(14.0, 0.0));
        dot(p, Pos2::new(inner.left() + 3.5, inner.center().y), status_color(sidebar::status_key(status)));
        // Right to left: ✕, Branch from here, Attach files…, then (later phases) the image, page
        // and doc chips; the title and the status strip take what is left.
        let mut title_right = inner.right();
        ui.scope_builder(UiBuilder::new().max_rect(inner).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
            let close = quiet_button(ui, "✕", has_pane, Vec2::new(9.0, 4.0), true)
                .on_hover_text("Close this session. It drops back into the list below Open.");
            if close.clicked() {
                self.actions.push(Action::CloseActive);
            }
            ui.add_space(8.0);
            let files = quiet_button(ui, "Files", has_pane, Vec2::new(10.0, 5.0), true)
                .on_hover_text("The session's folder beside the terminal: read Markdown and pictures, edit a text file (.env, configs).");
            if files.clicked() {
                self.actions.push(Action::ToggleFiles);
            }
            ui.add_space(8.0);
            let fork = quiet_button(ui, "Branch from here", has_pane && has_id, Vec2::new(10.0, 5.0), true)
                .on_hover_text("Copies this conversation into a new session. The original is untouched.");
            if fork.clicked() {
                self.actions.push(Action::Branch);
            }
            ui.add_space(8.0);
            let attach = quiet_button(ui, "Attach files…", has_pane, Vec2::new(10.0, 5.0), true).on_hover_text(
                "Puts the paths of the files you pick at the prompt, for Claude to read. Ctrl+V does the same for files copied in Explorer.",
            );
            if attach.clicked() {
                self.actions.push(Action::Attach);
            }
            ui.add_space(6.0);
            // The chips, in the strip's quiet style; the details are in the tooltips.
            ui.spacing_mut().item_spacing.x = 2.0;
            self.image_chip.chip(ui, &images, images_added, front);
            self.image_chip.pictures.keep(&images, self.image_chip.big.as_deref());
            let pages: Vec<&std::path::PathBuf> = written.pages.iter().filter(|p| p.is_file()).collect();
            if let Some(page) = written.pages.first() {
                let tip = format!(
                    "{}\n{}\nClick to open it in your browser. Right-click for the session's other pages.",
                    file_name(page),
                    page.display()
                );
                let tint = self.page_flash(ui.ctx(), active_id, written.new_page);
                let chip = chip_button(ui, '\u{E9D2}', None, tint).on_hover_text(tip);
                if chip.clicked() {
                    self.actions.push(Action::OpenPage(page.clone()));
                }
                // The session's pages, the newest first, each opening in the browser.
                chip.context_menu(|ui| {
                    ui.set_min_width(176.0);
                    for p in &pages {
                        if menu_item(ui, &file_name(p), None, None, true).clicked() {
                            self.actions.push(Action::OpenPage((*p).clone()));
                            ui.close();
                        }
                    }
                });
            }
            if let Some(doc) = &written.doc {
                let tip = format!("{}\n{}\nClick to read it here, again to close it.", file_name(doc), doc.display());
                if chip_button(ui, '\u{F000}', None, None).on_hover_text(tip).clicked() {
                    self.actions.push(Action::ToggleDoc);
                }
            }
            title_right = ui.min_rect().left();
        });
        let x = inner.left() + 7.0 + 10.0;
        let g = elided(ui, &title, ui_font(12.0), TEXT_MUTED, title_right - x - 10.0);
        let y = rect.center().y - g.size().y / 2.0;
        let title_w = if title.is_empty() { 0.0 } else { g.size().x };
        ui.painter().galley(Pos2::new(x, y), g, TEXT_MUTED);
        // The status strip follows the title (margin 10) and is cut off, not squeezed, by the
        // buttons.
        let strip = self.status_frame.strip.as_ref().filter(|_| has_pane);
        status_view::draw_strip(ui, strip, self.settings.theme, x + title_w + 10.0, title_right, rect.center().y);
    }

    fn draw_pane(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        let ctx = ui.ctx().clone();
        let Some(i) = self.active.and_then(|id| self.panes.iter().position(|p| p.id() == id)) else {
            let text = if lock(&self.model).roots_exist { EMPTY_PANES } else { sidebar::EMPTY_STATE };
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, ui_font(13.0), TEXT_FAINT);
            return;
        };
        let font = self.font.clone();
        let list_focus = self.list_focus;
        self.doc_seq += 1;
        let seq = self.doc_seq;
        let pane = &mut self.panes[i];
        // An open viewer follows the session's newest doc and its edits.
        if let Some(v) = pane.doc.as_mut() {
            let w = pane.shared.written();
            v.follow(w.doc.as_deref(), w.generation, &pane.cwd, &mut self.md_cache, seq);
        }
        let (term_rect, doc_rect) = match (&pane.doc, &pane.files) {
            (Some(v), _) => doc_view::layout(rect, v.mode, pane.doc_share),
            (None, Some(_)) => doc_view::layout(rect, doc_view::Mode::Split, pane.doc_share),
            (None, None) => (Some(rect), rect),
        };
        let mut links = Vec::new();
        let mut paste = false;
        // In full mode the viewer covers the terminal, which keeps its size and is not drawn.
        if let Some(term_rect) = term_rect {
            let focus_now = std::mem::take(&mut self.focus_terminal);
            let (resp, out) = ui
                .scope_builder(UiBuilder::new().max_rect(term_rect), |ui| {
                    ui.push_id(widget::view_id(pane.id()), |ui| widget::show(ui, &pane.shared.term, &mut pane.view, &font, true))
                        .inner
                })
                .inner;
            if focus_now || (!resp.has_focus() && !list_focus && ctx.memory(|m| m.focused().is_none())) {
                resp.request_focus();
            }
            if resp.has_focus() {
                self.list_focus = false;
            }
            for bytes in &out.input {
                pane.shared.type_keys(bytes);
            }
            if pane.doc.is_none() && pane.files.is_none() {
                self.grid = pane.shared.term.size();
            }
            if let Some(size) = out.resized {
                pane.shared.resize(size);
            }
            if let Some(text) = out.copy {
                ctx.copy_text(text);
            }
            links.extend(out.link);
            // A Ctrl+clicked path shows in Kilno's own Files panel, beside this terminal.
            if let Some(path) = out.open_path {
                pane.doc = None;
                pane.files.get_or_insert_with(|| crate::files_panel::FilesPanel::new(&pane.cwd)).reveal(ui.ctx(), &path);
            }
            paste = out.paste_requested;
        }
        if let Some(v) = pane.doc.as_mut() {
            let out = doc_view::show(ui, doc_rect, rect, v, &mut pane.doc_share, &mut self.md_cache);
            if out.close {
                pane.doc = None;
            }
            if out.close || out.focus_terminal {
                self.focus_terminal = true;
            }
            links.extend(out.links);
        }
        if let Some(f) = pane.files.as_mut() {
            let out = crate::files_panel::show(ui, doc_rect, rect, f, &mut pane.doc_share, &mut self.md_cache);
            if out.close {
                pane.files = None;
                self.focus_terminal = true;
            }
            links.extend(out.links);
        }
        for link in links {
            self.actions.push(Action::AskLink(link));
        }
        if self.image_chip.big_view(ui, rect).focus_terminal {
            self.focus_terminal = true;
        }
        if paste {
            let p = &self.panes[i];
            self.paste(p);
        }
    }

    /// The Settings page (SettingsPane), in the terminal side's place, panes hidden but running.
    /// What it changes is saved and applied at once (MainWindow.SettingsChanged).
    fn draw_settings(&mut self, ui: &mut Ui) {
        use crate::settings_page::{Info, Out};
        let info = Info { shot_key_taken: self.screenshot_key_taken, newer: self.update.to, waiting: self.update.waiting };
        for out in self.settings_page.show(ui, &self.settings, &info) {
            self.actions.push(match out {
                Out::Close => Action::CloseSettings,
                Out::Changed(s) => Action::ApplySettings(s),
                // Settings › Agents is not a page: it opens the setup window.
                Out::OpenSetup => Action::OpenSetup,
                Out::Update => Action::Update,
            });
        }
    }

    /// The 7 px grab band with the hairline down its middle; hover or drag shows 2 px Working.
    fn splitter(&mut self, ui: &mut Ui, rect: Rect, total: f32) {
        let resp = ui.interact(rect, Id::NULL.with("splitter"), Sense::drag()).on_hover_cursor(CursorIcon::ResizeHorizontal);
        if resp.dragged() {
            self.sidebar_w += resp.drag_delta().x;
        }
        self.sidebar_w = clamp_sidebar(self.sidebar_w, total);
        let x = rect.center().x;
        if resp.hovered() || resp.dragged() {
            ui.painter().vline(x, rect.y_range(), Stroke::new(2.0, WORKING));
        } else {
            ui.painter().vline(x, rect.y_range(), Stroke::new(1.0, HAIRLINE));
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let term_focused = self.active.is_some() && !self.list_focus && ctx.memory(|m| m.focused().is_some())
            && !ctx.memory(|m| m.focused() == Some(Id::NULL.with("search")))
            && self.rename.is_none();
        // Ctrl+, opens Settings, Esc closes it; inside a terminal these keys are the agent's.
        let (comma, esc, f2, enter, del) = ctx.input(|i| {
            (
                i.modifiers.command_only() && i.key_pressed(Key::Comma),
                i.key_pressed(Key::Escape),
                i.key_pressed(Key::F2),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::Delete),
            )
        });
        if comma && (!term_focused || self.settings_open) {
            self.actions.push(Action::OpenSettings);
        }
        if esc && self.settings_open {
            self.actions.push(Action::CloseSettings);
        }
        if self.list_focus && self.rename.is_none() {
            if let Some(id) = self.selected.clone() {
                if f2 {
                    self.actions.push(Action::StartRename(id));
                } else if enter {
                    self.actions.push(Action::Open(id));
                } else if del {
                    let hidden = lock(&self.model).sidebar.get(&id).is_some_and(|r| r.hidden && !r.is_open());
                    if !hidden {
                        self.actions.push(Action::Hide(id));
                    }
                }
            }
        }
    }

    fn handle_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                AppEvent::Status(id, s) => {
                    let mut was = None;
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id() == id) {
                        was = Some(p.status);
                        p.status = s;
                    }
                    let Some(was) = was else { continue };
                    let name = {
                        let mut m = lock(&self.model);
                        m.sidebar.set_pane_status(id, s);
                        m.sidebar.by_pane(id).map(|r| r.name().to_string())
                    };
                    let name = name.or_else(|| self.pane(id).map(|p| p.title.clone())).unwrap_or_default();
                    // A status change fires once per real change, so a repeat cannot notify twice.
                    if was != s {
                        self.island.pane_status(id, &name, was, s);
                    }
                    self.update_when_idle();
                }
                AppEvent::Handoff(folder) => {
                    if folder.is_empty() {
                        self.reveal(ctx);
                    } else {
                        self.adopt_folder(&folder, None);
                    }
                }
                AppEvent::UpdateFound(v) => {
                    // A wait already under way keeps the release it was started for.
                    if self.settings.update_check && !self.update.waiting {
                        self.update.to = Some(v);
                    }
                }
                AppEvent::Island(cmd) => match cmd {
                    IslandCmd::ShowSession(id) => {
                        if self.pane(id).is_some() {
                            let key = lock(&self.model).sidebar.by_pane(id).map(|r| r.session_id.clone());
                            self.selected = key.or(self.selected.take());
                            self.activate(id);
                        }
                    }
                    IslandCmd::SendImage(pane, path) => self.send_image(pane, path),
                    IslandCmd::Adopt(folder, prefill) => self.adopt_folder(&folder.to_string_lossy(), prefill),
                    IslandCmd::Quit => self.quit(ctx),
                },
            }
        }
    }

    fn set_visibility(&self) {
        for p in &self.panes {
            let v = self.shown && !self.settings_open && self.active == Some(p.id());
            p.shared.visible.store(v, Ordering::Relaxed);
        }
        self.status.set_shown(self.shown);
    }

    /// ShowStatusBar: the active pane's strip (Claude's from its status file, Codex's from its
    /// transcript) with its folder's git state, and the footer's limits: the active pane's
    /// agent's account, or with no pane Codex's when Claude is not in use. OpenCode reports no
    /// account limits, so its panes show none.
    fn status_frame(&mut self) -> StatusFrame {
        let now = Utc::now();
        let kind = self.active_pane().map_or(if self.settings.use_claude { AgentKind::Claude } else { AgentKind::Codex }, |p| p.agent);
        let codex = kind != AgentKind::Claude;
        let limits = match kind {
            AgentKind::Claude => self.status.claude_limits(),
            AgentKind::Codex => self.codex_limits(),
            AgentKind::OpenCode => None,
        };
        let strip = self.active_pane().and_then(|pane| {
            let id = pane.shared.session_id()?;
            let session = if codex {
                lock(&self.model).sidebar.get(&id).and_then(|r| r.info.as_ref()).and_then(|i| i.status.clone())
            } else {
                self.status.store.get(&id)
            };
            let git = self.status.git.get(&pane.cwd.to_string_lossy());
            strip::header(session.as_ref(), git.as_ref(), limits.as_ref(), &self.settings, now)
        });
        let mut meters = strip::footer(limits.as_ref(), &self.settings, now);
        if let Some(l) = &limits {
            let as_of = l.updated.with_timezone(&chrono::Local).format("%H:%M");
            for m in &mut meters {
                m.tip.push_str(&format!("\nAs of {as_of}. Click to read again."));
            }
        }
        StatusFrame { strip, meters }
    }

    /// The newest limits any Codex transcript reported, not the open session's (as old as its
    /// last turn). Recomputed only when the rows change.
    fn codex_limits(&mut self) -> Option<AccountLimits> {
        let m = lock(&self.model);
        if self.codex_limits.0 != m.generation {
            let statuses = m
                .sidebar
                .rows()
                .filter_map(|r| r.info.as_ref())
                .filter(|i| i.agent == AgentKind::Codex)
                .filter_map(|i| i.status.as_ref());
            self.codex_limits = (m.generation, parse::newest(statuses));
        }
        self.codex_limits.1.clone()
    }
}

/// "RAM · CPU   182 MB · 3%"; hovering lists Kilno and each session.
fn usage_line(ui: &mut Ui, r: Rect, u: &crate::usage::Usage, with_sessions: bool) {
    let p = ui.painter();
    let label = if with_sessions { "RAM · CPU (Kilno + sessions)" } else { "RAM · CPU (Kilno)" };
    p.text(Pos2::new(r.left(), r.center().y), Align2::LEFT_CENTER, label, ui_font(11.5), MUTED_AA);
    p.text(Pos2::new(r.right(), r.center().y), Align2::RIGHT_CENTER, crate::usage::line(&u.total), ui_font(11.5), TEXT_PRIMARY);
    let resp = ui.interact(r, Id::NULL.with("usage-line"), egui::Sense::hover());
    resp.on_hover_ui(|ui| {
        egui::Grid::new("usage-parts").num_columns(2).spacing(Vec2::new(18.0, 4.0)).show(ui, |ui| {
            for part in &u.parts {
                ui.label(&part.label);
                ui.label(crate::usage::line(part));
                ui.end_row();
            }
        });
        if !with_sessions {
            ui.label(egui::RichText::new("Settings › Status bar can count the sessions too.").color(MUTED_AA).size(11.0));
        }
    });
}

/// The main window's handle, for the island's "Kilno is in front".
fn main_hwnd(cc: &eframe::CreationContext<'_>) -> isize {
    #[cfg(windows)]
    if let Ok(h) = raw_window_handle::HasWindowHandle::window_handle(cc) {
        if let raw_window_handle::RawWindowHandle::Win32(w) = h.as_raw() {
            return w.hwnd.get();
        }
    }
    let _ = cc;
    0
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Files copied in Explorer, pasted (TP:342-368): each image as a bracketed path and a space
/// (the agent attaches it as [Image #n]), then the other files typed, quoted when they hold a
/// space, with one trailing space. Also returns the images.
fn file_paste(files: &[String]) -> (Vec<u8>, Vec<std::path::PathBuf>) {
    let (images, others): (Vec<&String>, Vec<&String>) =
        files.iter().partition(|f| kilno_core::images::is_image(std::path::Path::new(f)));
    let mut out = Vec::new();
    for i in &images {
        out.extend(bracketed_path(i));
        out.push(b' ');
    }
    out.extend(kilno_core::text::insert_paths(&others).into_bytes());
    (out, images.into_iter().map(std::path::PathBuf::from).collect())
}

fn clamp_sidebar(w: f32, total: f32) -> f32 {
    let max = SIDEBAR_MAX.min(total - SPLITTER_W - TERMINAL_MIN).max(SIDEBAR_MIN);
    w.clamp(SIDEBAR_MIN, max)
}

/// Section header (Open / Today / Earlier): chevron, label 11.5 semibold, count 11 faint.
fn section_header(ui: &mut Ui, label: &str, count: usize, open: bool) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 30.0), Sense::click());
    let p = ui.painter();
    let hovered = resp.hovered();
    if hovered {
        p.rect_filled(rect, 0, crate::background::shade(ROW_HOVER, 0.05));
    }
    let cy = rect.center().y;
    chevron(p, Pos2::new(rect.left() + 10.0 + 3.0, cy), open, if hovered { TEXT_MUTED } else { TEXT_FAINT });
    p.text(Pos2::new(rect.left() + 24.0, cy), Align2::LEFT_CENTER, label, semibold(11.5), TEXT_PRIMARY);
    p.text(Pos2::new(rect.right() - 12.0, cy), Align2::RIGHT_CENTER, count.to_string(), ui_font(11.0), TEXT_FAINT);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Folder sub-header: margin 26,10,10,4; name 11 TextMuted and the count.
fn folder_header(ui: &mut Ui, name: &str, count: usize) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 29.0), Sense::hover());
    let y = rect.top() + 10.0;
    let c = ui.painter().layout_no_wrap(count.to_string(), ui_font(11.0), TEXT_FAINT);
    let cw = c.size().x;
    ui.painter().galley(Pos2::new(rect.right() - 10.0 - cw, y), c, TEXT_FAINT);
    let g = elided(ui, name, ui_font(11.0), TEXT_MUTED, rect.width() - 26.0 - 10.0 - cw - 8.0);
    ui.painter().galley(Pos2::new(rect.left() + 26.0, y), g, TEXT_MUTED);
}

/// TinyCombo: 11 px muted text and a small down chevron, RaisedBg on hover.
fn tiny_combo(ui: &mut Ui, text: &str) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), ui_font(11.0), TEXT_MUTED);
    let size = Vec2::new(g.size().x + 14.0 + 14.0, 22.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, 4, crate::background::shade(RAISED_BG, 0.07));
    }
    let gy = rect.center().y - g.size().y / 2.0;
    let gx = rect.left() + 7.0;
    let tw = g.size().x;
    p.galley(Pos2::new(gx, gy), g, TEXT_MUTED);
    let c = Pos2::new(gx + tw + 10.0, rect.center().y + 1.0);
    let s = Stroke::new(1.3, TEXT_FAINT);
    p.line_segment([c + Vec2::new(-4.0, -2.0), c + Vec2::new(0.0, 2.0)], s);
    p.line_segment([c + Vec2::new(0.0, 2.0), c + Vec2::new(4.0, -2.0)], s);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn tiny_popup(button: &egui::Response, add: impl FnOnce(&mut Ui)) {
    Popup::menu(button).align(RectAlign::BOTTOM_START).gap(3.0).frame(popup_frame(5, HAIRLINE)).show(|ui| {
        ui.set_min_width(button.rect.width().max(100.0));
        add(ui);
    });
}

/// TinyComboItem: 11 px, padding 8,5, hover #2B313C, the chosen one in Working.
fn tiny_item(ui: &mut Ui, label: &str, chosen: bool) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 24.0), Sense::click());
    let hovered = resp.hovered();
    if hovered {
        ui.painter().rect_filled(rect, 3, POPUP_HOVER);
    }
    let c = if chosen {
        WORKING
    } else if hovered {
        TEXT_PRIMARY
    } else {
        TEXT_MUTED
    };
    ui.painter().text(Pos2::new(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, label, ui_font(11.0), c);
    resp
}

/// The folder picker's full-width QuietButton.
fn quiet_button_wide(ui: &mut Ui, text: &str, w: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 28.0), Sense::click());
    let hovered = resp.hovered();
    if hovered {
        ui.painter().rect_filled(rect, 4, RAISED_BG);
    }
    ui.painter().rect_stroke(rect, 4, Stroke::new(1.0, HAIRLINE), StrokeKind::Inside);
    let c = if hovered { TEXT_PRIMARY } else { TEXT_MUTED };
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, ui_font(12.0), c);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

impl eframe::App for KilnoApp {
    /// Runs while hidden too: pane events, the reveal request, and close-only-hides.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_events(ctx);
        if self.reveal.swap(false, Ordering::SeqCst) {
            self.reveal(ctx);
        }
        // The setup window's outcome (it wakes this pass) while the main window is hidden.
        if self.setup.as_ref().is_some_and(|s| setup_window::lock(s).outcome.is_some()) {
            ctx.send_viewport_cmd(ViewportCommand::RequestPaintWhileHidden);
            ctx.request_repaint();
        }
        // The window's X only hides: sessions keep running, the island brings it back.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            self.shown = false;
            Popup::close_all(ctx);
        }
        self.set_visibility();
        // RAM and CPU: measured only while the window shows and the option is on.
        let mode = self.settings.resource_usage.as_str();
        let sessions = if mode == "all" {
            self.panes.iter().filter_map(|p| Some((self.pane_name(p), p.shared.pid()?))).collect()
        } else {
            Vec::new()
        };
        self.usage.set(self.shown && mode != "off", sessions);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if !self.first_frame_done {
            self.first_frame_done = true;
            crate::mark("first frame");
            // eframe shows the root window after its first frame whatever the builder said; a
            // login start (--background) is the island only, so it goes straight back.
            if !self.shown {
                ui.ctx().send_viewport_cmd(ViewportCommand::Visible(false));
            }
        }
        let ctx = ui.ctx().clone();
        static FRAME_LOG: std::sync::OnceLock<Option<std::ffi::OsString>> = std::sync::OnceLock::new();
        if let Some(path) = FRAME_LOG.get_or_init(|| std::env::var_os("KILNO_FRAME_LOG")) {
            // BENCH.md: when and why frames are drawn while idle.
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let cause = ctx.repaint_causes().iter().map(|c| c.to_string()).collect::<Vec<_>>().join("; ");
                let _ = writeln!(f, "{:?} {cause}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis());
            }
        }
        self.shown = ctx.input(|i| i.viewport().visible()).unwrap_or(true) && self.shown;
        self.setup_frame(&ctx);
        if !self.shown {
            // Here only for the setup window (or a paint asked for while hidden): nothing of
            // the main window is drawn.
            self.run_actions(&ctx, frame);
            return;
        }
        self.handle_keys(&ctx);
        // Debug builds: KILNO_DEV_FILES opens the Files panel on the first pane, and the file it
        // names (relative to the pane's folder) when that is a file (rendering checks).
        if cfg!(debug_assertions) {
            if let Some(pane) = self.panes.first_mut().filter(|p| p.files.is_none()) {
                if let Some(file) = std::env::var_os("KILNO_DEV_FILES").filter(|_| !self.dev_files_done) {
                    self.dev_files_done = true;
                    let mut f = crate::files_panel::FilesPanel::new(&pane.cwd);
                    if pane.cwd.join(&file).is_file() {
                        f.open_file(&ctx, &pane.cwd.join(file));
                    }
                    pane.files = Some(f);
                }
            }
        }
        self.status_frame = self.status_frame();
        let full = ui.max_rect();
        self.sidebar_w = clamp_sidebar(self.sidebar_w, full.width());
        let side = Rect::from_min_size(full.min, Vec2::new(self.sidebar_w, full.height()));
        let split = Rect::from_min_size(Pos2::new(side.right(), full.top()), Vec2::new(SPLITTER_W, full.height()));
        let term = Rect::from_min_max(Pos2::new(split.right(), full.top()), full.max);
        self.background.paint(ui, full, self.settings.background_image.as_deref(), self.settings.background_opacity);
        let look = crate::title_bar::look(&self.settings, self.background.accent);
        let session = if self.settings_open { None } else { self.active_pane().map(|p| self.pane_name(p)) };
        let title = crate::title_bar::title(&self.settings, session.as_deref());
        self.title_bar.apply(&ctx, self.hwnd, look, title);
        ui.scope_builder(UiBuilder::new().max_rect(side), |ui| self.draw_sidebar(ui));
        ui.painter().rect_filled(split, 0, Color32::TRANSPARENT);
        self.splitter(ui, split, full.width());
        ui.scope_builder(UiBuilder::new().max_rect(term), |ui| self.draw_terminal_side(ui));
        self.update_confirm(&ctx);
        self.link_confirm(&ctx);
        self.run_actions(&ctx, frame);
        self.set_visibility();
        // Ages ("3 min ago") and the Today bucket move with the clock, and the strip's and the
        // footer's countdowns and staleness (at most every 30 s while visible).
        ctx.request_repaint_after(crate::status::CLOCK);
        if let Some(next) = crate::latency::on_frame(self.active_pane().map(|p| &*p.shared)) {
            ctx.request_repaint_after(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_files_put_images_first_as_attachments_then_type_the_rest() {
        let files = [r"C:\a b.txt".to_string(), r"C:\shot.PNG".to_string(), r"C:\c.md".to_string()];
        let (bytes, images) = file_paste(&files);
        assert_eq!(String::from_utf8(bytes).unwrap(), "\x1b[200~C:\\shot.PNG\x1b[201~ \"C:\\a b.txt\" C:\\c.md ");
        assert_eq!(images, vec![std::path::PathBuf::from(r"C:\shot.PNG")]);
        assert_eq!(file_paste(&[]).0, Vec::<u8>::new());
    }

    #[test]
    fn sidebar_width_stays_in_its_range_and_leaves_the_terminal_its_minimum() {
        assert_eq!(clamp_sidebar(100.0, 1280.0), SIDEBAR_MIN);
        assert_eq!(clamp_sidebar(900.0, 1280.0), SIDEBAR_MAX);
        assert_eq!(clamp_sidebar(500.0, 760.0), 760.0 - 7.0 - 360.0);
        assert_eq!(clamp_sidebar(300.0, 400.0), SIDEBAR_MIN);
    }
}
