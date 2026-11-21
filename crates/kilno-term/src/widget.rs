//! The egui widget that draws a [`Terminal`] and turns input into bytes.
//!
//! It never writes to the PTY itself: [`ViewOutput`] hands the bytes to the pane, which also
//! feeds them to the status tracker (a key answers a question; output right after a key is
//! echo, not work).

use crate::input::{self, MouseAction, MouseButton};
use crate::palette::{self, resolve};
use crate::{GridSize, Terminal};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use egui::{
    Align2, Color32, CursorIcon, Event, EventFilter, FontId, Id, Key, Modifiers, PointerButton, Pos2,
    Rect, Response, Sense, Stroke, Ui, Vec2,
};
use std::time::{Duration, Instant};
use unicode_normalization::UnicodeNormalization;

/// Padding around the grid (terminal.html: 10 top, 4 right, 6 bottom, 12 left).
const PAD_LEFT: f32 = 12.0;
const PAD_TOP: f32 = 10.0;
const PAD_RIGHT: f32 = 4.0;
const PAD_BOTTOM: f32 = 6.0;
const LINE_HEIGHT: f32 = 1.25;
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(60);
const BLINK: Duration = Duration::from_millis(600);
/// The bar stops blinking (stays solid) after this long without input or output in the pane,
/// so an idle pane asks for no repaints at all (PORTING.md D14).
const BLINK_IDLE: Duration = Duration::from_secs(5);
const MULTI_CLICK: Duration = Duration::from_millis(400);

/// Per-pane widget state kept by the app between frames.
#[derive(Default)]
pub struct ViewState {
    pending_resize: Option<(GridSize, Instant)>,
    preedit: String,
    mouse_button: Option<MouseButton>,
    selecting: bool,
    last_click: Option<(Instant, Point, u8)>,
    blink_epoch: Option<Instant>,
    /// The terminal generation last seen, and when the pane last had input or output.
    seen_generation: u64,
    active_at: Option<Instant>,
    had_focus: bool,
    scroll_accum: f32,
    /// The pane's folder: relative paths in its text are looked up under it.
    pub cwd: Option<std::path::PathBuf>,
    /// The local path under the pointer while Ctrl is held: (display row, first column, end
    /// column), underlined.
    path_hover: Option<(i32, usize, usize)>,
    /// The last candidate looked up and what it resolved to, so a held pointer does not ask
    /// the disk every frame.
    path_cache: Option<(String, Option<std::path::PathBuf>)>,
}

/// What happened this frame that the pane must act on.
#[derive(Default, Debug)]
pub struct ViewOutput {
    /// Bytes to write to the PTY, one entry per key / report / commit, in order.
    pub input: Vec<Vec<u8>>,
    /// Ctrl+V (or Ctrl+Shift+V): the pane reads the clipboard (text or files) and pastes.
    pub paste_requested: bool,
    /// Text to put on the clipboard (Ctrl+C with a selection, or an OSC 52 write).
    pub copy: Option<String>,
    /// The grid settled on a new size; the PTY must be resized to it.
    pub resized: Option<GridSize>,
    /// An OSC 8 hyperlink was clicked; the pane confirms before opening it.
    pub link: Option<String>,
    /// A local path was Ctrl+clicked: the app shows it in its Files panel.
    pub open_path: Option<std::path::PathBuf>,
    /// The widget gained (true) or lost (false) keyboard focus this frame.
    pub focus_changed: Option<bool>,
}

/// Fonts and metrics for drawing.
#[derive(Clone)]
pub struct TermFont {
    pub font: FontId,
}

impl Default for TermFont {
    fn default() -> Self {
        Self { font: FontId::monospace(13.0) }
    }
}

fn c32(c: Rgb) -> Color32 {
    Color32::from_rgb(c.r, c.g, c.b)
}

/// Cell size in points for this font.
pub fn cell_size(ui: &Ui, font: &FontId) -> Vec2 {
    let w = ui.fonts_mut(|f| f.glyph_width(font, 'M'));
    Vec2::new(w.max(1.0), (font.size * LINE_HEIGHT).round().max(1.0))
}

/// Grid that fits `avail` (points) with the C# padding.
pub fn fit(avail: Vec2, cell: Vec2) -> GridSize {
    let cols = ((avail.x - PAD_LEFT - PAD_RIGHT) / cell.x).floor().max(2.0) as u16;
    let rows = ((avail.y - PAD_TOP - PAD_BOTTOM) / cell.y).floor().max(1.0) as u16;
    GridSize { cols, rows }
}

/// Chars the terminal font draws itself; others (CJK, emoji, symbols) are placed one by one so
/// a fallback font's different advance cannot push the rest of the row out of its cells.
fn in_run(c: char) -> bool {
    let u = c as u32;
    u < 0x0250 || (0x1E00..0x1F00).contains(&u) || (0x2010..0x2070).contains(&u) || (0x2500..0x25A0).contains(&u)
}

/// What a cell shows: its char plus any combining marks, composed (NFC) so a decomposed
/// letter draws as its precomposed form; egui does no shaping (PORTING.md §4.6).
fn cell_text(c: char, zerowidth: Option<&[char]>) -> String {
    let mut s = String::new();
    s.push(c);
    match zerowidth {
        Some(z) if !z.is_empty() => {
            s.extend(z);
            s.nfc().collect()
        }
        _ => s,
    }
}

/// Draw the terminal in the available space and handle its input.
pub fn show(ui: &mut Ui, term: &Terminal, state: &mut ViewState, tf: &TermFont, visible: bool) -> (Response, ViewOutput) {
    let mut out = ViewOutput::default();
    let avail = ui.available_size();
    let (rect, response) = ui.allocate_exact_size(avail, Sense::click_and_drag());
    let id = response.id;
    let cell = cell_size(ui, &tf.font);
    let origin = rect.min + Vec2::new(PAD_LEFT, PAD_TOP);

    // Resize, debounced 60 ms after the last change, never while minimised (a minimised window
    // has been seen to report a false, narrower size) and never to the same size.
    let want = fit(avail, cell);
    let minimized = ui.input(|i| i.viewport().minimized == Some(true));
    if !minimized && avail.x > 1.0 && avail.y > 1.0 && want != term.size() {
        let now = Instant::now();
        match state.pending_resize {
            Some((s, at)) if s == want && now.duration_since(at) >= RESIZE_DEBOUNCE => {
                term.resize(want, (cell.x.round() as u16, cell.y.round() as u16));
                out.resized = Some(want);
                state.pending_resize = None;
            }
            Some((s, _)) if s == want => ui.ctx().request_repaint_after(RESIZE_DEBOUNCE),
            _ => {
                state.pending_resize = Some((want, now));
                ui.ctx().request_repaint_after(RESIZE_DEBOUNCE);
            }
        }
    } else {
        state.pending_resize = None;
    }

    let focused = response.has_focus();
    if focused != state.had_focus {
        state.had_focus = focused;
        out.focus_changed = Some(focused);
        if let Some(r) = term.focus_report(focused) {
            out.input.push(r.to_vec());
        }
        state.blink_epoch = Some(Instant::now());
    }
    if response.clicked() || response.drag_started() {
        response.request_focus();
    }
    if focused {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
            )
        });
    }

    // Output (anything that changed the grid) counts as activity for the blink.
    let generation = term.generation();
    if generation != state.seen_generation || state.active_at.is_none() {
        state.seen_generation = generation;
        state.active_at = Some(Instant::now());
    }
    // A synchronized update the app never ended shows once its timeout passes.
    if let Some(at) = term.sync_deadline() {
        let now = Instant::now();
        if at <= now {
            term.flush_sync();
        } else {
            ui.ctx().request_repaint_after(at - now);
        }
    }

    let mode = term.mode();
    handle_pointer(ui, term, state, &response, rect, origin, cell, mode, &mut out);
    if focused {
        handle_keys(ui, term, state, mode, &mut out);
    }
    for text in term.take_clipboard_requests() {
        out.copy = Some(text);
    }

    if visible {
        paint(ui, term, state, rect, origin, cell, tf, focused, &response);
        if let Some((row, s, e)) = state.path_hover {
            let y = origin.y + (row + 1) as f32 * cell.y - 1.5;
            let x = |c: usize| origin.x + c as f32 * cell.x;
            ui.painter_at(rect).hline(x(s)..=x(e), y, egui::Stroke::new(1.0, c32(palette::FOREGROUND)));
        }
    }
    if focused {
        // The IME candidate window sits at the cursor cell.
        let t = term.lock();
        let cur = t.grid().cursor.point;
        drop(t);
        let pos = origin + Vec2::new(cur.column.0 as f32 * cell.x, cur.line.0 as f32 * cell.y);
        let cursor_rect = Rect::from_min_size(pos, Vec2::new(1.0, cell.y));
        ui.output_mut(|o| {
            o.ime = Some(egui::output::IMEOutput {
                purpose: egui::IMEPurpose::Normal,
                rect,
                cursor_rect,
                should_interrupt_composition: false,
            })
        });
    }
    (response, out)
}

fn point_at(pos: Pos2, origin: Pos2, cell: Vec2, size: GridSize, display_offset: usize) -> (Point, Side) {
    let x = ((pos.x - origin.x) / cell.x).max(0.0);
    let y = ((pos.y - origin.y) / cell.y).max(0.0);
    let col = (x.floor() as usize).min(size.cols as usize - 1);
    let row = (y.floor() as usize).min(size.rows as usize - 1);
    let side = if x.fract() < 0.5 { Side::Left } else { Side::Right };
    (Point::new(Line(row as i32 - display_offset as i32), Column(col)), side)
}

#[allow(clippy::too_many_arguments)]
fn handle_pointer(
    ui: &Ui,
    term: &Terminal,
    state: &mut ViewState,
    response: &Response,
    rect: Rect,
    origin: Pos2,
    cell: Vec2,
    mode: TermMode,
    out: &mut ViewOutput,
) {
    let size = term.size();
    let (events, mods) = ui.input(|i| (i.events.clone(), i.modifiers));
    let hovered = response.hovered() || response.dragged();
    // Shift forces a selection while the app reports the mouse (xterm behaviour).
    let report = mode.intersects(TermMode::MOUSE_MODE) && !mods.shift;
    let offset = term.lock().grid().display_offset();

    for ev in &events {
        match ev {
            Event::PointerButton { pos, button, pressed, modifiers } if rect.contains(*pos) || state.selecting => {
                let (pt, side) = point_at(*pos, origin, cell, size, offset);
                let b = match button {
                    PointerButton::Primary => MouseButton::Left,
                    PointerButton::Middle => MouseButton::Middle,
                    PointerButton::Secondary => MouseButton::Right,
                    _ => continue,
                };
                // Ctrl+click on a local path shows it in the Files panel, even while the app reports the mouse.
                if b == MouseButton::Left && modifiers.ctrl && rect.contains(*pos) {
                    if let Some((path, _, _)) = path_at(term, state, pt) {
                        if *pressed {
                            out.open_path = Some(path);
                        }
                        continue;
                    }
                }
                if report {
                    let act = if *pressed { MouseAction::Press } else { MouseAction::Release };
                    let vp = (pt.column.0, (pt.line.0 + offset as i32).max(0) as usize);
                    if let Some(bytes) = input::mouse_report(Some(b), act, *modifiers, vp.0, vp.1, mode) {
                        out.input.push(bytes);
                    }
                    state.mouse_button = pressed.then_some(b);
                    continue;
                }
                if b != MouseButton::Left {
                    continue; // no context menu, no right-click paste (C# had none)
                }
                if *pressed {
                    if let Some(link) = link_at(term, pt) {
                        out.link = Some(link);
                        continue;
                    }
                    let now = Instant::now();
                    let count = match state.last_click {
                        Some((t, p, n)) if now.duration_since(t) < MULTI_CLICK && p == pt => n % 3 + 1,
                        _ => 1,
                    };
                    state.last_click = Some((now, pt, count));
                    let mut t = term.lock();
                    if modifiers.shift && t.selection.is_some() {
                        if let Some(s) = t.selection.as_mut() {
                            s.update(pt, side);
                        }
                    } else {
                        let ty = match count {
                            2 => SelectionType::Semantic,
                            3 => SelectionType::Lines,
                            _ if modifiers.alt => SelectionType::Block,
                            _ => SelectionType::Simple,
                        };
                        t.selection = Some(Selection::new(ty, pt, side));
                    }
                    drop(t);
                    state.selecting = true;
                    term.touch();
                } else {
                    state.selecting = false;
                    let mut t = term.lock();
                    if t.selection.as_ref().is_some_and(|s| s.is_empty()) {
                        t.selection = None;
                    }
                    drop(t);
                    term.touch();
                }
            }
            Event::PointerMoved(pos) => {
                let (pt, side) = point_at(*pos, origin, cell, size, offset);
                if report && hovered {
                    let vp = (pt.column.0, (pt.line.0 + offset as i32).max(0) as usize);
                    if let Some(bytes) = input::mouse_report(state.mouse_button, MouseAction::Motion, mods, vp.0, vp.1, mode) {
                        out.input.push(bytes);
                    }
                } else if state.selecting {
                    let mut t = term.lock();
                    if let Some(s) = t.selection.as_mut() {
                        s.update(pt, side);
                    }
                    drop(t);
                    term.touch();
                }
            }
            Event::MouseWheel { delta, unit, .. } if response.hovered() => {
                let lines = match unit {
                    egui::MouseWheelUnit::Line => delta.y,
                    egui::MouseWheelUnit::Page => delta.y * size.rows as f32,
                    egui::MouseWheelUnit::Point => delta.y / cell.y,
                };
                state.scroll_accum += lines;
                let n = state.scroll_accum.trunc() as i32;
                if n == 0 {
                    continue;
                }
                state.scroll_accum -= n as f32;
                if report {
                    let pos = ui.input(|i| i.pointer.hover_pos()).unwrap_or(origin);
                    let (pt, _) = point_at(pos, origin, cell, size, offset);
                    let b = if n > 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
                    for _ in 0..n.unsigned_abs() {
                        if let Some(bytes) = input::mouse_report(Some(b), MouseAction::Press, mods, pt.column.0, pt.line.0.max(0) as usize, mode) {
                            out.input.push(bytes);
                        }
                    }
                } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
                    let key = if n > 0 { b"\x1bOA" } else { b"\x1bOB" };
                    for _ in 0..(n.unsigned_abs() * 3) {
                        out.input.push(key.to_vec());
                    }
                } else {
                    term.scroll(Scroll::Delta(n * 3));
                }
            }
            _ => {}
        }
    }
    if hovered {
        let pos = ui.input(|i| i.pointer.hover_pos());
        let pt = pos.map(|p| point_at(p, origin, cell, size, offset).0);
        let on_link = pt.is_some_and(|pt| link_at(term, pt).is_some());
        state.path_hover = match pt {
            Some(pt) if mods.ctrl && !on_link => {
                path_at(term, state, pt).map(|(_, s, e)| (pt.line.0 + offset as i32, s, e))
            }
            _ => {
                // Asked again on the next Ctrl: a file may have appeared since.
                if !mods.ctrl {
                    state.path_cache = None;
                }
                None
            }
        };
        let hand = on_link || state.path_hover.is_some();
        ui.ctx().set_cursor_icon(if hand { CursorIcon::PointingHand } else { CursorIcon::Text });
    } else {
        state.path_hover = None;
    }
}

/// The existing local path under `pt` and the columns it covers on that row.
fn path_at(term: &Terminal, state: &mut ViewState, pt: Point) -> Option<(std::path::PathBuf, usize, usize)> {
    let row: Vec<char> = {
        let t = term.lock();
        let g = t.grid();
        if pt.line.0 < -(g.history_size() as i32) || pt.line.0 >= g.screen_lines() as i32 {
            return None;
        }
        let r = &g[pt.line];
        (0..g.columns())
            .map(|c| {
                let cell = &r[Column(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) { '\u{FFFF}' } else { cell.c }
            })
            .collect()
    };
    let (text, s, e) = crate::paths::candidate(&row, pt.column.0)?;
    let found = match &state.path_cache {
        Some((t, found)) if *t == text => found.clone(),
        _ => {
            let found = crate::paths::resolve(&text, state.cwd.as_deref());
            state.path_cache = Some((text, found.clone()));
            found
        }
    };
    found.map(|p| (p, s, e))
}

fn link_at(term: &Terminal, pt: Point) -> Option<String> {
    let t = term.lock();
    let g = t.grid();
    if pt.line.0 < -(g.history_size() as i32) || pt.line.0 >= g.screen_lines() as i32 {
        return None;
    }
    g[pt].hyperlink().map(|h| h.uri().to_string())
}

fn handle_keys(ui: &Ui, term: &Terminal, state: &mut ViewState, mode: TermMode, out: &mut ViewOutput) {
    let events = ui.input(|i| i.events.clone());
    let mut ctrl_c_done = false;
    let mut ctrl_v_done = false;
    let mut suppress_text = false;
    let mut typed = false;
    for ev in &events {
        match ev {
            Event::Copy | Event::Key { key: Key::C, pressed: true, modifiers: Modifiers { ctrl: true, alt: false, .. }, .. } => {
                if ctrl_c_done {
                    continue;
                }
                ctrl_c_done = true;
                // Ctrl+C copies when there is a selection (and clears it), else interrupts.
                match term.selection_text() {
                    Some(text) => {
                        out.copy = Some(text);
                        term.clear_selection();
                    }
                    None => {
                        out.input.push(vec![0x03]);
                        typed = true;
                    }
                }
                suppress_text = true;
            }
            Event::Paste(_) | Event::Key { key: Key::V, pressed: true, modifiers: Modifiers { ctrl: true, alt: false, .. }, .. } => {
                if !ctrl_v_done {
                    ctrl_v_done = true;
                    out.paste_requested = true;
                    typed = true;
                }
                suppress_text = true;
            }
            Event::Cut => {
                out.input.push(vec![0x18]);
                typed = true;
            }
            Event::Key { key, pressed: true, modifiers, .. } => {
                if let Some(bytes) = input::encode_key(*key, *modifiers, mode) {
                    out.input.push(bytes);
                    typed = true;
                    suppress_text = modifiers.ctrl || modifiers.alt;
                } else {
                    suppress_text = false;
                }
            }
            Event::Text(t) if !suppress_text && state.preedit.is_empty() => {
                out.input.push(t.as_bytes().to_vec());
                typed = true;
            }
            Event::Ime(egui::ImeEvent::Preedit { text, .. }) => {
                state.preedit = text.clone();
                term.touch();
            }
            Event::Ime(egui::ImeEvent::Commit(text)) => {
                state.preedit.clear();
                if !text.is_empty() {
                    out.input.push(text.as_bytes().to_vec());
                    typed = true;
                }
            }
            _ => {}
        }
    }
    if typed {
        // Typing jumps back to the live screen and restarts the cursor blink.
        if term.lock().grid().display_offset() != 0 {
            term.scroll(Scroll::Bottom);
        }
        state.blink_epoch = Some(Instant::now());
        state.active_at = state.blink_epoch;
    }
}

/// The bar's blink: whether it shows now, and when to look again (None: it rests solid and needs
/// no repaint). `since_epoch` is the time since the blink restarted, `since_active` the time
/// since the pane's last input or output.
pub fn blink(since_epoch: Duration, since_active: Duration) -> (bool, Option<Duration>) {
    if since_active >= BLINK_IDLE {
        return (true, None);
    }
    let ms = since_epoch.as_millis();
    let period = BLINK.as_millis();
    let on = (ms / period).is_multiple_of(2);
    let next_toggle = Duration::from_millis((period - ms % period) as u64);
    (on, Some(next_toggle.min(BLINK_IDLE - since_active)))
}

/// Cell foreground/background after bold-as-bright, dim, inverse and hidden.
fn colors(cell: &alacritty_terminal::term::cell::Cell, overrides: &alacritty_terminal::term::color::Colors) -> (Rgb, Rgb) {
    let mut fg = cell.fg;
    if cell.flags.contains(Flags::BOLD) {
        if let Color::Named(n) = fg {
            if (n as usize) < 8 {
                fg = Color::Named(n.to_bright());
            }
        } else if let Color::Indexed(i) = fg {
            if i < 8 {
                fg = Color::Indexed(i + 8);
            }
        }
    }
    let mut f = resolve(fg, overrides);
    let mut b = resolve(cell.bg, overrides);
    if cell.flags.contains(Flags::DIM) {
        f = Rgb { r: (f.r as u16 * 2 / 3) as u8, g: (f.g as u16 * 2 / 3) as u8, b: (f.b as u16 * 2 / 3) as u8 };
    }
    if cell.flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut f, &mut b);
    }
    if cell.flags.contains(Flags::HIDDEN) {
        f = b;
    }
    (f, b)
}

#[allow(clippy::too_many_arguments)]
fn paint(ui: &Ui, term: &Terminal, state: &mut ViewState, rect: Rect, origin: Pos2, cell: Vec2, tf: &TermFont, focused: bool, response: &Response) {
    let painter = ui.painter_at(rect);
    let bg = c32(palette::BACKGROUND);
    // Under a background picture the app has veiled this spot already; a second fill would
    // darken the terminal past the picture's strength, unlike the sidebar beside it.
    if palette::BACKGROUND_ALPHA.load(std::sync::atomic::Ordering::Relaxed) == 255 {
        painter.rect_filled(rect, 0.0, bg);
    }
    let row_h = ui.fonts_mut(|f| f.row_height(&tf.font));
    let text_dy = ((cell.y - row_h) / 2.0).max(0.0);
    let t = term.lock();
    let content = t.renderable_content();
    let offset = content.display_offset;
    let sel = content.selection;
    let overrides = content.colors;
    let cursor = content.cursor;
    let show_cursor = content.mode.contains(TermMode::SHOW_CURSOR);
    let bg_default = palette::BACKGROUND;

    // Background runs, then text runs, per row.
    let mut cur_row: i32 = i32::MIN;
    let mut bg_run: Option<(usize, usize, Rgb)> = None;
    let mut text_run = (0usize, String::new(), Rgb { r: 0, g: 0, b: 0 }, Flags::empty());
    let flush_bg = |run: &mut Option<(usize, usize, Rgb)>, row: i32| {
        if let Some((c0, c1, col)) = run.take() {
            if col != bg_default {
                let r = Rect::from_min_size(
                    origin + Vec2::new(c0 as f32 * cell.x, row as f32 * cell.y),
                    Vec2::new((c1 - c0) as f32 * cell.x, cell.y),
                );
                painter.rect_filled(r, 0.0, c32(col));
            }
        }
    };
    let flush_text = |run: &mut (usize, String, Rgb, Flags), row: i32| {
        if run.1.trim().is_empty() {
            run.1.clear();
            return;
        }
        let pos = origin + Vec2::new(run.0 as f32 * cell.x, row as f32 * cell.y + text_dy);
        painter.text(pos, Align2::LEFT_TOP, &run.1, tf.font.clone(), c32(run.2));
        decorate(&painter, run.3, pos.x, run.1.chars().count(), row, origin, cell, run.2);
        run.1.clear();
    };

    let mut deco: Vec<(Flags, usize, usize, i32, Rgb)> = Vec::new();
    for ind in content.display_iter {
        let row = ind.point.line.0 + offset as i32;
        let col = ind.point.column.0;
        if row != cur_row {
            if cur_row != i32::MIN {
                flush_bg(&mut bg_run, cur_row);
                flush_text(&mut text_run, cur_row);
            }
            cur_row = row;
        }
        let c = ind.cell;
        if c.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let (fg, mut bg) = colors(c, overrides);
        if sel.is_some_and(|s| s.contains(ind.point)) {
            bg = palette::SELECTION;
        }
        let width = if c.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
        match &mut bg_run {
            Some((_, end, col_)) if *col_ == bg && *end == col => *end = col + width,
            _ => {
                flush_bg(&mut bg_run, row);
                bg_run = Some((col, col + width, bg));
            }
        }
        let ch = cell_text(c.c, c.zerowidth());
        // One char after composing: a decomposed Vietnamese letter becomes one precomposed letter.
        let mut chars = ch.chars();
        let single = match (chars.next(), chars.next()) {
            (Some(g), None) => Some(g),
            _ => None,
        };
        let simple = width == 1 && single.is_some_and(in_run);
        let fl = c.flags & (Flags::ALL_UNDERLINES | Flags::STRIKEOUT);
        if simple {
            if text_run.1.is_empty() || text_run.2 != fg || text_run.3 != fl || text_run.0 + text_run.1.chars().count() != col {
                flush_text(&mut text_run, row);
                text_run = (col, String::new(), fg, fl);
            }
            text_run.1.push_str(&ch);
        } else {
            flush_text(&mut text_run, row);
            text_run = (col + width, String::new(), fg, fl);
            if c.c != ' ' {
                let center = origin + Vec2::new((col as f32 + width as f32 / 2.0) * cell.x, row as f32 * cell.y + cell.y / 2.0);
                painter.text(center, Align2::CENTER_CENTER, &ch, tf.font.clone(), c32(fg));
                if !fl.is_empty() {
                    deco.push((fl, col, width, row, fg));
                }
            }
        }
    }
    if cur_row != i32::MIN {
        flush_bg(&mut bg_run, cur_row);
        flush_text(&mut text_run, cur_row);
    }
    for (fl, col, w, row, fg) in deco {
        decorate(&painter, fl, origin.x + col as f32 * cell.x, w, row, origin, cell, fg);
    }

    // Cursor: a blinking bar while focused, an outline block otherwise; only on the live screen.
    let cur_line = cursor.point.line.0 + offset as i32;
    if show_cursor && (0..t.screen_lines() as i32).contains(&cur_line) {
        let pos = origin + Vec2::new(cursor.point.column.0 as f32 * cell.x, cur_line as f32 * cell.y);
        if focused {
            let epoch = *state.blink_epoch.get_or_insert_with(Instant::now);
            let active = *state.active_at.get_or_insert(epoch);
            let (on, next) = blink(epoch.elapsed(), active.elapsed());
            if on {
                painter.rect_filled(Rect::from_min_size(pos, Vec2::new(2.0, cell.y)), 0.0, c32(palette::CURSOR));
            }
            if let Some(next) = next {
                ui.ctx().request_repaint_after(next);
            }
        } else {
            painter.rect_stroke(
                Rect::from_min_size(pos, cell),
                0.0,
                Stroke::new(1.0, c32(palette::CURSOR)),
                egui::StrokeKind::Inside,
            );
        }
        if !state.preedit.is_empty() {
            let r = painter.text(pos, Align2::LEFT_TOP, &state.preedit, tf.font.clone(), c32(palette::FOREGROUND));
            painter.line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0, c32(palette::FOREGROUND)));
        }
    }

    // Scrollbar: only when there is history; thin, on the right.
    let history = t.grid().history_size();
    if history > 0 {
        let total = (history + t.screen_lines()) as f32;
        let h = rect.height() * t.screen_lines() as f32 / total;
        let top = rect.top() + (rect.height() - h) * (1.0 - offset as f32 / history as f32);
        let bar = Rect::from_min_size(Pos2::new(rect.right() - 6.0, top), Vec2::new(5.0, h.max(12.0)));
        let col = if response.hovered() { palette::SCROLLBAR_HOVER } else { palette::SCROLLBAR };
        painter.rect_filled(bar, 2.0, c32(col));
    }
    let _ = NamedColor::Foreground;
}

#[allow(clippy::too_many_arguments)]
fn decorate(painter: &egui::Painter, fl: Flags, x: f32, width: usize, row: i32, origin: Pos2, cell: Vec2, fg: Rgb) {
    if fl.is_empty() {
        return;
    }
    let y0 = origin.y + row as f32 * cell.y;
    let x1 = x + width as f32 * cell.x;
    let stroke = Stroke::new(1.0, c32(fg));
    if fl.intersects(Flags::ALL_UNDERLINES) {
        let y = y0 + cell.y - 2.0;
        painter.line_segment([Pos2::new(x, y), Pos2::new(x1, y)], stroke);
        if fl.contains(Flags::DOUBLE_UNDERLINE) {
            painter.line_segment([Pos2::new(x, y - 2.0), Pos2::new(x1, y - 2.0)], stroke);
        }
    }
    if fl.contains(Flags::STRIKEOUT) {
        let y = y0 + cell.y / 2.0;
        painter.line_segment([Pos2::new(x, y), Pos2::new(x1, y)], stroke);
    }
}

/// Stable id for a pane's widget, so focus survives switching panes.
pub fn view_id(pane: u64) -> Id {
    Id::NULL.with(("kilno-term", pane))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_with_padding() {
        let s = fit(Vec2::new(12.0 + 4.0 + 80.0 * 8.0, 10.0 + 6.0 + 24.0 * 16.0), Vec2::new(8.0, 16.0));
        assert_eq!(s, GridSize { cols: 80, rows: 24 });
    }

    #[test]
    fn the_bar_blinks_then_rests_solid_when_the_pane_is_idle() {
        let ms = Duration::from_millis;
        assert_eq!(blink(ms(0), ms(0)), (true, Some(ms(600))));
        assert_eq!(blink(ms(700), ms(700)), (false, Some(ms(500))));
        // The rest comes on time, not at the next toggle.
        assert_eq!(blink(ms(4900), ms(4900)), (true, Some(ms(100))));
        assert_eq!(blink(ms(5000), ms(5000)), (true, None));
        assert_eq!(blink(ms(9000), ms(6000)), (true, None));
        // Activity starts it again.
        assert_eq!(blink(ms(9000), ms(10)).1, Some(ms(600)));
    }

    #[test]
    fn runs() {
        assert!(in_run('a'));
        assert!(in_run('\u{1ebf}')); // ế
        assert!(in_run('\u{2502}')); // box drawing
        assert!(!in_run('\u{4e2d}'));
        assert!(!in_run('\u{1f600}'));
    }

    /// With eframe's `clipboard_shortcuts` off (kilno-app), Ctrl+V reaches the widget as a key
    /// even when the clipboard holds only files (egui-winit would otherwise swallow it), and the
    /// widget asks the pane to paste: the pane reads text or the Explorer file list itself.
    #[test]
    fn ctrl_v_as_a_plain_key_requests_a_paste() {
        let term = Terminal::new(GridSize { cols: 20, rows: 4 }, std::sync::Arc::new(|_: &[u8]| {}));
        let ctx = egui::Context::default();
        let mut state = ViewState::default();
        let tf = TermFont::default();
        let mut pasted = false;
        for frame in 0..3 {
            let mut input = egui::RawInput::default();
            if frame == 2 {
                input.events.push(Event::Key {
                    key: Key::V,
                    physical_key: Some(Key::V),
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::CTRL,
                });
            }
            let mut o = ctx.run_ui(input, |ui| {
                let (resp, out) = show(ui, &term, &mut state, &tf, false);
                if frame == 0 {
                    resp.request_focus();
                }
                pasted |= out.paste_requested;
                assert!(out.input.iter().all(|b| b != &vec![0x16]), "no raw ^V is sent");
            });
            o.textures_delta.clear();
        }
        assert!(pasted);
    }

    #[test]
    fn decomposed_vietnamese_composes() {
        assert_eq!(cell_text('e', Some(&['\u{0302}', '\u{0301}'])), "\u{1ebf}");
        assert_eq!(cell_text('a', None), "a");
        assert!(in_run('\u{1ebf}'), "the composed letter is drawn in the row's text run");
    }

    /// A decomposed letter fed through the terminal keeps its marks in the cell and composes.
    #[test]
    fn decomposed_input_keeps_marks_in_the_cell() {
        let t = Terminal::new(GridSize { cols: 10, rows: 2 }, std::sync::Arc::new(|_: &[u8]| {}));
        t.feed("e\u{0302}\u{0301}".as_bytes());
        let g = t.lock();
        let cell = &g.grid()[alacritty_terminal::index::Line(0)][alacritty_terminal::index::Column(0)];
        assert_eq!(cell_text(cell.c, cell.zerowidth()), "\u{1ebf}");
    }
}
