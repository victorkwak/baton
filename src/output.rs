//! Output helpers: broken-pipe-safe printing, sanitizing untrusted text, and
//! the JSON envelope.

/// Version of the `--format json` envelope and record fields.
pub const SCHEMA_VERSION: u32 = 1;

/// `println!` that exits quietly when stdout is closed (`baton ... | head`).
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if let Err(e) = writeln!(std::io::stdout().lock(), $($arg)*) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
            eprintln!("baton: writing output: {e}");
            std::process::exit(1);
        }
    }};
}

/// Remove control characters (ANSI escapes included) from text written by
/// another agent, keeping newlines and tabs only where `keep_newlines`.
pub fn sanitize(text: &str, keep_newlines: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // Drop an ANSI escape sequence: ESC [ ... final byte, or ESC + one char.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for d in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&d) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            '\n' | '\t' if keep_newlines => out.push(c),
            '\n' | '\t' => out.push(' '),
            c if c.is_control() => {}
            // Bidirectional overrides can hide text; drop them.
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {}
            c => out.push(c),
        }
    }
    out
}

/// A tab-separated field: sanitized, tabs and newlines become spaces.
pub fn field(text: &str) -> String {
    sanitize(text, false)
}

/// The `--format json` envelope.
pub fn envelope(command: &str, items: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "command": command,
        "items": items,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escapes_and_controls() {
        assert_eq!(sanitize("a\u{1b}[31mred\u{1b}[0m b", true), "ared b");
        assert_eq!(sanitize("x\u{7}y\u{202e}z", true), "xyz");
        assert_eq!(sanitize("one\ntwo\tthree", false), "one two three");
        assert_eq!(sanitize("one\ntwo", true), "one\ntwo");
    }
}
