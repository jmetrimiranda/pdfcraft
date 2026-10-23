//! Hooks for a host embedding PdfCraft, such as a same-origin page framing the web build
//! (`apps/pdfcraft-web`, `?host=parent`). The host hands documents in tagged with its own id,
//! asks for saves, and hears what the user is doing: which tagged document is in front, its
//! page, activity, unsaved work and the theme.
//!
//! This crate stays free of any browser API: the host owns the transport (`postMessage` on the
//! web), the throttling and the debouncing. Here the app only takes requests from shared queues
//! once per frame and reports *changes* through [`HostEventFn`].
//!
//! A tag is an opaque string the host chose (one per document it knows); it belongs to the
//! open document, not to a tab position, so it survives tabs being moved, and is forgotten when
//! the document is closed. Pages are 0-based page indices.

use std::collections::{HashMap, HashSet};

use pdfcraft_engine::DocId;

use crate::PdfCraftApp;

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
}

/// Requests from the host, taken once per frame.
pub type HostRequests = std::sync::Arc<std::sync::Mutex<Vec<HostRequest>>>;

/// What the app tells the host. Each is sent when it changes, not every frame.
#[derive(Clone, Debug, PartialEq, Eq)]
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
            }
        }
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
}
