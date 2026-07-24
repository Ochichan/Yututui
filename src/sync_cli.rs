//! One-shot `ytt sync` commands.
//!
//! Credentials are read from the terminal with echo disabled and are never accepted as command
//! arguments. Human and JSON status output intentionally excludes endpoints, paths, and secrets.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use age::secrecy::SecretString;
use zeroize::Zeroize;

use yututui::personal_state::{DeviceId, PersonalStatePaths};
use yututui::remote::{self, WEB_DAV_SYNC_CAPABILITY};
use yututui::sync::service::{self, PairingJoinPreview, SyncServiceError};
use yututui::sync::{
    MAX_CUSTOM_CA_PEM_BYTES, PrivateStore, RecoveryKit, SyncAuditAction, SyncAuditEntry,
    SyncAuditOutcome, SyncAuditStore, SyncPaths, VaultCredential,
};

const EXIT_OK: i32 = 0;
const EXIT_RUNTIME: i32 = 1;
const EXIT_USAGE: i32 = 2;
const RECOVERY_KIT_MAX_BYTES: u64 = 64 * 1024;

const SYNC_USAGE: &str = "\
Usage: ytt sync <command>

Encrypted, bidirectional personal-state synchronization over WebDAV.

Commands:
  setup                         Create a vault and save a required recovery kit
  status [--json]               Show the five-state sync status
  now                           Merge local and remote personal state now
  pair create                   Create a ten-minute device connection code
  pair join <CODE>              Join after approval and preview the first merge
  pair join --resume            Resume an interrupted join without re-entering the code
  pair cancel                   Discard an unfinished, unapproved local join
  devices [--json]              List active and removed devices
  revoke <DEVICE_ID>            Remove a device and rotate the encrypted checkpoint
  recovery export --to <DIR>    Verify and copy an existing recovery kit
  audit [--json]                Show the redacted sync audit log

WebDAV credentials are prompted with echo disabled. State stored on WebDAV is always encrypted.
";

#[derive(PartialEq, Eq)]
enum Command {
    Help,
    Setup,
    Status { json: bool },
    Now,
    PairCreate,
    PairJoin { code: Option<String> },
    PairCancel,
    Devices { json: bool },
    Revoke { device_id: String },
    RecoveryExport { directory: String },
    Audit { json: bool },
}

impl std::fmt::Debug for Command {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Help => formatter.write_str("Help"),
            Self::Setup => formatter.write_str("Setup"),
            Self::Status { json } => formatter
                .debug_struct("Status")
                .field("json", json)
                .finish(),
            Self::Now => formatter.write_str("Now"),
            Self::PairCreate => formatter.write_str("PairCreate"),
            Self::PairJoin { .. } => formatter.write_str("PairJoin { code: [redacted] }"),
            Self::PairCancel => formatter.write_str("PairCancel"),
            Self::Devices { json } => formatter
                .debug_struct("Devices")
                .field("json", json)
                .finish(),
            Self::Revoke { device_id } => formatter
                .debug_struct("Revoke")
                .field("device_id", device_id)
                .finish(),
            Self::RecoveryExport { .. } => {
                formatter.write_str("RecoveryExport { directory: [redacted] }")
            }
            Self::Audit { json } => formatter.debug_struct("Audit").field("json", json).finish(),
        }
    }
}

pub fn run(args: &[String]) -> i32 {
    let command = match parse(args) {
        Ok(command) => command,
        Err(message) => return usage_error(&message),
    };
    let result = match command {
        Command::Help => {
            print!("{SYNC_USAGE}");
            return EXIT_OK;
        }
        Command::Setup => run_setup(),
        Command::Status { json } => run_status(json),
        Command::Now => run_now(),
        Command::PairCreate => run_pair_create(),
        Command::PairJoin { mut code } => {
            let result = run_pair_join(code.as_deref());
            code.zeroize();
            result
        }
        Command::PairCancel => run_pair_cancel(),
        Command::Devices { json } => run_devices(json),
        Command::Revoke { device_id } => run_revoke(&device_id),
        Command::RecoveryExport { directory } => run_recovery_export(&directory),
        Command::Audit { json } => run_audit(json),
    };
    match result {
        Ok(()) => EXIT_OK,
        Err(error) => {
            eprintln!("ytt sync: {error}");
            EXIT_RUNTIME
        }
    }
}

fn parse(args: &[String]) -> Result<Command, String> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err("missing command".to_owned());
    };
    if matches!(command, "-h" | "--help" | "help") {
        return Ok(Command::Help);
    }
    match command {
        "setup" => no_args(&args[1..], Command::Setup, "setup"),
        "status" => json_flag(&args[1..]).map(|json| Command::Status { json }),
        "now" => no_args(&args[1..], Command::Now, "now"),
        "devices" => json_flag(&args[1..]).map(|json| Command::Devices { json }),
        "audit" => json_flag(&args[1..]).map(|json| Command::Audit { json }),
        "revoke" => exactly_one(&args[1..], "revoke requires one DEVICE_ID")
            .map(|device_id| Command::Revoke { device_id }),
        "pair" => parse_pair(&args[1..]),
        "recovery" => parse_recovery(&args[1..]),
        other => Err(format!("unknown command `{other}`")),
    }
}

fn parse_pair(args: &[String]) -> Result<Command, String> {
    match args.first().map(String::as_str) {
        Some("create") => no_args(&args[1..], Command::PairCreate, "pair create"),
        Some("join") => match &args[1..] {
            [flag] if flag == "--resume" => Ok(Command::PairJoin { code: None }),
            values => exactly_one(values, "pair join requires one CODE or `--resume`")
                .map(|code| Command::PairJoin { code: Some(code) }),
        },
        Some("cancel") => no_args(&args[1..], Command::PairCancel, "pair cancel"),
        Some("-h" | "--help") => Ok(Command::Help),
        Some(other) => Err(format!("unknown pair command `{other}`")),
        None => Err("pair requires `create` or `join <CODE>`".to_owned()),
    }
}

fn parse_recovery(args: &[String]) -> Result<Command, String> {
    if args
        .first()
        .is_some_and(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        return Ok(Command::Help);
    }
    if args.first().map(String::as_str) != Some("export") {
        return Err("recovery requires `export --to <DIR>`".to_owned());
    }
    let tail = &args[1..];
    let directory = match tail {
        [flag, directory] if flag == "--to" && !directory.is_empty() => directory.clone(),
        [assignment] => assignment
            .strip_prefix("--to=")
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| "recovery export requires `--to <DIR>`".to_owned())?,
        _ => return Err("recovery export requires exactly one `--to <DIR>`".to_owned()),
    };
    Ok(Command::RecoveryExport { directory })
}

fn no_args(args: &[String], command: Command, label: &str) -> Result<Command, String> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        return Ok(Command::Help);
    }
    if args.is_empty() {
        Ok(command)
    } else {
        Err(format!("{label} does not accept arguments"))
    }
}

fn json_flag(args: &[String]) -> Result<bool, String> {
    match args {
        [] => Ok(false),
        [flag] if flag == "--json" => Ok(true),
        [flag] if matches!(flag.as_str(), "-h" | "--help") => Err("help".to_owned()),
        _ => Err("only `--json` is accepted".to_owned()),
    }
}

fn exactly_one(args: &[String], message: &str) -> Result<String, String> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        return Err("help".to_owned());
    }
    match args {
        [value] if !value.is_empty() && !value.starts_with('-') => Ok(value.clone()),
        _ => Err(message.to_owned()),
    }
}

fn run_status(json: bool) -> Result<(), String> {
    let status = if let Some(owner) = find_sync_owner()? {
        owner.read_sync_status()?
    } else {
        initialize_reader()?;
        service::read_status(&sync_paths()?).map_err(service_error)?
    };
    print_status(status, json)
}

fn print_status(
    status: yututui::sync::service::SyncStatusReport,
    json: bool,
) -> Result<(), String> {
    if json {
        print_json(&status)
    } else {
        println!("{}", terminal_safe_text(&status.label));
        if let Some(action) = status.recovery_action {
            println!("Next: {}", terminal_safe_text(&action));
        }
        Ok(())
    }
}

fn run_devices(json: bool) -> Result<(), String> {
    initialize_reader()?;
    let devices = load_personal_state_read_only()?
        .as_ref()
        .map(service::read_devices)
        .unwrap_or_default();
    if json {
        return print_json(&devices);
    }
    if devices.is_empty() {
        println!("No sync devices.");
        return Ok(());
    }
    for device in devices {
        let state = if device.active { "Active" } else { "Removed" };
        println!(
            "{}  {}  {state}",
            device.device_id,
            terminal_safe_text(&device.name)
        );
    }
    Ok(())
}

fn run_audit(json: bool) -> Result<(), String> {
    initialize_reader()?;
    let entries =
        service::read_audit(&sync_paths()?, yututui::signals::unix_now()).map_err(service_error)?;
    if json {
        return print_json(&entries);
    }
    if entries.is_empty() {
        println!("No sync activity yet.");
        return Ok(());
    }
    for entry in entries.iter().rev() {
        println!("{}  {}", entry.at_unix, entry.summary());
    }
    Ok(())
}

fn run_now() -> Result<(), String> {
    if let Some(owner) = find_sync_owner()? {
        return owner.send(remote::proto::RemoteCommand::SyncNow);
    }
    initialize_writer()?;
    let snapshot = service::load_local_snapshot().map_err(service_error)?;
    let applied = service::sync_now(
        &snapshot.state,
        snapshot.playlist_revision,
        &personal_paths()?,
        &sync_paths()?,
    )
    .map_err(service_error)?;
    print_sync_summary(&applied.summary);
    Ok(())
}

struct SyncOwner {
    runtime: tokio::runtime::Runtime,
    instance: remote::proto::InstanceFile,
}

impl SyncOwner {
    fn request(
        &self,
        command: remote::proto::RemoteCommand,
    ) -> Result<remote::proto::RemoteResponse, String> {
        self.runtime
            .block_on(remote::client::send_to(self.instance.clone(), command))
            .map_err(|error| terminal_safe_text(&error.human_message()))
    }

    fn send(self, command: remote::proto::RemoteCommand) -> Result<(), String> {
        let response = self.request(command)?;
        let message = response.message.or(response.reason).unwrap_or_else(|| {
            if response.ok {
                "Sync completed.".to_owned()
            } else {
                "The running ytt instance rejected the sync request.".to_owned()
            }
        });
        let message = terminal_safe_text(&yututui::util::sanitize::sanitize_error_text(message));
        if response.ok {
            println!("{message}");
            Ok(())
        } else {
            Err(message)
        }
    }

    fn read_sync_status(self) -> Result<service::SyncStatusReport, String> {
        let response = self.request(remote::proto::RemoteCommand::Status)?;
        if !response.ok {
            let message = response
                .message
                .or(response.reason)
                .unwrap_or_else(|| "the running ytt instance rejected status".to_owned());
            return Err(terminal_safe_text(
                &yututui::util::sanitize::sanitize_error_text(message),
            ));
        }
        response
            .status
            .and_then(|status| status.personal_sync)
            .ok_or_else(|| "the running ytt instance did not provide sync status".to_owned())
    }
}

fn find_sync_owner() -> Result<Option<SyncOwner>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "could not start the local sync client".to_owned())?;
    let instance = runtime
        .block_on(remote::client::instance_with_capability(
            WEB_DAV_SYNC_CAPABILITY,
        ))
        .map_err(|error| terminal_safe_text(&error.human_message()))?;
    Ok(instance.map(|instance| SyncOwner { runtime, instance }))
}

fn run_setup() -> Result<(), String> {
    initialize_writer()?;
    let endpoint = prompt_required("WebDAV endpoint (HTTPS, or loopback HTTP): ")?;
    let custom_ca_pem = prompt_custom_ca()?;
    let username = prompt_line("Username (leave blank for a bearer token): ")?;
    let secret = prompt_secret(if username.is_empty() {
        "Bearer token: "
    } else {
        "Password: "
    })?;
    let credential = if username.is_empty() {
        VaultCredential::bearer_token(SecretString::from(secret))
    } else {
        VaultCredential::password(username, SecretString::from(secret))
    }
    .map_err(|_| "the WebDAV credential is invalid".to_owned())?;
    let device_name = prompt_device_name()?;
    let default_directory = default_recovery_directory()?;
    let prompt = format!(
        "Recovery kit directory [{}]: ",
        terminal_safe_path(&default_directory)?
    );
    let entered = prompt_line(&prompt)?;
    let directory = if entered.trim().is_empty() {
        default_directory
    } else {
        resolve_existing_directory(entered.trim())?
    };
    let recovery_file = unused_recovery_path(&directory);
    println!(
        "The recovery kit will be saved and read back before sync is enabled. \
         WebDAV credentials are not included."
    );
    if !confirm("Create the encrypted vault now? [y/N]: ")? {
        println!("Setup cancelled; no sync state was changed.");
        return Ok(());
    }
    let snapshot = service::load_local_snapshot().map_err(service_error)?;
    let result = service::setup(
        &snapshot.state,
        snapshot.playlist_revision,
        &personal_paths()?,
        &sync_paths()?,
        service::SetupRequest {
            endpoint,
            custom_ca_pem,
            device_name,
            credential,
            recovery_file: recovery_file.clone(),
        },
    )
    .map_err(service_error)?;
    println!(
        "Encrypted personal sync is ready for device {}.",
        result.device_id.as_str()
    );
    if result.resumed {
        println!(
            "The recovery kit was already saved and verified (checksum {}).",
            result.recovery_checksum
        );
    } else {
        println!(
            "Recovery kit saved to {} (checksum {}).",
            terminal_safe_path(&recovery_file)?,
            result.recovery_checksum
        );
    }
    print_sync_summary(&result.summary);
    Ok(())
}

fn run_pair_create() -> Result<(), String> {
    initialize_writer()?;
    let snapshot = service::load_local_snapshot().map_err(service_error)?;
    let paths = sync_paths()?;
    let now = yututui::signals::unix_now();
    let mut host =
        service::create_pairing_invite(&snapshot.state, &paths, now).map_err(service_error)?;
    if host.resumed() {
        println!("Resuming the pending device connection.");
    }
    println!("Device connection code: {}", host.code());
    println!("This one-time code expires in ten minutes. Waiting for the new device…");
    loop {
        match service::poll_pairing_request(
            &snapshot.state,
            &paths,
            &mut host,
            yututui::signals::unix_now(),
        ) {
            Ok(Some(review)) => {
                println!(
                    "New device: {} ({})\nFingerprint: {}",
                    terminal_safe_text(&review.device_name),
                    review.device_id,
                    review.fingerprint
                );
                if !confirm("Approve this device? [y/N]: ")? {
                    println!("Device was not approved.");
                    return Ok(());
                }
                service::approve_pairing_request(
                    &snapshot.state,
                    snapshot.playlist_revision,
                    &personal_paths()?,
                    &paths,
                    &mut host,
                    review,
                    yututui::signals::unix_now(),
                )
                .map_err(service_error)?;
                println!("Device approved.");
                return Ok(());
            }
            Ok(None) => thread::sleep(Duration::from_secs(1)),
            Err(error) => return Err(service_error(error)),
        }
    }
}

fn run_pair_join(code: Option<&str>) -> Result<(), String> {
    initialize_writer()?;
    let snapshot = service::load_local_snapshot().map_err(service_error)?;
    let paths = sync_paths()?;
    let preview = if let Some(code) = code {
        let endpoint = prompt_required("WebDAV endpoint: ")?;
        let custom_ca_pem = prompt_custom_ca()?;
        let username = prompt_line("Username (leave blank for a bearer token): ")?;
        let secret = prompt_secret(if username.is_empty() {
            "Bearer token: "
        } else {
            "Password: "
        })?;
        let credential = if username.is_empty() {
            VaultCredential::bearer_token(SecretString::from(secret))
        } else {
            VaultCredential::password(username, SecretString::from(secret))
        }
        .map_err(|_| "the WebDAV credential is invalid".to_owned())?;
        let device_name = prompt_device_name()?;
        println!("Waiting for approval on an existing device (up to ten minutes)…");
        service::begin_pairing_join(
            &snapshot.state,
            &paths,
            endpoint,
            custom_ca_pem,
            credential,
            code,
            device_name,
            yututui::signals::unix_now(),
        )
        .map_err(pair_join_error)?
    } else {
        println!("Resuming the pending device connection…");
        service::resume_pairing_join(&snapshot.state, &paths).map_err(pair_join_error)?
    };
    print_join_summary(&preview);
    if !confirm("Complete this deletion-free merge? [y/N]: ")? {
        service::defer_pairing_join(&paths, &preview).map_err(service_error)?;
        println!(
            "Not now. Approval and device keys were kept; run `ytt sync pair join --resume` later."
        );
        return Ok(());
    }
    service::apply_pairing_join(
        &snapshot.state,
        snapshot.playlist_revision,
        &personal_paths()?,
        &paths,
        preview,
    )
    .map_err(service_error)?;
    println!("This device is connected and the first merge is complete.");
    Ok(())
}

fn run_pair_cancel() -> Result<(), String> {
    initialize_writer()?;
    println!(
        "If another device already approved this attempt, remove its old device entry there \
         before connecting again."
    );
    if !confirm("Discard this unfinished device connection? [y/N]: ")? {
        println!("The unfinished device connection was kept.");
        return Ok(());
    }
    service::cancel_pairing_join(&sync_paths()?).map_err(service_error)?;
    println!("Local pending credentials and device keys were removed.");
    Ok(())
}

fn pair_join_error(error: SyncServiceError) -> String {
    match error {
        SyncServiceError::PendingApproval => {
            "approval is not available yet; retry `ytt sync pair join --resume`, or use \
             `ytt sync pair cancel` to discard this local attempt"
                .to_owned()
        }
        SyncServiceError::PairingExpired => format!(
            "{error}; run `ytt sync pair join --resume` to recover a published approval, or \
             `ytt sync pair cancel` to discard the unfinished local attempt"
        ),
        SyncServiceError::PairingNeedsCleanup => format!(
            "{error}; retry with the original code if available, or run \
             `ytt sync pair cancel` to discard the unfinished local attempt"
        ),
        _ => service_error(error),
    }
}

fn run_revoke(raw_device_id: &str) -> Result<(), String> {
    let target =
        DeviceId::new(raw_device_id.to_owned()).map_err(|_| "invalid device ID".to_owned())?;
    if let Some(owner) = find_sync_owner()? {
        initialize_reader()?;
        let state = load_personal_state_read_only()?
            .ok_or_else(|| "that active device was not found".to_owned())?;
        if !confirm_revoke(&state, &target)? {
            return Ok(());
        }
        return owner.send(remote::proto::RemoteCommand::SyncRevokeDevice {
            device_id: target.as_str().to_owned(),
        });
    }
    initialize_writer()?;
    let snapshot = service::load_local_snapshot().map_err(service_error)?;
    if !confirm_revoke(&snapshot.state, &target)? {
        return Ok(());
    }
    let applied = service::revoke_device_now(
        &snapshot.state,
        snapshot.playlist_revision,
        &target,
        &personal_paths()?,
        &sync_paths()?,
    )
    .map_err(service_error)?;
    println!("Device removed. Change the shared WebDAV password if that device knew it.");
    print_sync_summary(&applied.summary);
    Ok(())
}

fn confirm_revoke(
    state: &yututui::personal_state::PersonalStateV2,
    target: &DeviceId,
) -> Result<bool, String> {
    let device = state
        .device_registry
        .get(target)
        .filter(|device| !device.revoked)
        .ok_or_else(|| "that active device was not found".to_owned())?;
    println!(
        "Remove device: {} ({})",
        terminal_safe_text(&device.name),
        device.device_id.as_str()
    );
    println!(
        "It will lose access to future encrypted checkpoints. Previously downloaded data cannot \
         be erased remotely."
    );
    if !confirm("Remove this device? [y/N]: ")? {
        println!("Device removal cancelled.");
        return Ok(false);
    }
    Ok(true)
}

fn run_recovery_export(raw_directory: &str) -> Result<(), String> {
    initialize_writer()?;
    let directory = resolve_existing_directory(raw_directory)?;
    let source = prompt_required("Existing recovery kit file: ")?;
    let source = resolve_input_file(&source, "could not resolve the recovery kit")?;
    let mut bytes =
        yututui::util::safe_fs::read_no_symlink_limited(&source, RECOVERY_KIT_MAX_BYTES)
            .map_err(|_| "could not safely read the recovery kit".to_owned())?;
    if bytes.is_empty() {
        return Err("the recovery kit is invalid".to_owned());
    }
    let kit = RecoveryKit::from_json(&bytes).map_err(|_| "the recovery kit is invalid".to_owned());
    bytes.zeroize();
    let kit = kit?;
    let snapshot = service::load_local_snapshot().map_err(service_error)?;
    if kit.dataset_id() != snapshot.state.dataset_id {
        return Err("the recovery kit belongs to a different personal-sync vault".to_owned());
    }
    let paths = sync_paths()?;
    let private = PrivateStore::new(paths.private_store())
        .and_then(|store| store.load())
        .map_err(|error| service_error(error.into()))?;
    let verifying_key = kit
        .recovery_verifying_key()
        .map_err(|_| "the recovery kit is invalid".to_owned())?;
    if private.recovery_recipient() != Some(kit.recovery_recipient().as_str())
        || private.recovery_verifying_key() != Some(verifying_key.as_str())
    {
        return Err(
            "the recovery kit does not match this vault's trusted recovery identity".to_owned(),
        );
    }
    let destination = unused_recovery_path(&directory);
    let checksum = kit
        .export_confirmed(&destination)
        .map_err(|_| "could not save and verify the recovery kit".to_owned())?;
    let now = yututui::signals::unix_now();
    let audit = SyncAuditEntry::new(
        now,
        SyncAuditAction::RecoveryExport,
        SyncAuditOutcome::Succeeded,
    )
    .and_then(|entry| {
        SyncAuditStore::new(paths.audit()).and_then(|store| store.append(now, entry).map(|_| ()))
    });
    if let Err(error) = audit {
        tracing::warn!(%error, "recovery kit was saved but its audit entry could not be retained");
    }
    println!(
        "Recovery kit saved to {} (checksum {}).",
        terminal_safe_path(&destination)?,
        checksum
    );
    Ok(())
}

fn initialize_reader() -> Result<(), String> {
    yututui::persist::initialize_persistence_reader()
        .map(|_| ())
        .map_err(|_| "could not open a coherent personal-state snapshot".to_owned())
}

fn initialize_writer() -> Result<(), String> {
    match yututui::persist::initialize_persistence_writer(false) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return Err(
                "another ytt process owns personal state; use its sync controls or close it and \
                 retry"
                    .to_owned(),
            );
        }
        Err(_) => return Err("could not secure the personal-state writer".to_owned()),
    }
    yututui::persist::preflight_all_startup_stores()
        .map_err(|_| "personal-state recovery must be completed before syncing".to_owned())
}

fn personal_paths() -> Result<PersonalStatePaths, String> {
    PersonalStatePaths::current().map_err(|_| "the data directory is unavailable".to_owned())
}

fn load_personal_state_read_only()
-> Result<Option<yututui::personal_state::PersonalStateV2>, String> {
    service::load_personal_state_read_only(&personal_paths()?).map_err(service_error)
}

fn sync_paths() -> Result<SyncPaths, String> {
    SyncPaths::current().map_err(|_| "the data directory is unavailable".to_owned())
}

fn prompt_line(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    io::stdout()
        .flush()
        .map_err(|_| "could not write the prompt".to_owned())?;
    let mut value = String::new();
    io::stdin()
        .read_line(&mut value)
        .map_err(|_| "could not read terminal input".to_owned())?;
    Ok(value.trim_end_matches(['\r', '\n']).to_owned())
}

fn prompt_required(prompt: &str) -> Result<String, String> {
    let value = prompt_line(prompt)?;
    if value.trim().is_empty() {
        Err("a required value was left blank".to_owned())
    } else {
        Ok(value.trim().to_owned())
    }
}

fn prompt_device_name() -> Result<String, String> {
    let name = prompt_required("Device name: ")?;
    if name.chars().any(is_terminal_unsafe_character) {
        Err("device names cannot contain terminal control or bidirectional characters".to_owned())
    } else {
        Ok(name)
    }
}

fn prompt_custom_ca() -> Result<Option<Vec<u8>>, String> {
    let raw = prompt_line("Custom CA file (leave blank to use system trust): ")?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let path = std::path::absolute(expand_tilde(raw.trim())?)
        .map_err(|_| "could not resolve the CA file".to_owned())?;
    let bytes =
        yututui::util::safe_fs::read_no_symlink_limited(&path, MAX_CUSTOM_CA_PEM_BYTES as u64)
            .map_err(|_| {
                "the CA file must be a readable regular non-symlink file within the size limit"
                    .to_owned()
            })?;
    if bytes.is_empty() {
        return Err("the CA file is empty or too large".to_owned());
    }
    Ok(Some(bytes))
}

fn prompt_secret(prompt: &str) -> Result<String, String> {
    let value =
        rpassword::prompt_password(prompt).map_err(|_| "could not read the secret".to_owned())?;
    if value.is_empty() {
        Err("the credential cannot be blank".to_owned())
    } else {
        Ok(value)
    }
}

fn confirm(prompt: &str) -> Result<bool, String> {
    Ok(matches!(
        prompt_line(prompt)?.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), String> {
    let rendered =
        serde_json::to_string_pretty(value).map_err(|_| "could not encode JSON".to_owned())?;
    println!("{rendered}");
    Ok(())
}

fn print_sync_summary(summary: &yututui::sync::manual::ManualSyncSummary) {
    if summary.downloaded_operations == 0 && summary.uploaded_operations == 0 {
        println!("Up to date; no personal-state changes were needed.");
    } else {
        println!(
            "Merged {} local and {} remote operation(s).",
            summary.uploaded_operations, summary.downloaded_operations
        );
    }
}

fn print_join_summary(preview: &PairingJoinPreview) {
    let summary = &preview.summary;
    println!(
        "First merge preview: {} operation(s), +{} favorite(s), +{} history item(s), \
         +{} radio favorite(s), +{} playlist(s), +{} playlist item(s), +{} signal track(s).",
        summary.operations_added,
        summary.favorites_added,
        summary.history_added,
        summary.radio_favorites_added,
        summary.playlists_added,
        summary.playlist_entries_added,
        summary.signal_tracks_added,
    );
    println!("This preview does not delete local or remote personal data.");
}

fn service_error(error: SyncServiceError) -> String {
    format!("{} ({}).", error, error.reason())
}

fn default_recovery_directory() -> Result<PathBuf, String> {
    directories::UserDirs::new()
        .and_then(|dirs| dirs.download_dir().map(Path::to_path_buf))
        .ok_or_else(|| "could not find Downloads; enter an existing recovery directory".to_owned())
        .and_then(|path| resolve_existing_directory_path(&path))
}

fn resolve_existing_directory(raw: &str) -> Result<PathBuf, String> {
    let path = expand_tilde(raw)?;
    resolve_existing_directory_path(&path)
}

fn resolve_existing_directory_path(path: &Path) -> Result<PathBuf, String> {
    let absolute =
        std::path::absolute(path).map_err(|_| "could not resolve the directory".to_owned())?;
    let metadata =
        fs::symlink_metadata(&absolute).map_err(|_| "the directory does not exist".to_owned())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("the destination must be an existing non-symlink directory".to_owned());
    }
    let canonical =
        fs::canonicalize(&absolute).map_err(|_| "could not resolve the directory".to_owned())?;
    terminal_safe_path(&canonical)?;
    Ok(canonical)
}

fn resolve_input_file(raw: &str, error: &str) -> Result<PathBuf, String> {
    std::path::absolute(expand_tilde(raw)?).map_err(|_| error.to_owned())
}

fn expand_tilde(raw: &str) -> Result<PathBuf, String> {
    if raw == "~" {
        return home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }
    #[cfg(windows)]
    if let Some(rest) = raw.strip_prefix(r"~\") {
        return Ok(home_dir()?.join(rest));
    }
    if raw.starts_with('~') {
        return Err("only `~` and paths beginning with `~/` are supported".to_owned());
    }
    Ok(PathBuf::from(raw))
}

fn home_dir() -> Result<PathBuf, String> {
    directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .ok_or_else(|| "could not find the user home directory".to_owned())
}

fn unused_recovery_path(directory: &Path) -> PathBuf {
    let now = yututui::signals::unix_now().max(0);
    for suffix in 0_u16..=u16::MAX {
        let name = if suffix == 0 {
            format!("yututui-recovery-kit-{now}.json")
        } else {
            format!("yututui-recovery-kit-{now}-{suffix}.json")
        };
        let path = directory.join(name);
        if !path.exists() {
            return path;
        }
    }
    directory.join(format!("yututui-recovery-kit-{now}-new.json"))
}

fn terminal_safe_path(path: &Path) -> Result<String, String> {
    let rendered = path.to_string_lossy();
    if rendered.chars().any(is_terminal_unsafe_character) {
        return Err("paths cannot contain terminal control or bidirectional characters".to_owned());
    }
    Ok(rendered.into_owned())
}

fn terminal_safe_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if is_terminal_unsafe_character(character) {
                '�'
            } else {
                character
            }
        })
        .collect()
}

fn is_terminal_unsafe_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{200b}'
                | '\u{200c}'
                | '\u{200d}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        )
}

fn usage_error(message: &str) -> i32 {
    if message == "help" {
        print!("{SYNC_USAGE}");
        EXIT_OK
    } else {
        eprintln!("ytt sync: {message}\n\n{SYNC_USAGE}");
        EXIT_USAGE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_documented_commands() {
        assert_eq!(parse(&args(&["setup"])), Ok(Command::Setup));
        assert_eq!(
            parse(&args(&["status", "--json"])),
            Ok(Command::Status { json: true })
        );
        assert_eq!(parse(&args(&["now"])), Ok(Command::Now));
        assert_eq!(parse(&args(&["pair", "create"])), Ok(Command::PairCreate));
        assert_eq!(
            parse(&args(&["pair", "join", "ABCD"])),
            Ok(Command::PairJoin {
                code: Some("ABCD".to_owned())
            })
        );
        assert_eq!(
            parse(&args(&["pair", "join", "--resume"])),
            Ok(Command::PairJoin { code: None })
        );
        assert_eq!(parse(&args(&["pair", "cancel"])), Ok(Command::PairCancel));
        assert_eq!(
            parse(&args(&["devices"])),
            Ok(Command::Devices { json: false })
        );
        assert_eq!(
            parse(&args(&["revoke", "dev-a"])),
            Ok(Command::Revoke {
                device_id: "dev-a".to_owned()
            })
        );
        assert_eq!(
            parse(&args(&["recovery", "export", "--to=/safe"])),
            Ok(Command::RecoveryExport {
                directory: "/safe".to_owned()
            })
        );
        assert_eq!(
            parse(&args(&["audit", "--json"])),
            Ok(Command::Audit { json: true })
        );
    }

    #[test]
    fn secret_bearing_values_are_not_cli_options() {
        assert!(parse(&args(&["setup", "--password", "secret"])).is_err());
        assert!(parse(&args(&["pair", "join", "CODE", "--token", "secret"])).is_err());
        assert!(parse(&args(&["now", "--endpoint", "https://example.test"])).is_err());
    }

    #[test]
    fn malformed_forms_fail_closed() {
        assert!(parse(&[]).is_err());
        assert!(parse(&args(&["status", "--json", "--json"])).is_err());
        assert!(parse(&args(&["revoke"])).is_err());
        assert!(parse(&args(&["recovery", "export"])).is_err());
        assert!(parse(&args(&["unknown"])).is_err());
    }
}
