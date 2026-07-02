//! `ytt auth <service>` — one-shot terminal account connections. A thin router; each
//! service's flow lives with its feature (scrobbling, Spotify transfer). Runs before any
//! terminal/runtime setup, like `ytt daemon`/`ytt doctor`.

const EXIT_USAGE: i32 = 2;

const USAGE: &str = "\
Usage: ytt auth <service>

Services:
  lastfm                 Connect Last.fm scrobbling (opens the approval page)
  listenbrainz <token>   Save a ListenBrainz user token (from listenbrainz.org/settings)
  spotify [flags]        Connect Spotify for playlist transfer (see `ytt auth spotify --help`)";

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("lastfm") => crate::scrobble::auth_cli::run_lastfm(),
        Some("listenbrainz") => {
            crate::scrobble::auth_cli::run_listenbrainz(args.get(1).map(String::as_str))
        }
        Some("spotify") => crate::transfer::cli::run_auth(&args[1..]),
        Some("--help" | "-h") | None => {
            println!("{USAGE}");
            if args.is_empty() { EXIT_USAGE } else { 0 }
        }
        Some(other) => {
            eprintln!("ytt auth: unknown service `{other}`");
            eprintln!("{USAGE}");
            EXIT_USAGE
        }
    }
}
