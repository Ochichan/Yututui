//! One-shot `ytt server` commands.
//!
//! Credentials are read with terminal echo disabled. Human and JSON output deliberately omit
//! origins, custom-CA paths, usernames, and secrets.

use std::io::{self, Write as _};
use std::path::PathBuf;
use std::time::Duration;

use age::secrecy::SecretString;
use serde::Serialize;

use yututui::open_subsonic::{
    CredentialKind, NativeHistoryHealth, OpenSubsonicPaths, OpenSubsonicStatus,
    OpenSubsonicStatusKind, OutboundScrobbleResolution, PlaylistCreateAttention, ServerCredential,
    ServerFeature, SetupIdentityIntent, SetupInput, abandon_playlist_create_attention,
    commit_setup, commit_store_set, list_playlist_create_attention, list_scrobble_attention_ids,
    load_store_set, read_status, remove_profile, resolve_scrobble_attention,
    test_and_prepare_setup, test_connection_read_only,
};
use yututui::personal_state::PlaylistId;

const EXIT_OK: i32 = 0;
const EXIT_RUNTIME: i32 = 1;
const EXIT_USAGE: i32 = 2;
const MAX_CUSTOM_CA_BYTES: u64 = 192 * 1024;
const STATUS_PROBE_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_DISPLAY_NAME_BYTES: usize = 1_024;
const MAX_ORIGIN_BYTES: usize = 4_096;
const MAX_USERNAME_BYTES: usize = 1_024;
const MAX_PASSWORD_BYTES: usize = 64 * 1_024;
const MAX_API_KEY_BYTES: usize = 2_048;
const MAX_CA_PATH_BYTES: usize = 16 * 1_024;
const MAX_CHOICE_BYTES: usize = 64;
const OPAQUE_SCROBBLE_ID_PREFIX: &str = "sub-scrobble-";
const OPAQUE_SCROBBLE_DIGEST_BYTES: usize = 64;

const SERVER_USAGE: &str = "\
Usage: ytt server <command>

Connect one OpenSubsonic or Navidrome music server.

Commands:
  setup             Test and save a music server connection
  status [--json]   Show the redacted connection status
  history enable --experimental
                    Enable experimental detailed Navidrome history
  history disable   Disable detailed history and remove its saved password
  scrobbles list [--json]
                    List opaque playback reports needing a decision
  scrobbles retry <OPAQUE_ID>
                    Treat one report as unsent and retry it
  scrobbles mark-sent <OPAQUE_ID>
                    Treat one report as sent without retrying it
  playlists pending [--json]
                    List local IDs for playlist creates needing a decision
  playlists abandon <LOCAL_PLAYLIST_ID>
                    Forget one create guard without deleting either copy
  remove            Remove the profile and credentials; keep local personal data

Passwords and API keys are prompted with echo disabled and are never accepted as arguments.
Experimental detailed history is off by default and never disables standard server access.
";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Help,
    Setup,
    Status { json: bool },
    HistoryEnableExperimental,
    HistoryDisable,
    ScrobblesList { json: bool },
    ScrobbleRetry { event_id: String },
    ScrobbleMarkSent { event_id: String },
    PlaylistCreatesPending { json: bool },
    PlaylistCreateAbandon { local_playlist_id: String },
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryEnableAction {
    Enable,
    UpdateDedicatedPassword,
    AlreadyEnabled,
}

#[derive(Serialize)]
struct JsonStatus<'a> {
    status: &'static str,
    display_name: Option<&'a str>,
    backend_id: Option<&'a str>,
    account_scope_id: Option<&'a str>,
    credential: Option<&'static str>,
    lan_http_enabled: bool,
    custom_ca_configured: bool,
    detailed_history_enabled: bool,
    detailed_history_status: &'static str,
    playback_reports_needing_decision: usize,
    playlist_creates_needing_decision: usize,
    playlist_create_attention: Vec<JsonPlaylistCreateAttention<'a>>,
    playlist_links_needing_decision: usize,
    playlist_updates_needing_reconnect: usize,
    playlist_contents_needing_review: usize,
}

#[derive(Serialize)]
struct JsonScrobbleAttention<'a> {
    count: usize,
    opaque_ids: &'a [String],
}

#[derive(Serialize)]
struct JsonPlaylistCreateAttention<'a> {
    local_playlist_id: &'a str,
    state: &'static str,
}

#[derive(Serialize)]
struct JsonPlaylistCreateAttentionList<'a> {
    count: usize,
    pending: Vec<JsonPlaylistCreateAttention<'a>>,
}

pub fn run(args: &[String]) -> i32 {
    let command = match parse(args) {
        Ok(command) => command,
        Err(message) => return usage_error(&message),
    };
    let result = match command {
        Command::Help => {
            print!("{SERVER_USAGE}");
            return EXIT_OK;
        }
        Command::Setup => run_setup(),
        Command::Status { json } => run_status(json),
        Command::HistoryEnableExperimental => run_history_enable(),
        Command::HistoryDisable => run_history_disable(),
        Command::ScrobblesList { json } => run_scrobbles_list(json),
        Command::ScrobbleRetry { event_id } => {
            run_scrobble_resolution(&event_id, OutboundScrobbleResolution::Retry)
        }
        Command::ScrobbleMarkSent { event_id } => {
            run_scrobble_resolution(&event_id, OutboundScrobbleResolution::MarkSent)
        }
        Command::PlaylistCreatesPending { json } => run_playlist_creates_pending(json),
        Command::PlaylistCreateAbandon { local_playlist_id } => {
            run_playlist_create_abandon(&local_playlist_id)
        }
        Command::Remove => run_remove(),
    };
    match result {
        Ok(()) => EXIT_OK,
        Err(error) => {
            eprintln!("ytt server: {error}");
            EXIT_RUNTIME
        }
    }
}

fn parse(args: &[String]) -> Result<Command, String> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err("missing command".to_owned());
    };
    match command {
        "-h" | "--help" | "help" => no_args(&args[1..], Command::Help, "help"),
        "setup" => no_args(&args[1..], Command::Setup, "setup"),
        "remove" => no_args(&args[1..], Command::Remove, "remove"),
        "history" => parse_history(&args[1..]),
        "scrobbles" => parse_scrobbles(&args[1..]),
        "playlists" => parse_playlists(&args[1..]),
        "status" => match &args[1..] {
            [] => Ok(Command::Status { json: false }),
            [flag] if flag == "--json" => Ok(Command::Status { json: true }),
            [flag] if matches!(flag.as_str(), "-h" | "--help") => Ok(Command::Help),
            _ => Err("status only accepts `--json`".to_owned()),
        },
        other => Err(format!("unknown command `{other}`")),
    }
}

fn parse_playlists(args: &[String]) -> Result<Command, String> {
    match args {
        [action] if action == "pending" => Ok(Command::PlaylistCreatesPending { json: false }),
        [action, flag] if action == "pending" && flag == "--json" => {
            Ok(Command::PlaylistCreatesPending { json: true })
        }
        [action, local_playlist_id] if action == "abandon" => Ok(Command::PlaylistCreateAbandon {
            local_playlist_id: local_playlist_id.clone(),
        }),
        [flag] | [_, flag] if matches!(flag.as_str(), "-h" | "--help" | "help") => {
            Ok(Command::Help)
        }
        [] => Err("playlists requires `pending` or `abandon <LOCAL_PLAYLIST_ID>`".to_owned()),
        _ => {
            Err("playlists accepts `pending [--json]` or `abandon <LOCAL_PLAYLIST_ID>`".to_owned())
        }
    }
}

fn parse_scrobbles(args: &[String]) -> Result<Command, String> {
    match args {
        [action] if action == "list" => Ok(Command::ScrobblesList { json: false }),
        [action, flag] if action == "list" && flag == "--json" => {
            Ok(Command::ScrobblesList { json: true })
        }
        [action, event_id] if action == "retry" && valid_opaque_scrobble_id(event_id) => {
            Ok(Command::ScrobbleRetry {
                event_id: event_id.clone(),
            })
        }
        [action, event_id] if action == "mark-sent" && valid_opaque_scrobble_id(event_id) => {
            Ok(Command::ScrobbleMarkSent {
                event_id: event_id.clone(),
            })
        }
        [flag] | [_, flag] if matches!(flag.as_str(), "-h" | "--help" | "help") => {
            Ok(Command::Help)
        }
        [action, _] if matches!(action.as_str(), "retry" | "mark-sent") => {
            Err("scrobble recovery requires the exact opaque ID from `scrobbles list`".to_owned())
        }
        [] => Err("scrobbles requires `list`, `retry`, or `mark-sent`".to_owned()),
        _ => Err(
            "scrobbles accepts `list [--json]`, `retry <OPAQUE_ID>`, or `mark-sent <OPAQUE_ID>`"
                .to_owned(),
        ),
    }
}

fn valid_opaque_scrobble_id(value: &str) -> bool {
    value
        .strip_prefix(OPAQUE_SCROBBLE_ID_PREFIX)
        .is_some_and(|digest| {
            digest.len() == OPAQUE_SCROBBLE_DIGEST_BYTES
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
}

fn parse_history(args: &[String]) -> Result<Command, String> {
    match args {
        [action, flag] if action == "enable" && flag == "--experimental" => {
            Ok(Command::HistoryEnableExperimental)
        }
        [action] if action == "enable" => {
            Err("history enable requires the explicit `--experimental` flag".to_owned())
        }
        [action] if action == "disable" => Ok(Command::HistoryDisable),
        [flag] if matches!(flag.as_str(), "-h" | "--help" | "help") => Ok(Command::Help),
        [_, flag] if matches!(flag.as_str(), "-h" | "--help") => Ok(Command::Help),
        [] => Err("history requires `enable --experimental` or `disable`".to_owned()),
        _ => Err("history only accepts `enable --experimental` or `disable`".to_owned()),
    }
}

fn no_args(args: &[String], command: Command, label: &str) -> Result<Command, String> {
    if args.is_empty() {
        Ok(command)
    } else if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        Ok(Command::Help)
    } else {
        Err(format!("{label} does not accept arguments"))
    }
}

fn run_setup() -> Result<(), String> {
    initialize_writer()?;
    let paths = paths()?;
    let current = read_status(&paths).map_err(|error| error.to_string())?;
    let identity_intent = if current.kind == OpenSubsonicStatusKind::Off {
        SetupIdentityIntent::Create
    } else {
        prompt_identity_intent()?
    };
    let display_name = prompt_required("Server name: ", MAX_DISPLAY_NAME_BYTES)?;
    let origin = prompt_required(
        "Server address (scheme, host, and optional port): ",
        MAX_ORIGIN_BYTES,
    )?;
    let credential_kind = prompt_credential_kind()?;
    let credential = match credential_kind {
        CredentialKind::Password => {
            let username = prompt_required("Username: ", MAX_USERNAME_BYTES)?;
            let password = prompt_secret("Password: ", MAX_PASSWORD_BYTES)?;
            ServerCredential::password(username, SecretString::from(password))
                .map_err(|error| error.to_string())?
        }
        CredentialKind::ApiKey => {
            let key = prompt_secret("API key: ", MAX_API_KEY_BYTES)?;
            ServerCredential::api_key(SecretString::from(key)).map_err(|error| error.to_string())?
        }
    };
    let custom_ca_pem = prompt_custom_ca()?;
    let allow_lan_http =
        confirm("Allow plain HTTP for this exact private/loopback address? [y/N]: ")?;
    let setup = SetupInput::new(
        display_name,
        origin,
        allow_lan_http,
        custom_ca_pem,
        credential,
        identity_intent,
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "could not start the bounded connection test".to_owned())?;
    let prepared = runtime
        .block_on(test_and_prepare_setup(&paths, setup))
        .map_err(|error| error.to_string())?;
    let api_key_supported = prepared
        .capabilities()
        .supports(ServerFeature::ApiKeyAuthentication);
    let status = commit_setup(&paths, prepared).map_err(|error| error.to_string())?;
    set_server_search_enabled(true)?;
    let name = status.display_name.as_deref().unwrap_or("Music server");
    println!("{name} is connected.");
    if credential_kind == CredentialKind::Password && api_key_supported {
        println!("This server also advertises API-key authentication.");
    }
    if status.uses_lan_http {
        println!("LAN HTTP is enabled only for the exact configured address.");
    }
    Ok(())
}

fn run_status(json: bool) -> Result<(), String> {
    initialize_reader()?;
    let status = checked_status(&paths()?)?;
    if json {
        println!("{}", render_json_status(&status)?);
        return Ok(());
    }
    print_human_status(&status);
    Ok(())
}

fn run_scrobbles_list(json: bool) -> Result<(), String> {
    initialize_reader()?;
    let ids = list_scrobble_attention_ids(&paths()?).map_err(|error| error.to_string())?;
    if json {
        println!("{}", render_json_scrobble_list(&ids)?);
    } else {
        for line in human_scrobble_list_lines(&ids) {
            println!("{line}");
        }
    }
    Ok(())
}

fn run_scrobble_resolution(
    event_id: &str,
    resolution: OutboundScrobbleResolution,
) -> Result<(), String> {
    initialize_writer()?;
    let prompt = match resolution {
        OutboundScrobbleResolution::Retry => {
            "The server may already have this report. Retrying can count it twice. Retry? [y/N]: "
        }
        OutboundScrobbleResolution::MarkSent => {
            "Treat this report as already sent and stop retrying? Local history is kept. [y/N]: "
        }
    };
    if !confirm(prompt)? {
        println!("Playback report was not changed.");
        return Ok(());
    }
    resolve_scrobble_attention(&paths()?, event_id, resolution)
        .map_err(|error| error.to_string())?;
    match resolution {
        OutboundScrobbleResolution::Retry => {
            println!("Playback report will retry from a fresh server baseline.");
        }
        OutboundScrobbleResolution::MarkSent => {
            println!("Playback report was marked as sent; local history was kept.");
        }
    }
    Ok(())
}

fn run_playlist_creates_pending(json: bool) -> Result<(), String> {
    initialize_reader()?;
    let attention = list_playlist_create_attention(&paths()?).map_err(|error| error.to_string())?;
    if json {
        println!("{}", render_json_playlist_create_attention(&attention)?);
    } else {
        for line in human_playlist_create_attention_lines(&attention) {
            println!("{line}");
        }
    }
    Ok(())
}

fn run_playlist_create_abandon(local_playlist_id: &str) -> Result<(), String> {
    let local_playlist_id = PlaylistId::new(local_playlist_id.to_owned()).map_err(|_| {
        "playlist recovery requires the exact local ID from `playlists pending`".to_owned()
    })?;
    initialize_writer()?;
    if !confirm(
        "A server copy may already exist. Forget only the retry guard and leave both copies untouched? [y/N]: ",
    )? {
        println!("Pending playlist creation was not changed.");
        return Ok(());
    }
    abandon_playlist_create_attention(&paths()?, &local_playlist_id)
        .map_err(|error| error.to_string())?;
    println!("Pending playlist creation was forgotten; neither copy was deleted.");
    Ok(())
}

fn run_history_enable() -> Result<(), String> {
    initialize_writer()?;
    let paths = paths()?;
    let mut store_set = load_store_set(&paths)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "connect a music server before enabling detailed history".to_owned())?;
    let action = history_enable_action(
        store_set.private_state.native_history_enabled(),
        store_set.bridge_state.native_history_health(),
        store_set.private_state.credential_kind(),
    );
    if action == HistoryEnableAction::AlreadyEnabled {
        println!("Experimental detailed history is already enabled.");
        return Ok(());
    }
    let expected = store_set.revisions();
    match store_set.private_state.credential_kind() {
        CredentialKind::Password => store_set
            .private_state
            .enable_native_history_reusing_server_password()
            .map_err(|error| error.to_string())?,
        CredentialKind::ApiKey => {
            let username = prompt_required("Navidrome username: ", MAX_USERNAME_BYTES)?;
            let password = prompt_secret("Navidrome password: ", MAX_PASSWORD_BYTES)?;
            store_set
                .private_state
                .enable_native_history_with_password(username, SecretString::from(password))
                .map_err(|error| error.to_string())?;
        }
    }
    store_set
        .bridge_state
        .set_native_history_health(NativeHistoryHealth::Probing);
    commit_store_set(&paths, expected, &mut store_set).map_err(|error| error.to_string())?;
    println!("Experimental detailed history is enabled.");
    if action == HistoryEnableAction::UpdateDedicatedPassword {
        println!("The detailed-history password was updated.");
    }
    println!("Standard OpenSubsonic access stays available if detailed history is unsupported.");
    Ok(())
}

fn history_enable_action(
    enabled: bool,
    health: NativeHistoryHealth,
    credential: CredentialKind,
) -> HistoryEnableAction {
    if !enabled {
        return HistoryEnableAction::Enable;
    }
    if health == NativeHistoryHealth::UpdatePassword && credential == CredentialKind::ApiKey {
        HistoryEnableAction::UpdateDedicatedPassword
    } else {
        HistoryEnableAction::AlreadyEnabled
    }
}

fn run_history_disable() -> Result<(), String> {
    initialize_writer()?;
    let paths = paths()?;
    let mut store_set = load_store_set(&paths)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "no music server is connected".to_owned())?;
    if !store_set.private_state.native_history_enabled() {
        println!("Experimental detailed history is already off.");
        return Ok(());
    }
    let expected = store_set.revisions();
    store_set.private_state.disable_native_history();
    store_set
        .bridge_state
        .set_native_history_health(NativeHistoryHealth::Off);
    commit_store_set(&paths, expected, &mut store_set).map_err(|error| error.to_string())?;
    println!("Experimental detailed history is off; standard server access was kept.");
    Ok(())
}

fn checked_status(paths: &OpenSubsonicPaths) -> Result<OpenSubsonicStatus, String> {
    let mut stored = read_status(paths).map_err(|error| error.to_string())?;
    if stored.kind != OpenSubsonicStatusKind::UpToDate {
        return Ok(stored);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "could not start the bounded connection test".to_owned())?;
    match runtime.block_on(async {
        tokio::time::timeout(STATUS_PROBE_TIMEOUT, test_connection_read_only(paths)).await
    }) {
        Ok(Ok(live)) => Ok(live),
        Ok(Err(_)) | Err(_) => {
            stored.kind = OpenSubsonicStatusKind::NeedsAttention;
            Ok(stored)
        }
    }
}

fn render_json_status(status: &OpenSubsonicStatus) -> Result<String, String> {
    let playlist_create_attention =
        json_playlist_create_attention(&status.playlist_create_attention);
    let value = JsonStatus {
        status: status_label(status.kind),
        display_name: status.display_name.as_deref(),
        backend_id: status.backend_id.as_ref().map(|id| id.as_str()),
        account_scope_id: status.account_scope_id.as_ref().map(|id| id.as_str()),
        credential: status.credential_kind.map(credential_label),
        lan_http_enabled: status.uses_lan_http,
        custom_ca_configured: status.uses_custom_ca,
        detailed_history_enabled: status.native_history_enabled,
        detailed_history_status: history_status_label(status.native_history_health),
        playback_reports_needing_decision: status.outbound_scrobbles_needing_attention,
        playlist_creates_needing_decision: status.playlist_creates_needing_attention,
        playlist_create_attention,
        playlist_links_needing_decision: status.playlist_links_needing_decision,
        playlist_updates_needing_reconnect: status.playlist_projections_needing_attention,
        playlist_contents_needing_review: status.playlist_contents_needing_attention,
    };
    serde_json::to_string_pretty(&value).map_err(|_| "could not encode status JSON".to_owned())
}

fn render_json_playlist_create_attention(
    attention: &[PlaylistCreateAttention],
) -> Result<String, String> {
    serde_json::to_string_pretty(&JsonPlaylistCreateAttentionList {
        count: attention.len(),
        pending: json_playlist_create_attention(attention),
    })
    .map_err(|_| "could not encode playlist-create JSON".to_owned())
}

fn json_playlist_create_attention(
    attention: &[PlaylistCreateAttention],
) -> Vec<JsonPlaylistCreateAttention<'_>> {
    attention
        .iter()
        .map(|pending| JsonPlaylistCreateAttention {
            local_playlist_id: pending.local_playlist_id.as_str(),
            state: pending.state.label(),
        })
        .collect()
}

fn render_json_scrobble_list(ids: &[String]) -> Result<String, String> {
    serde_json::to_string_pretty(&JsonScrobbleAttention {
        count: ids.len(),
        opaque_ids: ids,
    })
    .map_err(|_| "could not encode playback-report JSON".to_owned())
}

fn human_scrobble_list_lines(ids: &[String]) -> Vec<String> {
    if ids.is_empty() {
        return vec!["No playback reports need a decision.".to_owned()];
    }
    let mut lines = Vec::with_capacity(ids.len().saturating_add(2));
    lines.push(playback_report_attention_summary(ids.len()));
    lines.extend(ids.iter().cloned());
    lines.push("Choose `retry <OPAQUE_ID>` or `mark-sent <OPAQUE_ID>` for each report.".to_owned());
    lines
}

fn human_playlist_create_attention_lines(attention: &[PlaylistCreateAttention]) -> Vec<String> {
    if attention.is_empty() {
        return vec!["No server playlist creations need a decision.".to_owned()];
    }
    let mut lines = Vec::with_capacity(attention.len().saturating_add(2));
    lines.push(playlist_create_attention_summary(attention.len()));
    lines.extend(attention.iter().map(|pending| {
        format!(
            "{}  {}",
            pending.local_playlist_id.as_str(),
            pending.state.label()
        )
    }));
    lines.push(
        "Use `ytt server playlists abandon <LOCAL_PLAYLIST_ID>` only after checking the server."
            .to_owned(),
    );
    lines
}

fn run_remove() -> Result<(), String> {
    initialize_writer()?;
    if !confirm("Remove the music server profile and saved credential? [y/N]: ")? {
        println!("Music server was not removed.");
        return Ok(());
    }
    remove_profile(&paths()?).map_err(|error| error.to_string())?;
    set_server_search_enabled(false)?;
    println!("Music server removed; local personal data was kept.");
    Ok(())
}

fn print_human_status(status: &OpenSubsonicStatus) {
    match status.kind {
        OpenSubsonicStatusKind::Off => println!("Off"),
        OpenSubsonicStatusKind::UpToDate => {
            println!("Up to date");
            if let Some(name) = status.display_name.as_deref() {
                println!("Server: {name}");
            }
            if let Some(kind) = status.credential_kind {
                println!("Credential: {}", credential_label(kind));
            }
            if status.uses_lan_http {
                println!("LAN HTTP: enabled for the exact configured address");
            }
            if status.uses_custom_ca {
                println!("Custom CA: configured");
            }
            println!(
                "History: {}",
                history_status_human(status.native_history_health)
            );
        }
        OpenSubsonicStatusKind::NeedsAttention => {
            println!("Needs attention");
            let mut showed_recovery = false;
            if status.outbound_scrobbles_needing_attention > 0 {
                showed_recovery = true;
                println!(
                    "{}",
                    playback_report_attention_summary(status.outbound_scrobbles_needing_attention)
                );
                println!("{}", playback_report_attention_action());
            }
            if status.playlist_creates_needing_attention > 0 {
                showed_recovery = true;
                println!(
                    "{}",
                    playlist_create_attention_summary(status.playlist_creates_needing_attention)
                );
                for pending in &status.playlist_create_attention {
                    println!(
                        "{}  {}",
                        pending.local_playlist_id.as_str(),
                        pending.state.label()
                    );
                }
                println!("Action: review them with `ytt server playlists pending`.");
            }
            if status.playlist_links_needing_decision > 0 {
                showed_recovery = true;
                println!(
                    "{}",
                    playlist_link_attention_summary(status.playlist_links_needing_decision)
                );
                println!("{}", playlist_link_attention_action());
            }
            if status.playlist_projections_needing_attention > 0 {
                showed_recovery = true;
                println!(
                    "{} playlist update(s) need a successful reconnect before retrying.",
                    status.playlist_projections_needing_attention
                );
                println!("Action: update and test the connection settings.");
            }
            if status.playlist_contents_needing_attention > 0 {
                showed_recovery = true;
                println!(
                    "{} linked playlist(s) contain tracks from outside this server account.",
                    status.playlist_contents_needing_attention
                );
                println!("Action: review those local playlists in Server Library.");
            }
            if !showed_recovery {
                println!("Action: update the connection settings");
            }
            println!(
                "History: {}",
                history_status_human(status.native_history_health)
            );
        }
    }
}

fn playlist_create_attention_summary(count: usize) -> String {
    if count == 1 {
        "1 server playlist creation needs a decision.".to_owned()
    } else {
        format!("{count} server playlist creations need a decision.")
    }
}

fn playlist_link_attention_summary(count: usize) -> String {
    if count == 1 {
        "1 linked server playlist is missing and needs a decision.".to_owned()
    } else {
        format!("{count} linked server playlists are missing and need a decision.")
    }
}

fn playlist_link_attention_action() -> &'static str {
    "Action: open Server Library and choose Restore, Unlink, or Delete local too."
}

fn playback_report_attention_summary(count: usize) -> String {
    if count == 1 {
        "1 playback report needs a decision.".to_owned()
    } else {
        format!("{count} playback reports need a decision.")
    }
}

fn playback_report_attention_action() -> &'static str {
    "Action: review them with `ytt server scrobbles list`."
}

fn status_label(kind: OpenSubsonicStatusKind) -> &'static str {
    match kind {
        OpenSubsonicStatusKind::Off => "off",
        OpenSubsonicStatusKind::UpToDate => "up_to_date",
        OpenSubsonicStatusKind::NeedsAttention => "needs_attention",
    }
}

fn history_status_label(health: NativeHistoryHealth) -> &'static str {
    match health {
        NativeHistoryHealth::Off => "off",
        NativeHistoryHealth::Probing => "probing",
        NativeHistoryHealth::Detailed => "detailed",
        NativeHistoryHealth::PlayCountsOnly => "play_counts_only",
        NativeHistoryHealth::UpdatePassword => "update_password",
    }
}

fn history_status_human(health: NativeHistoryHealth) -> &'static str {
    match health {
        NativeHistoryHealth::Off => "play counts only (detailed history off)",
        NativeHistoryHealth::Probing => "checking detailed history; play counts remain available",
        NativeHistoryHealth::Detailed => "detailed history available (experimental)",
        NativeHistoryHealth::PlayCountsOnly => "play counts only (detailed history unsupported)",
        NativeHistoryHealth::UpdatePassword => {
            "update the detailed-history password; play counts remain available"
        }
    }
}

fn credential_label(kind: CredentialKind) -> &'static str {
    match kind {
        CredentialKind::ApiKey => "api_key",
        CredentialKind::Password => "password",
    }
}

fn initialize_reader() -> Result<(), String> {
    yututui::persist::initialize_persistence_reader()
        .map(|_| ())
        .map_err(|_| "could not open a coherent local snapshot".to_owned())
}

fn initialize_writer() -> Result<(), String> {
    yututui::persist::initialize_persistence_writer(false)
        .map_err(|error| writer_initialization_error(&error).to_owned())?;
    yututui::persist::preflight_all_startup_stores()
        .map_err(|_| "local recovery must be completed before changing the server".to_owned())
}

fn writer_initialization_error(error: &io::Error) -> &'static str {
    if error.kind() == io::ErrorKind::WouldBlock {
        "another ytt process owns settings; use its controls or close it and retry"
    } else {
        "could not secure the settings writer"
    }
}

fn paths() -> Result<OpenSubsonicPaths, String> {
    OpenSubsonicPaths::current().map_err(|_| "the data directory is unavailable".to_owned())
}

fn prompt_line(prompt: &str, max_bytes: usize) -> Result<String, String> {
    print!("{prompt}");
    io::stdout()
        .flush()
        .map_err(|_| "could not write the prompt".to_owned())?;
    read_bounded_line(&mut io::stdin().lock(), max_bytes).map_err(|error| match error.kind() {
        io::ErrorKind::InvalidData => "terminal input exceeds its byte limit".to_owned(),
        _ => "could not read terminal input".to_owned(),
    })
}

fn read_bounded_line(reader: &mut impl io::BufRead, max_bytes: usize) -> io::Result<String> {
    let mut value = Vec::with_capacity(max_bytes.min(4 * 1024));
    let mut overflow = false;
    loop {
        let (consumed, finished) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                break;
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            let content = &available[..newline.unwrap_or(available.len())];
            let remaining = max_bytes.saturating_sub(value.len());
            if content.len() > remaining {
                overflow = true;
                value.extend_from_slice(&content[..remaining]);
            } else {
                value.extend_from_slice(content);
            }
            (consumed, newline.is_some())
        };
        reader.consume(consumed);
        if finished {
            break;
        }
    }
    if overflow {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "input exceeds its byte limit",
        ));
    }
    if value.last() == Some(&b'\r') {
        value.pop();
    }
    String::from_utf8(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "input is not UTF-8"))
}

fn prompt_required(prompt: &str, max_bytes: usize) -> Result<String, String> {
    let value = prompt_line(prompt, max_bytes)?;
    if value.trim().is_empty() {
        Err("a required value was left blank".to_owned())
    } else {
        Ok(value.trim().to_owned())
    }
}

fn prompt_secret(prompt: &str, max_bytes: usize) -> Result<String, String> {
    let mut value = zeroize::Zeroizing::new(
        rpassword::prompt_password(prompt).map_err(|_| "could not read the secret".to_owned())?,
    );
    if value.is_empty() {
        Err("the credential cannot be blank".to_owned())
    } else if value.len() > max_bytes {
        Err("the credential exceeds its byte limit".to_owned())
    } else {
        Ok(std::mem::take(&mut *value))
    }
}

fn prompt_credential_kind() -> Result<CredentialKind, String> {
    let value = prompt_required("Credential type [password/api-key]: ", MAX_CHOICE_BYTES)?;
    match value.trim().to_ascii_lowercase().as_str() {
        "password" | "pass" | "p" => Ok(CredentialKind::Password),
        "api-key" | "api_key" | "apikey" | "key" | "k" => Ok(CredentialKind::ApiKey),
        _ => Err("credential type must be `password` or `api-key`".to_owned()),
    }
}

fn prompt_identity_intent() -> Result<SetupIdentityIntent, String> {
    println!("Existing connection:");
    println!("  1  Same server and account — keep existing song links");
    println!("  2  Different server or account — start a separate identity");
    match prompt_required("Choose 1 or 2: ", MAX_CHOICE_BYTES)?.as_str() {
        "1" => Ok(SetupIdentityIntent::UpdateSameServerAndAccount),
        "2" => Ok(SetupIdentityIntent::ReplaceServerOrAccount),
        _ => Err("connection identity must be explicitly selected as `1` or `2`".to_owned()),
    }
}

fn set_server_search_enabled(enabled: bool) -> Result<(), String> {
    let mut config = yututui::config::Config::load();
    config
        .search
        .set_enabled(yututui::search_source::SearchSource::OpenSubsonic, enabled);
    config.search = config.search.normalized();
    config
        .save()
        .map_err(|_| "music server was changed but search settings could not be saved".to_owned())
}

fn prompt_custom_ca() -> Result<Option<Vec<u8>>, String> {
    let raw = prompt_line(
        "Custom CA file (leave blank to use system trust): ",
        MAX_CA_PATH_BYTES,
    )?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let path = std::path::absolute(PathBuf::from(raw.trim()))
        .map_err(|_| "could not resolve the CA file".to_owned())?;
    let bytes = yututui::util::safe_fs::read_no_symlink_limited(&path, MAX_CUSTOM_CA_BYTES)
        .map_err(|_| {
            "the CA file must be a readable regular non-symlink file within the size limit"
                .to_owned()
        })?;
    if bytes.is_empty() {
        return Err("the CA file is empty".to_owned());
    }
    Ok(Some(bytes))
}

fn confirm(prompt: &str) -> Result<bool, String> {
    Ok(is_affirmative_confirmation(&prompt_line(
        prompt,
        MAX_CHOICE_BYTES,
    )?))
}

fn is_affirmative_confirmation(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn usage_error(message: &str) -> i32 {
    eprintln!("ytt server: {message}");
    eprintln!("Try `ytt server --help`.");
    EXIT_USAGE
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use age::secrecy::SecretString;

    use super::*;
    use yututui::open_subsonic::{
        ConfiguredPrivateOrigin, OpenSubsonicBridgeState, OpenSubsonicPrivateState,
        OpenSubsonicProfile, OpenSubsonicStoreSet, PlaylistCreateRecoveryState, StoreRevisions,
        commit_store_set,
    };

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn opaque_scrobble_id(digit: char) -> String {
        format!(
            "{OPAQUE_SCROBBLE_ID_PREFIX}{}",
            digit.to_string().repeat(64)
        )
    }

    #[test]
    fn parses_public_commands_without_secret_arguments() {
        assert_eq!(parse(&args(&["setup"])), Ok(Command::Setup));
        assert_eq!(
            parse(&args(&["status", "--json"])),
            Ok(Command::Status { json: true })
        );
        assert_eq!(
            parse(&args(&["history", "enable", "--experimental"])),
            Ok(Command::HistoryEnableExperimental)
        );
        assert_eq!(
            parse(&args(&["history", "disable"])),
            Ok(Command::HistoryDisable)
        );
        assert_eq!(parse(&args(&["remove"])), Ok(Command::Remove));
    }

    #[test]
    fn rejects_setup_and_remove_arguments() {
        assert!(parse(&args(&["setup", "https://secret.example"])).is_err());
        assert!(parse(&args(&["remove", "--force"])).is_err());
    }

    #[test]
    fn history_requires_explicit_opt_in_and_never_accepts_a_password_argument() {
        assert!(parse(&args(&["history", "enable"])).is_err());
        assert!(parse(&args(&["history", "--experimental"])).is_err());
        assert!(
            parse(&args(&[
                "history",
                "enable",
                "--experimental",
                "password-sentinel"
            ]))
            .is_err()
        );
        assert!(parse(&args(&["history", "disable", "--force"])).is_err());
        assert!(SERVER_USAGE.contains("history enable --experimental"));
        assert!(SERVER_USAGE.contains("off by default"));
        assert!(!SERVER_USAGE.contains("password-sentinel"));
    }

    #[test]
    fn enabled_api_key_history_prompts_again_only_when_password_needs_update() {
        assert_eq!(
            history_enable_action(
                true,
                NativeHistoryHealth::UpdatePassword,
                CredentialKind::ApiKey
            ),
            HistoryEnableAction::UpdateDedicatedPassword
        );
        assert_eq!(
            history_enable_action(true, NativeHistoryHealth::Detailed, CredentialKind::ApiKey),
            HistoryEnableAction::AlreadyEnabled
        );
        assert_eq!(
            history_enable_action(
                true,
                NativeHistoryHealth::UpdatePassword,
                CredentialKind::Password
            ),
            HistoryEnableAction::AlreadyEnabled,
            "the main server password is updated through server setup"
        );
    }

    #[test]
    fn status_only_accepts_json() {
        assert!(parse(&args(&["status", "--verbose"])).is_err());
        assert_eq!(parse(&args(&["status", "-h"])), Ok(Command::Help));
    }

    #[test]
    fn scrobble_recovery_parser_requires_an_exact_opaque_id() {
        let retry_id = opaque_scrobble_id('a');
        let mark_sent_id = opaque_scrobble_id('9');
        assert_eq!(
            parse(&args(&["scrobbles", "list"])),
            Ok(Command::ScrobblesList { json: false })
        );
        assert_eq!(
            parse(&args(&["scrobbles", "list", "--json"])),
            Ok(Command::ScrobblesList { json: true })
        );
        assert_eq!(
            parse(&["scrobbles".to_owned(), "retry".to_owned(), retry_id.clone()]),
            Ok(Command::ScrobbleRetry { event_id: retry_id })
        );
        assert_eq!(
            parse(&[
                "scrobbles".to_owned(),
                "mark-sent".to_owned(),
                mark_sent_id.clone(),
            ]),
            Ok(Command::ScrobbleMarkSent {
                event_id: mark_sent_id
            })
        );

        for invalid in [
            "sub-scrobble-short",
            "sub-scrobble-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "https://secret.example/report",
            "password-sentinel",
        ] {
            assert!(parse(&args(&["scrobbles", "retry", invalid])).is_err());
            assert!(parse(&args(&["scrobbles", "mark-sent", invalid])).is_err());
        }
        assert!(
            parse(&[
                "scrobbles".to_owned(),
                "retry".to_owned(),
                opaque_scrobble_id('b'),
                "credential-sentinel".to_owned(),
            ])
            .is_err()
        );
        assert_eq!(
            parse(&args(&["scrobbles", "retry", "--help"])),
            Ok(Command::Help)
        );
        assert!(SERVER_USAGE.contains("scrobbles list [--json]"));
        assert!(SERVER_USAGE.contains("scrobbles retry <OPAQUE_ID>"));
        assert!(SERVER_USAGE.contains("scrobbles mark-sent <OPAQUE_ID>"));
        assert!(!SERVER_USAGE.contains("credential-sentinel"));
    }

    #[test]
    fn scrobble_recovery_output_contains_only_counts_and_opaque_ids() {
        let ids = vec![opaque_scrobble_id('a'), opaque_scrobble_id('b')];
        let json = render_json_scrobble_list(&ids).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 2);
        assert_eq!(
            object.get("count").and_then(serde_json::Value::as_u64),
            Some(2)
        );
        assert_eq!(
            object
                .get("opaque_ids")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(2)
        );
        assert!(!json.contains("http"));
        assert!(!json.contains("secret"));
        assert!(!json.contains("path"));

        let human = human_scrobble_list_lines(&ids);
        assert_eq!(human[0], "2 playback reports need a decision.");
        assert_eq!(&human[1..=2], ids.as_slice());
        assert_eq!(
            human[3],
            "Choose `retry <OPAQUE_ID>` or `mark-sent <OPAQUE_ID>` for each report."
        );
        assert_eq!(
            human_scrobble_list_lines(&[]),
            vec!["No playback reports need a decision."]
        );
    }

    #[test]
    fn playlist_create_recovery_parser_and_output_use_only_local_ids() {
        assert_eq!(
            parse(&args(&["playlists", "pending"])),
            Ok(Command::PlaylistCreatesPending { json: false })
        );
        assert_eq!(
            parse(&args(&["playlists", "pending", "--json"])),
            Ok(Command::PlaylistCreatesPending { json: true })
        );
        assert_eq!(
            parse(&args(&["playlists", "abandon", "local-opaque"])),
            Ok(Command::PlaylistCreateAbandon {
                local_playlist_id: "local-opaque".to_owned(),
            })
        );
        assert!(parse(&args(&["playlists", "abandon"])).is_err());
        assert!(parse(&args(&["playlists", "pending", "--verbose"])).is_err());
        assert!(SERVER_USAGE.contains("playlists pending [--json]"));
        assert!(SERVER_USAGE.contains("playlists abandon <LOCAL_PLAYLIST_ID>"));

        let attention = vec![
            PlaylistCreateAttention {
                local_playlist_id: PlaylistId::new("local-a").unwrap(),
                state: PlaylistCreateRecoveryState::ServerIdentityUnknown,
            },
            PlaylistCreateAttention {
                local_playlist_id: PlaylistId::new("local-b").unwrap(),
                state: PlaylistCreateRecoveryState::ReadbackNeeded,
            },
        ];
        let json = render_json_playlist_create_attention(&attention).unwrap();
        assert!(json.contains(r#""local_playlist_id": "local-a""#));
        assert!(json.contains(r#""state": "server_identity_unknown""#));
        assert!(json.contains(r#""state": "readback_needed""#));
        assert!(!json.contains("server-known"));
        assert!(!json.contains("Private name"));

        let human = human_playlist_create_attention_lines(&attention);
        assert_eq!(human[0], "2 server playlist creations need a decision.");
        assert_eq!(human[1], "local-a  server_identity_unknown");
        assert_eq!(human[2], "local-b  readback_needed");
        assert_eq!(
            human_playlist_create_attention_lines(&[]),
            vec!["No server playlist creations need a decision."]
        );
    }

    #[test]
    fn destructive_scrobble_choices_default_to_no() {
        for rejected in ["", "n", "no", "later", "1"] {
            assert!(!is_affirmative_confirmation(rejected));
        }
        for accepted in ["y", "Y", "yes", " YES "] {
            assert!(is_affirmative_confirmation(accepted));
        }
    }

    #[test]
    fn scrobble_mutation_reports_owner_rejection_without_error_details() {
        let blocked = io::Error::new(io::ErrorKind::WouldBlock, "owner-secret-sentinel");
        assert_eq!(
            writer_initialization_error(&blocked),
            "another ytt process owns settings; use its controls or close it and retry"
        );
        assert!(!writer_initialization_error(&blocked).contains("sentinel"));

        let other = io::Error::new(io::ErrorKind::PermissionDenied, "path-secret-sentinel");
        assert_eq!(
            writer_initialization_error(&other),
            "could not secure the settings writer"
        );
        assert!(!writer_initialization_error(&other).contains("sentinel"));
    }

    #[test]
    fn status_labels_are_stable() {
        assert_eq!(status_label(OpenSubsonicStatusKind::Off), "off");
        assert_eq!(credential_label(CredentialKind::ApiKey), "api_key");
        assert_eq!(
            history_status_label(NativeHistoryHealth::UpdatePassword),
            "update_password"
        );
    }

    #[test]
    fn bounded_line_reader_drains_oversize_input_and_counts_utf8_bytes() {
        let mut reader = Cursor::new(b"toolong\nok\n".to_vec());
        assert_eq!(
            read_bounded_line(&mut reader, 3).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(read_bounded_line(&mut reader, 3).unwrap(), "ok");

        let mut exact = Cursor::new("é\n".as_bytes());
        assert_eq!(read_bounded_line(&mut exact, 2).unwrap(), "é");
        let mut split = Cursor::new("é\n".as_bytes());
        assert_eq!(
            read_bounded_line(&mut split, 1).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn offline_saved_profile_is_redacted_needs_attention_in_json() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let root = std::env::temp_dir().join(format!(
            "yututui-server-cli-status-{}-{port}",
            std::process::id()
        ));
        yututui::persist::initialize_persistence_writer_for_roots([&root], false).unwrap();
        yututui::util::safe_fs::ensure_private_dir(&root).unwrap();
        let paths = OpenSubsonicPaths::for_data_root(root.clone());
        let profile = OpenSubsonicProfile::new(
            "Offline fixture",
            ConfiguredPrivateOrigin::new(&format!("https://127.0.0.1:{port}/"), false).unwrap(),
            None,
        )
        .unwrap();
        let private_state = OpenSubsonicPrivateState::new(
            profile.backend_id().clone(),
            profile.account_scope_id().clone(),
            ServerCredential::api_key(SecretString::from("json-secret-sentinel".to_owned()))
                .unwrap(),
        );
        let bridge_state = OpenSubsonicBridgeState::new(
            profile.backend_id().clone(),
            profile.account_scope_id().clone(),
        );
        let mut store_set =
            OpenSubsonicStoreSet::new(profile, private_state, bridge_state).unwrap();
        commit_store_set(&paths, StoreRevisions::MISSING, &mut store_set).unwrap();

        let status = checked_status(&paths).unwrap();
        assert_eq!(status.kind, OpenSubsonicStatusKind::NeedsAttention);
        let json = render_json_status(&status).unwrap();
        assert!(json.contains(r#""status": "needs_attention""#));
        assert!(json.contains(r#""detailed_history_status": "off""#));
        assert!(json.contains(r#""playback_reports_needing_decision": 0"#));
        assert!(json.contains(r#""playlist_creates_needing_decision": 0"#));
        assert!(json.contains(r#""playlist_links_needing_decision": 0"#));
        assert!(json.contains(r#""playlist_contents_needing_review": 0"#));
        assert!(json.contains("Offline fixture"));
        assert!(!json.contains("json-secret-sentinel"));
        assert!(!json.contains("https://"));

        let mut playback_attention = status;
        playback_attention.outbound_scrobbles_needing_attention = 2;
        let json = render_json_status(&playback_attention).unwrap();
        assert!(json.contains(r#""playback_reports_needing_decision": 2"#));
        assert_eq!(
            playback_report_attention_summary(1),
            "1 playback report needs a decision."
        );
        assert_eq!(
            playback_report_attention_summary(2),
            "2 playback reports need a decision."
        );
        assert_eq!(
            playback_report_attention_action(),
            "Action: review them with `ytt server scrobbles list`."
        );
        playback_attention.playlist_creates_needing_attention = 1;
        playback_attention.playlist_create_attention = vec![PlaylistCreateAttention {
            local_playlist_id: PlaylistId::new("local-status").unwrap(),
            state: PlaylistCreateRecoveryState::ReadbackNeeded,
        }];
        playback_attention.playlist_links_needing_decision = 1;
        playback_attention.playlist_projections_needing_attention = 2;
        playback_attention.playlist_contents_needing_attention = 3;
        let json = render_json_status(&playback_attention).unwrap();
        assert!(json.contains(r#""playlist_creates_needing_decision": 1"#));
        assert!(json.contains(r#""local_playlist_id": "local-status""#));
        assert!(json.contains(r#""playlist_links_needing_decision": 1"#));
        assert!(json.contains(r#""playlist_updates_needing_reconnect": 2"#));
        assert!(json.contains(r#""playlist_contents_needing_review": 3"#));
        assert_eq!(
            playlist_create_attention_summary(1),
            "1 server playlist creation needs a decision."
        );
        assert_eq!(
            playlist_link_attention_summary(1),
            "1 linked server playlist is missing and needs a decision."
        );
        assert_eq!(
            playlist_link_attention_summary(2),
            "2 linked server playlists are missing and need a decision."
        );
        assert_eq!(
            playlist_link_attention_action(),
            "Action: open Server Library and choose Restore, Unlink, or Delete local too."
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
