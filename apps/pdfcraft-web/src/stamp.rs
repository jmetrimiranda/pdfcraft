//! A host's own entry in the document information dictionary of the files it is handed
//! (`pdfcraft:open` with `infoKey`), so a copy of the file found later can be matched to it.

use std::borrow::Cow;
use std::sync::Arc;

/// `bytes` with `key = value` in the document information dictionary, appended as an
/// incremental update (the bytes before it stay as they are, so signatures keep covering them).
/// When the entry is there already, `bytes` come back unchanged. `password` reads an encrypted
/// file, whose new entry is encrypted like the rest.
///
/// `key` must be a custom key (`pdfcraft_organize::check_custom_key`); `value` is trimmed, and
/// an empty one is refused rather than removing the entry. `/ModDate` isn't changed: the entry
/// is bookkeeping, not an edit by the user.
pub fn stamp_info<'a>(bytes: &'a [u8], key: &str, value: &str, password: Option<&str>) -> Result<Cow<'a, [u8]>, String> {
    pdfcraft_organize::check_custom_key(key).map_err(|e| e.to_string())?;
    let value = value.trim();
    if value.is_empty() {
        return Err("the value is empty".to_string());
    }
    let mut doc = pdfcraft_cos::Document::open_with_password(Arc::new(bytes.to_vec()), password).map_err(|e| e.to_string())?;
    if pdfcraft_organize::info(&doc, key).as_deref() == Some(value) {
        return Ok(Cow::Borrowed(bytes));
    }
    pdfcraft_organize::set_info(&mut doc, key, value).map_err(|e| e.to_string())?;
    let out = pdfcraft_cos::write_incremental(&doc, &pdfcraft_cos::SaveOptions::default()).map_err(|e| e.to_string())?;
    Ok(Cow::Owned(out))
}

#[cfg(test)]
mod tests {
    use super::stamp_info;
    use pdfcraft_testkit::Pdf;
    use std::sync::Arc;

    const KEY: &str = "HostBookId";

    fn page() -> Pdf {
        Pdf::new()
            .obj("<< /Type /Catalog /Pages 2 0 R >>")
            .obj("<< /Type /Pages /Kids [3 0 R] /Count 1 >>")
            .obj("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] /Contents 4 0 R >>")
            .stream("", b"BT /F1 12 Tf (hi) Tj ET")
    }

    fn info(bytes: &[u8], key: &str, password: Option<&str>) -> Option<String> {
        let doc = pdfcraft_cos::Document::open_with_password(Arc::new(bytes.to_vec()), password).unwrap();
        pdfcraft_organize::info(&doc, key)
    }

    /// `bytes` with `/Title` set, as a user's edit saved on top of them.
    fn edited(bytes: &[u8], title: &str) -> Vec<u8> {
        let mut doc = pdfcraft_cos::Document::open(Arc::new(bytes.to_vec())).unwrap();
        pdfcraft_organize::set_info(&mut doc, "Title", title).unwrap();
        pdfcraft_cos::write_incremental(&doc, &Default::default()).unwrap()
    }

    #[test]
    fn adds_the_entry_to_an_indirect_info_dictionary_and_keeps_the_rest() {
        let original = page().obj("<< /Title (Kept) >>").trailer_entry("/Info 5 0 R").build();
        let stamped = stamp_info(&original, KEY, "book-1", None).unwrap();
        assert!(stamped.starts_with(&original), "an incremental update keeps the original bytes");
        assert_eq!(info(&stamped, KEY, None).as_deref(), Some("book-1"));
        assert_eq!(info(&stamped, "Title", None).as_deref(), Some("Kept"));
    }

    #[test]
    fn adds_the_entry_to_an_inline_info_dictionary() {
        let original = page().trailer_entry("/Info << /Author (Ana) >>").build();
        let stamped = stamp_info(&original, KEY, "book-1", None).unwrap();
        assert_eq!(info(&stamped, KEY, None).as_deref(), Some("book-1"));
        assert_eq!(info(&stamped, "Author", None).as_deref(), Some("Ana"));
    }

    #[test]
    fn adds_an_info_dictionary_when_there_is_none() {
        let original = page().build();
        assert_eq!(info(&original, KEY, None), None);
        let stamped = stamp_info(&original, KEY, "book-1", None).unwrap();
        assert_eq!(info(&stamped, KEY, None).as_deref(), Some("book-1"));
    }

    #[test]
    fn stamping_twice_changes_nothing() {
        let stamped = stamp_info(&page().build(), KEY, "book-1", None).unwrap().into_owned();
        let again = stamp_info(&stamped, KEY, "book-1", None).unwrap();
        assert!(matches!(again, std::borrow::Cow::Borrowed(_)));
        assert_eq!(&*again, stamped.as_slice());
        // Another value replaces it.
        let other = stamp_info(&stamped, KEY, "book-2", None).unwrap();
        assert_eq!(info(&other, KEY, None).as_deref(), Some("book-2"));
    }

    #[test]
    fn stamps_an_encrypted_file_that_reopens_with_its_password() {
        let mut doc = pdfcraft_cos::Document::open(Arc::new(page().obj("<< /Title (Secret) >>").trailer_entry("/Info 5 0 R").build())).unwrap();
        doc.set_encryption(&pdfcraft_cos::NewEncryption {
            algorithm: pdfcraft_cos::Algorithm::Aes256,
            user_password: "pw",
            owner_password: "owner",
            permissions: -1,
            encrypt_metadata: true,
            seed: [7; 32],
        })
        .unwrap();
        let encrypted = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
        assert!(stamp_info(&encrypted, KEY, "book-1", None).is_err(), "it can't be read without the password");
        let stamped = stamp_info(&encrypted, KEY, "book-1", Some("pw")).unwrap();
        assert!(pdfcraft_cos::Document::open(Arc::new(stamped.to_vec())).is_err(), "the file stays encrypted");
        assert_eq!(info(&stamped, KEY, Some("pw")).as_deref(), Some("book-1"));
        assert_eq!(info(&stamped, "Title", Some("pw")).as_deref(), Some("Secret"));
        assert_eq!(info(&stamped, KEY, Some("owner")).as_deref(), Some("book-1"));
    }

    #[test]
    fn unreadable_bytes_and_bad_arguments_are_errors() {
        assert!(stamp_info(b"", KEY, "book-1", None).is_err());
        assert!(stamp_info(b"not a pdf at all", KEY, "book-1", None).is_err());
        assert!(stamp_info(&[0xff; 4096], KEY, "book-1", None).is_err());
        let original = page().build();
        // A truncated file may be repaired or refused; either way, no panic.
        for end in [1, 9, original.len() / 3, original.len() / 2, original.len() - 7] {
            let _ = stamp_info(&original[..end], KEY, "book-1", None);
        }
        assert!(stamp_info(&original, "Title", "book-1", None).is_err(), "a standard key isn't a host's");
        assert!(stamp_info(&original, "Has Space", "book-1", None).is_err());
        assert!(stamp_info(&original, "", "book-1", None).is_err());
        assert!(stamp_info(&original, KEY, "  ", None).is_err());
    }

    /// The host stamps every save of a document whose editing session never saw the stamp: each
    /// saved file must hold the user's edits and the stamp.
    #[test]
    fn a_save_after_an_edit_keeps_the_edit_and_the_stamp() {
        let original = page().build();
        let first = stamp_info(&original, KEY, "book-1", None).unwrap().into_owned();
        // The session saves its edit on top of the unstamped file it opened…
        let second_in = edited(&original, "Edited");
        let second = stamp_info(&second_in, KEY, "book-1", None).unwrap();
        assert_eq!(info(&second, KEY, None).as_deref(), Some("book-1"));
        assert_eq!(info(&second, "Title", None).as_deref(), Some("Edited"));
        // …or on top of a stamped one it opened later: nothing more to add.
        let third_in = edited(&first, "Again");
        let third = stamp_info(&third_in, KEY, "book-1", None).unwrap();
        assert_eq!(&*third, third_in.as_slice());
        assert_eq!(info(&third, "Title", None).as_deref(), Some("Again"));
        assert_eq!(info(&third, KEY, None).as_deref(), Some("book-1"));
    }
}
