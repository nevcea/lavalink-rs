//! Log-only redaction. Wire errors keep their upstream-compatible text; only the
//! operator-facing copy loses URL credentials and expiring query parameters.

const MAX_IDENTIFIER_CHARS: usize = 512;
const MAX_ERROR_CHARS: usize = 2048;

pub(crate) fn safe_identifier(identifier: &str) -> String {
    let text = sanitize_http_url(identifier).unwrap_or_else(|| identifier.to_owned());
    truncate(text, MAX_IDENTIFIER_CHARS)
}

pub(crate) fn safe_error(error: impl std::fmt::Display) -> String {
    let text = error.to_string();
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text.as_str();

    while let Some(start) = next_http_url(rest) {
        redacted.push_str(&rest[..start]);
        let url_and_tail = &rest[start..];
        let end = url_and_tail
            .find(|character: char| character.is_whitespace() || matches!(character, '<' | '>' | '"' | '\''))
            .unwrap_or(url_and_tail.len());
        let token = &url_and_tail[..end];
        let core = token.trim_end_matches([')', ']', '}', ',', ';']);
        let suffix = &token[core.len()..];
        redacted.push_str(&sanitize_http_url(core).unwrap_or_else(|| core.to_owned()));
        redacted.push_str(suffix);
        rest = &url_and_tail[end..];
    }
    redacted.push_str(rest);

    truncate(redacted, MAX_ERROR_CHARS)
}

fn next_http_url(text: &str) -> Option<usize> {
    [text.find("http://"), text.find("https://")]
        .into_iter()
        .flatten()
        .min()
}

fn sanitize_http_url(text: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(text).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

fn truncate(text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text;
    }
    let mut truncated: String = text.chars().take(limit).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_drop_url_secrets_but_keep_searches() {
        assert_eq!(
            safe_identifier("https://user:password@example.com/audio?id=secret#fragment"),
            "https://example.com/audio"
        );
        assert_eq!(safe_identifier("ytsearch:the query"), "ytsearch:the query");
    }

    #[test]
    fn errors_redact_parenthesized_urls_without_losing_the_context() {
        assert_eq!(
            safe_error("request failed (https://cdn.example/audio?token=secret), retrying"),
            "request failed (https://cdn.example/audio), retrying"
        );
    }

    #[test]
    fn identifier_truncation_stays_on_a_utf8_boundary() {
        let identifier = "가".repeat(MAX_IDENTIFIER_CHARS + 1);
        let safe = safe_identifier(&identifier);
        assert_eq!(safe.chars().count(), MAX_IDENTIFIER_CHARS + 1);
        assert!(safe.ends_with('…'));
    }
}
