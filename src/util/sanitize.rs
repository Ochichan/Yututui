//! Redaction for errors/log text that may contain paths or secrets.

const MAX_ERROR_TEXT: usize = 1024;
const SECRET_PREFIXES: &[&str] = &["ghp_", "github_pat_", "sk-", "npm_", "xox", "AKIA", "AIza"];
const COOKIE_KEYS: &[&str] = &["SID=", "SAPISID=", "APISID=", "HSID=", "SSID="];

pub fn sanitize_error_text(input: impl AsRef<str>) -> String {
    let mut out = input.as_ref().to_owned();
    if let Some(home) = home_dir_string()
        && home.len() > 1
    {
        out = out.replace(&home, "~");
    }
    out = redact_cookie_values(&out);
    out = redact_prefixed_tokens(&out);
    out = redact_long_tokens(&out);
    if out.len() > MAX_ERROR_TEXT {
        out.truncate(MAX_ERROR_TEXT);
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
}
