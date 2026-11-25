//! The Files panel: the session's folder as a tree beside the terminal (where the doc viewer
//! goes), and the file picked in it: markdown rendered, a picture fitted, or a text file in a
//! plain editor (no highlighting) to fix a `.env` or a config quickly. Nothing is run.

use crate::doc_view::{bar_button, grip, markdown};
use crate::theme::*;
use egui::{Align2, CursorIcon, Id, Key, Modifiers, Pos2, Rect, Sense, Stroke, TextEdit, TextureHandle, Ui, UiBuilder, Vec2};
use egui_commonmark::CommonMarkCache;
use kilno_core::files::{self, Entry, Kind};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const BAR_H: f32 = 33.0;
const ROW_H: f32 = 24.0;
const INDENT: f32 = 14.0;
/// Search results shown for the filter.
const FOUND_MAX: usize = 300;

pub struct FilesPanel {
    root: PathBuf,
    open: HashSet<PathBuf>,
    listings: HashMap<PathBuf, Vec<Entry>>,
    filter: String,
    /// The filter the results are for, and them.
    found: Option<(String, Vec<Entry>)>,
    view: View,
    /// Each file opened gets a fresh scroll id.
    seq: u64,
    /// A path to show once the editor's unsaved edits are saved or discarded.
    pending: Option<PathBuf>,
    /// A folder the tree scrolls to on its next draw.
    scroll_to: Option<PathBuf>,
}

enum View {
    Tree,
    Doc { path: PathBuf, text: String, id: Id },
    Image { path: PathBuf, tex: Option<TextureHandle> },
    Text(Box<Editor>),
    /// A file that is not opened here (binary, too big, unreadable), and why.
    Note { path: PathBuf, text: String },
}

struct Editor {
    path: PathBuf,
    text: String,
    saved: String,
    /// The file had CRLF line endings: edited with LF, saved with CRLF again.
    crlf: bool,
    mtime: Option<chrono::DateTime<chrono::Utc>>,
    /// Seconds (egui time) of the last look at the file on disk.
    looked: f64,
    /// The file changed on disk while there were unsaved edits.
    stale: bool,
    /// Leaving with unsaved edits waits for Save, Discard or Cancel; the bool is "close".
    leaving: Option<bool>,
    error: Option<String>,
    /// The Ctrl+F bar: what is looked for, and None when it is closed.
    find: Option<String>,
    /// Where the next search starts (a char index), and the last one found nothing.
    find_from: usize,
    find_miss: bool,
    /// A line to scroll the editor to on its next draw.
    jump: Option<usize>,
}

impl Editor {
    fn open(path: &Path) -> Result<Self, String> {
        let raw = files::read_text(path).map_err(|e| e.to_string())?;
        let crlf = raw.contains("\r\n");
        let text = if crlf { raw.replace("\r\n", "\n") } else { raw };
        Ok(Self {
            path: path.to_path_buf(),
            saved: text.clone(),
            text,
            crlf,
            mtime: kilno_core::time::mtime(path),
            looked: 0.0,
            stale: false,
            leaving: None,
            error: None,
            find: None,
            find_from: 0,
            find_miss: false,
            jump: None,
        })
    }

    /// Selects the next match of the find bar's text after the last one (wrapping round) and
    /// moves the focus to the editor, so the match shows selected.
    fn find_next(&mut self, ctx: &egui::Context) {
        let Some(q) = self.find.as_deref().filter(|q| !q.is_empty()) else { return };
        let Some((s, e)) = find_in(&self.text, q, self.find_from) else {
            self.find_miss = true;
            return;
        };
        self.find_miss = false;
        self.find_from = e;
        self.jump = Some(self.text.chars().take(s).filter(|c| *c == '\n').count());
        let id = editor_id(&self.path);
        let mut st = TextEdit::load_state(ctx, id).unwrap_or_default();
        st.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(s), egui::text::CCursor::new(e))));
        st.store(ctx, id);
        ctx.memory_mut(|m| m.request_focus(id));
    }

    fn dirty(&self) -> bool {
        self.text != self.saved
    }

    fn save(&mut self) {
        let out = if self.crlf { self.text.replace('\n', "\r\n") } else { self.text.clone() };
        match files::save(&self.path, &out) {
            Ok(()) => {
                self.saved = self.text.clone();
                self.mtime = kilno_core::time::mtime(&self.path);
                self.stale = false;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("Couldn't save: {e}")),
        }
    }

    fn reload(&mut self) {
        match Self::open(&self.path) {
            Ok(fresh) => *self = fresh,
            Err(e) => self.error = Some(format!("Couldn't reload: {e}")),
        }
    }

    /// At most once a second: an edit on disk (the agent's) reloads a clean file, and flags
    /// a file with unsaved edits.
    fn watch(&mut self, now: f64) {
        if now - self.looked < 1.0 {
            return;
        }
        self.looked = now;
        let mtime = kilno_core::time::mtime(&self.path);
        if mtime != self.mtime && mtime.is_some() {
            if self.dirty() {
                self.stale = true;
            } else {
                self.reload();
            }
        }
    }
}

#[derive(Default)]
pub struct FilesOut {
    pub close: bool,
    /// Links clicked in a markdown file, not yet opened.
    pub links: Vec<String>,
}

impl FilesPanel {
    pub fn new(root: &Path) -> Self {
        let mut p = Self {
            root: root.to_path_buf(),
            open: HashSet::new(),
            listings: HashMap::new(),
            filter: String::new(),
            found: None,
            view: View::Tree,
            seq: 0,
            pending: None,
            scroll_to: None,
        };
        p.listings.insert(root.to_path_buf(), files::list(root));
        p
    }

    /// Unsaved edits in the editor.
    pub fn dirty(&self) -> bool {
        matches!(&self.view, View::Text(e) if e.dirty())
    }

    pub fn open_file(&mut self, ctx: &egui::Context, path: &Path) {
        self.seq += 1;
        let path = path.to_path_buf();
        self.view = match files::kind(&path) {
            Kind::Markdown => {
                let dir = path.parent().unwrap_or(&self.root).to_path_buf();
                let text = kilno_core::doc::prepare(&kilno_core::doc::read(&path), &dir, &self.root);
                View::Doc { path, text, id: Id::NULL.with(("files-doc", self.seq)) }
            }
            Kind::Image => {
                let tex = crate::image_chip::decode(&path, 0).map(|i| ctx.load_texture("files-image", i, egui::TextureOptions::LINEAR));
                View::Image { path, tex }
            }
            Kind::Text => match Editor::open(&path) {
                Ok(e) => View::Text(Box::new(e)),
                Err(e) => View::Note { path, text: format!("Couldn't open it: {e}") },
            },
            Kind::TooBig => View::Note { path, text: "Too big to open here (over 1 MB).".into() },
            Kind::Binary => View::Note { path, text: "Not a text file: Kilno opens Markdown, pictures and text here.".into() },
        };
    }

    /// A path Ctrl+clicked in the terminal: a file opens here, a folder is opened in the tree
    /// with the folders above it. A path outside the panel's folder makes its folder the root.
    /// Unsaved edits ask Save / Discard / Cancel first.
    pub fn reveal(&mut self, ctx: &egui::Context, path: &Path) {
        if let View::Text(e) = &mut self.view {
            if e.dirty() {
                e.leaving = Some(false);
                self.pending = Some(path.to_path_buf());
                return;
            }
        }
        let is_dir = path.is_dir();
        let rel = relative(&self.root, path);
        let rel = match rel {
            Some(rel) => rel,
            None => {
                let Some(folder) = (if is_dir { Some(path) } else { path.parent() }) else { return };
                *self = Self::new(folder);
                relative(&self.root, path).unwrap_or_default()
            }
        };
        self.filter.clear();
        // The folders down to it, opened (a file's own folder last; a folder itself too).
        let parts: Vec<_> = rel.components().collect();
        let opened = if is_dir { parts.len() } else { parts.len().saturating_sub(1) };
        let mut dir = self.root.clone();
        for c in &parts[..opened] {
            dir.push(c);
            self.listings.entry(dir.clone()).or_insert_with(|| files::list(&dir));
            self.open.insert(dir.clone());
        }
        if is_dir {
            self.view = View::Tree;
            self.scroll_to = Some(dir);
        } else {
            self.open_file(ctx, &self.root.join(&rel));
        }
    }

    fn refresh(&mut self) {
        let dirs: Vec<PathBuf> = self.listings.keys().cloned().collect();
        for d in dirs {
            self.listings.insert(d.clone(), files::list(&d));
        }
        self.found = None;
    }
}

const FIND_ID: &str = "files-find";

fn editor_id(path: &Path) -> Id {
    Id::NULL.with(("files-text", path))
}

/// The first match of `q` in `text` at or after char `from`, else from the start, ignoring
/// case; as char indices (start, end).
fn find_in(text: &str, q: &str, from: usize) -> Option<(usize, usize)> {
    // One char to one char, so indices stay those of `text`.
    let low = |s: &str| s.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect::<Vec<_>>();
    let (hay, needle) = (low(text), low(q));
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    let at = |i: usize| hay[i..].starts_with(&needle);
    let last = hay.len() - needle.len();
    (from.min(last + 1)..=last).chain(0..from.min(last + 1)).find(|&i| at(i)).map(|i| (i, i + needle.len()))
}

/// `path` below `root` (the same folder counts, as an empty path), matched as the platform
/// does (any case on Windows); None outside it.
fn relative(root: &Path, path: &Path) -> Option<PathBuf> {
    use kilno_core::paths::{is_inside, normalize};
    let (root, path) = (normalize(root), normalize(path));
    is_inside(&root, &path).then(|| path.components().skip(root.components().count()).collect())
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// Draws the panel in `rect`; `body` is the whole pane, for the grip.
pub fn show(ui: &mut Ui, rect: Rect, body: Rect, f: &mut FilesPanel, share: &mut f32, cache: &mut CommonMarkCache) -> FilesOut {
    let mut out = FilesOut::default();
    let p = ui.painter().clone();
    p.rect_filled(rect, 0, crate::background::over_veil(TERMINAL_BG));
    p.vline(rect.left() + 0.5, rect.y_range(), Stroke::new(1.0, HAIRLINE));

    // The bar: "Files" or "‹ Files" and the file's name; on the right ✕, then Save or ⟳.
    let bar = Rect::from_min_size(rect.min, Vec2::new(rect.width(), BAR_H));
    p.hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0, HAIRLINE));
    let inner = Rect::from_min_max(bar.min + Vec2::new(10.0, 6.0), bar.max - Vec2::new(8.0, 7.0));
    let mut back = false;
    let mut close = false;
    let mut save = false;
    let mut name_left = inner.left();
    let mut name_right = inner.right();
    ui.scope_builder(UiBuilder::new().max_rect(inner).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
        if !matches!(f.view, View::Tree) && bar_button(ui, "‹ Files", false).on_hover_text("Back to the folder").clicked() {
            back = true;
        }
        name_left = ui.min_rect().right() + 8.0;
    });
    ui.scope_builder(UiBuilder::new().max_rect(inner).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if bar_button(ui, "✕", false).on_hover_text("Back to the terminal").clicked() {
            close = true;
        }
        match &f.view {
            View::Text(e) => {
                let label = if e.dirty() { "Save" } else { "Saved" };
                if bar_button(ui, label, e.dirty()).on_hover_text("Ctrl+S").clicked() {
                    save = true;
                }
            }
            View::Tree if bar_button(ui, "⟳", false).on_hover_text("Read the folders again").clicked() => f.refresh(),
            _ => {}
        }
        name_right = ui.min_rect().left() - 6.0;
    });
    let title = match &f.view {
        View::Tree => format!("Files · {}", name_of(&f.root)),
        View::Doc { path, .. } | View::Image { path, .. } | View::Note { path, .. } => name_of(path),
        View::Text(e) => format!("{}{}", name_of(&e.path), if e.dirty() { " ●" } else { "" }),
    };
    let g = elided(ui, &title, ui_font(12.0), TEXT_PRIMARY, (name_right - name_left).max(0.0));
    p.galley(Pos2::new(name_left, inner.center().y - g.size().y / 2.0), g, TEXT_PRIMARY);

    if let View::Text(e) = &mut f.view {
        if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S)) {
            save = true;
        }
        if save {
            e.save();
        }
        // Leaving with unsaved edits asks first.
        if (back || close) && e.dirty() {
            e.leaving = Some(close);
            back = false;
            close = false;
        }
    }
    if back {
        f.view = View::Tree;
    }
    out.close = close;

    let mut content = Rect::from_min_max(Pos2::new(rect.left() + 1.0, bar.bottom()), rect.max);
    // Save or Discard on the "Unsaved changes" strip: Some(true) closes, Some(false) goes back.
    let mut left: Option<bool> = None;
    let mut cancelled = false;
    match &mut f.view {
        View::Tree => tree(ui, content, f),
        View::Doc { text, id, .. } => out.links = markdown(ui, content, *id, text, cache),
        View::Image { tex, .. } => match tex {
            Some(t) => {
                let area = content.shrink(16.0);
                let s = t.size_vec2();
                let k = (area.width() / s.x).min(area.height() / s.y).min(1.0);
                p.image(t.id(), Rect::from_center_size(area.center(), s * k), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), egui::Color32::WHITE);
            }
            None => note(&p, content, "Couldn't read this picture."),
        },
        View::Note { text, .. } => note(&p, content, text),
        View::Text(e) => {
            e.watch(ui.input(|i| i.time));
            // One strip under the bar: leaving, a change on disk, or a failed save.
            let strip = Rect::from_min_size(content.min, Vec2::new(content.width(), 32.0));
            let mut shown = true;
            if let Some(closing) = e.leaving {
                match strip_row(ui, strip, "Unsaved changes.", &["Save", "Discard", "Cancel"]) {
                    Some(0) => {
                        e.save();
                        if !e.dirty() {
                            e.leaving = None;
                            left = Some(closing);
                        }
                    }
                    Some(1) => {
                        e.leaving = None;
                        left = Some(closing);
                    }
                    Some(_) => {
                        e.leaving = None;
                        cancelled = true;
                    }
                    None => {}
                }
            } else if e.stale {
                match strip_row(ui, strip, "Changed on disk.", &["Reload", "Keep mine"]) {
                    Some(0) => e.reload(),
                    Some(_) => {
                        e.stale = false;
                        e.mtime = kilno_core::time::mtime(&e.path);
                    }
                    None => {}
                }
            } else if let Some(err) = e.error.clone() {
                if strip_row(ui, strip, &err, &["OK"]).is_some() {
                    e.error = None;
                }
            } else {
                shown = false;
            }
            if shown {
                content.min.y = strip.bottom();
            }
            // Ctrl+F opens the find bar, F3 finds the next; only while the editor or the bar has
            // the keys (in the terminal they are the agent's).
            let find_id = Id::NULL.with(FIND_ID);
            let focus = ui.memory(|m| m.focused());
            if focus == Some(editor_id(&e.path)) || focus == Some(find_id) {
                if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::F)) {
                    e.find.get_or_insert_default();
                    ui.memory_mut(|m| m.request_focus(find_id));
                }
                if e.find.is_some() && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F3)) {
                    e.find_next(ui.ctx());
                }
            }
            if e.find.is_some() {
                let bar = Rect::from_min_size(content.min, Vec2::new(content.width(), 32.0));
                if find_bar(ui, bar, e, find_id) {
                    e.find = None;
                    ui.memory_mut(|m| m.request_focus(editor_id(&e.path)));
                }
                content.min.y = bar.bottom();
            }
            editor(ui, content, e);
        }
    }
    match left {
        Some(true) => out.close = true,
        Some(false) => {
            f.view = View::Tree;
            // A path Ctrl+clicked while the edits were unsaved: shown now.
            if let Some(path) = f.pending.take() {
                f.reveal(ui.ctx(), &path);
            }
        }
        None => {}
    }
    if cancelled || left == Some(true) {
        f.pending = None;
    }
    grip(ui, rect, body, share);
    out
}

fn note(p: &egui::Painter, r: Rect, text: &str) {
    p.text(r.center_top() + Vec2::new(0.0, 40.0), Align2::CENTER_TOP, text, ui_font(13.0), TEXT_MUTED);
}

/// A message and its buttons, left to right; which button was clicked.
fn strip_row(ui: &mut Ui, r: Rect, text: &str, buttons: &[&str]) -> Option<usize> {
    ui.painter().rect_filled(r, 0, RAISED_BG);
    ui.painter().hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0, HAIRLINE));
    let mut clicked = None;
    let inner = r.shrink2(Vec2::new(10.0, 6.0));
    ui.scope_builder(UiBuilder::new().max_rect(inner).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (i, b) in buttons.iter().enumerate().rev() {
            if bar_button(ui, b, i == 0).clicked() {
                clicked = Some(i);
            }
        }
        let left = ui.min_rect().left() - 8.0;
        let g = elided(ui, text, ui_font(12.0), TEXT_PRIMARY, (left - inner.left()).max(0.0));
        ui.painter().galley(Pos2::new(inner.left(), inner.center().y - g.size().y / 2.0), g, TEXT_PRIMARY);
    });
    clicked
}

/// The find bar: the text, Next, ✕. Enter or Next selects the next match in the editor; true
/// when it is closed (✕ or Esc).
fn find_bar(ui: &mut Ui, r: Rect, e: &mut Editor, id: Id) -> bool {
    ui.painter().rect_filled(r, 0, RAISED_BG);
    ui.painter().hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0, HAIRLINE));
    let (mut next, mut close) = (false, false);
    ui.scope_builder(UiBuilder::new().max_rect(r.shrink2(Vec2::new(10.0, 5.0))).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        close = bar_button(ui, "✕", false).on_hover_text("Close (Esc)").clicked();
        next = bar_button(ui, "Next", true).on_hover_text("Enter or F3").clicked();
        if e.find_miss {
            ui.label(egui::RichText::new("No match").font(ui_font(12.0)).color(TEXT_MUTED));
        }
        let q = e.find.get_or_insert_default();
        let out = TextEdit::singleline(q).id(id).hint_text("Find").font(ui_font(12.0)).desired_width(ui.available_width()).show(ui);
        if out.response.changed() {
            e.find_miss = false;
        }
        if out.response.lost_focus() {
            next |= ui.input(|i| i.key_pressed(Key::Enter));
            close |= ui.input(|i| i.key_pressed(Key::Escape));
        }
    });
    if next {
        e.find_next(ui.ctx());
    }
    close
}

/// The plain editor: line numbers, monospace, no wrapping (so the numbers stay on their lines).
fn editor(ui: &mut Ui, r: Rect, e: &mut Editor) {
    ui.scope_builder(UiBuilder::new().max_rect(r.shrink2(Vec2::new(6.0, 4.0))), |ui| {
        let mut area = egui::ScrollArea::both().id_salt(("files-editor", &e.path)).auto_shrink(false);
        // A find shows its line a few lines down from the top.
        if let Some(line) = e.jump.take() {
            let row = ui.fonts_mut(|f| f.row_height(&mono(13.0)));
            area = area.vertical_scroll_offset(line.saturating_sub(3) as f32 * row);
        }
        area.show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let lines = e.text.split('\n').count();
                let numbers: String = (1..=lines).map(|n| format!("{n}\n")).collect();
                ui.add(egui::Label::new(egui::RichText::new(numbers.trim_end()).font(mono(13.0)).color(TEXT_FAINT)).selectable(false));
                ui.add_space(6.0);
                ui.add(
                    TextEdit::multiline(&mut e.text)
                        .id(editor_id(&e.path))
                        .font(mono(13.0))
                        .text_color(TEXT_PRIMARY)
                        .frame(egui::Frame::NONE)
                        .code_editor()
                        .lock_focus(true)
                        .desired_width(f32::INFINITY)
                        .desired_rows(lines.max(1)),
                );
            });
        });
    });
}

/// The folder tree, or the files matching the filter.
fn tree(ui: &mut Ui, r: Rect, f: &mut FilesPanel) {
    let search = Rect::from_min_size(r.min + Vec2::new(10.0, 8.0), Vec2::new(r.width() - 20.0, 26.0));
    let entered = ui.scope_builder(UiBuilder::new().max_rect(search), |ui| {
        let out = TextEdit::singleline(&mut f.filter)
            .id(Id::NULL.with("files-filter"))
            .hint_text("Filter, or paste a full path and press Enter")
            .font(ui_font(12.0))
            .desired_width(search.width())
            .show(ui);
        out.response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))
    });
    // A full path (Explorer's "Copy as path" quotes it): shown on Enter, not searched for.
    let typed = f.filter.trim().trim_matches('"').to_string();
    let as_path = Path::new(&typed).is_absolute().then(|| PathBuf::from(&typed));
    if let Some(p) = as_path.as_ref().filter(|p| entered.inner && p.exists()) {
        f.reveal(ui.ctx(), p);
        return;
    }
    let list = Rect::from_min_max(Pos2::new(r.left(), search.bottom() + 6.0), r.max);
    let mut clicked: Option<Entry> = None;
    let query = if as_path.is_some() { String::new() } else { f.filter.trim().to_string() };
    if !query.is_empty() && f.found.as_ref().map(|(q, _)| q) != Some(&query) {
        f.found = Some((query.clone(), files::find(&f.root, &query, FOUND_MAX)));
    }
    ui.scope_builder(UiBuilder::new().max_rect(list), |ui| {
        egui::ScrollArea::vertical().id_salt("files-tree").auto_shrink(false).show(ui, |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            if let Some(p) = &as_path {
                ui.add_space(12.0);
                let note = if p.exists() { "  Press Enter to open it." } else { "  No such file or folder." };
                ui.label(egui::RichText::new(note).color(TEXT_MUTED));
            } else if query.is_empty() {
                let mut rows = Vec::new();
                flatten(f, &f.root.clone(), 0, &mut rows);
                let target = f.scroll_to.take();
                for (e, depth) in rows {
                    let open = f.open.contains(&e.path);
                    let resp = row(ui, &e, depth, open, None);
                    if target.as_ref() == Some(&e.path) {
                        resp.scroll_to_me(Some(egui::Align::TOP));
                    }
                    if resp.clicked() {
                        clicked = Some(e);
                    }
                }
            } else if let Some((_, found)) = &f.found {
                if found.is_empty() {
                    ui.add_space(12.0);
                    ui.label(egui::RichText::new("  No file matches.").color(TEXT_MUTED));
                }
                for e in found {
                    let rel = e.path.strip_prefix(&f.root).unwrap_or(&e.path).parent().map(|d| d.display().to_string());
                    if row(ui, e, 0, false, rel.filter(|d| !d.is_empty()).as_deref()).clicked() {
                        clicked = Some(e.clone());
                    }
                }
            }
        });
    });
    if let Some(e) = clicked {
        if e.is_dir {
            if !f.open.remove(&e.path) {
                f.listings.entry(e.path.clone()).or_insert_with(|| files::list(&e.path));
                f.open.insert(e.path);
            }
        } else {
            f.open_file(ui.ctx(), &e.path);
        }
    }
}

/// The visible rows: `dir`'s entries, and those of its open folders under them.
fn flatten(f: &FilesPanel, dir: &Path, depth: usize, rows: &mut Vec<(Entry, usize)>) {
    for e in f.listings.get(dir).into_iter().flatten() {
        rows.push((e.clone(), depth));
        if e.is_dir && f.open.contains(&e.path) {
            flatten(f, &e.path, depth + 1, rows);
        }
    }
}

fn row(ui: &mut Ui, e: &Entry, depth: usize, open: bool, folder: Option<&str>) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(r, 0, RAISED_BG);
    }
    let x = r.left() + 12.0 + depth as f32 * INDENT;
    if e.is_dir {
        p.text(Pos2::new(x, r.center().y), Align2::LEFT_CENTER, if open { "▾" } else { "▸" }, ui_font(11.0), TEXT_MUTED);
    }
    let color = if e.is_dir { TEXT_PRIMARY } else { MUTED_AA };
    let g = elided(ui, &e.name, ui_font(12.5), color, (r.right() - x - 26.0).max(0.0));
    let w = g.size().x;
    p.galley(Pos2::new(x + 14.0, r.center().y - g.size().y / 2.0), g, color);
    if let Some(dir) = folder {
        let left = x + 14.0 + w + 8.0;
        let g = elided(ui, dir, ui_font(11.0), TEXT_FAINT, (r.right() - left - 8.0).max(0.0));
        p.galley(Pos2::new(left, r.center().y - g.size().y / 2.0), g, TEXT_FAINT);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch_later(p: &Path, secs: u64) {
        let t = std::time::SystemTime::now() + std::time::Duration::from_secs(secs);
        std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn edits_keep_crlf_and_follow_the_disk() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(".env");
        std::fs::write(&p, "A=1\r\nB=2\r\n").unwrap();
        let mut e = Editor::open(&p).unwrap();
        assert_eq!(e.text, "A=1\nB=2\n", "edited with LF");
        e.text = e.text.replace("A=1", "A=9");
        assert!(e.dirty());
        e.save();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "A=9\r\nB=2\r\n", "saved with CRLF again");
        assert!(!e.dirty());

        // The agent edits it: a clean editor reloads.
        std::fs::write(&p, "A=9\r\nB=3\r\n").unwrap();
        touch_later(&p, 5);
        e.watch(10.0);
        assert_eq!(e.text, "A=9\nB=3\n");

        // With unsaved edits it only flags the change.
        e.text.push_str("C=4\n");
        std::fs::write(&p, "A=0\r\n").unwrap();
        touch_later(&p, 10);
        e.watch(20.0);
        assert!(e.stale && e.text.ends_with("C=4\n"));
    }

    #[test]
    fn find_goes_on_from_the_last_match_and_wraps() {
        let t = "KEY=1\nkey=2\nĐường=3";
        assert_eq!(find_in(t, "key", 0), Some((0, 3)));
        assert_eq!(find_in(t, "key", 3), Some((6, 9)), "the next one, ignoring case");
        assert_eq!(find_in(t, "key", 9), Some((0, 3)), "wraps to the start");
        assert_eq!(find_in(t, "đường", 0), Some((12, 17)), "char indices, not bytes");
        assert_eq!(find_in(t, "nope", 0), None);
        assert_eq!(find_in(t, "", 0), None);
        assert_eq!(find_in("ab", "abc", 0), None);
    }

    #[test]
    fn reveal_opens_the_folders_down_to_a_path() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("proj");
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::write(root.join("src/deep/.env"), "A=1\n").unwrap();
        std::fs::write(d.path().join("other.txt"), "x").unwrap();
        let ctx = egui::Context::default();
        let mut f = FilesPanel::new(&root);

        f.reveal(&ctx, &root.join("src/deep"));
        assert!(matches!(f.view, View::Tree));
        assert!(f.open.contains(&root.join("src")) && f.open.contains(&root.join("src/deep")));
        assert_eq!(f.scroll_to.as_deref(), Some(root.join("src/deep").as_path()));

        f.reveal(&ctx, &root.join("src/deep/.env"));
        assert!(matches!(&f.view, View::Text(e) if e.path == root.join("src/deep/.env")));

        // Unsaved edits ask first; the path is shown after Discard.
        if let View::Text(e) = &mut f.view {
            e.text.push_str("B=2\n");
        }
        f.reveal(&ctx, &root.join("src"));
        assert!(matches!(&f.view, View::Text(e) if e.leaving == Some(false)));
        assert_eq!(f.pending.as_deref(), Some(root.join("src").as_path()));

        // Outside the folder: its own folder becomes the root.
        let mut g = FilesPanel::new(&root);
        g.reveal(&ctx, &d.path().join("other.txt"));
        assert_eq!(g.root, d.path());
        assert!(matches!(g.view, View::Text(_)));
    }
}
