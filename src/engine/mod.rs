//! The synthesis engine — base plus ordered patches into one output branch.
//!
//! Everything operates on an ephemeral local bare repository and is a pure
//! function of repository state, which makes the engine unit-testable against
//! local bare repositories with no network.

pub mod fetch;
pub mod overlay;
pub mod pipeline;
pub mod push;
pub mod rebase;
pub mod replay;
pub mod stack;

/// Redact credentials from a transport URL for logs and error messages.
///
/// `https://x-access-token:SECRET@github.com/…` becomes
/// `https://***@github.com/…`. URLs without userinfo (including `file://`
/// paths) pass through untouched. Call this anywhere a URL meets output.
pub fn redact_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('@') {
            Some((_, host)) => format!("{scheme}://***@{host}"),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

/// Redact embedded credentials from every URL-like substring in free text.
///
/// Same rule as [`redact_url`], applied repeatedly: each `scheme://…@…`
/// span becomes `scheme://***@…`. Last line of defense for error chains —
/// transports can echo URLs we never formatted ourselves. Plain emails
/// (`user@example.com`, no `://`) pass through untouched.
pub fn redact_text(text: &str) -> String {
    fn is_terminator(c: char) -> bool {
        c.is_whitespace() || matches!(c, '\'' | '"' | '`' | '<' | '>' | '(' | ')')
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(scheme_at) = rest.find("://") {
        let after = &rest[scheme_at + 3..];
        match after.find('@') {
            Some(at) if !after[..at].chars().any(is_terminator) => {
                out.push_str(&rest[..scheme_at + 3]);
                out.push_str("***@");
                rest = &after[at + 1..];
            }
            _ => {
                // No userinfo here (or one containing spaces, which cannot
                // be a URL) — emit through the scheme marker and continue.
                out.push_str(&rest[..scheme_at + 3]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_embedded_tokens() {
        assert_eq!(
            redact_url("https://x-access-token:secret123@github.com/o/r.git"),
            "https://***@github.com/o/r.git"
        );
    }

    #[test]
    fn leaves_plain_urls_alone() {
        assert_eq!(
            redact_url("https://github.com/o/r.git"),
            "https://github.com/o/r.git"
        );
        assert_eq!(redact_url("file:///tmp/r.git"), "file:///tmp/r.git");
        assert_eq!(redact_url("/tmp/r.git"), "/tmp/r.git");
    }

    #[test]
    fn redacts_text_with_embedded_urls() {
        assert_eq!(
            redact_text(
                "fetch branch `main` from `https://x-access-token:s3cret@github.com/o/r.git` failed"
            ),
            "fetch branch `main` from `https://***@github.com/o/r.git` failed"
        );
    }

    #[test]
    fn leaves_text_without_userinfo_alone() {
        assert_eq!(
            redact_text("contact user@example.com about https://github.com/o/r.git"),
            "contact user@example.com about https://github.com/o/r.git"
        );
    }
}
