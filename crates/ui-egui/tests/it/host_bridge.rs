//! The host hooks (`host_inbox`, `host_requests`, `host_events`, `host_save`) that the web
//! build's `?host=parent` bridge drives: tagged opens, saves asked for by the host, and the
//! events it hears.

use std::sync::{Arc, Mutex};

use egui_kittest::Harness;
use pdfcraft_testkit::Pdf;
use pdfcraft_ui_egui::{HostEvent, HostOpen, HostRequest, HostSave, PdfCraftApp};

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
