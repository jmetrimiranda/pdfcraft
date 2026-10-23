//! The host hooks (`host_inbox`, `host_requests`, `host_events`, `host_save`,
//! `host_menu_items`) that the web build's `?host=parent` bridge drives: tagged opens, saves and
//! gotos asked for by the host, its items in the text-selection menus, and the events it hears.

use std::sync::{Arc, Mutex};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use pdfcraft_testkit::Pdf;
use pdfcraft_ui_egui::{HostEvent, HostMenuItem, HostOpen, HostRequest, HostSave, MAX_HOST_FIND_CHARS, MAX_HOST_TEXT_CHARS, PdfCraftApp};

/// An `n`-page document; page `i` shows "Page i+1".
fn fixture(n: usize) -> Vec<u8> {
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    let mut pdf = Pdf::new()
        .obj("<< /Type /Catalog /Pages 2 0 R >>")
        .obj(format!("<< /Type /Pages /Kids [{}] /Count {n} /MediaBox [0 0 200 300] >>", kids.join(" ")))
        .obj("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>");
    for i in 0..n {
        pdf = pdf
            .obj(format!("<< /Type /Page /Parent 2 0 R /Contents {} 0 R /Resources << /Font << /F1 3 0 R >> >> >>", 5 + 2 * i))
            .stream("", format!("BT /F1 24 Tf 20 150 Td (Page {}) Tj ET", i + 1));
    }
    pdf.build()
}

fn encrypted(bytes: Vec<u8>, password: &str) -> Vec<u8> {
    let mut doc = pdfcraft_cos::Document::open(Arc::new(bytes)).unwrap();
    doc.set_encryption(&pdfcraft_cos::NewEncryption {
        algorithm: pdfcraft_cos::Algorithm::Aes256,
        user_password: password,
        owner_password: "owner",
        permissions: -1,
        encrypt_metadata: true,
        seed: [3; 32],
    })
    .unwrap();
    pdfcraft_cos::write_full(&doc, &Default::default()).unwrap()
}

#[derive(Debug)]
struct Saved {
    save_as: bool,
    tag: Option<String>,
    bytes: Vec<u8>,
}

struct Host {
    h: Harness<'static, PdfCraftApp>,
    events: Arc<Mutex<Vec<HostEvent>>>,
    saves: Arc<Mutex<Vec<Saved>>>,
}

impl Host {
    fn new() -> Self {
        let events = Arc::new(Mutex::new(Vec::new()));
        let saves = Arc::new(Mutex::new(Vec::new()));
        let (event_sink, save_sink) = (events.clone(), saves.clone());
        let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(move |_cc| {
            let mut app = PdfCraftApp::new();
            app.set_option("language", "en").unwrap();
            app.host_events = Some(Box::new(move |e| event_sink.lock().unwrap().push(e)));
            app.host_save = Some(Box::new(move |s: &HostSave<'_>| {
                save_sink.lock().unwrap().push(Saved { save_as: s.save_as, tag: s.tag.map(str::to_owned), bytes: s.bytes.to_vec() });
                Ok(())
            }));
            app
        });
        h.run_steps(2);
        Self { h, events, saves }
    }

    fn open(&mut self, tag: Option<&str>, page: Option<usize>, bytes: Vec<u8>) {
        let open = HostOpen { name: "book.pdf".into(), bytes, tag: tag.map(str::to_owned), page };
        self.h.state().host_inbox.lock().unwrap().push(open);
        self.h.run_steps(3);
    }

    fn request(&mut self, request: HostRequest) {
        self.h.state().host_requests.lock().unwrap().push(request);
        self.h.run_steps(3);
    }

    /// The events since the last call.
    fn take(&self) -> Vec<HostEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
}

fn active(tag: &str, page: usize, page_count: usize) -> HostEvent {
    HostEvent::Active { tag: Some(tag.into()), page: Some(page), page_count }
}

fn is_tagged(e: &HostEvent) -> bool {
    match e {
        HostEvent::Active { tag, .. } => tag.is_some(),
        HostEvent::Dirty { tags, .. } => !tags.is_empty(),
        HostEvent::Theme { .. } => false,
        _ => true,
    }
}

#[test]
fn a_tagged_open_reports_its_tag_page_and_page_count() {
    let mut host = Host::new();
    let first = host.take();
    assert!(first.contains(&HostEvent::Active { tag: None, page: None, page_count: 0 }), "nothing open yet: {first:?}");
    assert!(first.contains(&HostEvent::Theme { dark: false }));
    assert!(first.contains(&HostEvent::Dirty { any: false, tags: vec![] }));

    host.open(Some("book-a"), Some(2), fixture(5));
    assert_eq!(host.h.state().views.len(), 1);
    assert_eq!(host.h.state().views[0].current, 2);
    let events = host.take();
    assert_eq!(events.iter().filter(|e| matches!(e, HostEvent::Active { .. })).collect::<Vec<_>>(), [&active("book-a", 2, 5)]);
    assert!(!events.iter().any(|e| matches!(e, HostEvent::Page { .. })), "the opening page is in Active: {events:?}");

    // A page past the end is the last page.
    host.open(Some("book-b"), Some(99), fixture(3));
    assert!(host.take().contains(&active("book-b", 2, 3)));
}

#[test]
fn the_same_tag_brings_its_tab_forward_instead_of_opening_it_again() {
    let mut host = Host::new();
    host.open(Some("book-a"), None, fixture(5));
    host.open(None, None, fixture(2));
    assert_eq!(host.h.state().active, Some(1));
    assert!(host.take().contains(&HostEvent::Active { tag: None, page: Some(0), page_count: 2 }));

    host.open(Some("book-a"), Some(4), fixture(5));
    assert_eq!(host.h.state().views.len(), 2, "no second copy");
    assert_eq!(host.h.state().active, Some(0));
    assert_eq!(host.h.state().views[0].current, 4);
    assert!(host.take().contains(&active("book-a", 4, 5)));

    // Asked for again while in front: reported again, so the host always hears back.
    host.open(Some("book-a"), None, fixture(5));
    assert_eq!(host.take(), [active("book-a", 4, 5)]);

    // The tag stays with its document when tabs move.
    host.h.state_mut().move_tab(0, 1);
    host.open(Some("book-a"), Some(1), fixture(5));
    assert_eq!(host.h.state().active, Some(1));
    assert_eq!(host.h.state().views[1].current, 1);
}

#[test]
fn a_page_change_is_reported_once() {
    let mut host = Host::new();
    host.open(Some("book-a"), None, fixture(5));
    host.take();
    host.h.state_mut().views[0].go_to_page(3);
    host.h.run_steps(5);
    assert_eq!(host.take(), [HostEvent::Page { tag: "book-a".into(), page: 3 }]);
    host.h.run_steps(3);
    assert!(host.take().is_empty());
}

#[test]
fn user_input_on_a_tagged_document_is_activity() {
    let mut host = Host::new();
    host.open(Some("book-a"), None, fixture(2));
    host.take();
    host.h.run_steps(2);
    assert!(!host.take().iter().any(|e| matches!(e, HostEvent::Activity { .. })), "no input, no activity");
    host.h.hover_at(egui::pos2(700.0, 450.0));
    host.h.run_steps(1);
    assert!(host.take().contains(&HostEvent::Activity { tag: "book-a".into() }));
}

#[test]
fn a_save_request_saves_the_tagged_document_even_when_clean() {
    let mut host = Host::new();
    host.open(Some("book-a"), None, fixture(2));
    host.open(Some("book-b"), None, fixture(3));
    host.request(HostRequest::Save { tag: "book-a".into() });
    let saves = std::mem::take(&mut *host.saves.lock().unwrap());
    assert_eq!(saves.len(), 1);
    assert_eq!(saves[0].tag.as_deref(), Some("book-a"));
    assert!(!saves[0].save_as);
    assert!(saves[0].bytes.starts_with(b"%PDF-"));
    assert_eq!(host.h.state().active, Some(1), "saving doesn't change the tab in front");

    // An unknown tag is answered, so the host stops waiting.
    host.take();
    host.request(HostRequest::Save { tag: "nope".into() });
    assert!(host.take().iter().any(|e| matches!(e, HostEvent::SaveFailed { tag, .. } if tag == "nope")));
    assert!(host.saves.lock().unwrap().is_empty());
}

#[test]
fn a_save_request_waits_while_the_user_types() {
    let mut host = Host::new();
    host.open(Some("book-a"), None, fixture(2));
    host.h.state_mut().views[0].comments.add_box = "half a comm".into();
    host.request(HostRequest::Save { tag: "book-a".into() });
    host.h.run_steps(3);
    assert!(host.saves.lock().unwrap().is_empty(), "the typing isn't cut short");
    host.h.state_mut().views[0].comments.add_box.clear();
    host.h.run_steps(3);
    let saves = host.saves.lock().unwrap();
    assert_eq!(saves.len(), 1, "saved once the typing ended");
    assert_eq!(saves[0].tag.as_deref(), Some("book-a"));
}

#[test]
fn an_empty_comment_editor_does_not_hold_a_save() {
    let mut host = Host::new();
    host.open(Some("book-a"), None, fixture(2));
    // A new highlight opens its note editor empty.
    host.h.state_mut().views[0].comments.editing = Some((0, 0, String::new()));
    host.request(HostRequest::Save { tag: "book-a".into() });
    assert_eq!(host.saves.lock().unwrap().len(), 1, "nothing typed yet, so nothing to cut short");

    host.h.state_mut().views[0].comments.editing = Some((0, 0, "a no".into()));
    host.request(HostRequest::Save { tag: "book-a".into() });
    assert_eq!(host.saves.lock().unwrap().len(), 1, "a note being typed holds the save");
    host.h.state_mut().views[0].comments.editing = None;
    host.h.run_steps(3);
    assert_eq!(host.saves.lock().unwrap().len(), 2, "saved once the note was left");
}

#[test]
fn a_save_by_the_user_carries_the_tag() {
    let mut host = Host::new();
    host.open(None, None, fixture(2));
    host.open(Some("book-a"), None, fixture(2));
    host.h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::S);
    host.h.run_steps(3);
    host.h.state_mut().active = Some(0);
    host.h.run_steps(1);
    host.h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::S);
    host.h.run_steps(3);
    let tags: Vec<Option<String>> = host.saves.lock().unwrap().iter().map(|s| s.tag.clone()).collect();
    assert_eq!(tags, [Some("book-a".to_string()), None]);
}

#[test]
fn dirty_lists_the_tags_with_unsaved_work() {
    let mut host = Host::new();
    host.open(Some("book-a"), None, fixture(2));
    host.open(Some("book-b"), None, fixture(2));
    host.take();
    assert!(host.h.state_mut().execute("page.rotate"));
    host.h.run_steps(3);
    assert!(host.take().contains(&HostEvent::Dirty { any: true, tags: vec!["book-b".into()] }));
    host.request(HostRequest::Save { tag: "book-b".into() });
    assert!(host.take().contains(&HostEvent::Dirty { any: false, tags: vec![] }), "a save clears it");
}

#[test]
fn untagged_documents_never_raise_tagged_events() {
    let mut host = Host::new();
    host.open(None, None, fixture(4));
    host.h.state_mut().views[0].go_to_page(2);
    host.h.hover_at(egui::pos2(700.0, 450.0));
    host.h.run_steps(3);
    assert!(host.h.state_mut().execute("page.rotate"));
    host.h.run_steps(3);
    let events = host.take();
    assert!(events.contains(&HostEvent::Dirty { any: true, tags: vec![] }));
    assert!(!events.iter().any(is_tagged), "{events:?}");
}

#[test]
fn a_tagged_open_that_fails_is_reported() {
    let mut host = Host::new();
    host.open(Some("broken"), None, b"this is not a PDF".to_vec());
    assert!(host.h.state().views.is_empty());
    assert!(host.take().iter().any(|e| matches!(e, HostEvent::OpenFailed { tag, .. } if tag == "broken")));
}

#[test]
fn a_cancelled_password_prompt_fails_the_open_and_the_queue_goes_on() {
    let mut host = Host::new();
    let open = |tag: &str, bytes| HostOpen { name: format!("{tag}.pdf"), bytes, tag: Some(tag.into()), page: None };
    host.h.state().host_inbox.lock().unwrap().extend([open("secret", encrypted(fixture(2), "pw")), open("next", fixture(3))]);
    host.h.run_steps(3);
    assert!(host.h.state().password_prompt.is_some());
    assert!(host.h.state().views.is_empty(), "the next file waits for the prompt");
    host.h.state_mut().password_prompt = None;
    host.h.run_steps(3);
    let events = host.take();
    assert!(events.iter().any(|e| matches!(e, HostEvent::OpenFailed { tag, .. } if tag == "secret")), "{events:?}");
    assert!(events.contains(&active("next", 0, 3)), "{events:?}");
    assert_eq!(host.h.state().views.len(), 1);
}

#[test]
fn a_tagged_open_behind_a_password_is_tagged_once_unlocked() {
    let mut host = Host::new();
    host.open(Some("secret"), Some(1), encrypted(fixture(2), "pw"));
    let prompt = host.h.state_mut().password_prompt.as_mut().expect("asks for the password");
    prompt.input = "pw".into();
    host.h.key_press(egui::Key::Enter);
    crate::support::wait_until(&mut host.h, "the unlocked document", |app| !app.views.is_empty());
    host.h.run_steps(3);
    assert!(host.take().contains(&active("secret", 1, 2)));
    host.request(HostRequest::Save { tag: "secret".into() });
    assert_eq!(host.saves.lock().unwrap().first().and_then(|s| s.tag.as_deref()), Some("secret"));
}

/// Pages of Helvetica text, 300×400 each: one list of lines per page, 20 points apart from the
/// top.
fn text_book(pages: &[&[&str]]) -> Vec<u8> {
    text_book_sized(pages, 14.0, 20.0)
}

fn text_book_sized(pages: &[&[&str]], size: f32, leading: f32) -> Vec<u8> {
    let n = pages.len();
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    let mut pdf = Pdf::new()
        .obj("<< /Type /Catalog /Pages 2 0 R >>")
        .obj(format!("<< /Type /Pages /Kids [{}] /Count {n} /MediaBox [0 0 300 400] >>", kids.join(" ")))
        .obj("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>");
    for (i, lines) in pages.iter().enumerate() {
        let body: Vec<String> = lines.iter().map(|l| format!("({l}) Tj 0 -{leading} Td")).collect();
        pdf = pdf
            .obj(format!("<< /Type /Page /Parent 2 0 R /Contents {} 0 R /Resources << /Font << /F1 3 0 R >> >> >>", 5 + 2 * i))
            .stream("", format!("BT /F1 {size} Tf 20 370 Td {} ET", body.join(" ")));
    }
    pdf.build()
}

impl Host {
    /// Wait until the text of `page` of tab `index` is loaded and on screen.
    fn wait_text(&mut self, index: usize, page: usize) {
        crate::support::wait_until(&mut self.h, "the page's text layer", move |app| {
            app.views.get(index).is_some_and(|v| v.glyph_screen_pos(page, 0).is_some())
        });
    }

    /// Wait for an event that `want` matches; returns it (and leaves the others).
    fn wait_event(&mut self, what: &str, want: impl Fn(&HostEvent) -> bool + 'static) -> HostEvent {
        let events = self.events.clone();
        let want = std::sync::Arc::new(want);
        let seen = want.clone();
        crate::support::wait_until(&mut self.h, what, move |_| events.lock().unwrap().iter().any(|e| seen(e)));
        self.events.lock().unwrap().iter().find(|e| want(e)).cloned().unwrap()
    }

    fn set_items(&mut self, items: &[(&str, &str)]) {
        *self.h.state().host_menu_items.lock().unwrap() =
            items.iter().map(|(key, label)| HostMenuItem { key: (*key).into(), label: (*label).into() }).collect();
        self.h.run_steps(2);
    }

    fn goto(&mut self, tag: &str, page: usize, find: Option<&str>) {
        let request = HostRequest::Goto { tag: tag.into(), page, find: find.map(str::to_owned) };
        self.h.state().host_requests.lock().unwrap().push(request);
    }
}

fn selections(events: &[HostEvent]) -> Vec<&HostEvent> {
    events.iter().filter(|e| matches!(e, HostEvent::Selection { .. })).collect()
}

fn is_found(e: &HostEvent) -> bool {
    matches!(e, HostEvent::Found { .. })
}

/// The page of the find bar's current match.
fn find_page(app: &PdfCraftApp, index: usize) -> Option<usize> {
    let f = app.views.get(index)?.find.as_ref()?;
    f.matches.get(f.current?).map(|m| m.0)
}

const TWO_LINES: &[&str] = &["The quick brown fox", "jumps over the dog"];

#[test]
fn a_selection_in_a_tagged_document_is_reported_then_cleared() {
    let mut host = Host::new();
    host.open(Some("book-a"), None, text_book(&[TWO_LINES]));
    host.wait_text(0, 0);
    host.take();
    assert!(host.h.state_mut().views[0].select_all());
    host.h.run_steps(3);
    let events = host.take();
    let sel = selections(&events);
    assert_eq!(sel.len(), 1, "one report per change: {events:?}");
    let HostEvent::Selection { tag, page, text, rects } = sel[0] else { unreachable!() };
    assert_eq!((tag.as_str(), *page, text.as_str()), ("book-a", 0, "The quick brown fox\njumps over the dog"));
    assert_eq!(rects.len(), 2, "one rectangle per line: {rects:?}");
    for r in rects {
        assert!(r[0] < r[2] && r[1] < r[3], "{r:?}");
        assert!(r[0] >= 0.0 && r[1] >= 0.0 && r[2] <= 300.0 && r[3] <= 400.0, "inside the page, view space: {r:?}");
    }
    assert!(rects[0][1] < rects[1][1], "y grows downwards: {rects:?}");

    host.h.run_steps(3);
    assert!(selections(&host.take()).is_empty(), "no change, no report");

    host.h.key_press(egui::Key::Escape);
    host.h.run_steps(2);
    assert_eq!(selections(&host.take()), [&HostEvent::Selection { tag: "book-a".into(), page: 0, text: String::new(), rects: vec![] }]);
}

#[test]
fn untagged_selections_are_not_reported_and_leaving_a_tagged_one_clears_it() {
    let mut host = Host::new();
    host.open(None, None, text_book(&[TWO_LINES]));
    host.wait_text(0, 0);
    assert!(host.h.state_mut().views[0].select_all());
    host.h.run_steps(3);
    assert!(selections(&host.take()).is_empty(), "an untagged document's selection is its own");

    host.open(Some("book-a"), None, text_book(&[TWO_LINES]));
    host.wait_text(1, 0);
    assert!(host.h.state_mut().views[1].select_all());
    host.h.run_steps(3);
    assert_eq!(selections(&host.take()).len(), 1);
    // The untagged tab comes to the front: the tagged selection is no longer the user's.
    host.h.state_mut().active = Some(0);
    host.h.run_steps(3);
    assert_eq!(selections(&host.take()), [&HostEvent::Selection { tag: "book-a".into(), page: 0, text: String::new(), rects: vec![] }]);
}

#[test]
fn host_items_show_in_the_selection_bar_only_with_selected_text() {
    let mut host = Host::new();
    host.open(Some("book-a"), None, text_book(&[TWO_LINES]));
    host.wait_text(0, 0);
    host.set_items(&[("card", "Criar card"), ("define", "Definir palavra")]);
    assert!(host.h.query_by_label("Criar card").is_none(), "nothing selected, no items");

    assert!(host.h.state_mut().views[0].select_all());
    host.h.run_steps(3);
    host.h.get_by_label("Add comment");
    host.h.get_by_label("Definir palavra");
    let shown = host.take().into_iter().filter(|e| matches!(e, HostEvent::MenuShown { .. })).collect::<Vec<_>>();
    // Once, or again if the bar settled elsewhere after its first, measuring frame.
    assert!((1..=2).contains(&shown.len()), "{shown:?}");
    let Some(HostEvent::MenuShown { tag, items }) = shown.last() else { unreachable!() };
    assert_eq!(tag.as_deref(), Some("book-a"));
    assert_eq!(items.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["card", "define"]);
    for (_, r) in items {
        assert!(r.iter().all(|v| v.is_finite()) && r[0] < r[2] && r[1] < r[3], "{r:?}");
        assert!(r[2] <= 1400.0 && r[3] <= 900.0, "on screen: {r:?}");
    }

    host.h.get_by_label("Criar card").click();
    host.h.run_steps(3);
    let events = host.take();
    let menu = events.iter().find(|e| matches!(e, HostEvent::Menu { .. })).unwrap_or_else(|| panic!("{events:?}"));
    let HostEvent::Menu { key, tag, page, text, rects, anchor } = menu else { unreachable!() };
    assert_eq!((key.as_str(), tag.as_deref(), *page), ("card", Some("book-a"), 0));
    assert_eq!(text, "The quick brown fox\njumps over the dog");
    assert_eq!(rects.len(), 2);
    assert!(anchor[0] < anchor[2] && anchor[1] < anchor[3] && anchor[2] <= 1400.0, "{anchor:?}");
    assert!(events.contains(&HostEvent::MenuShown { tag: Some("book-a".into()), items: vec![] }), "the bar closed: {events:?}");
    assert!(host.h.query_by_label("Criar card").is_none(), "the bar closes, as after Copy");
    assert!(host.h.state().views[0].selected_text().is_some(), "the words stay selected");
    assert!(selections(&events).is_empty(), "the selection didn't change");

    // [] removes them.
    host.set_items(&[]);
    host.h.state_mut().views[0].select_text(0, 0, 2);
    host.h.run_steps(3);
    host.h.get_by_label("Add comment");
    assert!(host.h.query_by_label("Criar card").is_none());
    assert!(!host.take().iter().any(|e| matches!(e, HostEvent::MenuShown { items, .. } if !items.is_empty())));
}

#[test]
fn host_items_show_in_the_context_menu_and_a_click_keeps_the_selection() {
    let mut host = Host::new();
    host.open(Some("book-a"), None, text_book(&[TWO_LINES]));
    host.wait_text(0, 0);
    host.set_items(&[("card", "Criar card"), ("define", "Definir palavra")]);
    let at = host.h.state().views[0].glyph_screen_pos(0, 1).unwrap();
    let right_click = |host: &mut Host| {
        for pressed in [true, false] {
            host.h.event(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed, modifiers: Default::default() });
        }
        host.h.run_steps(3);
    };
    // Nothing selected: the menu has no host items.
    right_click(&mut host);
    host.h.get_by_label_contains("Copy");
    assert!(host.h.query_by_label("Criar card").is_none());
    host.h.key_press(egui::Key::Escape);
    host.h.run_steps(2);

    host.h.state_mut().views[0].select_text(0, 0, 8);
    host.h.run_steps(2);
    // The bar's items are the bar's; close it the way Copy does, to see the menu's alone.
    host.h.get_by_label("Copy").click();
    host.h.run_steps(2);
    host.take();
    right_click(&mut host);
    host.h.get_by_label("Criar card");
    let shown = host.take();
    assert!(shown.iter().any(|e| matches!(e, HostEvent::MenuShown { items, .. } if items.len() == 2)), "{shown:?}");
    host.h.get_by_label("Definir palavra").click();
    host.h.run_steps(3);
    let events = host.take();
    let expected = host.h.state().views[0].selected_text().expect("the words stay selected");
    assert!(
        events.iter().any(|e| matches!(e, HostEvent::Menu { key, tag, page: 0, text, .. } if key == "define" && tag.as_deref() == Some("book-a") && *text == expected)),
        "{events:?}"
    );
    assert!(host.h.query_by_label("Criar card").is_none(), "the menu closed");
}

#[test]
fn goto_lands_on_the_page_and_finds_the_passage_there() {
    let mut host = Host::new();
    let filler: &[&str] = &["Nothing to see here"];
    host.open(Some("book-a"), None, text_book(&[filler, filler, filler, &["Some words before", "the target passage here"], filler, filler]));
    host.take();
    host.goto("book-a", 3, Some("  target \n passage "));
    let found = host.wait_event("Found", is_found);
    assert_eq!(found, HostEvent::Found { tag: "book-a".into(), page: 3, query: "target passage".into(), found: true });
    assert_eq!(host.h.state().views[0].current, 3);
    assert_eq!(host.h.state().views[0].find.as_ref().map(|f| f.query.as_str()), Some("target passage"), "in PdfCraft's find bar");
    assert_eq!(find_page(host.h.state(), 0), Some(3), "its current match is on the page");
    let events = host.take();
    assert!(events.contains(&active("book-a", 3, 6)), "{events:?}");
    assert_eq!(events.iter().filter(|e| is_found(e)).count(), 1, "answered once: {events:?}");
    host.h.run_steps(5);
    assert!(!host.take().iter().any(is_found));
    assert_eq!(find_page(host.h.state(), 0), Some(3), "texts arriving later don't move the match");

    // Without a passage: just the page, and no Found.
    host.goto("book-a", 1, None);
    host.h.run_steps(3);
    assert_eq!(host.h.state().views[0].current, 1);
    assert!(!host.take().iter().any(is_found));
}

#[test]
fn goto_takes_the_nearest_match_within_two_pages() {
    let mut host = Host::new();
    let filler: &[&str] = &["Nothing to see here"];
    let marked: &[&str] = &["A unique marker"];
    host.open(Some("book-a"), None, text_book(&[marked, filler, filler, filler, marked, filler, filler]));
    host.goto("book-a", 3, Some("unique marker"));
    let found = host.wait_event("Found", is_found);
    assert_eq!(found, HostEvent::Found { tag: "book-a".into(), page: 4, query: "unique marker".into(), found: true });
    assert_eq!(find_page(host.h.state(), 0), Some(4));
}

#[test]
fn goto_falls_back_to_the_first_words_of_a_broken_passage() {
    let mut host = Host::new();
    let filler: &[&str] = &["Nothing to see here"];
    // Hyphenated across lines: the passage as a reader would copy it isn't on the page.
    let page: &[&str] = &["alpha beta gamma delta epsilon zeta trans-", "lation eta theta"];
    host.open(Some("book-a"), None, text_book(&[filler, filler, page, filler]));
    host.goto("book-a", 2, Some("Alpha beta gamma delta epsilon zeta translation eta theta"));
    let found = host.wait_event("Found", is_found);
    assert_eq!(
        found,
        HostEvent::Found { tag: "book-a".into(), page: 2, query: "Alpha beta gamma delta epsilon zeta translation eta theta".into(), found: true }
    );
    assert_eq!(host.h.state().views[0].find.as_ref().map(|f| f.query.as_str()), Some("Alpha beta gamma delta epsilon zeta"));
    assert_eq!(find_page(host.h.state(), 0), Some(2));
}

#[test]
fn goto_reports_a_passage_that_is_not_there_or_an_unknown_tag() {
    let mut host = Host::new();
    let filler: &[&str] = &["Nothing to see here"];
    host.open(Some("book-a"), None, text_book(&[filler, filler, filler]));
    host.goto("book-a", 2, Some("words that never appear"));
    let found = host.wait_event("Found", is_found);
    assert_eq!(found, HostEvent::Found { tag: "book-a".into(), page: 2, query: "words that never appear".into(), found: false });
    assert_eq!(host.h.state().views[0].current, 2, "still on the page");
    assert!(host.h.state().views[0].find.is_none(), "the find bar stays closed");

    host.take();
    host.goto("nope", 7, Some("x"));
    host.h.run_steps(1);
    assert_eq!(
        host.take().into_iter().filter(is_found).collect::<Vec<_>>(),
        [HostEvent::Found { tag: "nope".into(), page: 7, query: "x".into(), found: false }]
    );
}

#[test]
fn a_newer_goto_or_the_user_scrolling_ends_a_pending_one() {
    let mut host = Host::new();
    let filler: &[&str] = &["Nothing to see here"];
    host.open(Some("book-a"), None, text_book(&[filler, filler, filler, &["the target passage"]]));
    host.wait_text(0, 0);
    host.take();
    // In the same frame the user scrolls: their move wins.
    host.goto("book-a", 3, Some("target passage"));
    host.h.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, -40.0),
        phase: egui::TouchPhase::Move,
        modifiers: Default::default(),
    });
    host.h.run_steps(1);
    assert!(host.take().contains(&HostEvent::Found { tag: "book-a".into(), page: 3, query: "target passage".into(), found: false }));

    host.goto("book-a", 3, Some("target passage"));
    host.goto("book-a", 2, Some("Nothing"));
    host.h.run_steps(1);
    let events = host.take();
    assert!(events.contains(&HostEvent::Found { tag: "book-a".into(), page: 3, query: "target passage".into(), found: false }), "{events:?}");
    let found = host.wait_event("Found", is_found);
    assert_eq!(found, HostEvent::Found { tag: "book-a".into(), page: 2, query: "Nothing".into(), found: true });
}

#[test]
fn huge_inputs_are_capped_and_never_panic() {
    let mut host = Host::new();
    // 250 lines of 100 characters: 25 000 characters on one page.
    let line = "abcdefghi ".repeat(10);
    let line = line.trim_end().to_string();
    let lines: Vec<&str> = (0..250).map(|_| line.as_str()).collect();
    host.open(Some("big"), None, text_book_sized(&[&lines], 1.4, 1.5));
    host.wait_text(0, 0);
    host.take();
    assert!(host.h.state_mut().views[0].select_all());
    host.h.run_steps(3);
    let events = host.take();
    let Some(HostEvent::Selection { text, rects, .. }) = selections(&events).first().copied() else { panic!("{events:?}") };
    assert_eq!(text.chars().count(), MAX_HOST_TEXT_CHARS, "cut at the cap");
    assert!(!rects.is_empty() && rects.len() <= pdfcraft_ui_egui::MAX_HOST_RECTS);

    // A passage far past the cap is cut before it is looked for.
    host.goto("big", 0, Some(&"abcdefghi ".repeat(20_000)));
    let found = host.wait_event("Found", is_found);
    let HostEvent::Found { query, found, .. } = found else { unreachable!() };
    assert!(query.chars().count() <= MAX_HOST_FIND_CHARS);
    assert!(found, "its first words are on the page");

    // Absurd pages and labels.
    host.goto("big", usize::MAX, None);
    host.set_items(&[("k", &"x".repeat(10_000))]);
    host.h.run_steps(3);
    assert_eq!(host.h.state().views[0].current, 0, "clamped to the last page");
}

#[test]
fn goto_on_a_document_without_pages_answers_without_panicking() {
    let mut host = Host::new();
    let empty = Pdf::new().obj("<< /Type /Catalog /Pages 2 0 R >>").obj("<< /Type /Pages /Kids [] /Count 0 >>").build();
    host.open(Some("empty"), None, empty);
    host.take();
    host.goto("empty", 3, Some("anything"));
    host.h.run_steps(3);
    let found: Vec<HostEvent> = host.take().into_iter().filter(is_found).collect();
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(matches!(&found[0], HostEvent::Found { found: false, .. }));
}
