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
    CredentialKind, OpenSubsonicPaths, OpenSubsonicStatus, OpenSubsonicStatusKind,
    ServerCredential, ServerFeature, SetupIdentityIntent, SetupInput, commit_setup, read_status,
    remove_profile, test_and_prepare_setup, test_connection_read_only,
};

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

const SERVER_USAGE: &str = "\
Usage: ytt server <command>

Connect one OpenSubsonic or Navidrome music server.

Commands:
  setup             Test and save a music server connection
  status [--json]   Show the redacted connection status
  remove            Remove the profile and credentials; keep local personal data

Passwords and API keys are prompted with echo disabled and are never accepted as arguments.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Help,
    Setup,
    Status { json: bool },
    Remove,
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
        "status" => match &args[1..] {
            [] => Ok(Command::Status { json: false }),
            [flag] if flag == "--json" => Ok(Command::Status { json: true }),
            [flag] if matches!(flag.as_str(), "-h" | "--help") => Ok(Command::Help),
            _ => Err("status only accepts `--json`".to_owned()),
        },
        other => Err(format!("unknown command `{other}`")),
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
    let value = JsonStatus {
        status: status_label(status.kind),
        display_name: status.display_name.as_deref(),
        backend_id: status.backend_id.as_ref().map(|id| id.as_str()),
        account_scope_id: status.account_scope_id.as_ref().map(|id| id.as_str()),
        credential: status.credential_kind.map(credential_label),
        lan_http_enabled: status.uses_lan_http,
        custom_ca_configured: status.uses_custom_ca,
    };
    serde_json::to_string_pretty(&value).map_err(|_| "could not encode status JSON".to_owned())
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
        }
        OpenSubsonicStatusKind::NeedsAttention => {
            println!("Needs attention");
            println!("Action: update the connection settings");
        }
    }
}

fn status_label(kind: OpenSubsonicStatusKind) -> &'static str {
    match kind {
        OpenSubsonicStatusKind::Off => "off",
        OpenSubsonicStatusKind::UpToDate => "up_to_date",
        OpenSubsonicStatusKind::NeedsAttention => "needs_attention",
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
    match yututui::persist::initialize_persistence_writer(false) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return Err(
                "another ytt process owns settings; use its controls or close it and retry"
                    .to_owned(),
            );
        }
        Err(_) => return Err("could not secure the settings writer".to_owned()),
    }
    yututui::persist::preflight_all_startup_stores()
        .map_err(|_| "local recovery must be completed before changing the server".to_owned())
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
    Ok(matches!(
        prompt_line(prompt, MAX_CHOICE_BYTES)?
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "y" | "yes"
    ))
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
        OpenSubsonicProfile, OpenSubsonicStoreSet, StoreRevisions, commit_store_set,
    };

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_public_commands_without_secret_arguments() {
        assert_eq!(parse(&args(&["setup"])), Ok(Command::Setup));
        assert_eq!(
            parse(&args(&["status", "--json"])),
            Ok(Command::Status { json: true })
        );
        assert_eq!(parse(&args(&["remove"])), Ok(Command::Remove));
    }

    #[test]
    fn rejects_setup_and_remove_arguments() {
        assert!(parse(&args(&["setup", "https://secret.example"])).is_err());
        assert!(parse(&args(&["remove", "--force"])).is_err());
    }

    #[test]
    fn status_only_accepts_json() {
        assert!(parse(&args(&["status", "--verbose"])).is_err());
        assert_eq!(parse(&args(&["status", "-h"])), Ok(Command::Help));
    }

    #[test]
    fn status_labels_are_stable() {
        assert_eq!(status_label(OpenSubsonicStatusKind::Off), "off");
        assert_eq!(credential_label(CredentialKind::ApiKey), "api_key");
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
        assert!(json.contains("Offline fixture"));
        assert!(!json.contains("json-secret-sentinel"));
        assert!(!json.contains("https://"));
        let _ = std::fs::remove_dir_all(root);
    }
}
