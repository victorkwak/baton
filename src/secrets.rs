//! Secret scan for posts (`docs/design.md`, "Secrets").
//!
//! Known token formats only. Generic high-entropy text and hex SHAs pass, so
//! commit ids, UUIDs and base64 test output never trip it. A finding names
//! the pattern, never the matched text.

use std::sync::LazyLock;

use regex::Regex;

/// One match: which pattern, and where (a field name chosen by the caller).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub pattern: &'static str,
}

struct Pattern {
    name: &'static str,
    re: Regex,
}

static PATTERNS: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    let p = |name, re: &str| Pattern {
        name,
        re: Regex::new(re).unwrap(),
    };
    vec![
        p(
            "PEM private key",
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----",
        ),
        p("GitLab token (glpat-)", r"\bglpat-[A-Za-z0-9_-]{20,}"),
        p("GitHub token (ghp_)", r"\bgh[pousr]_[A-Za-z0-9]{36,}"),
        p(
            "GitHub token (github_pat_)",
            r"\bgithub_pat_[A-Za-z0-9_]{22,}",
        ),
        p("Anthropic key (sk-ant-)", r"\bsk-ant-[A-Za-z0-9_-]{20,}"),
        p("Slack token (xox[bp]-)", r"\bxox[bp]-[A-Za-z0-9-]{10,}"),
    ]
});

static AWS_KEY_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b").unwrap());
/// A 40-character AWS secret candidate, delimited (the regex crate has no
/// look-around, so the delimiters are matched and the middle captured).
static AWS_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[^A-Za-z0-9/+=])([A-Za-z0-9/+]{40})(?:[^A-Za-z0-9/+=]|$)").unwrap()
});
static HEX40: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-fA-F]{40}$").unwrap());

/// How far from an AWS key id a secret key counts as "next to" it, in bytes.
const AWS_WINDOW: usize = 300;

/// Every known secret pattern in `text`.
pub fn scan(text: &str) -> Vec<Finding> {
    let mut found: Vec<Finding> = PATTERNS
        .iter()
        .filter(|p| p.re.is_match(text))
        .map(|p| Finding { pattern: p.name })
        .collect();
    if aws_pair(text) {
        found.push(Finding {
            pattern: "AWS secret key next to an access key id",
        });
    }
    found
}

fn aws_pair(text: &str) -> bool {
    AWS_KEY_ID.find_iter(text).any(|id| {
        let start = floor_char(text, id.start().saturating_sub(AWS_WINDOW));
        let end = ceil_char(text, (id.end() + AWS_WINDOW).min(text.len()));
        let window = &text[start..end];
        AWS_SECRET.captures_iter(window).any(|c| {
            let s = &c[1];
            // Hex SHAs are allowed; an AWS secret mixes cases and digits.
            !HEX40.is_match(s)
                && s.bytes().any(|b| b.is_ascii_uppercase())
                && s.bytes().any(|b| b.is_ascii_lowercase())
        })
    })
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(text: &str) -> bool {
        !scan(text).is_empty()
    }

    #[test]
    fn each_pattern_is_found() {
        assert!(hit("x\n-----BEGIN OPENSSH PRIVATE KEY-----\nabc"));
        assert!(hit("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(hit("-----BEGIN PRIVATE KEY-----"));
        assert!(hit("-----BEGIN PGP PRIVATE KEY BLOCK-----"));
        assert!(hit("token glpat-abcdefghijklmnopqrst12"));
        assert!(hit("ghp_abcdefghijklmnopqrstuvwxyz0123456789"));
        assert!(hit("github_pat_11ABCDEFG0123456789_abcdefghijklmnop"));
        assert!(hit("ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnopqrstuv"));
        assert!(hit("xoxb-1234567890-abcdef"));
        assert!(hit("xoxp-1234567890-abcdef"));
        assert!(hit(
            "aws_access_key_id = AKIAIOSFODNN7EXAMPLE\naws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
        ));
        assert!(hit(
            "ASIAIOSFODNN7EXAMPLE wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
        ));
        let f = scan("ghp_abcdefghijklmnopqrstuvwxyz0123456789");
        assert_eq!(f[0].pattern, "GitHub token (ghp_)");
    }

    #[test]
    fn harmless_text_passes() {
        // 40-hex SHAs, also next to an AWS key id.
        assert!(!hit("commit 3f786850e387550fdab836ed7e6dc881de23001b"));
        assert!(!hit(
            "AKIAIOSFODNN7EXAMPLE 3f786850e387550fdab836ed7e6dc881de23001b"
        ));
        // UUIDs.
        assert!(!hit("session 123e4567-e89b-12d3-a456-426614174000"));
        // Base64 test output, without an AWS key id near it.
        assert!(!hit(
            "dGhpcyBpcyBzb21lIGJhc2U2NCB0ZXN0IG91dHB1dCBmb3IgYmF0b24="
        ));
        assert!(!hit("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"));
        // An AWS key id alone (no secret next to it).
        assert!(!hit("the role uses AKIAIOSFODNN7EXAMPLE"));
        // Lookalikes.
        assert!(!hit("-----BEGIN PUBLIC KEY-----"));
        assert!(!hit("ghp_short"));
        assert!(!hit("task sk-ant- prefix only"));
        assert!(!hit("01JBX3Z9Q8W7E6R5T4Y3U2I1O0 ulid"));
    }

    #[test]
    fn window_respects_char_boundaries() {
        let text = format!("{}AKIAIOSFODNN7EXAMPLE{}", "é".repeat(400), "ü".repeat(400));
        assert!(!hit(&text));
    }
}
