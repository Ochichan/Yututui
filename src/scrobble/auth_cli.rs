//! `ytt auth lastfm` — the one-shot terminal connection flow, for daemon/headless
//! setups where the Settings screen isn't around. Mirrors the TUI flow: request a
//! token, open the approval page, poll `auth.getSession`, persist the session key.

use std::time::{Duration, Instant};

use super::lastfm::{LastfmClient, SessionPoll};
use super::service::ScrobbleError;
use crate::config::Config;

const EXIT_OK: i32 = 0;
const EXIT_FAILED: i32 = 1;
const AUTH_POLL: Duration = Duration::from_secs(5);
const AUTH_BUDGET: Duration = Duration::from_secs(300);

pub fn run_lastfm() -> i32 {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ytt auth lastfm: could not start runtime: {e}");
            return EXIT_FAILED;
        }
    };
    rt.block_on(connect_lastfm())
}

/// `ytt auth listenbrainz <token>` — validate the token against the configured (or
/// default) instance, then persist it.
pub fn run_listenbrainz(token: Option<&str>) -> i32 {
    let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) else {
        eprintln!("ytt auth listenbrainz: missing token.");
        eprintln!("Copy it from https://listenbrainz.org/settings/ and run:");
        eprintln!("  ytt auth listenbrainz <token>");
        return EXIT_FAILED;
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ytt auth listenbrainz: could not start runtime: {e}");
            return EXIT_FAILED;
        }
    };
    rt.block_on(async {
        let mut cfg = Config::load();
        let api_url = cfg
            .scrobble
            .listenbrainz
            .api_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| super::listenbrainz::DEFAULT_API_URL.to_owned());
        match super::listenbrainz::validate_token(&api_url, token).await {
            Ok(username) => {
                cfg.scrobble.listenbrainz.token = Some(token.to_owned());
                if let Err(e) = cfg.save() {
                    eprintln!("ytt auth listenbrainz: token valid, but saving config failed: {e}");
                    return EXIT_FAILED;
                }
                match username {
                    Some(name) => println!("Connected as {name}. Listens will be submitted."),
                    None => println!("Token accepted. Listens will be submitted."),
                }
                println!(
                    "If the YuTuTui! daemon is running, restart it (`ytt daemon stop`, then `ytt daemon start`) to pick this up."
                );
                EXIT_OK
            }
            Err(e) => {
                eprintln!("ytt auth listenbrainz: {e}");
                EXIT_FAILED
            }
        }
    })
}

async fn connect_lastfm() -> i32 {
    let mut cfg = Config::load();
    let Some(app) = cfg.scrobble_settings().lastfm_app else {
        eprintln!("ytt auth lastfm: no Last.fm application credentials available.");
        eprintln!("This build ships none embedded — create an API account at");
        eprintln!("https://www.last.fm/api/account/create and put the key + shared secret");
        eprintln!("into config.json under scrobble.lastfm.api_key / api_secret.");
        return EXIT_FAILED;
    };
    let http = reqwest::Client::builder()
        .user_agent(format!("yututui/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_default();
    let client = LastfmClient::new(http, app.api_key, app.api_secret, None);

    let token = match client.get_token().await {
        Ok(token) => token,
        Err(e) => {
            eprintln!("ytt auth lastfm: {e}");
            return EXIT_FAILED;
        }
    };
    let url = client.auth_url(&token);
    println!("Approve YuTuTui! in your browser:");
    println!();
    println!("  {url}");
    println!();
    let opened = crate::util::browser::open_in_browser_checked(&url);
    if !opened.launched() {
        eprintln!(
            "ytt auth lastfm: could not open a browser automatically: {}",
            opened.failure_summary()
        );
        eprintln!("ytt auth lastfm: paste the URL above into your browser to continue.");
    }
    println!("Waiting for approval (up to 5 minutes; Ctrl-C to abort)…");

    let deadline = Instant::now() + AUTH_BUDGET;
    loop {
        tokio::time::sleep(AUTH_POLL).await;
        if Instant::now() >= deadline {
            eprintln!("ytt auth lastfm: authorization timed out — run it again.");
            return EXIT_FAILED;
        }
        match client.get_session(&token).await {
            Ok(SessionPoll::Pending) => {}
            Ok(SessionPoll::Granted { key, username }) => {
                cfg.scrobble.lastfm.session_key = Some(key);
                cfg.scrobble.lastfm.username = Some(username.clone());
                if let Err(e) = cfg.save() {
                    eprintln!("ytt auth lastfm: connected, but saving config failed: {e}");
                    return EXIT_FAILED;
                }
                println!("Connected as {username}. Scrobbling is on.");
                println!(
                    "If the YuTuTui! daemon is running, restart it (`ytt daemon stop`, then `ytt daemon start`) to pick this up."
                );
                return EXIT_OK;
            }
            // Transient trouble mid-poll: keep waiting out the budget.
            Err(ScrobbleError::Network(_) | ScrobbleError::RateLimited(_)) => {}
            Err(e) => {
                eprintln!("ytt auth lastfm: {e}");
                return EXIT_FAILED;
            }
        }
    }
}
