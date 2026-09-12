//! Terminal-safe rendering helpers for untrusted registry and manifest text.

use std::borrow::Cow;

/// Strip Unicode formatting characters while preserving terminal styling
/// emitted by trusted formatters.
pub fn strip_formatting(text: &str) -> Cow<'_, str> {
    if text.is_ascii() {
        return Cow::Borrowed(text);
    }
    if text.chars().any(is_format_character) {
        Cow::Owned(
            text.chars()
                .filter(|ch| !is_format_character(*ch))
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    }
}

/// Strip control and Unicode formatting characters before displaying text,
/// preserving newlines and tabs for multi-line output.
pub fn sanitize(text: &str) -> Cow<'_, str> {
    if is_safe_ascii::<true>(text) {
        return Cow::Borrowed(text);
    }
    if text
        .chars()
        .any(|ch| is_format_character(ch) || ch.is_control() && ch != '\n' && ch != '\t')
    {
        Cow::Owned(
            text.chars()
                .filter(|ch| {
                    !is_format_character(*ch) && (!ch.is_control() || *ch == '\n' || *ch == '\t')
                })
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    }
}

/// Strip every control and Unicode formatting character from a single-line
/// terminal field.
pub fn sanitize_inline(text: &str) -> Cow<'_, str> {
    if needs_inline_sanitization(text) {
        Cow::Owned(
            text.chars()
                .filter(|ch| !ch.is_control() && !is_format_character(*ch))
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    }
}

/// Whether a single-line terminal field needs sanitization.
pub fn needs_inline_sanitization(text: &str) -> bool {
    !is_safe_ascii::<false>(text)
        && text
            .chars()
            .any(|ch| ch.is_control() || is_format_character(ch))
}

#[inline]
fn is_safe_ascii<const MULTILINE: bool>(text: &str) -> bool {
    text.bytes().all(|byte| {
        byte.is_ascii()
            && (!byte.is_ascii_control() || MULTILINE && (byte == b'\n' || byte == b'\t'))
    })
}

fn is_format_character(ch: char) -> bool {
    matches!(
        ch,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}',
    )
}

#[cfg(test)]
mod tests {
    use super::{sanitize, sanitize_inline, strip_formatting};
    use std::borrow::Cow;

    #[test]
    fn strips_unicode_format_characters() {
        let text = "safe\u{00AD}\u{202E}\u{2066}\u{E0020}text";
        assert_eq!(sanitize(text), "safetext");
        assert_eq!(sanitize_inline(text), "safetext");
        assert_eq!(strip_formatting(text), "safetext");
    }

    #[test]
    fn multiline_sanitizer_preserves_newlines_and_tabs() {
        let text = "safe\n\ttext";
        assert_eq!(sanitize(text), text);
        assert_eq!(sanitize_inline(text), "safetext");
    }

    #[test]
    fn borrows_text_that_does_not_need_sanitizing() {
        assert!(matches!(sanitize("safe text"), Cow::Borrowed(_)));
        assert!(matches!(sanitize_inline("safe text"), Cow::Borrowed(_)));
    }
}
