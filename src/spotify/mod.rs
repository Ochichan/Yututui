//! Spotify Web API: OAuth PKCE auth ([`auth`]), a minimal hand-rolled client
//! ([`client`]), and the response models ([`models`]).
//!
//! Hand-rolled rather than rspotify: we need ~10 endpoints, all plain JSON with offset
//! pagination, and the house rules (single native-TLS stack, no `Debug` on secret-bearing
//! types, `json_limited` body caps, `safe_fs` 0600 atomic writes, `sanitize_error_text`
//! logging) are exactly the things a general-purpose SDK brings its own opinions about.

pub mod auth;
pub mod client;
pub mod models;

pub const API_BASE: &str = "https://api.spotify.com/v1";
pub const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
pub const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
/// Everything the transfer flows need: read own playlists (private/collab included) and
/// liked songs, create/modify playlists for export.
pub const SCOPES: &str = "playlist-read-private playlist-read-collaborative user-library-read playlist-modify-private playlist-modify-public";
/// Response-body cap, matching the `src/util/http.rs` norm.
pub const BODY_MAX: usize = 2 * 1024 * 1024;
