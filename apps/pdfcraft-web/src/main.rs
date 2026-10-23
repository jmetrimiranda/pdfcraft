//! PdfCraft in the browser. Build: `cd apps/pdfcraft-web && trunk build --release`
//! (trunk generates the JS loader; no handwritten JS — plan/execution-plan.md §1).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

// Used by the wasm bridge (`host`) and by the tests; the native placeholder binary doesn't.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod stamp;

#[cfg(target_arch = "wasm32")]
fn main() {
    use eframe::wasm_bindgen::JsCast;
    use pdfcraft_ui_egui::PdfCraftApp;

    let options = eframe::WebOptions { renderer: eframe::Renderer::Glow, ..Default::default() };
    wasm_bindgen_futures::spawn_local(async move {
        let Some(canvas) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("pdfcraft"))
            .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
        else {
            return;
        };
        let started = eframe::WebRunner::new()
            .start(
                canvas,
                options,
                Box::new(|cc| {
                    let mut app = PdfCraftApp::new();
                    // "printcraft": settings saved under the app's former name, PrintCraft.
                    if let Some(json) = cc.storage.and_then(|s| s.get_string("pdfcraft").or_else(|| s.get_string("printcraft"))) {
                        app.restore(&json);
                    }
                    // `?file=<url>` opens a PDF from a URL (same-origin or CORS-enabled).
                    if let Some(url) = query_param("file") {
                        let inbox = app.startup_inbox.clone();
                        let failed = app.failed_inbox.clone();
                        let ctx = cc.egui_ctx.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            let name = url.rsplit('/').next().unwrap_or("document.pdf").split('?').next().unwrap_or("document.pdf").to_string();
                            match fetch_bytes(&url).await {
                                Ok(bytes) => {
                                    if let Ok(mut q) = inbox.lock() {
                                        q.push((name, bytes));
                                    }
                                }
                                Err(e) => {
                                    eframe::web_sys::console::error_1(&format!("PdfCraft: could not fetch {url}: {e}").into());
                                    // Shown in the app too, not only in the console (#173).
                                    if let Ok(mut q) = failed.lock() {
                                        q.push((name, e));
                                    }
                                }
                            }
                            ctx.request_repaint();
                        });
                    }
                    // eframe never raises a close request on a browser reload or tab close, so `guard_quit` never
                    // prompts; ask the browser to confirm instead while a document has unsaved work (#812).
                    warn_before_unload(app.unsaved_flag.clone());
                    // `?host=parent`: a same-origin page embedding PdfCraft in an iframe (such
                    // as the Nextcloud app) passes files in and takes saves back (`host` below).
                    if query_param("host").as_deref() == Some("parent")
                        && let Some(bridge) = host::Bridge::new()
                    {
                        // `?author=`: the host's name for the user signs their comments,
                        // unless they chose a name in PdfCraft (the web default is "Guest").
                        if let Some(author) = query_param("author").map(|a| a.trim().to_string()).filter(|a| !a.is_empty())
                            && app.comment_prefs.author == "Guest"
                        {
                            app.comment_prefs.author = author.chars().take(200).collect();
                        }
                        bridge.listen(app.startup_inbox.clone(), app.host_inbox.clone(), app.host_requests.clone(), cc.egui_ctx.clone());
                        app.host_save = Some(bridge.save_fn());
                        app.host_events = Some(bridge.events_fn());
                        bridge.post_ready();
                    } else if query_param("host").as_deref() == Some("parent") {
                        eframe::web_sys::console::warn_1(
                            &"PdfCraft: ?host=parent needs a framing page on the same origin; saves download instead".into(),
                        );
                    }
                    Ok(Box::new(app))
                }),
            )
            .await;
        if let Err(e) = started {
            eframe::web_sys::console::error_1(&e);
            if query_param("host").as_deref() == Some("parent")
                && let Some(bridge) = host::Bridge::new()
            {
                bridge.post_failed(&e.as_string().unwrap_or_else(|| format!("{e:?}")));
            }
        }
    });
}

#[cfg(target_arch = "wasm32")]
fn warn_before_unload(unsaved: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use eframe::wasm_bindgen::{JsCast, closure::Closure};
    let Some(window) = web_sys::window() else { return };
    let handler = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
        if unsaved.load(std::sync::atomic::Ordering::Relaxed) {
            event.prevent_default();
        }
    });
    if window.add_event_listener_with_callback("beforeunload", handler.as_ref().unchecked_ref()).is_ok() {
        handler.forget();
    }
}

#[cfg(target_arch = "wasm32")]
fn query_param(key: &str) -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    web_sys::UrlSearchParams::new_with_str(&search).ok()?.get(key)
}

#[cfg(target_arch = "wasm32")]
async fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    use eframe::wasm_bindgen::JsCast;
    let window = web_sys::window().ok_or("no window")?;
    let resp = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url)).await.map_err(|e| format!("{e:?}"))?;
    let resp: web_sys::Response = resp.dyn_into().map_err(|_| "not a response")?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let buf = wasm_bindgen_futures::JsFuture::from(resp.array_buffer().map_err(|e| format!("{e:?}"))?).await.map_err(|e| format!("{e:?}"))?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}

/// The `postMessage` bridge to a same-origin parent page (`?host=parent`). This comment is the
/// protocol reference. Messages are plain objects with a `type`. Only the parent window on the
/// app's own origin is heard or answered; with a parent on another origin there is no bridge:
/// the app runs as if unframed and saves download.
///
/// An `id` is an opaque string the host chooses for a document it hands in (one per document
/// it knows). Pages are 0-based page indices.
///
/// From the parent:
///
/// - `pdfcraft:open` `{ name, bytes, id?, page?, infoKey? }`: `bytes` is an `ArrayBuffer` or
///   `Uint8Array` holding a PDF. Without `id` it opens in a tab, like a `?file=` URL. With an
///   `id`, a tab already showing that id comes to the front (no second copy); otherwise the file
///   opens in a new tab tagged with it. Either way `page`, if given, is shown (clamped to the page
///   count). `infoKey`, a custom document-information key such as `"HostBookId"`, asks for
///   `infoKey = id` to be written into the information dictionary of every file saved from that
///   tab; an invalid key (a standard one, or one with spaces or delimiters) is ignored.
/// - `pdfcraft:request-save` `{ id }`: save that tab as Save would (never Save As), delivered as
///   `pdfcraft:save`, even when it has nothing unsaved. While the user is typing into it or a
///   dialog is open the save waits until that ends. An unknown `id` gets `pdfcraft:save-failed`.
///
/// To the parent:
///
/// - `pdfcraft:ready` `{ version, bridge }`: the app is listening; send files now. `bridge` is
///   the protocol level (1: everything listed here), absent in builds before it.
/// - `pdfcraft:save` `{ name, saveAs, bytes, id }` when a tab is saved (by the user, the
///   unsaved-changes prompt or `pdfcraft:request-save`); `bytes` is a transferred `ArrayBuffer`,
///   `id` the tab's id or `null`. The parent stores the file and reports its own errors. If the
///   `infoKey` entry can't be written the bytes come without it (and a console warning): a save
///   is never lost to it.
/// - `pdfcraft:save-failed` `{ id, error }`: a `pdfcraft:request-save` could not be done.
/// - `pdfcraft:dirty` `{ dirty, ids }` whenever either changes: whether any tab has unsaved work,
///   and the ids of the tagged tabs that have.
/// - `pdfcraft:active` `{ id, page, pageCount }` when the tab in front changes (`id` is `null`
///   for a tab without one; with no tab at all `page` is `null` and `pageCount` 0), and when a
///   document opened or brought forward with an `id` is ready.
/// - `pdfcraft:page` `{ id, page }` when the page of the tagged tab in front changes: once it
///   settles, and at most every 250 ms while the user scrolls.
/// - `pdfcraft:activity` `{ id }` when the user interacts (pointer, wheel, keys, touch) with the
///   tagged tab in front while the page is visible; at most once every 10 s per id.
/// - `pdfcraft:theme` `{ theme }`, `"light"` or `"dark"`: once after `pdfcraft:ready`, then
///   whenever the theme the interface is drawn with changes, so the host can match it.
/// - `pdfcraft:failed` `{ error }` when the app couldn't start, or `{ error, id }` when a tagged
///   document couldn't be opened (unreadable, or its password prompt was cancelled).
#[cfg(target_arch = "wasm32")]
mod host {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use eframe::wasm_bindgen::{JsCast, JsValue, closure::Closure};
    use pdfcraft_ui_egui::{HostEvent, HostOpen, HostRequest};

    /// The protocol level sent with `pdfcraft:ready`.
    const BRIDGE_LEVEL: u32 = 1;
    /// `pdfcraft:page` is sent at most this often (ms).
    const PAGE_INTERVAL_MS: i32 = 250;
    /// `pdfcraft:activity` is sent at most this often per id (ms).
    const ACTIVITY_INTERVAL_MS: f64 = 10_000.0;
    /// Longer ids are refused: an id is a short handle, not a payload.
    const MAX_ID_BYTES: usize = 1024;

    #[derive(Clone)]
    pub struct Bridge {
        window: web_sys::Window,
        parent: web_sys::Window,
        origin: String,
        state: Rc<RefCell<State>>,
    }

    #[derive(Default)]
    struct State {
        /// The `infoKey` each id asked for.
        info_keys: HashMap<String, String>,
        /// When `pdfcraft:activity` was last sent for each id (ms since the epoch).
        activity: HashMap<String, f64>,
        /// The page waiting for the next `pdfcraft:page`, and the last one sent.
        page_pending: Option<(String, usize)>,
        page_sent: Option<(String, usize)>,
        page_timer: bool,
    }

    impl Bridge {
        /// `None` when the page isn't framed (there is no parent to talk to), or when the parent
        /// is on another origin: messages only go to this origin, so the browser would drop
        /// every save without an error while the document showed as saved (#2167).
        pub fn new() -> Option<Self> {
            let window = web_sys::window()?;
            let parent = window.parent().ok().flatten()?;
            if js_sys::Object::is(&parent, &window) {
                return None;
            }
            let origin = window.location().origin().ok()?;
            // Reading another origin's location throws, which is `None` too.
            if parent.location().origin().ok()? != origin {
                return None;
            }
            Some(Self { window, parent, origin, state: Rc::default() })
        }

        fn post(&self, message: &js_sys::Object, transfer: Option<&JsValue>) -> Result<(), String> {
            let sent = match transfer {
                Some(t) => self.parent.post_message_with_transfer(message, &self.origin, &js_sys::Array::of1(t)),
                None => self.parent.post_message(message, &self.origin),
            };
            sent.map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))
        }

        fn send(&self, fields: &[(&str, JsValue)]) {
            if let Some(m) = message(fields) {
                let _ = self.post(&m, None);
            }
        }

        pub fn post_ready(&self) {
            self.send(&[("type", "pdfcraft:ready".into()), ("version", env!("CARGO_PKG_VERSION").into()), ("bridge", BRIDGE_LEVEL.into())]);
        }

        /// The app couldn't start (no WebGL, for example): let the parent say so.
        pub fn post_failed(&self, error: &str) {
            self.send(&[("type", "pdfcraft:failed".into()), ("error", error.into())]);
        }

        /// Hear `pdfcraft:open` and `pdfcraft:request-save` from the parent. Files without an id
        /// are queued like a `?file=` download (`startup`); tagged ones go to the host inbox.
        pub fn listen(
            &self,
            startup: pdfcraft_ui_egui::Inbox,
            inbox: pdfcraft_ui_egui::HostInbox,
            requests: pdfcraft_ui_egui::HostRequests,
            ctx: egui::Context,
        ) {
            let bridge = self.clone();
            let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
                if event.origin() != bridge.origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &bridge.parent)) {
                    return;
                }
                let data = event.data();
                let id = string(&data, "id").filter(|id| !id.is_empty());
                if id.as_ref().is_some_and(|id| id.len() > MAX_ID_BYTES) {
                    eframe::web_sys::console::warn_1(&"PdfCraft: ignored a host message whose id is too long".into());
                    return;
                }
                match string(&data, "type").as_deref() {
                    Some("pdfcraft:open") => {
                        let name = string(&data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "document.pdf".to_string());
                        let Some(bytes) = bytes(&data) else {
                            // A tagged open is always answered, so the host stops waiting.
                            if let Some(tag) = id {
                                let error = "bytes must be an ArrayBuffer or a Uint8Array";
                                bridge.send(&[("type", "pdfcraft:failed".into()), ("error", error.into()), ("id", tag.as_str().into())]);
                            }
                            return;
                        };
                        match id {
                            None => {
                                if let Ok(mut q) = startup.lock() {
                                    q.push((name, bytes));
                                }
                            }
                            Some(tag) => {
                                if let Some(key) = string(&data, "infoKey") {
                                    if pdfcraft_organize::check_custom_key(&key).is_ok() {
                                        bridge.state.borrow_mut().info_keys.insert(tag.clone(), key);
                                    } else {
                                        eframe::web_sys::console::warn_1(&format!("PdfCraft: ignored the invalid infoKey {key:?}").into());
                                    }
                                }
                                if let Ok(mut q) = inbox.lock() {
                                    q.push(HostOpen { name, bytes, tag: Some(tag), page: page(&data, "page") });
                                }
                            }
                        }
                    }
                    Some("pdfcraft:request-save") => {
                        let Some(tag) = id else { return };
                        if let Ok(mut q) = requests.lock() {
                            q.push(HostRequest::Save { tag });
                        }
                    }
                    _ => return,
                }
                ctx.request_repaint();
            });
            if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
                // The listener lives as long as the page.
                on_message.forget();
            }
        }

        pub fn save_fn(&self) -> pdfcraft_ui_egui::HostSaveFn {
            let bridge = self.clone();
            Box::new(move |save: &pdfcraft_ui_egui::HostSave<'_>| {
                let key = save.tag.and_then(|tag| bridge.state.borrow().info_keys.get(tag).cloned());
                let stamped = match (save.tag, key) {
                    (Some(tag), Some(key)) => crate::stamp::stamp_info(save.bytes, &key, tag, save.password).unwrap_or_else(|e| {
                        eframe::web_sys::console::warn_1(&format!("PdfCraft: saved without {key}: {e}").into());
                        std::borrow::Cow::Borrowed(save.bytes)
                    }),
                    _ => std::borrow::Cow::Borrowed(save.bytes),
                };
                let buffer = js_sys::Uint8Array::from(&*stamped).buffer();
                let m = message(&[
                    ("type", "pdfcraft:save".into()),
                    ("name", save.name.into()),
                    ("saveAs", save.save_as.into()),
                    ("bytes", buffer.clone().into()),
                    ("id", optional(save.tag)),
                ])
                .ok_or_else(|| "couldn't build the message".to_string())?;
                bridge.post(&m, Some(&buffer))
            })
        }

        pub fn events_fn(&self) -> pdfcraft_ui_egui::HostEventFn {
            let bridge = self.clone();
            Box::new(move |event: HostEvent| bridge.on_event(event))
        }

        fn on_event(&self, event: HostEvent) {
            match event {
                HostEvent::Active { tag, page, page_count } => {
                    {
                        let mut state = self.state.borrow_mut();
                        state.page_pending = None;
                        state.page_sent = tag.clone().zip(page);
                    }
                    self.send(&[
                        ("type", "pdfcraft:active".into()),
                        ("id", optional(tag.as_deref())),
                        ("page", page.map_or(JsValue::NULL, number)),
                        ("pageCount", number(page_count)),
                    ]);
                }
                HostEvent::Page { tag, page } => {
                    let schedule = {
                        let mut state = self.state.borrow_mut();
                        state.page_pending = Some((tag, page));
                        !std::mem::replace(&mut state.page_timer, true)
                    };
                    if schedule && !self.schedule_page() {
                        // No timer: send it now rather than never.
                        self.flush_page();
                    }
                }
                HostEvent::Activity { tag } => {
                    let visible = self.window.document().is_none_or(|d| d.visibility_state() == web_sys::VisibilityState::Visible);
                    if !visible {
                        return;
                    }
                    let now = js_sys::Date::now();
                    let due = {
                        let mut state = self.state.borrow_mut();
                        let due = state.activity.get(&tag).is_none_or(|last| now - last >= ACTIVITY_INTERVAL_MS || now < *last);
                        if due {
                            // Ids of documents long closed only cost a few bytes; still, keep it bounded.
                            if state.activity.len() >= 1024 {
                                state.activity.clear();
                            }
                            state.activity.insert(tag.clone(), now);
                        }
                        due
                    };
                    if due {
                        self.send(&[("type", "pdfcraft:activity".into()), ("id", tag.as_str().into())]);
                    }
                }
                HostEvent::Dirty { any, tags } => {
                    let ids: js_sys::Array = tags.iter().map(|t| JsValue::from_str(t)).collect();
                    self.send(&[("type", "pdfcraft:dirty".into()), ("dirty", any.into()), ("ids", ids.into())]);
                }
                HostEvent::OpenFailed { tag, error } => {
                    self.state.borrow_mut().info_keys.remove(&tag);
                    self.send(&[("type", "pdfcraft:failed".into()), ("error", error.as_str().into()), ("id", tag.as_str().into())]);
                }
                HostEvent::SaveFailed { tag, error } => {
                    self.send(&[("type", "pdfcraft:save-failed".into()), ("id", tag.as_str().into()), ("error", error.as_str().into())]);
                }
                HostEvent::Theme { dark } => {
                    self.send(&[("type", "pdfcraft:theme".into()), ("theme", if dark { "dark" } else { "light" }.into())]);
                }
                // Events of a later protocol level.
                _ => {}
            }
        }

        /// Send the pending `pdfcraft:page` in `PAGE_INTERVAL_MS`. `false` if no timer could be set.
        fn schedule_page(&self) -> bool {
            let bridge = self.clone();
            let fire = Closure::once_into_js(move || bridge.flush_page());
            self.window.set_timeout_with_callback_and_timeout_and_arguments_0(fire.unchecked_ref(), PAGE_INTERVAL_MS).is_ok()
        }

        fn flush_page(&self) {
            let send = {
                let mut state = self.state.borrow_mut();
                state.page_timer = false;
                match state.page_pending.take() {
                    Some(page) if state.page_sent.as_ref() != Some(&page) => {
                        state.page_sent = Some(page.clone());
                        Some(page)
                    }
                    _ => None,
                }
            };
            if let Some((tag, page)) = send {
                self.send(&[("type", "pdfcraft:page".into()), ("id", tag.as_str().into()), ("page", number(page))]);
            }
        }
    }

    fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
        let m = js_sys::Object::new();
        for (key, value) in fields {
            js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
        }
        Some(m)
    }

    fn string(data: &JsValue, key: &str) -> Option<String> {
        js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_string()
    }

    /// A page index the parent sent: a finite, non-negative number (fractions round down).
    fn page(data: &JsValue, key: &str) -> Option<usize> {
        let n = js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_f64()?;
        // `as` saturates at usize::MAX; the app clamps to the page count.
        (n.is_finite() && n >= 0.0).then(|| n.floor() as usize)
    }

    /// The file in `bytes`: an `ArrayBuffer` or a `Uint8Array`.
    fn bytes(data: &JsValue) -> Option<Vec<u8>> {
        let bytes = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
        if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
            Some(array.to_vec())
        } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
            Some(js_sys::Uint8Array::new(&bytes).to_vec())
        } else {
            None
        }
    }

    fn optional(s: Option<&str>) -> JsValue {
        s.map_or(JsValue::NULL, JsValue::from_str)
    }

    fn number(n: usize) -> JsValue {
        // Page numbers and counts are far below 2^53, where f64 stops being exact.
        JsValue::from_f64(n as f64)
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("pdfcraft-web targets wasm32: run `trunk serve` or `trunk build --release` in apps/pdfcraft-web");
}
