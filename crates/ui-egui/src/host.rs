//! Hooks for a host embedding PdfCraft, such as a same-origin page framing the web build
//! (`apps/pdfcraft-web`, `?host=parent`). The host hands documents in tagged with its own id,
//! asks for saves, sends the reader to a page (and a passage on it), adds its own items to the
//! text-selection menus, underlines words with its own temporary marks, and hears what the user
//! is doing: which tagged document is in front, its page, the text selected in it, clicks on its
//! marks, activity, unsaved work and the theme.
//!
//! This crate stays free of any browser API: the host owns the transport (`postMessage` on the
//! web), the throttling and the debouncing. Here the app only takes requests from shared queues
//! once per frame and reports *changes* through [`HostEventFn`].
//!
//! A tag is an opaque string the host chose (one per document it knows); it belongs to the
//! open document, not to a tab position, so it survives tabs being moved, and is forgotten when
//! the document is closed. Pages are 0-based page indices.
//!
//! Rectangles are `[x0, y0, x1, y1]`: on a page in view space (points, y down, the page as
//! shown unrotated), on screen in egui points.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use pdfcraft_engine::DocId;

use crate::PdfCraftApp;
use crate::canvas::DocView;

/// The longest selected text reported (characters); longer selections are cut.
pub const MAX_HOST_TEXT_CHARS: usize = 20_000;
/// The most selection rectangles reported; the rest are dropped.
pub const MAX_HOST_RECTS: usize = 2_000;
/// The longest passage a [`HostRequest::Goto`] looks for (characters), after collapsing spaces.
pub const MAX_HOST_FIND_CHARS: usize = 500;
/// How long a [`HostRequest::Goto`] waits for the text of its page before giving up (seconds).
const GOTO_TIMEOUT_S: f64 = 5.0;
/// The same in frames, for hosts whose clock stands still.
const GOTO_TIMEOUT_FRAMES: u32 = 600;
/// How far from the asked page a passage is still "near" it (pages).
const GOTO_NEAR_PAGES: usize = 2;
/// The most mark sets a document holds; a new set past it is ignored.
pub const MAX_HOST_MARK_SETS: usize = 16;
/// The most marks one set holds on one page; later ones are dropped.
pub const MAX_HOST_MARKS_PER_PAGE: usize = 4_000;
/// The most rectangles one mark holds; later ones are dropped.
pub const MAX_HOST_MARK_RECTS: usize = 8;
/// The most rectangles a document holds over all its sets and pages; past it, rectangles are dropped.
pub const MAX_HOST_MARK_RECTS_TOTAL: usize = 200_000;
/// How far around a mark's rectangles a click still lands on it (page points).
const MARK_HIT_SLOP: f32 = 2.0;
/// The underline's thickness in CSS pixels (device pixels ÷ the display's ratio).
const MARK_LINE_CSS_PX: f32 = 2.0;

/// A document the host hands in: `name` is shown on the tab, `bytes` hold the file.
///
/// With a `tag`, a tab already showing that tag is brought to the front instead of opening a
/// second copy; `page` (clamped to the page count) is shown in either case. Without a tag it
/// opens like any other file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostOpen {
    pub name: String,
    pub bytes: Vec<u8>,
    pub tag: Option<String>,
    pub page: Option<usize>,
}

/// Documents from the host, taken once per frame under the same conditions as
/// [`PdfCraftApp::startup_inbox`] (never over a prompt, an import or an open in flight).
pub type HostInbox = std::sync::Arc<std::sync::Mutex<Vec<HostOpen>>>;

/// Something the host asks of a tagged document. More kinds may come; match with a wildcard.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum HostRequest {
    /// Save the tagged document through the normal Save path (never Save As), even when it has
    /// no unsaved changes; the bytes go to [`PdfCraftApp::host_save`]. While the user is typing
    /// (text, a form field, a comment) or a dialog is open, the save waits until that ends.
    /// When it can't be done, [`HostEvent::SaveFailed`] says why.
    Save { tag: String },
    /// Bring the tagged document to the front at `page` (clamped to the page count). With
    /// `find`, also look for that passage on or near the page through the find bar: the first
    /// match on the page, else the nearest one within two pages, else the same for the first
    /// six and then the first three of its words (a passage broken by hyphenation or a page
    /// furniture line still lands). [`HostEvent::Found`] tells how it went, once per request.
    Goto { tag: String, page: usize, find: Option<String> },
    /// Replace the tagged document's marks of `set` on each page listed (in that order; a page
    /// with no marks clears it), leaving its other pages as they are. See [`HostMark`]. A set
    /// that is changed comes on top of the others. Pages past the page count are ignored, and so
    /// is an unknown tag, or a new set when the document already has [`MAX_HOST_MARK_SETS`].
    Marks { tag: String, set: String, pages: Vec<(usize, Vec<HostMark>)> },
    /// Remove every mark of `set` from the tagged document.
    ClearMarks { tag: String, set: String },
}

/// A host's mark: an underline drawn above the page under each of `rects` (page view space,
/// `[x0, y0, x1, y1]`) in `color`. Marks are the host's overlay, never part of the document:
/// they are not annotations, are never saved, never make the document dirty and never enter
/// undo. They follow zoom, rotation and scrolling, stay while the tab is in the background, and
/// go when the document closes.
///
/// A mark's index is its position among its page's marks of its set, in the order sent; a mark
/// whose rectangles are all invalid (not finite, or with a negative size) keeps its index but
/// draws nothing and can't be clicked. At most [`MAX_HOST_MARKS_PER_PAGE`] marks per page and
/// set, [`MAX_HOST_MARK_RECTS`] rectangles per mark and [`MAX_HOST_MARK_RECTS_TOTAL`] per document
/// are kept.
#[derive(Clone, Debug, PartialEq)]
pub struct HostMark {
    pub rects: Vec<[f32; 4]>,
    pub color: egui::Color32,
}

/// An item the host adds to PdfCraft's own text-selection menus (the right-click menu and the
/// bar beside selected text), after Copy. `label` is shown as given (the host translates it);
/// clicking it sends [`HostEvent::Menu`] with `key`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostMenuItem {
    pub key: String,
    pub label: String,
}

/// The host's selection-menu items, in order (shown only while text is selected).
pub type HostMenuItems = Arc<Mutex<Vec<HostMenuItem>>>;

/// Requests from the host, taken once per frame.
pub type HostRequests = std::sync::Arc<std::sync::Mutex<Vec<HostRequest>>>;

/// What the app tells the host. Each is sent when it changes, not every frame.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum HostEvent {
    /// The tab in front changed, or a tagged document finished opening (or was asked for again).
    /// `tag` is `None` for a document the host didn't tag; with no document at all `page` is
    /// `None` and `page_count` is 0.
    Active { tag: Option<String>, page: Option<usize>, page_count: usize },
    /// The current page of the tagged document in front changed.
    Page { tag: String, page: usize },
    /// The user did something (pointer, wheel, keys, touch) while the tagged document was in front.
    /// Sent on every frame with input: the host throttles it.
    Activity { tag: String },
    /// Whether any tab has unsaved work, and the tags of the tagged documents that have.
    Dirty { any: bool, tags: Vec<String> },
    /// A tagged document could not be opened (unreadable file, password prompt cancelled…).
    OpenFailed { tag: String, error: String },
    /// A [`HostRequest::Save`] could not be done.
    SaveFailed { tag: String, error: String },
    /// The theme the interface is drawn with now.
    Theme { dark: bool },
    /// The text selected in the tagged document in front changed: its page, text (as Copy puts
    /// it, at most [`MAX_HOST_TEXT_CHARS`]) and one rectangle per selected piece of a line (page
    /// view space, at most [`MAX_HOST_RECTS`]). A cleared selection, or one left behind when
    /// another document comes to the front, is reported once with empty `text` and `rects`.
    /// Sent on every change while the user drags: the host debounces it.
    Selection { tag: String, page: usize, text: String, rects: Vec<[f32; 4]> },
    /// A [`HostMenuItem`] was clicked over selected text. `anchor` is the last selection
    /// rectangle on screen (egui points), for the host to place its own popup by. The text stays
    /// selected.
    Menu { key: String, tag: Option<String>, page: usize, text: String, rects: Vec<[f32; 4]>, anchor: [f32; 4] },
    /// Where the host's menu items are on screen (egui points) while a menu or the selection bar
    /// shows them; again when they move, and once with no items when they are gone.
    MenuShown { tag: Option<String>, items: Vec<(String, [f32; 4])> },
    /// How a [`HostRequest::Goto`] with a passage (or one for an unknown tag) went: `page` is the
    /// page of the match, or the page asked for when there is none; `query` the passage as looked
    /// for (spaces collapsed, cut to [`MAX_HOST_FIND_CHARS`]).
    Found { tag: String, page: usize, query: String, found: bool },
    /// A primary click (press and release without a drag) with the Select or Hand tool landed on
    /// a [`HostMark`] (its rectangles, 2 points around). `index` is the mark's index on `page` in
    /// `set`; `rect` the union of its rectangles on screen (egui points). Where sets overlap, the
    /// one changed last wins. The click still does what it always does.
    MarkClick { tag: String, set: String, index: usize, page: usize, rect: [f32; 4] },
    /// The marks of `set` on `page` were drawn for the first time since they last changed:
    /// `count` is how many that page holds (as indexed). Once per change, when the page is first
    /// on screen afterwards.
    MarksShown { tag: String, set: String, page: usize, count: usize },
}

/// Receives [`HostEvent`]s (set by the host).
pub type HostEventFn = Box<dyn FnMut(HostEvent)>;

/// A saved PDF on its way to the host (see [`crate::HostSaveFn`]).
#[derive(Clone, Copy, Debug)]
pub struct HostSave<'a> {
    pub name: &'a str,
    pub bytes: &'a [u8],
    /// `true` for Save As.
    pub save_as: bool,
    /// The document's tag, if the host gave it one.
    pub tag: Option<&'a str>,
    /// The password the document was opened with, so the host can read encrypted bytes before
    /// storing them (to add its own metadata, say). It must never leave the host's process.
    pub password: Option<&'a str>,
}

/// The app's side of the host hooks (see the module documentation).
#[derive(Default)]
pub(crate) struct HostState {
    /// The host's tag of each open document it tagged.
    pub(crate) tags: HashMap<DocId, String>,
    /// A tagged open waiting on a password prompt (or a native open in flight).
    pending: Option<PendingOpen>,
    /// Saves that wait for typing or a dialog to end, oldest first.
    deferred_saves: Vec<String>,
    /// The document and tag last reported in front; `None` before the first report.
    active: Option<(Option<DocId>, Option<String>)>,
    page: Option<usize>,
    /// Report the document in front again, even if it didn't change.
    force_active: bool,
    dirty: Option<(bool, Vec<String>)>,
    dark: Option<bool>,
    /// The selection last reported; `None` when none is (or the last one was cleared).
    selection: Option<SelectionReport>,
    /// The host items last reported on screen.
    menu_shown: Option<ShownItems>,
    /// A goto waiting for its page's text to look for its passage.
    goto: Option<PendingGoto>,
}

/// The tag of the document whose menu shows the host's items, and each item's key and rectangle.
type ShownItems = (Option<String>, Vec<(String, [f32; 4])>);

#[derive(Clone, Debug, PartialEq)]
struct SelectionReport {
    tag: String,
    page: usize,
    text: String,
    rects: Vec<[f32; 4]>,
}

struct PendingGoto {
    tag: String,
    doc: DocId,
    page: usize,
    /// The passage as asked, then its shorter fallbacks.
    queries: Vec<String>,
    started: f64,
    frames: u32,
}

/// What the selection menus hand from one frame's drawing to the next frame's [`HostState`]
/// update, through the context's temporary data (the menus are drawn with only the view).
#[derive(Clone, Default)]
struct MenuFrame {
    items: Arc<Vec<HostMenuItem>>,
    /// The items drawn this frame: the document, the key and the screen rectangle.
    shown: Vec<(DocId, String, [f32; 4])>,
    click: Option<MenuClick>,
}

#[derive(Clone)]
struct MenuClick {
    doc: DocId,
    key: String,
    page: usize,
    text: String,
    rects: Vec<[f32; 4]>,
    anchor: [f32; 4],
}

fn menu_frame_id() -> egui::Id {
    egui::Id::new("pdfcraft-host-selection-menu")
}

fn rect_array(r: egui::Rect) -> [f32; 4] {
    [r.min.x, r.min.y, r.max.x, r.max.y]
}

/// `text` cut to at most `max` characters (at a character boundary).
fn cap_chars(mut text: String, max: usize) -> String {
    if let Some((end, _)) = text.char_indices().nth(max) {
        text.truncate(end);
    }
    text
}

/// A passage to look for: trimmed, runs of whitespace as one space, at most
/// [`MAX_HOST_FIND_CHARS`]. `None` when nothing is left.
pub(crate) fn normalize_query(find: &str) -> Option<String> {
    let collapsed = find.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped = cap_chars(collapsed, MAX_HOST_FIND_CHARS);
    let trimmed = capped.trim_end();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The passage, then its first six and first three words when it has more.
fn query_fallbacks(query: &str) -> Vec<String> {
    let words: Vec<&str> = query.split(' ').collect();
    let mut out = vec![query.to_string()];
    for n in [6, 3] {
        if words.len() > n
            && let Some(first) = words.get(..n)
        {
            out.push(first.join(" "));
        }
    }
    out
}

/// The pages within [`GOTO_NEAR_PAGES`] of `page`, nearest first (the next page before the
/// previous one at the same distance), inside `0..count`.
fn near_pages(page: usize, count: usize) -> Vec<usize> {
    let mut out = vec![page];
    for d in 1..=GOTO_NEAR_PAGES {
        if let Some(p) = page.checked_add(d).filter(|p| *p < count) {
            out.push(p);
        }
        if let Some(p) = page.checked_sub(d) {
            out.push(p);
        }
    }
    out.retain(|p| *p < count);
    out
}

/// The host's items at the end of one of PdfCraft's text-selection menus, after a separator:
/// in a column (the right-click menu) or a row (the selection bar). Nothing without items or
/// without selected text. Returns whether one was clicked; the click itself reaches the host as
/// [`HostEvent::Menu`] on the next frame.
pub(crate) fn selection_menu_items(ui: &mut egui::Ui, view: &DocView, row: bool) -> bool {
    let items = ui.data(|d| d.get_temp::<MenuFrame>(menu_frame_id())).map(|f| f.items).unwrap_or_default();
    if items.is_empty() {
        return false;
    }
    let Some((page, text, rects)) = view.host_selection() else { return false };
    ui.separator();
    let mut shown = Vec::with_capacity(items.len());
    let mut clicked = None;
    let mut buttons = |ui: &mut egui::Ui| {
        for item in items.iter() {
            let resp = ui.button(item.label.as_str());
            shown.push((view.id, item.key.clone(), rect_array(resp.rect)));
            if resp.clicked() {
                clicked = Some(item.key.clone());
            }
        }
    };
    if row {
        ui.horizontal(|ui| buttons(ui));
    } else {
        buttons(ui);
    }
    let anchor = rects.last().zip(view.page_xform(page)).map_or([0.0; 4], |(r, xf)| rect_array(xf.view_rect(*r)));
    let click = clicked.map(|key| MenuClick {
        doc: view.id,
        key,
        page,
        text: cap_chars(text, MAX_HOST_TEXT_CHARS),
        rects: rects.into_iter().take(MAX_HOST_RECTS).collect(),
        anchor,
    });
    let was_clicked = click.is_some();
    ui.data_mut(|d| {
        let frame = d.get_temp_mut_or_default::<MenuFrame>(menu_frame_id());
        frame.shown.extend(shown);
        if click.is_some() {
            frame.click = click;
        }
    });
    if was_clicked {
        ui.ctx().request_repaint();
    }
    was_clicked
}

/// A document's [`HostMark`]s, kept on its view (see [`HostRequest::Marks`]).
#[derive(Clone, Debug, Default)]
pub(crate) struct DocMarks {
    /// The sets, the one changed last at the end: drawn last (on top) and first to take a click.
    sets: Vec<MarkSet>,
    /// Rectangles held over all sets and pages, against [`MAX_HOST_MARK_RECTS_TOTAL`].
    rects: usize,
    /// Pages drawn for the first time since their marks changed: set, page and mark count.
    shown: Vec<(String, usize, usize)>,
    /// The last click on a mark, for the next [`HostState`] update.
    click: Option<MarkHit>,
}

#[derive(Clone, Debug)]
struct MarkSet {
    name: String,
    pages: BTreeMap<usize, PageMarks>,
}

#[derive(Clone, Debug)]
struct PageMarks {
    marks: Vec<HostMark>,
    /// Drawn since it last changed (reported through [`HostEvent::MarksShown`]).
    shown: bool,
}

impl PageMarks {
    fn rect_count(&self) -> usize {
        self.marks.iter().map(|m| m.rects.len()).sum()
    }
}

/// A click on a mark: the set, the mark's index, the page and the mark on screen.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MarkHit {
    pub(crate) set: String,
    pub(crate) index: usize,
    pub(crate) page: usize,
    pub(crate) rect: [f32; 4],
}

/// A finite rectangle with no negative side, as `[x0, y0, x1, y1]`.
fn valid_mark_rect(r: &[f32; 4]) -> bool {
    r.iter().all(|v| v.is_finite()) && r[2] >= r[0] && r[3] >= r[1]
}

impl DocMarks {
    /// Whether the document holds no marks.
    pub(crate) fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }

    /// See [`HostRequest::Marks`]; `page_count` bounds the pages.
    pub(crate) fn set(&mut self, name: String, pages: Vec<(usize, Vec<HostMark>)>, page_count: usize) {
        if name.is_empty() {
            return;
        }
        let mut set = match self.sets.iter().position(|s| s.name == name) {
            Some(at) => self.sets.remove(at),
            None if self.sets.len() >= MAX_HOST_MARK_SETS => return,
            None => MarkSet { name, pages: BTreeMap::new() },
        };
        for (page, marks) in pages {
            if page >= page_count {
                continue;
            }
            if let Some(old) = set.pages.remove(&page) {
                self.rects = self.rects.saturating_sub(old.rect_count());
            }
            let mut kept = Vec::with_capacity(marks.len().min(MAX_HOST_MARKS_PER_PAGE));
            for mark in marks.into_iter().take(MAX_HOST_MARKS_PER_PAGE) {
                let room = MAX_HOST_MARK_RECTS_TOTAL.saturating_sub(self.rects).min(MAX_HOST_MARK_RECTS);
                let rects: Vec<[f32; 4]> = mark.rects.into_iter().filter(valid_mark_rect).take(room).collect();
                self.rects = self.rects.saturating_add(rects.len());
                kept.push(HostMark { rects, color: mark.color });
            }
            if kept.iter().any(|m| !m.rects.is_empty()) {
                set.pages.insert(page, PageMarks { marks: kept, shown: false });
            }
        }
        if !set.pages.is_empty() {
            self.sets.push(set);
        }
    }

    /// See [`HostRequest::ClearMarks`].
    pub(crate) fn clear(&mut self, name: &str) {
        if let Some(at) = self.sets.iter().position(|s| s.name == name) {
            let set = self.sets.remove(at);
            let gone: usize = set.pages.values().map(PageMarks::rect_count).sum();
            self.rects = self.rects.saturating_sub(gone);
        }
    }

    /// The marks of `set` on `page`, as held (for tests and inspection).
    #[cfg(test)]
    fn page(&self, set: &str, page: usize) -> Option<&[HostMark]> {
        self.sets.iter().find(|s| s.name == set)?.pages.get(&page).map(|p| p.marks.as_slice())
    }

    /// Underline the marks of `page` (on screen, through `xf`), the last set changed on top. A
    /// line is `width` points thick, just under each rectangle's bottom as the page is read (so
    /// it turns with the view). Records the sets drawn for the first time since they changed.
    pub(crate) fn paint(&mut self, painter: &egui::Painter, xf: &crate::canvas::PageXform, page: usize, width: f32) {
        let zoom = xf.zoom();
        if !(zoom.is_finite() && zoom > 0.0 && width.is_finite() && width > 0.0) {
            return;
        }
        // Half a point of air under the glyphs, then the line's own half thickness (page points).
        let drop = (0.5 + width / 2.0) / zoom;
        let to_screen = |x: f32, y: f32| xf.norm_to_screen(x / xf.pw, y / xf.ph);
        for set in &mut self.sets {
            let Some(marks) = set.pages.get_mut(&page) else { continue };
            for mark in &marks.marks {
                let stroke = egui::Stroke::new(width, mark.color);
                for r in &mark.rects {
                    let y = r[3] + drop;
                    painter.line_segment([to_screen(r[0], y), to_screen(r[2], y)], stroke);
                }
            }
            if !marks.shown {
                marks.shown = true;
                self.shown.push((set.name.clone(), page, marks.marks.len()));
                // Reported by the next frame's update.
                painter.ctx().request_repaint();
            }
        }
    }

    /// The mark under the view-space point `(vx, vy)` of `page`: the last set changed first,
    /// then the lowest index. Its rectangle is the union of the mark's rectangles (view space).
    pub(crate) fn hit(&self, page: usize, (vx, vy): (f32, f32)) -> Option<(String, usize, [f32; 4])> {
        for set in self.sets.iter().rev() {
            let Some(marks) = set.pages.get(&page) else { continue };
            for (index, mark) in marks.marks.iter().enumerate() {
                let inside = mark
                    .rects
                    .iter()
                    .any(|r| vx >= r[0] - MARK_HIT_SLOP && vx <= r[2] + MARK_HIT_SLOP && vy >= r[1] - MARK_HIT_SLOP && vy <= r[3] + MARK_HIT_SLOP);
                if !inside {
                    continue;
                }
                let union = mark.rects.iter().fold([f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY], |u, r| {
                    [u[0].min(r[0]), u[1].min(r[1]), u[2].max(r[2]), u[3].max(r[3])]
                });
                return Some((set.name.clone(), index, union));
            }
        }
        None
    }

    /// Remember a click on a mark for the next update to report.
    pub(crate) fn clicked(&mut self, hit: MarkHit) {
        self.click = Some(hit);
    }

    fn take_events(&mut self) -> (Vec<(String, usize, usize)>, Option<MarkHit>) {
        (std::mem::take(&mut self.shown), self.click.take())
    }
}

/// The underline thickness in egui points: [`MARK_LINE_CSS_PX`] CSS pixels. The display's own
/// ratio (`native_pixels_per_point`, the browser's device pixel ratio on the web) over the points'
/// size in pixels, so PdfCraft's interface scale doesn't thicken it.
pub(crate) fn mark_line_width(ctx: &egui::Context) -> f32 {
    let ppp = ctx.pixels_per_point();
    let native = ctx.input(|i| i.viewport().native_pixels_per_point).unwrap_or(ppp);
    let width = MARK_LINE_CSS_PX * native / ppp;
    if width.is_finite() { width.clamp(0.5, 8.0) } else { MARK_LINE_CSS_PX }
}

struct PendingOpen {
    tag: String,
    page: Option<usize>,
    /// The documents open before it, so the new one is the one not among them.
    known: HashSet<DocId>,
}

impl PdfCraftApp {
    fn emit(&mut self, event: HostEvent) {
        if let Some(report) = self.host_events.as_mut() {
            report(event);
        }
    }

    fn view_with_tag(&self, tag: &str) -> Option<usize> {
        self.views.iter().position(|v| self.host.tags.get(&v.id).is_some_and(|t| t == tag))
    }

    /// Open what the host handed in. Called where the startup files are taken, under the same
    /// conditions; a document that raises a password prompt stops the queue until it's answered.
    pub(crate) fn take_host_opens(&mut self) {
        self.resolve_pending_host_open();
        if self.host.pending.is_some() {
            return;
        }
        let queued = self.host_inbox.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        let mut queued = queued.into_iter();
        while let Some(open) = queued.next() {
            self.host_open(open);
            if self.host.pending.is_some() {
                // The rest wait for the prompt, ahead of anything that arrived meanwhile.
                let rest: Vec<HostOpen> = queued.collect();
                if !rest.is_empty()
                    && let Ok(mut q) = self.host_inbox.lock()
                {
                    q.splice(0..0, rest);
                }
                return;
            }
        }
    }

    fn host_open(&mut self, open: HostOpen) {
        let HostOpen { name, bytes, tag, page } = open;
        let Some(tag) = tag else {
            if let Err(e) = self.open_bytes(&name, None, bytes) {
                self.notify_fmt("Couldn't open {name}: {e}", &[("name", &name), ("e", &e)]);
            }
            return;
        };
        if let Some(index) = self.view_with_tag(&tag) {
            self.active = Some(index);
            if let (Some(page), Some(view)) = (page, self.views.get_mut(index)) {
                view.go_to_page(page);
            }
            self.host.force_active = true;
            return;
        }
        let known = self.views.iter().map(|v| v.id).collect();
        match self.open_bytes(&name, None, bytes) {
            Ok(()) => {
                self.host.pending = Some(PendingOpen { tag, page, known });
                self.resolve_pending_host_open();
            }
            Err(e) => {
                self.notify_fmt("Couldn't open {name}: {e}", &[("name", &name), ("e", &e)]);
                self.emit(HostEvent::OpenFailed { tag, error: e });
            }
        }
    }

    /// Once nothing is being asked or opened any more, the tagged open has either produced a new
    /// document (tag it, show its page) or failed (the prompt was cancelled, the file unreadable).
    fn resolve_pending_host_open(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        let opening = !self.pending_opens.is_empty();
        #[cfg(target_arch = "wasm32")]
        let opening = false;
        if opening || self.password_prompt.is_some() {
            return;
        }
        let Some(pending) = self.host.pending.take() else { return };
        match self.views.iter().position(|v| !pending.known.contains(&v.id)) {
            Some(index) => {
                if let Some(view) = self.views.get_mut(index) {
                    self.host.tags.insert(view.id, pending.tag);
                    if let Some(page) = pending.page {
                        view.go_to_page(page);
                    }
                }
                self.host.force_active = true;
            }
            None => {
                let error = "the document could not be opened".to_string();
                self.emit(HostEvent::OpenFailed { tag: pending.tag, error });
            }
        }
    }

    /// Whether a host save of tab `index` must wait: the user is typing into it (text, a form
    /// field, a comment) or a dialog is open. Saving would cut the typing short.
    fn host_save_waits(&self, index: usize) -> bool {
        if self.modal_open() || self.palette_open {
            return true;
        }
        let Some(v) = self.views.get(index) else { return false };
        let form_typing =
            self.session.get(v.id).is_some_and(|doc| v.forms.focus.as_ref().is_some_and(|f| crate::forms_ui::draft_edit(f, &doc.form).is_some()));
        form_typing
            || v.pending_edit.is_some()
            || v.line_editor.is_some()
            || v.content.draft.is_some()
            || v.fill_text.is_some()
            // The empty editor that opens on a new highlight or note isn't typing yet; waiting
            // for it would hold the save until the user dismisses it.
            || v.comments.composer.as_ref().is_some_and(|c| !c.text.trim().is_empty())
            || v.comments.editing.as_ref().is_some_and(|(_, _, text)| !text.trim().is_empty())
            || !v.comments.reply.trim().is_empty()
            || !v.comments.add_box.trim().is_empty()
    }

    /// Run the host's requests and report what changed since the last frame.
    pub(crate) fn host_tick(&mut self, ctx: &egui::Context) {
        // Forget the tags of closed documents.
        let open: HashSet<DocId> = self.views.iter().map(|v| v.id).collect();
        self.host.tags.retain(|id, _| open.contains(id));

        let requests = self.host_requests.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        for request in requests {
            match request {
                HostRequest::Save { tag } => {
                    if !self.host.deferred_saves.contains(&tag) {
                        self.host.deferred_saves.push(tag);
                    }
                }
                HostRequest::Goto { tag, page, find } => self.host_goto(ctx, tag, page, find),
                HostRequest::Marks { tag, set, pages } => {
                    let Some(index) = self.view_with_tag(&tag) else { continue };
                    let count = self.views.get(index).and_then(|v| self.session.get(v.id)).map_or(0, |d| d.info.pages.len());
                    if let Some(view) = self.views.get_mut(index) {
                        view.host_marks.set(set, pages, count);
                    }
                    ctx.request_repaint();
                }
                HostRequest::ClearMarks { tag, set } => {
                    if let Some(view) = self.view_with_tag(&tag).and_then(|i| self.views.get_mut(i)) {
                        view.host_marks.clear(&set);
                    }
                    ctx.request_repaint();
                }
            }
        }
        self.report_marks();
        self.resolve_goto(ctx);
        let saves = std::mem::take(&mut self.host.deferred_saves);
        for tag in saves {
            let Some(index) = self.view_with_tag(&tag) else {
                self.emit(HostEvent::SaveFailed { tag, error: "no open document has this id".into() });
                continue;
            };
            if self.host_save.is_none() {
                self.emit(HostEvent::SaveFailed { tag, error: "there is no host to save to".into() });
            } else if self.host_save_waits(index) {
                self.host.deferred_saves.push(tag);
            } else if !self.save_view(index, crate::SaveTarget::InPlace) {
                self.emit(HostEvent::SaveFailed { tag, error: "the document could not be saved".into() });
            }
        }
        if !self.host.deferred_saves.is_empty() {
            // Look again soon: the typing may end without any input reaching this window.
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }

        if self.host_events.is_none() {
            return;
        }
        self.report_active();
        self.report_selection();
        self.report_menu(ctx);
        self.report_activity(ctx);
        let tags = {
            let mut tags: Vec<String> = (0..self.views.len())
                .filter(|&i| self.has_unsaved_work(i))
                .filter_map(|i| self.views.get(i).and_then(|v| self.host.tags.get(&v.id)).cloned())
                .collect();
            tags.sort();
            tags
        };
        let dirty = (self.first_dirty().is_some(), tags);
        if self.host.dirty.as_ref() != Some(&dirty) {
            self.host.dirty = Some(dirty.clone());
            self.emit(HostEvent::Dirty { any: dirty.0, tags: dirty.1 });
        }
        let dark = self.theme == crate::ThemeKind::Dark;
        if self.host.dark != Some(dark) {
            self.host.dark = Some(dark);
            self.emit(HostEvent::Theme { dark });
        }
    }

    fn report_active(&mut self) {
        let front = self.active_ids().map(|(index, id)| {
            let tag = self.host.tags.get(&id).cloned();
            let page = self.views.get(index).map_or(0, |v| v.current);
            let count = self.session.get(id).map_or(0, |d| d.info.pages.len());
            (id, tag, page, count)
        });
        let key = (front.as_ref().map(|f| f.0), front.as_ref().and_then(|f| f.1.clone()));
        if self.host.force_active || self.host.active.as_ref() != Some(&key) {
            self.host.force_active = false;
            self.host.active = Some(key);
            self.host.page = front.as_ref().map(|f| f.2);
            let event = match front {
                Some((_, tag, page, page_count)) => HostEvent::Active { tag, page: Some(page), page_count },
                None => HostEvent::Active { tag: None, page: None, page_count: 0 },
            };
            self.emit(event);
            return;
        }
        if let Some((_, Some(tag), page, _)) = front
            && self.host.page != Some(page)
        {
            self.host.page = Some(page);
            self.emit(HostEvent::Page { tag, page });
        }
    }

    /// Report the clicks on marks and the marks drawn since the last frame.
    fn report_marks(&mut self) {
        let mut events = Vec::new();
        for view in &mut self.views {
            let (shown, click) = view.host_marks.take_events();
            let Some(tag) = self.host.tags.get(&view.id) else { continue };
            for (set, page, count) in shown {
                events.push(HostEvent::MarksShown { tag: tag.clone(), set, page, count });
            }
            if let Some(MarkHit { set, index, page, rect }) = click {
                events.push(HostEvent::MarkClick { tag: tag.clone(), set, index, page, rect });
            }
        }
        for event in events {
            self.emit(event);
        }
    }

    fn report_activity(&mut self, ctx: &egui::Context) {
        let Some(tag) = self.active_ids().and_then(|(_, id)| self.host.tags.get(&id).cloned()) else { return };
        let active = ctx.input(|i| {
            i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::PointerMoved(_)
                        | egui::Event::PointerButton { .. }
                        | egui::Event::MouseWheel { .. }
                        | egui::Event::Key { pressed: true, .. }
                        | egui::Event::Text(_)
                        | egui::Event::Touch { .. }
                        | egui::Event::Zoom(_)
                )
            })
        });
        if active {
            self.emit(HostEvent::Activity { tag });
        }
    }

    /// Report the text selected in the tagged document in front when it changes, and clear the
    /// one reported before once it is gone (deselected, or its document left the front).
    fn report_selection(&mut self) {
        let now = self.active_ids().and_then(|(index, id)| {
            let tag = self.host.tags.get(&id)?.clone();
            let (page, text, rects) = self.views.get(index)?.host_selection()?;
            Some(SelectionReport { tag, page, text: cap_chars(text, MAX_HOST_TEXT_CHARS), rects: rects.into_iter().take(MAX_HOST_RECTS).collect() })
        });
        match now {
            Some(report) => {
                if let Some(last) = self.host.selection.as_ref()
                    && last.tag != report.tag
                {
                    let (tag, page) = (last.tag.clone(), last.page);
                    self.emit(HostEvent::Selection { tag, page, text: String::new(), rects: Vec::new() });
                }
                if self.host.selection.as_ref() != Some(&report) {
                    self.host.selection = Some(report.clone());
                    let SelectionReport { tag, page, text, rects } = report;
                    self.emit(HostEvent::Selection { tag, page, text, rects });
                }
            }
            None => {
                if let Some(SelectionReport { tag, page, .. }) = self.host.selection.take() {
                    self.emit(HostEvent::Selection { tag, page, text: String::new(), rects: Vec::new() });
                }
            }
        }
    }

    /// Hand this frame's host menu items to the selection menus, and report what last frame's
    /// menus showed and which item was clicked.
    fn report_menu(&mut self, ctx: &egui::Context) {
        let items = self.host_menu_items.lock().map(|v| v.clone()).unwrap_or_default();
        let (shown, click) = ctx.data_mut(|d| {
            let frame = d.get_temp_mut_or_default::<MenuFrame>(menu_frame_id());
            if frame.items.as_slice() != items.as_slice() {
                frame.items = Arc::new(items);
            }
            (std::mem::take(&mut frame.shown), frame.click.take())
        });
        let tag_of = |app: &Self, doc: DocId| app.host.tags.get(&doc).cloned();
        let shown_now: ShownItems = match shown.first() {
            Some((doc, _, _)) => {
                let doc = *doc;
                (tag_of(self, doc), shown.into_iter().filter(|(d, _, _)| *d == doc).map(|(_, key, rect)| (key, rect)).collect())
            }
            None => (self.host.menu_shown.as_ref().and_then(|(tag, _)| tag.clone()), Vec::new()),
        };
        let changed = match self.host.menu_shown.as_ref() {
            Some(last) => *last != shown_now,
            None => !shown_now.1.is_empty(),
        };
        if changed {
            self.host.menu_shown = Some(shown_now.clone());
            let (tag, items) = shown_now;
            self.emit(HostEvent::MenuShown { tag, items });
        }
        if let Some(MenuClick { doc, key, page, text, rects, anchor }) = click {
            let tag = tag_of(self, doc);
            self.emit(HostEvent::Menu { key, tag, page, text, rects, anchor });
        }
    }

    /// Start a [`HostRequest::Goto`]: the document to the front at its page, and its passage to
    /// look for once the page's text is in.
    fn host_goto(&mut self, ctx: &egui::Context, tag: String, page: usize, find: Option<String>) {
        if let Some(old) = self.host.goto.take() {
            // A newer goto replaces it: still answered, so the host stops waiting.
            let query = old.queries.into_iter().next().unwrap_or_default();
            self.emit(HostEvent::Found { tag: old.tag, page: old.page, query, found: false });
        }
        let query = find.as_deref().and_then(normalize_query);
        let Some(index) = self.view_with_tag(&tag) else {
            self.emit(HostEvent::Found { tag, page, query: query.unwrap_or_default(), found: false });
            return;
        };
        self.active = Some(index);
        self.host.force_active = true;
        let Some(view) = self.views.get_mut(index) else { return };
        view.go_to_page(page);
        let (doc, page) = (view.id, view.current);
        let Some(query) = query else { return };
        let count = self.session.get(doc).map_or(0, |d| d.info.pages.len());
        if count == 0 {
            self.emit(HostEvent::Found { tag, page, query, found: false });
            return;
        }
        let started = ctx.input(|i| i.time);
        self.host.goto = Some(PendingGoto { tag, doc, page, queries: query_fallbacks(&query), started, frames: 0 });
        ctx.request_repaint();
    }

    /// Look for a pending goto's passage once its page's text is in; give up when the user moves
    /// on (input on the page, another tab), the document closes, or the text never comes.
    fn resolve_goto(&mut self, ctx: &egui::Context) {
        let Some(goto) = self.host.goto.as_mut() else { return };
        goto.frames = goto.frames.saturating_add(1);
        let (doc, page) = (goto.doc, goto.page);
        let timed_out = ctx.input(|i| i.time) - goto.started > GOTO_TIMEOUT_S || goto.frames > GOTO_TIMEOUT_FRAMES;
        let index = self.views.iter().position(|v| v.id == doc);
        let moved_on = index.is_none()
            || self.active != index
            || ctx.input(|i| {
                i.events.iter().any(|e| {
                    matches!(
                        e,
                        egui::Event::PointerButton { pressed: true, .. }
                            | egui::Event::MouseWheel { .. }
                            | egui::Event::Key { pressed: true, .. }
                            | egui::Event::Text(_)
                            | egui::Event::Zoom(_)
                    )
                })
            });
        let count = self.session.get(doc).map_or(0, |d| d.info.pages.len());
        let Some(view) = index.and_then(|i| self.views.get_mut(i)) else {
            return self.finish_goto(None);
        };
        if moved_on || timed_out {
            return self.finish_goto(None);
        }
        let near = near_pages(page, count);
        let known = |p: &usize| view.page_text(*p).is_some() || view.text_failed.contains(p);
        if !near.iter().all(known) {
            // The canvas extracts these pages' text, on screen or not.
            view.host_text_pages.clone_from(&near);
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
            return;
        }
        let queries = self.host.goto.as_ref().map(|g| g.queries.clone()).unwrap_or_default();
        let hit = queries.iter().find_map(|q| {
            near.iter().find_map(|&p| {
                let text = view.page_text(p)?;
                let range = text.find_opts(q, false, false).into_iter().next()?;
                Some((q.clone(), p, range))
            })
        });
        let Some((query, p, range)) = hit else { return self.finish_goto(None) };
        // Through PdfCraft's own find bar: the passage stays highlighted and Next/Previous go on
        // from it.
        let find = view.find.get_or_insert_with(crate::canvas::Find::default);
        find.query = query;
        find.in_panel = false;
        find.case_sensitive = false;
        find.whole_words = false;
        find.focus = false;
        view.rerun_find();
        let at = view.find.as_ref().and_then(|f| f.matches.iter().position(|m| m.0 == p && m.1 == range));
        match at {
            Some(i) => {
                view.go_to_match(i);
                self.finish_goto(Some(p));
            }
            None => self.finish_goto(None),
        }
    }

    fn finish_goto(&mut self, found_on: Option<usize>) {
        let Some(goto) = self.host.goto.take() else { return };
        if let Some(view) = self.views.iter_mut().find(|v| v.id == goto.doc) {
            view.host_text_pages.clear();
        }
        let query = goto.queries.into_iter().next().unwrap_or_default();
        self.emit(HostEvent::Found { tag: goto.tag, page: found_on.unwrap_or(goto.page), query, found: found_on.is_some() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CORAL: egui::Color32 = egui::Color32::from_rgb(0xd8, 0x5a, 0x30);

    fn mark(rects: &[[f32; 4]]) -> HostMark {
        HostMark { rects: rects.to_vec(), color: CORAL }
    }

    fn word(x: f32) -> HostMark {
        mark(&[[x, 100.0, x + 30.0, 112.0]])
    }

    fn rects_of(m: &DocMarks, set: &str, page: usize) -> Vec<Vec<[f32; 4]>> {
        m.page(set, page).map(|p| p.iter().map(|m| m.rects.clone()).collect()).unwrap_or_default()
    }

    #[test]
    fn marks_replace_only_the_pages_sent() {
        let mut m = DocMarks::default();
        m.set("vocab".into(), vec![(0, vec![word(10.0), word(50.0)]), (2, vec![word(10.0)])], 5);
        assert_eq!(rects_of(&m, "vocab", 0).len(), 2);
        assert_eq!(m.rects, 3);
        // Page 0 is replaced, page 2 stays.
        m.set("vocab".into(), vec![(0, vec![word(90.0)])], 5);
        assert_eq!(rects_of(&m, "vocab", 0), [vec![[90.0, 100.0, 120.0, 112.0]]]);
        assert_eq!(rects_of(&m, "vocab", 2).len(), 1);
        assert_eq!(m.rects, 2);
        // An empty page entry clears the page; the set goes once it has no page left.
        m.set("vocab".into(), vec![(0, vec![])], 5);
        assert!(m.page("vocab", 0).is_none());
        m.set("vocab".into(), vec![(2, vec![mark(&[])])], 5);
        assert!(m.is_empty());
        assert_eq!(m.rects, 0);
        // Pages past the end are ignored.
        m.set("vocab".into(), vec![(5, vec![word(0.0)]), (usize::MAX, vec![word(0.0)])], 5);
        assert!(m.is_empty());
    }

    #[test]
    fn clear_removes_one_set_and_its_rect_budget() {
        let mut m = DocMarks::default();
        m.set("a".into(), vec![(0, vec![word(0.0)]), (1, vec![word(0.0)])], 2);
        m.set("b".into(), vec![(0, vec![word(0.0)])], 2);
        m.clear("a");
        assert!(m.page("a", 0).is_none() && m.page("a", 1).is_none());
        assert_eq!(rects_of(&m, "b", 0).len(), 1);
        assert_eq!(m.rects, 1);
        m.clear("nope");
        m.clear("b");
        assert!(m.is_empty());
        assert_eq!(m.rects, 0);
    }

    #[test]
    fn invalid_rects_are_dropped_but_their_mark_keeps_its_index() {
        let mut m = DocMarks::default();
        let bad = mark(&[[f32::NAN, 0.0, 1.0, 1.0], [0.0, f32::INFINITY, 1.0, 1.0], [5.0, 0.0, 4.0, 1.0], [0.0, 5.0, 1.0, 4.0]]);
        m.set("v".into(), vec![(0, vec![bad, word(40.0)])], 1);
        assert_eq!(rects_of(&m, "v", 0), [vec![], vec![[40.0, 100.0, 70.0, 112.0]]]);
        let hit = m.hit(0, (50.0, 105.0)).map(|h| h.1);
        assert_eq!(hit, Some(1), "the second mark is still index 1");
    }

    #[test]
    fn limits_hold() {
        let mut m = DocMarks::default();
        for i in 0..MAX_HOST_MARK_SETS + 3 {
            m.set(format!("s{i}"), vec![(0, vec![word(0.0)])], 1);
        }
        assert_eq!(m.sets.len(), MAX_HOST_MARK_SETS);
        assert!(m.page(&format!("s{}", MAX_HOST_MARK_SETS), 0).is_none(), "a seventeenth set is ignored");
        // An existing set can still change.
        m.set("s0".into(), vec![(0, vec![word(1.0), word(2.0)])], 1);
        assert_eq!(rects_of(&m, "s0", 0).len(), 2);

        let mut m = DocMarks::default();
        let many: Vec<[f32; 4]> = (0..20).map(|i| [i as f32, 0.0, i as f32 + 1.0, 1.0]).collect();
        m.set("v".into(), vec![(0, vec![mark(&many)])], 1);
        assert_eq!(rects_of(&m, "v", 0)[0].len(), MAX_HOST_MARK_RECTS);
        let lots: Vec<HostMark> = (0..MAX_HOST_MARKS_PER_PAGE + 10).map(|i| word(i as f32)).collect();
        m.set("v".into(), vec![(0, lots)], 1);
        assert_eq!(rects_of(&m, "v", 0).len(), MAX_HOST_MARKS_PER_PAGE);

        // The document-wide budget: 60 pages × 4 000 marks × 1 rect > 200 000.
        let mut m = DocMarks::default();
        let pages: Vec<(usize, Vec<HostMark>)> = (0..60).map(|p| (p, (0..MAX_HOST_MARKS_PER_PAGE).map(|i| word(i as f32)).collect())).collect();
        m.set("v".into(), pages, 60);
        assert_eq!(m.rects, MAX_HOST_MARK_RECTS_TOTAL);
        let held: usize = (0..60).map(|p| rects_of(&m, "v", p).iter().map(Vec::len).sum::<usize>()).sum();
        assert_eq!(held, MAX_HOST_MARK_RECTS_TOTAL);
        // Clearing frees the budget for another set.
        m.clear("v");
        m.set("w".into(), vec![(0, vec![word(0.0)])], 60);
        assert_eq!(m.rects, 1);
    }

    #[test]
    fn hits_inflate_by_two_points_and_the_last_set_changed_wins() {
        let mut m = DocMarks::default();
        m.set("a".into(), vec![(0, vec![word(10.0), mark(&[[100.0, 100.0, 130.0, 112.0], [10.0, 120.0, 40.0, 132.0]])])], 1);
        assert_eq!(m.hit(0, (20.0, 106.0)), Some(("a".into(), 0, [10.0, 100.0, 40.0, 112.0])));
        assert_eq!(m.hit(0, (8.5, 113.5)).map(|h| h.1), Some(0), "within 2 points");
        assert_eq!(m.hit(0, (7.0, 106.0)), None, "past 2 points");
        assert_eq!(m.hit(0, (20.0, 125.0)), Some(("a".into(), 1, [10.0, 100.0, 130.0, 132.0])), "a mark's union of rectangles");
        assert_eq!(m.hit(1, (20.0, 106.0)), None, "another page");

        m.set("b".into(), vec![(0, vec![word(15.0)])], 1);
        assert_eq!(m.hit(0, (20.0, 106.0)).map(|h| (h.0, h.1)), Some(("b".into(), 0)));
        // Changing "a" brings it back on top.
        m.set("a".into(), vec![(0, vec![word(10.0)])], 1);
        assert_eq!(m.hit(0, (20.0, 106.0)).map(|h| h.0), Some("a".into()));
        m.clear("a");
        assert_eq!(m.hit(0, (20.0, 106.0)).map(|h| h.0), Some("b".into()));
    }

    #[test]
    fn hits_survive_hostile_points() {
        let mut m = DocMarks::default();
        m.set("a".into(), vec![(0, vec![word(10.0)])], 1);
        for p in [(f32::NAN, 0.0), (f32::INFINITY, f32::NEG_INFINITY), (f32::MAX, f32::MIN)] {
            assert_eq!(m.hit(0, p), None);
        }
    }
}
