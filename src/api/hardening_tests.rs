use super::*;

#[test]
fn metadata_sanitizer_handles_deterministic_fuzz_corpus() {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..512 {
        let mut raw = String::new();
        for _ in 0..420 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            raw.push(match state % 12 {
                0 => '\n',
                1 => '\r',
                2 => '\t',
                3 => '\u{202e}',
                4 => '\u{2066}',
                5 => '\u{200d}',
                6 => '한',
                7 => 'é',
                _ => char::from_u32(0x20 + (state % 0x5f) as u32).unwrap(),
            });
        }
        let clean = sanitize_metadata_text(&raw, 37);
        assert!(clean.chars().count() <= 37);
        assert!(!clean.chars().any(char::is_control));
        assert!(!clean.chars().any(is_forbidden_metadata_char));
    }
}

#[test]
fn playable_url_validator_handles_deterministic_fuzz_corpus() {
    let schemes = ["http", "https", "file", "smb", "ftp", "data"];
    let hosts = [
        "example.com",
        "LOCALHOST",
        "127.0.0.1",
        "10.1.2.3",
        "169.254.169.254",
        "[::1]",
        "public.example",
    ];
    let mut state = 0xa409_3822_299f_31d0u64;
    for _ in 0..512 {
        state = state
            .wrapping_mul(2862933555777941757)
            .wrapping_add(3037000493);
        let scheme = schemes[(state as usize) % schemes.len()];
        state = state.rotate_left(17);
        let host = hosts[(state as usize) % hosts.len()];
        let auth = if state & 0x40 != 0 { "user:pass@" } else { "" };
        let control = if state & 0x80 != 0 { "\n" } else { "" };
        let raw = format!("{scheme}://{auth}{host}/audio-{state:x}{control}.mp3");

        if let Ok(clean) = validate_playable_url(SearchSource::RadioBrowser, &raw) {
            let parsed = reqwest::Url::parse(&clean).expect("accepted URL parses");
            assert!(matches!(parsed.scheme(), "http" | "https"));
            assert!(parsed.username().is_empty());
            assert!(parsed.password().is_none());
            assert!(!clean.chars().any(char::is_control));
        }
    }
}
