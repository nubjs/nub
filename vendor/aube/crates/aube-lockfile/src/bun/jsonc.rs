/// Strip JSONC features (line comments, block comments, trailing commas)
/// to produce valid JSON. Respects string literals.
///
/// Output length is byte-identical to the input — comment bytes and
/// trailing commas become spaces (newlines inside block comments are
/// preserved). That keeps every byte offset in `cleaned` pointing at
/// the same byte in the original file, so a `serde_json` parse error
/// on the stripped buffer lines up with the user's editor line/column
/// when rendered against the original source via `miette`'s fancy
/// handler.
pub(super) fn strip_jsonc(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;

    while i < bytes.len() {
        let c = bytes[i];

        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c < 0x80 {
                if c == b'\\' {
                    escape = true;
                } else if c == b'"' {
                    in_string = false;
                }
            }
            i += 1;
            continue;
        }

        // Line comment: replace every byte up to (not including) the
        // newline with a space. The `\n` itself is kept.
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(b' ');
                i += 1;
            }
            continue;
        }

        // Block comment: replace every byte with a space, but keep
        // embedded newlines so line numbers don't shift.
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            out.push(b' ');
            out.push(b' ');
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                out.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                i += 1;
            }
            if i + 1 < bytes.len() {
                // consume the closing `*/`
                out.push(b' ');
                out.push(b' ');
                i += 2;
            } else {
                // unterminated block comment — mirror every remaining
                // byte to preserve length, keeping newlines intact.
                while i < bytes.len() {
                    out.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                    i += 1;
                }
            }
            continue;
        }

        // Trailing comma: replace `,` with a space when the next
        // non-whitespace char is `}` or `]`.
        if c == b',' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] < 0x80 && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'}' || bytes[j] == b']') {
                out.push(b' ');
                i += 1;
                continue;
            }
        }

        if c == b'"' {
            in_string = true;
        }

        out.push(c);
        i += 1;
    }

    String::from_utf8(out).expect("strip_jsonc preserves UTF-8 validity")
}
