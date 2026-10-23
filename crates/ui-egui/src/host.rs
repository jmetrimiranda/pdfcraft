//! Hooks for a host embedding PdfCraft, such as a same-origin page framing the web build
//! (`apps/pdfcraft-web`, `?host=parent`). The host hands documents in tagged with its own id,
//! asks for saves, sends the reader to a page (and a passage on it), adds its own items to the
//! text-selection menus, and hears what the user is doing: which tagged document is in front,
//! its page, the text selected in it, activity, unsaved work and the theme.
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

use std::collections::{HashMap, HashSet};
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
            }
        }
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
