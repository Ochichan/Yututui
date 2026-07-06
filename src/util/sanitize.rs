//! Redaction for errors/log text that may contain paths or secrets.

const MAX_ERROR_TEXT: usize = 1024;
const SECRET_PREFIXES: &[&str] = &["ghp_", "github_pat_", "sk-", "npm_", "xox", "AKIA", "AIza"];
const COOKIE_KEYS: &[&str] = &[
    "SID=",
    "SAPISID=",
    "APISID=",
    "HSID=",
    "SSID=",
    "LOGIN_INFO=",
    "VISITOR_INFO1_LIVE=",
];
/// Named secret parameters (OAuth / API / provider), matched case-insensitively as
/// `name=value`, `name: value`, or `"name":"value"`. Covers URL query strings, form bodies,
/// JSON error bodies, and header lines regardless of the secret's length — catching short
/// session keys that slip past the long-mixed-token heuristic. Deliberately excludes the very
/// generic `token`/`key`/`sig` bare names (the long-token pass already redacts their values)
/// to avoid mangling ordinary prose.
const SENSITIVE_PARAMS: &[&str] = &[
    "access_token",
    "refresh_token",
    "id_token",
    "client_secret",
    "api_key",
    "session_key",
    "password",
    "signature",
];

pub fn sanitize_error_text(input: impl AsRef<str>) -> String {
    let mut out = input.as_ref().to_owned();
    if let Some(home) = home_dir_string()
        && home.len() > 1
    {
        out = out.replace(&home, "~");
    }
    out = redact_cookie_values(&out);
    out = redact_keyed_values(&out);
    out = redact_prefixed_tokens(&out);
    out = redact_long_tokens(&out);
    if out.len() > MAX_ERROR_TEXT {
        // Truncate at a UTF-8 char boundary at or below the cap. `String::truncate` panics if
        // the byte index lands mid-codepoint, which non-ASCII error text (Korean status
        // messages, non-ASCII paths) reaches once it passes the cap.
        let mut end = MAX_ERROR_TEXT;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push_str("...");
    }
    out
}

fn home_dir_string() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .filter(|s| !s.trim().is_empty())
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/')
}

fn redact_prefixed_tokens(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let rest = &input[i..];
        if let Some(prefix) = SECRET_PREFIXES.iter().find(|p| rest.starts_with(**p)) {
            out.push_str(prefix);
            out.push_str("<redacted>");
            i += prefix.len();
            while i < input.len() {
                let c = input[i..].chars().next().unwrap();
                if !is_token_char(c) {
                    break;
                }
                i += c.len_utf8();
            }
        } else {
            let c = rest.chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

fn redact_cookie_values(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let rest = &input[i..];
        if let Some(key) = COOKIE_KEYS.iter().find(|k| rest.starts_with(**k)) {
            out.push_str(key);
            out.push_str("<redacted>");
            i += key.len();
            while i < input.len() {
                let c = input[i..].chars().next().unwrap();
                if c == ';' || c.is_whitespace() {
                    break;
                }
                i += c.len_utf8();
            }
        } else {
            let c = rest.chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_param_delim(b: u8) -> bool {
    matches!(
        b,
        b'&' | b';' | b',' | b'"' | b'\'' | b' ' | b'\t' | b'\n' | b'\r' | b'}' | b')'
    )
}

fn is_ws_or_quote(b: u8) -> bool {
    matches!(b, b'"' | b'\'' | b' ' | b'\t' | b'\n' | b'\r')
}

/// From just past a key name, consume an optional closing quote, spaces, a required `=`/`:`,
/// then spaces and an optional opening quote; return the value's start index (or `None` when
/// no assignment follows, so a bare word like `password_hint` is left alone).
fn assignment_value_start(bytes: &[u8], mut pos: usize) -> Option<usize> {
    if bytes.get(pos) == Some(&b'"') {
        pos += 1;
    }
    while bytes.get(pos) == Some(&b' ') {
        pos += 1;
    }
    match bytes.get(pos) {
        Some(&b'=') | Some(&b':') => pos += 1,
        _ => return None,
    }
    while bytes.get(pos) == Some(&b' ') {
        pos += 1;
    }
    if bytes.get(pos) == Some(&b'"') {
        pos += 1;
    }
    Some(pos)
}

/// Emit `<redacted>` and skip the value bytes up to the first `is_delim` byte; return the new
/// scan position.
fn redact_value(input: &str, start: usize, out: &mut String, is_delim: fn(u8) -> bool) -> usize {
    out.push_str("<redacted>");
    let bytes = input.as_bytes();
    let mut k = start;
    while k < input.len() && !is_delim(bytes[k]) {
        k += 1;
    }
    k
}

/// Redact `Authorization: Bearer <token>` and `name=value` for [`SENSITIVE_PARAMS`], keeping
/// the key/operator verbatim so the log still reads sensibly.
fn redact_keyed_values(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let boundary = i == 0 || !is_word_byte(bytes[i - 1]);
        if boundary && lower[i..].starts_with("bearer ") {
            out.push_str(&input[i..i + "bearer ".len()]);
            i = redact_value(input, i + "bearer ".len(), &mut out, is_ws_or_quote);
            continue;
        }
        if boundary
            && let Some(name) = SENSITIVE_PARAMS.iter().find(|p| lower[i..].starts_with(**p))
            && let Some(val_start) = assignment_value_start(bytes, i + name.len())
        {
            out.push_str(&input[i..val_start]);
            i = redact_value(input, val_start, &mut out, is_param_delim);
            continue;
        }
        let c = input[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

fn redact_long_tokens(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut token = String::new();
    for c in input.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '=') {
            token.push(c);
        } else {
            push_redacted_token(&mut out, &token);
            token.clear();
            out.push(c);
        }
    }
    push_redacted_token(&mut out, &token);
    out
}

fn push_redacted_token(out: &mut String, token: &str) {
    if token.len() >= 24
        && token.chars().any(|c| c.is_ascii_alphabetic())
        && token.chars().any(|c| c.is_ascii_digit())
    {
        out.push_str("<redacted>");
    } else {
        out.push_str(token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_known_prefixes_and_cookies() {
        let text = "key sk-abc1234567890abcdef token SID=secret; SAPISID=alsosecret";
        let out = sanitize_error_text(text);
        assert!(out.contains("sk-<redacted>"));
        assert!(out.contains("SID=<redacted>"));
        assert!(out.contains("SAPISID=<redacted>"));
        assert!(!out.contains("alsosecret"));
    }

    #[test]
    fn redacts_long_mixed_tokens() {
        let out = sanitize_error_text("bad token abcdefghijklmnop1234567890 end");
        assert!(out.contains("<redacted>"));
        assert!(!out.contains("abcdefghijklmnop1234567890"));
    }

    #[test]
    fn redacts_named_params_and_bearer_tokens() {
        // URL/form params (short values the long-token heuristic would miss).
        let out = sanitize_error_text("GET /cb?access_token=short1&code=xyz refresh_token=r2");
        assert!(out.contains("access_token=<redacted>"), "{out}");
        assert!(out.contains("refresh_token=<redacted>"), "{out}");
        assert!(out.contains("code=xyz"), "non-secret params kept: {out}");

        // JSON error body.
        let out = sanitize_error_text(r#"{"client_secret":"s3","error":"bad"}"#);
        assert!(out.contains(r#""client_secret":"<redacted>""#), "{out}");
        assert!(out.contains(r#""error":"bad""#), "non-secret fields kept: {out}");

        // Authorization header.
        let out = sanitize_error_text("Authorization: Bearer ya29.abcDEF");
        assert!(out.contains("Bearer <redacted>"), "{out}");
        assert!(!out.contains("ya29.abcDEF"), "{out}");

        // Extra YouTube cookies.
        let out = sanitize_error_text("Cookie: LOGIN_INFO=deadbeef; VISITOR_INFO1_LIVE=abc");
        assert!(out.contains("LOGIN_INFO=<redacted>"), "{out}");
        assert!(out.contains("VISITOR_INFO1_LIVE=<redacted>"), "{out}");
    }

    #[test]
    fn keyed_redaction_does_not_mangle_ordinary_prose() {
        // A sensitive word without an assignment operator is left untouched.
        let out = sanitize_error_text("the password could not be verified");
        assert_eq!(out, "the password could not be verified");
    }

    #[test]
    fn truncating_long_non_ascii_text_never_panics_mid_codepoint() {
        // Multi-byte chars straddling the 1 KiB cap must not panic `String::truncate`.
        // "가" is 3 bytes; a long run guarantees a codepoint crosses the boundary.
        let out = sanitize_error_text("가".repeat(2000));
        assert!(out.ends_with("..."));
        assert!(out.len() <= MAX_ERROR_TEXT + 3); // "..." appended
        // Still valid UTF-8 (didn't split a codepoint).
        assert!(out.chars().all(|c| c == '가' || c == '.'));
    }
}
