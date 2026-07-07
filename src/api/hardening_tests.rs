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

#[test]
fn playable_url_validator_accepts_public_http_urls() {
    let url = validate_playable_url(SearchSource::Jamendo, " HTTPS://Example.com/audio.mp3 ")
        .expect("public https URL should be accepted");
    assert_eq!(url, "https://example.com/audio.mp3");
}

#[test]
fn playable_url_validator_rejects_unsafe_schemes_and_hosts() {
    for raw in [
        "file:///tmp/song.mp3",
        "smb://example.com/share/song.mp3",
        "ftp://example.com/song.mp3",
        "data:text/plain,hello",
        "javascript:alert(1)",
        "https://user:pass@example.com/song.mp3",
        "http://localhost/song.mp3",
        "http://127.0.0.1/song.mp3",
        "http://10.0.0.4/song.mp3",
        "http://169.254.169.254/latest/meta-data",
        "http://[::1]/song.mp3",
        "http://[fd00::1]/song.mp3",
        "https://example.com/bad\npath",
    ] {
        assert!(
            validate_playable_url(SearchSource::RadioBrowser, raw).is_err(),
            "{raw} should be rejected"
        );
    }
}

#[test]
fn playable_url_validator_reports_specific_failure_classes() {
    assert_eq!(
        validate_playable_url(SearchSource::Jamendo, "   ").unwrap_err(),
        PlayableUrlError::Empty
    );
    assert_eq!(
        validate_playable_url(
            SearchSource::Jamendo,
            &"x".repeat(MAX_PLAYABLE_URL_BYTES + 1)
        )
        .unwrap_err(),
        PlayableUrlError::TooLong {
            max: MAX_PLAYABLE_URL_BYTES
        }
    );
    assert_eq!(
        validate_playable_url(SearchSource::Jamendo, "https://user@example.com/a").unwrap_err(),
        PlayableUrlError::Credentials
    );
    assert_eq!(
        validate_playable_url(SearchSource::Jamendo, "https://service.localhost/a").unwrap_err(),
        PlayableUrlError::Localhost
    );
    assert_eq!(
        validate_playable_url(SearchSource::Jamendo, "http://0.0.0.0/a").unwrap_err(),
        PlayableUrlError::BlockedIp("0.0.0.0".to_owned())
    );
    assert_eq!(
        validate_playable_url(SearchSource::Jamendo, "mailto:test@example.com").unwrap_err(),
        PlayableUrlError::UnsupportedScheme("mailto".to_owned())
    );
}

#[test]
fn playable_url_error_display_is_specific_for_every_failure_class() {
    let cases = [
        (PlayableUrlError::Empty, "playable URL is empty"),
        (
            PlayableUrlError::TooLong { max: 8192 },
            "playable URL exceeds 8192 bytes",
        ),
        (
            PlayableUrlError::ControlCharacter,
            "playable URL contains a control character",
        ),
        (
            PlayableUrlError::Invalid("relative URL without a base".to_owned()),
            "invalid playable URL: relative URL without a base",
        ),
        (
            PlayableUrlError::UnsupportedScheme("file".to_owned()),
            "unsupported playable URL scheme: file",
        ),
        (
            PlayableUrlError::MissingHost,
            "playable URL is missing a host",
        ),
        (
            PlayableUrlError::Credentials,
            "playable URL must not include credentials",
        ),
        (
            PlayableUrlError::Localhost,
            "playable URL host is local-only",
        ),
        (
            PlayableUrlError::BlockedIp("127.0.0.1".to_owned()),
            "playable URL host is not public: 127.0.0.1",
        ),
        (
            PlayableUrlError::DnsResolution {
                host: "stream.example".to_owned(),
            },
            "playable URL host did not resolve: stream.example",
        ),
        (
            PlayableUrlError::DestinationBlockedIp {
                host: "stream.example".to_owned(),
                ip: "10.0.0.1".to_owned(),
            },
            "playable URL host resolved to a non-public address: stream.example -> 10.0.0.1",
        ),
        (
            PlayableUrlError::RedirectLimit { max: 5 },
            "playable URL exceeded 5 redirects",
        ),
        (
            PlayableUrlError::RedirectMissingLocation,
            "playable URL redirect is missing a Location header",
        ),
        (
            PlayableUrlError::RedirectInvalid("bad URL".to_owned()),
            "invalid playable URL redirect target: bad URL",
        ),
        (
            PlayableUrlError::ProbeFailed("timeout".to_owned()),
            "playable URL destination probe failed: timeout",
        ),
    ];

    for (error, expected) in cases {
        assert_eq!(error.to_string(), expected);
    }
}
