//! The parts of the host bridge (`host` in `main.rs`) that need no browser: checking what the
//! parent sends, converting rectangles for it, and deciding when a debounced message is due.

use pdfcraft_ui_egui::{HostMark, HostMenuItem, MAX_HOST_MARK_RECTS, MAX_HOST_MARK_RECTS_TOTAL};

/// At most this many host menu items are kept; later entries are dropped.
pub const MAX_MENU_ITEMS: usize = 8;
/// Keys are 1–32 of `a-z`, `0-9`, `_` and `-`.
pub const MAX_MENU_KEY: usize = 32;
/// Labels are at most this many characters, after trimming.
pub const MAX_MENU_LABEL: usize = 64;
/// A `pdfcraft:goto` passage longer than this (characters) is cut before it reaches the app,
/// which cuts it again after collapsing its spaces.
pub const MAX_GOTO_FIND_CHARS: usize = 10_000;
/// `pdfcraft:selection` is sent once the selection has rested this long (ms).
pub const SELECTION_DEBOUNCE_MS: f64 = 300.0;
/// Mark set names are 1–64 characters.
pub const MAX_MARK_SET_CHARS: usize = 64;
/// At most this many entries of a `pdfcraft:marks` message are read; the app keeps fewer.
pub const MAX_MARK_ENTRIES: usize = MAX_HOST_MARK_RECTS_TOTAL;
/// At most this many rectangles of an entry are read (the app keeps [`MAX_HOST_MARK_RECTS`]).
pub const MAX_MARK_ENTRY_RECTS: usize = MAX_HOST_MARK_RECTS;
/// A mark whose colour is missing or not `#rrggbb` / `#rrggbbaa`: coral.
pub const DEFAULT_MARK_COLOR: egui::Color32 = egui::Color32::from_rgb(0xd8, 0x5a, 0x30);

/// The items of a `pdfcraft:menu-items` message, one `(key, label)` per array entry (`None`
/// for a missing or non-string field). Invalid entries are dropped, as are entries past
/// [`MAX_MENU_ITEMS`] and repeated keys (the first wins); labels are trimmed.
pub fn menu_items(entries: impl IntoIterator<Item = (Option<String>, Option<String>)>) -> Vec<HostMenuItem> {
    let mut out: Vec<HostMenuItem> = Vec::new();
    for (key, label) in entries.into_iter().take(MAX_MENU_ITEMS) {
        let (Some(key), Some(label)) = (key, label) else { continue };
        let label = label.trim();
        let key_ok =
            (1..=MAX_MENU_KEY).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        let label_ok = !label.is_empty() && label.chars().count() <= MAX_MENU_LABEL;
        if key_ok && label_ok && !out.iter().any(|i| i.key == key) {
            out.push(HostMenuItem { key, label: label.to_string() });
        }
    }
    out
}

/// A page index the parent sent in `pdfcraft:goto`: a finite, non-negative whole number.
pub fn goto_page(n: f64) -> Option<usize> {
    // `as` saturates at usize::MAX; the app clamps to the page count.
    (n.is_finite() && n >= 0.0 && n.fract() == 0.0).then_some(n as usize)
}

/// The `seq` of a `pdfcraft:selection-now`, echoed in its `pdfcraft:selection-sent`: a whole
/// number from 0 to `u32::MAX`; anything else is `None` (answered without one).
pub fn request_seq(n: f64) -> Option<u32> {
    (n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= f64::from(u32::MAX)).then_some(n as u32)
}

/// A `pdfcraft:goto` passage, cut to [`MAX_GOTO_FIND_CHARS`]; `None` when blank.
pub fn goto_find(find: &str) -> Option<String> {
    let cut = match find.char_indices().nth(MAX_GOTO_FIND_CHARS) {
        Some((end, _)) => find.get(..end).unwrap_or(find),
        None => find,
    };
    (!cut.trim().is_empty()).then(|| cut.to_string())
}

/// One entry of a `pdfcraft:marks` message as read from the parent: `page`, `rects` (each an
/// `[x, y, width, height]` of finite numbers, or `None`), `style` and `color` (`None` for a
/// missing or ill-typed field).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MarkEntry {
    pub page: Option<f64>,
    pub rects: Vec<Option<[f64; 4]>>,
    pub style: Option<String>,
    pub color: Option<String>,
}

/// A `pdfcraft:marks` / `pdfcraft:clear-marks` set name: 1–[`MAX_MARK_SET_CHARS`] characters.
pub fn mark_set(set: &str) -> Option<String> {
    (!set.is_empty() && set.chars().count() <= MAX_MARK_SET_CHARS).then(|| set.to_string())
}

/// `#rrggbb` or `#rrggbbaa` (hex digits in either case); anything else is [`DEFAULT_MARK_COLOR`].
pub fn mark_color(color: Option<&str>) -> egui::Color32 {
    let parse = |hex: &str| -> Option<egui::Color32> {
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i: usize| hex.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex.len() {
            6 => Some(egui::Color32::from_rgb(byte(0)?, byte(2)?, byte(4)?)),
            8 => Some(egui::Color32::from_rgba_unmultiplied(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
            _ => None,
        }
    };
    color.and_then(|c| c.strip_prefix('#')).and_then(parse).unwrap_or(DEFAULT_MARK_COLOR)
}

/// A mark rectangle `[x, y, width, height]` (page view space, points) as the app's
/// `[x0, y0, x1, y1]`; `None` when not finite (also once in `f32`) or when a side is negative.
pub fn mark_rect(r: [f64; 4]) -> Option<[f32; 4]> {
    let [x, y, w, h] = r;
    if !(r.iter().all(|v| v.is_finite()) && w >= 0.0 && h >= 0.0) {
        return None;
    }
    // Out-of-range values become infinite in f32, and are refused below.
    let out = [x as f32, y as f32, (x + w) as f32, (y + h) as f32];
    out.iter().all(|v| v.is_finite()).then_some(out)
}

/// The entries of a `pdfcraft:marks` message, by page in the order each page first appears,
/// each page's marks in array order (a mark's index is its position there). An entry without a
/// whole, non-negative page is dropped; one with an unknown `style` (only `"underline"`, the
/// default when absent) keeps its index but draws nothing, as do rectangles that aren't valid.
pub fn marks(entries: impl IntoIterator<Item = MarkEntry>) -> Vec<(usize, Vec<HostMark>)> {
    let mut out: Vec<(usize, Vec<HostMark>)> = Vec::new();
    for entry in entries.into_iter().take(MAX_MARK_ENTRIES) {
        let Some(page) = entry.page.and_then(goto_page) else { continue };
        let underline = entry.style.as_deref().is_none_or(|s| s == "underline");
        let rects = if underline { entry.rects.into_iter().take(MAX_MARK_ENTRY_RECTS).flatten().filter_map(mark_rect).collect() } else { Vec::new() };
        let mark = HostMark { rects, color: mark_color(entry.color.as_deref()) };
        match out.iter_mut().find(|(p, _)| *p == page) {
            Some((_, list)) => list.push(mark),
            None => out.push((page, vec![mark])),
        }
    }
    out
}

/// A page rectangle `[x0, y0, x1, y1]` (view space, points) as the parent gets it:
/// `[x, y, width, height]`, corners in either order. `None` when not finite.
pub fn page_rect(r: [f32; 4]) -> Option<[f64; 4]> {
    let [x0, y0, x1, y1] = r.map(f64::from);
    [x0, y0, x1, y1].iter().all(|v| v.is_finite()).then(|| [x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs()])
}

/// A screen rectangle `[x0, y0, x1, y1]` in egui points as `[x, y, width, height]` in CSS
/// pixels of the frame: points × `pixels_per_point` are device pixels, ÷ `device_pixel_ratio`
/// CSS pixels (they differ when PdfCraft's interface is scaled). A ratio that isn't a positive
/// number counts as 1. `None` when not finite.
pub fn screen_rect(r: [f32; 4], pixels_per_point: f32, device_pixel_ratio: f64) -> Option<[f64; 4]> {
    let ratio = if device_pixel_ratio.is_finite() && device_pixel_ratio > 0.0 { device_pixel_ratio } else { 1.0 };
    let ppp = f64::from(pixels_per_point);
    let scale = if ppp.is_finite() && ppp > 0.0 { ppp / ratio } else { 1.0 / ratio };
    page_rect(r).map(|v| v.map(|x| x * scale)).filter(|v| v.iter().all(|x| x.is_finite()))
}

/// A trailing debounce: of the values pushed while they keep changing, only the last is sent,
/// once none has come for the delay; a value equal to the last one sent isn't sent again.
#[derive(Debug)]
pub struct Debounce<T> {
    delay_ms: f64,
    pending: Option<T>,
    sent: Option<T>,
    /// When the pending value is due (ms, the caller's clock).
    due: f64,
    timer: bool,
}

/// What a debounce timer firing should do.
#[derive(Debug, PartialEq)]
pub enum Fire<T> {
    /// Send this value now.
    Send(T),
    /// Not yet: set the timer again for this many ms.
    Wait(f64),
    /// Nothing to send.
    Idle,
}

impl<T: PartialEq> Debounce<T> {
    pub fn new(delay_ms: f64) -> Self {
        Self { delay_ms, pending: None, sent: None, due: 0.0, timer: false }
    }

    /// A new value at `now`. Returns whether a timer must be set (for the delay): none is
    /// running yet.
    pub fn push(&mut self, value: T, now: f64) -> bool {
        self.pending = Some(value);
        self.due = now + self.delay_ms;
        !std::mem::replace(&mut self.timer, true)
    }

    /// The timer fired at `now`.
    pub fn fire(&mut self, now: f64) -> Fire<T> {
        if self.pending.is_none() {
            self.timer = false;
            return Fire::Idle;
        }
        // A clock that went backwards (`now` far before the deadline) sends rather than waits
        // for a time that may never come.
        let wait = self.due - now;
        if wait > 0.0 && wait <= self.delay_ms {
            return Fire::Wait(wait);
        }
        self.timer = false;
        match self.pending.take() {
            Some(value) if self.sent.as_ref() != Some(&value) => {
                self.sent = None;
                Fire::Send(value)
            }
            _ => Fire::Idle,
        }
    }

    /// The pending value now, without waiting for the delay (`None` when there is none or it
    /// equals the last one sent). A running timer is left to find nothing.
    pub fn flush(&mut self) -> Option<T> {
        match self.pending.take() {
            Some(value) if self.sent.as_ref() != Some(&value) => {
                self.sent = None;
                Some(value)
            }
            _ => None,
        }
    }

    /// Record what was sent (after [`Fire::Send`] or [`Debounce::flush`]), so an equal value isn't sent again.
    pub fn sent(&mut self, value: T) {
        self.sent = Some(value);
    }

    /// The timer couldn't be set: forget it, so the next push tries again.
    pub fn no_timer(&mut self) {
        self.timer = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, label: &str) -> (Option<String>, Option<String>) {
        (Some(key.into()), Some(label.into()))
    }

    #[test]
    fn menu_items_keep_valid_entries_in_order() {
        let items = menu_items([
            entry("card", "Criar card"),
            entry("Bad", "Upper-case key"),
            entry("", "Empty key"),
            entry(&"k".repeat(33), "Long key"),
            entry("x y", "Space in key"),
            entry("define", "  Definir palavra  "),
            entry("blank", "   "),
            entry("long", &"é".repeat(65)),
            (None, Some("No key".into())),
            (Some("nolabel".into()), None),
            entry("card", "Duplicate"),
        ]);
        let got: Vec<(&str, &str)> = items.iter().map(|i| (i.key.as_str(), i.label.as_str())).collect();
        assert_eq!(got, [("card", "Criar card"), ("define", "Definir palavra")]);
        assert_eq!(menu_items([entry("a-1_b", &"é".repeat(64))]).len(), 1, "64 characters, not bytes");
        assert!(menu_items([]).is_empty(), "[] removes every item");
    }

    #[test]
    fn menu_items_stop_at_eight() {
        let many: Vec<_> = (0..12).map(|i| entry(&format!("k{i}"), "Item")).collect();
        let items = menu_items(many);
        assert_eq!(items.len(), MAX_MENU_ITEMS);
        assert_eq!(items.last().map(|i| i.key.as_str()), Some("k7"));
    }

    #[test]
    fn goto_pages_are_whole_non_negative_numbers() {
        assert_eq!(goto_page(0.0), Some(0));
        assert_eq!(goto_page(41.0), Some(41));
        for bad in [-1.0, 1.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(goto_page(bad), None, "{bad}");
        }
        assert_eq!(goto_page(1e300), Some(usize::MAX), "saturates; the app clamps it");
    }

    #[test]
    fn goto_passages_are_cut_and_blank_is_none() {
        assert_eq!(goto_find("  "), None);
        assert_eq!(goto_find("a passage").as_deref(), Some("a passage"));
        let long = "é".repeat(MAX_GOTO_FIND_CHARS + 5);
        assert_eq!(goto_find(&long).map(|s| s.chars().count()), Some(MAX_GOTO_FIND_CHARS));
    }

    #[test]
    fn request_seqs_are_whole_u32s() {
        assert_eq!(request_seq(0.0), Some(0));
        assert_eq!(request_seq(7.0), Some(7));
        assert_eq!(request_seq(f64::from(u32::MAX)), Some(u32::MAX));
        for bad in [-1.0, 0.5, f64::from(u32::MAX) + 1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(request_seq(bad), None, "{bad}");
        }
    }

    fn mark(page: f64, rects: &[[f64; 4]], style: Option<&str>, color: Option<&str>) -> MarkEntry {
        MarkEntry { page: Some(page), rects: rects.iter().copied().map(Some).collect(), style: style.map(Into::into), color: color.map(Into::into) }
    }

    #[test]
    fn mark_rects_become_x0_y0_x1_y1() {
        assert_eq!(mark_rect([10.0, 20.0, 100.0, 14.0]), Some([10.0, 20.0, 110.0, 34.0]));
        assert_eq!(mark_rect([0.0, 0.0, 0.0, 0.0]), Some([0.0; 4]), "an empty rectangle is still one");
        for bad in [[f64::NAN, 0.0, 1.0, 1.0], [0.0, f64::INFINITY, 1.0, 1.0], [0.0, 0.0, -1.0, 1.0], [0.0, 0.0, 1.0, -0.5], [1e300, 0.0, 1.0, 1.0]] {
            assert_eq!(mark_rect(bad), None, "{bad:?}");
        }
        // Round trip with the rectangles the parent hears in pdfcraft:selection.
        let r = [12.5, 30.0, 47.25, 14.0];
        assert_eq!(mark_rect(r).and_then(page_rect), Some(r));
    }

    #[test]
    fn mark_colors_are_hex_or_coral() {
        assert_eq!(mark_color(Some("#ba7517")), egui::Color32::from_rgb(0xba, 0x75, 0x17));
        assert_eq!(mark_color(Some("#BA7517")), egui::Color32::from_rgb(0xba, 0x75, 0x17));
        assert_eq!(mark_color(Some("#d85a3080")), egui::Color32::from_rgba_unmultiplied(0xd8, 0x5a, 0x30, 0x80));
        for bad in [None, Some(""), Some("ba7517"), Some("#ba751"), Some("#ba75170"), Some("#gg0000"), Some("red"), Some("#+1+2+3"), Some("#ééé")]
        {
            assert_eq!(mark_color(bad), DEFAULT_MARK_COLOR, "{bad:?}");
        }
        assert_eq!(DEFAULT_MARK_COLOR, egui::Color32::from_rgb(0xd8, 0x5a, 0x30));
    }

    #[test]
    fn mark_sets_are_short_names() {
        assert_eq!(mark_set("vocab").as_deref(), Some("vocab"));
        assert_eq!(mark_set(""), None);
        assert_eq!(mark_set(&"é".repeat(MAX_MARK_SET_CHARS)).map(|s| s.chars().count()), Some(MAX_MARK_SET_CHARS));
        assert_eq!(mark_set(&"a".repeat(MAX_MARK_SET_CHARS + 1)), None);
    }

    #[test]
    fn marks_group_by_page_and_keep_their_indices() {
        let coral = Some("#d85a30");
        let got = marks([
            mark(3.0, &[[10.0, 20.0, 30.0, 12.0]], Some("underline"), coral),
            mark(1.0, &[[1.0, 2.0, 3.0, 4.0], [5.0, 6.0, 7.0, 8.0]], None, Some("#ba7517")),
            mark(3.0, &[[0.0, 0.0, 5.0, 5.0]], Some("box"), coral),
            mark(3.0, &[[f64::NAN, 0.0, 1.0, 1.0], [40.0, 20.0, 10.0, 12.0]], Some("underline"), Some("bad")),
            mark(-1.0, &[[0.0, 0.0, 1.0, 1.0]], None, coral),
            mark(2.5, &[[0.0, 0.0, 1.0, 1.0]], None, coral),
            MarkEntry { page: None, ..Default::default() },
            mark(5.0, &[], None, None),
        ]);
        let pages: Vec<usize> = got.iter().map(|(p, _)| *p).collect();
        assert_eq!(pages, [3, 1, 5], "pages in first-seen order; bad pages dropped");
        let (_, three) = &got[0];
        assert_eq!(three.len(), 3);
        assert_eq!(three[0].rects, [[10.0, 20.0, 40.0, 32.0]]);
        assert!(three[1].rects.is_empty(), "an unknown style keeps index 1 but draws nothing");
        assert_eq!(three[2].rects, [[40.0, 20.0, 50.0, 32.0]], "the invalid rectangle alone is dropped");
        assert_eq!(three[2].color, DEFAULT_MARK_COLOR);
        assert_eq!(got[1].1[0].rects.len(), 2);
        assert_eq!(got[1].1[0].color, egui::Color32::from_rgb(0xba, 0x75, 0x17));
        assert_eq!(got[2].1.len(), 1, "an empty entry clears its page");
        assert!(got[2].1[0].rects.is_empty());
    }

    #[test]
    fn marks_read_at_most_eight_rects_per_entry() {
        let many: Vec<[f64; 4]> = (0..20).map(|i| [f64::from(i), 0.0, 1.0, 1.0]).collect();
        let got = marks([mark(0.0, &many, None, None)]);
        assert_eq!(got[0].1[0].rects.len(), MAX_HOST_MARK_RECTS);
        assert_eq!(got[0].1[0].rects.last(), Some(&[7.0, 0.0, 8.0, 1.0]));
    }

    #[test]
    fn page_rects_become_x_y_width_height() {
        assert_eq!(page_rect([10.0, 20.0, 110.0, 34.0]), Some([10.0, 20.0, 100.0, 14.0]));
        assert_eq!(page_rect([110.0, 34.0, 10.0, 20.0]), Some([10.0, 20.0, 100.0, 14.0]), "corners in any order");
        assert_eq!(page_rect([f32::NAN, 0.0, 1.0, 1.0]), None);
        assert_eq!(page_rect([0.0, f32::INFINITY, 1.0, 1.0]), None);
    }

    #[test]
    fn screen_rects_become_css_pixels() {
        // At 100% interface scale on a 2× display, points are CSS pixels.
        assert_eq!(screen_rect([10.0, 20.0, 30.0, 40.0], 2.0, 2.0), Some([10.0, 20.0, 20.0, 20.0]));
        // PdfCraft's interface at 125% on a 1× display: a point is 1.25 CSS pixels.
        assert_eq!(screen_rect([8.0, 8.0, 16.0, 24.0], 1.25, 1.0), Some([10.0, 10.0, 10.0, 20.0]));
        // A broken ratio counts as 1.
        for ratio in [0.0, -1.0, f64::NAN] {
            assert_eq!(screen_rect([0.0, 0.0, 4.0, 4.0], 1.0, ratio), Some([0.0, 0.0, 4.0, 4.0]), "{ratio}");
        }
        assert_eq!(screen_rect([0.0, 0.0, f32::MAX, 1.0], f32::MAX, 1e-300), None, "overflow");
    }

    #[test]
    fn debounce_sends_only_the_last_value_once_it_rests() {
        let mut d = Debounce::new(300.0);
        assert!(d.push("a", 0.0), "the first value sets the timer");
        assert!(!d.push("ab", 100.0), "a running timer is reused");
        assert!(!d.push("abc", 200.0));
        // The timer set at 0 fires at 300: "abc" came at 200, so wait until 500.
        assert_eq!(d.fire(300.0), Fire::Wait(200.0));
        assert_eq!(d.fire(500.0), Fire::Send("abc"));
        d.sent("abc");
        assert_eq!(d.fire(600.0), Fire::Idle);

        // The same value again (selected, cleared and selected again within the window) is
        // not sent twice.
        assert!(d.push("", 1000.0));
        assert!(!d.push("abc", 1100.0));
        assert_eq!(d.fire(1400.0), Fire::Idle);
        // A different value afterwards is.
        assert!(d.push("", 2000.0));
        assert_eq!(d.fire(2300.0), Fire::Send(""));
    }

    #[test]
    fn debounce_flush_sends_the_pending_value_before_its_delay() {
        let mut d = Debounce::new(300.0);
        assert_eq!(d.flush(), None, "nothing pending");
        assert!(d.push("a", 0.0));
        assert_eq!(d.flush(), Some("a"), "no wait for the delay");
        d.sent("a");
        assert_eq!(d.fire(300.0), Fire::Idle, "the running timer finds nothing");
        assert!(d.push("ab", 400.0), "a new timer for the next value");
        assert_eq!(d.flush(), Some("ab"));
        d.sent("ab");
        assert!(!d.push("ab", 500.0), "the timer from 400 has not fired yet");
        assert_eq!(d.flush(), None, "the value already sent isn't sent again");
    }

    #[test]
    fn debounce_survives_a_clock_that_jumps_back_and_a_missing_timer() {
        let mut d = Debounce::new(300.0);
        assert!(d.push(1, 10_000.0));
        assert_eq!(d.fire(0.0), Fire::Send(1), "a clock gone backwards doesn't hold the value forever");
        assert!(d.push(2, 0.0));
        d.no_timer();
        assert!(d.push(3, 10.0), "a timer is asked for again");
    }
}
