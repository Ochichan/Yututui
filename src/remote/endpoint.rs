//! Per-user resolution of the control socket + instance descriptor, and token minting.
//!
//! The socket lives in an app-private runtime dir: a secure `$XDG_RUNTIME_DIR` child when
//! available, otherwise a private child of the platform temp dir. Unix creates/verifies that
//! app directory as `0700`; instance descriptors are written as `0600`. The instance
//! descriptor sits next to the socket, so a `ytt -r` client and the running instance always
//! agree on its location without the data dir. Reads still fall back to the pre-hardening
//! descriptor path so a newly installed `ytt -r` can reach an older running instance.
//!
//! The path is a filesystem path used with `GenericFilePath` (not the namespaced/abstract
//! form): on Linux the abstract namespace bypasses filesystem permissions, which would
//! let any local user connect — the `0700` dir is exactly the boundary we want.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(all(test, unix))]
use std::os::unix::fs::PermissionsExt;

use super::proto::InstanceFile;
use crate::util::{runtime, safe_fs};

const MAX_INSTANCE_BYTES: u64 = 8 * 1024;

/// Fail-closed errors from the current (non-legacy) instance descriptor.
///
/// Messages deliberately omit paths, endpoint names, and tokens so callers can safely print
/// them without disclosing control-channel credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrentInstanceError {
    NotFound,
    Unreadable,
    UnsafePermissions,
    Malformed,
    UnexpectedEndpoint,
    InvalidToken,
}

impl fmt::Display for CurrentInstanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            CurrentInstanceError::NotFound => {
                "no running ytt instance found — start one with `ytt`."
            }
            CurrentInstanceError::Unreadable => {
                "could not safely read the current ytt instance descriptor."
            }
            CurrentInstanceError::UnsafePermissions => {
                "refusing a current ytt instance descriptor that is not private."
            }
            CurrentInstanceError::Malformed => "malformed current ytt instance descriptor.",
            CurrentInstanceError::UnexpectedEndpoint => {
                "current ytt instance descriptor names an unexpected endpoint."
            }
            CurrentInstanceError::InvalidToken => {
                "current ytt instance descriptor contains an invalid token."
            }
        };
        f.write_str(message)
    }
}

impl std::error::Error for CurrentInstanceError {}

fn user_tag() -> String {
    runtime::filesystem_user_tag()
}

/// The primary control-socket endpoint name (Unix path / Windows pipe name).
pub fn socket_endpoint() -> io::Result<String> {
    #[cfg(windows)]
    {
        Ok(windows_socket_endpoint(&user_tag()))
    }
    #[cfg(unix)]
    {
        Ok(runtime::app_runtime_dir()?
            .join(format!("yututui-remote-{}.sock", user_tag()))
            .to_string_lossy()
            .into_owned())
    }
}

/// Endpoint for an explicit secondary instance (`--new-instance`): pid-qualified so it can
/// never collide with or displace the primary.
pub fn alt_socket_endpoint(pid: u32) -> io::Result<String> {
    #[cfg(windows)]
    {
        Ok(windows_alt_socket_endpoint(&user_tag(), pid))
    }
    #[cfg(unix)]
    {
        Ok(runtime::app_runtime_dir()?
            .join(format!("yututui-remote-{}-{}.sock", user_tag(), pid))
            .to_string_lossy()
            .into_owned())
    }
}

/// Path to the instance descriptor (endpoint + token), beside the socket.
pub fn instance_path() -> io::Result<PathBuf> {
    Ok(runtime::app_runtime_dir()?.join(format!("yututui-remote-{}.json", user_tag())))
}

fn current_socket_endpoint_path() -> String {
    #[cfg(windows)]
    {
        windows_socket_endpoint(&user_tag())
    }
    #[cfg(unix)]
    {
        runtime::app_runtime_dir_path()
            .join(format!("yututui-remote-{}.sock", user_tag()))
            .to_string_lossy()
            .into_owned()
    }
}

fn current_instance_path() -> PathBuf {
    runtime::app_runtime_dir_path().join(format!("yututui-remote-{}.json", user_tag()))
}

fn legacy_socket_endpoint() -> String {
    #[cfg(windows)]
    {
        windows_socket_endpoint(&user_tag())
    }
    #[cfg(unix)]
    {
        runtime::legacy_runtime_base()
            .join(format!("yututui-remote-{}.sock", user_tag()))
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(any(windows, test))]
fn windows_socket_endpoint(user_tag: &str) -> String {
    format!(r"\\.\pipe\yututui-remote-{user_tag}")
}

#[cfg(any(windows, test))]
fn windows_alt_socket_endpoint(user_tag: &str, pid: u32) -> String {
    format!(r"\\.\pipe\yututui-remote-{user_tag}-{pid}")
}

fn legacy_instance_path() -> PathBuf {
    runtime::legacy_runtime_base().join(format!("yututui-remote-{}.json", user_tag()))
}

/// Publish the instance descriptor so `ytt -r` clients can find + authenticate to us.
///
/// Written atomically (sibling temp file + rename) so a concurrent `ytt -r` reading the
/// descriptor never observes a half-written file — rename is atomic within one filesystem,
/// and the temp file is always beside the target, so it is.
pub fn write_instance(file: &InstanceFile) -> io::Result<()> {
    safe_fs::write_private_atomic_json(&instance_path()?, file)
}

/// Read the published descriptor, if any (absent / corrupt → `None`).
///
/// General remote-control probes retain the historic best-effort behavior and the legacy
/// shared-runtime fallback. Capability-gated operations must use [`read_current_instance`]
/// instead so they can distinguish absence from a damaged or untrusted descriptor.
pub fn read_instance() -> Option<InstanceFile> {
    let data = instance_path()
        .ok()
        .and_then(|path| read_instance_bytes(path.as_path()))
        .or_else(|| read_instance_bytes(legacy_instance_path().as_path()))?;
    serde_json::from_slice(&data).ok()
}

/// Read only the current private descriptor and validate its local trust boundary.
///
/// Unlike [`read_instance`], this never falls back to the legacy shared-runtime path. The
/// endpoint must be the exact canonical primary endpoint produced by this build, the token must
/// retain its generated 128-bit hex shape, and Unix reads require a current-user `0700` parent
/// plus a current-user `0600` regular descriptor. New remote surfaces use this stricter reader;
/// legacy one-shot commands keep their compatibility fallback.
pub fn read_current_instance() -> Result<InstanceFile, CurrentInstanceError> {
    // Resolve only: unlike the server/write path, a strict read must not create the runtime
    // directory or repair unsafe modes before validating them.
    let path = current_instance_path();
    let expected_endpoint = current_socket_endpoint_path();
    read_current_instance_at(&path, &expected_endpoint)
}

fn read_current_instance_at(
    path: &Path,
    expected_endpoint: &str,
) -> Result<InstanceFile, CurrentInstanceError> {
    let data = read_private_current_bytes(path)?;
    let instance: InstanceFile =
        serde_json::from_slice(&data).map_err(|_| CurrentInstanceError::Malformed)?;
    validate_current_instance(instance, expected_endpoint)
}

fn validate_current_instance(
    instance: InstanceFile,
    expected_endpoint: &str,
) -> Result<InstanceFile, CurrentInstanceError> {
    if instance.endpoint != expected_endpoint {
        return Err(CurrentInstanceError::UnexpectedEndpoint);
    }
    if instance.token.len() != 32 || !instance.token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CurrentInstanceError::InvalidToken);
    }
    Ok(instance)
}

fn read_private_current_bytes(path: &Path) -> Result<Vec<u8>, CurrentInstanceError> {
    safe_fs::read_private_file_limited(path, MAX_INSTANCE_BYTES).map_err(map_current_read_error)
}

fn map_current_read_error(error: io::Error) -> CurrentInstanceError {
    #[cfg(unix)]
    let symlink_loop = error.raw_os_error() == Some(libc::ELOOP);
    #[cfg(not(unix))]
    let symlink_loop = false;

    if error.kind() == io::ErrorKind::NotFound {
        CurrentInstanceError::NotFound
    } else if error.kind() == io::ErrorKind::PermissionDenied || symlink_loop {
        CurrentInstanceError::UnsafePermissions
    } else {
        CurrentInstanceError::Unreadable
    }
}

fn read_instance_bytes(path: &Path) -> Option<Vec<u8>> {
    safe_fs::read_no_symlink_limited(path, MAX_INSTANCE_BYTES).ok()
}

/// Remove `path` only while it still advertises `expected`.
///
/// A retiring owner can overlap a fast successor which has already published a fresh atomic
/// descriptor. Comparing the stable instance identity before unlinking prevents the old guard
/// from deleting that successor's advertisement. Callers additionally release the socket path
/// before their listener stops, so a cooperative successor cannot replace this file between the
/// comparison and removal.
pub(crate) fn remove_instance_file_if_matches(
    path: &Path,
    expected: &InstanceFile,
) -> io::Result<bool> {
    let current = match read_instance_bytes(path)
        .and_then(|bytes| serde_json::from_slice::<InstanceFile>(&bytes).ok())
    {
        Some(current) => current,
        None => return Ok(false),
    };
    if current.app_pid != expected.app_pid
        || current.token != expected.token
        || current.endpoint != expected.endpoint
    {
        return Ok(false);
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// A 128-bit OS-CSPRNG token, hex-encoded.
pub fn gen_token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let mut s = String::with_capacity(32);
    for byte in bytes {
        s.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    Ok(s)
}

/// Legacy primary endpoint, used only to avoid launching over an older running instance.
pub fn legacy_primary_endpoint_for_probe() -> String {
    legacy_socket_endpoint()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::proto::{InstanceMode, PROTOCOL_VERSION};

    #[test]
    fn token_is_32_hex_chars() {
        let t = gen_token().unwrap();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn endpoints_are_distinct_and_short() {
        let primary = socket_endpoint().unwrap();
        let alt = alt_socket_endpoint(std::process::id()).unwrap();
        assert_ne!(primary, alt);
        // Unix sun_path caps at ~104 bytes; keep well under it. The alt (pid-qualified)
        // endpoint is the longer of the two, so it's the one that matters most.
        #[cfg(unix)]
        {
            assert!(
                primary.len() < 104,
                "endpoint too long: {primary} ({} bytes)",
                primary.len()
            );
            assert!(
                alt.len() < 104,
                "alt endpoint too long: {alt} ({} bytes)",
                alt.len()
            );
        }
    }

    #[test]
    fn windows_endpoints_use_per_user_named_pipe_names() {
        let primary = windows_socket_endpoint("User123");
        let alt = windows_alt_socket_endpoint("User123", 42);

        assert_eq!(primary, r"\\.\pipe\yututui-remote-User123");
        assert_eq!(alt, r"\\.\pipe\yututui-remote-User123-42");
        assert_ne!(primary, alt);
    }

    #[test]
    fn user_tag_is_filesystem_safe() {
        let tag = user_tag();
        assert!(!tag.is_empty());
        assert!(tag.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    fn instance_for(endpoint: &str, token: &str) -> InstanceFile {
        InstanceFile {
            app_pid: 7,
            endpoint: endpoint.to_string(),
            token: token.to_string(),
            created_unix: 1,
            mode: InstanceMode::Daemon,
            protocol_version: PROTOCOL_VERSION,
            capabilities: vec!["status".to_string()],
        }
    }

    #[test]
    fn current_descriptor_requires_canonical_endpoint_and_hex_token() {
        let endpoint = socket_endpoint().unwrap();
        let valid = instance_for(&endpoint, "0123456789abcdef0123456789ABCDEF");
        assert!(validate_current_instance(valid, &endpoint).is_ok());

        let redirected = instance_for("unexpected", "0123456789abcdef0123456789abcdef");
        assert_eq!(
            validate_current_instance(redirected, &endpoint).unwrap_err(),
            CurrentInstanceError::UnexpectedEndpoint
        );

        for token in [
            "short",
            "0123456789abcdef0123456789abcdeg",
            "0123456789abcdef0123456789abcdef0",
        ] {
            let invalid = instance_for(&endpoint, token);
            assert_eq!(
                validate_current_instance(invalid, &endpoint).unwrap_err(),
                CurrentInstanceError::InvalidToken
            );
        }
    }

    #[cfg(unix)]
    fn private_test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "yututui-current-instance-{label}-{}-{}",
            std::process::id(),
            gen_token().unwrap()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[cfg(unix)]
    fn write_test_descriptor(path: &Path, instance: &InstanceFile) {
        std::fs::write(path, serde_json::to_vec(instance).unwrap()).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn current_descriptor_read_enforces_private_unix_modes() {
        let dir = private_test_dir("modes");
        let path = dir.join("instance.json");
        let endpoint = dir.join("remote.sock").to_string_lossy().into_owned();
        let instance = instance_for(&endpoint, "0123456789abcdef0123456789abcdef");
        write_test_descriptor(&path, &instance);

        let read = read_current_instance_at(&path, &endpoint).unwrap();
        assert_eq!(read.app_pid, 7);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            read_current_instance_at(&path, &endpoint).unwrap_err(),
            CurrentInstanceError::UnsafePermissions
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            read_current_instance_at(&path, &endpoint).unwrap_err(),
            CurrentInstanceError::UnsafePermissions
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o7777,
            0o755,
            "strict reads must not repair an unsafe runtime directory"
        );

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn current_descriptor_read_rejects_symlink_and_malformed_file() {
        use std::os::unix::fs::symlink;

        let dir = private_test_dir("shape");
        let target = dir.join("target.json");
        let link = dir.join("instance.json");
        let endpoint = dir.join("remote.sock").to_string_lossy().into_owned();
        let instance = instance_for(&endpoint, "0123456789abcdef0123456789abcdef");
        write_test_descriptor(&target, &instance);
        symlink(&target, &link).unwrap();

        assert_eq!(
            read_current_instance_at(&link, &endpoint).unwrap_err(),
            CurrentInstanceError::UnsafePermissions
        );

        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, b"{not json}").unwrap();
        std::fs::set_permissions(&link, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_current_instance_at(&link, &endpoint).unwrap_err(),
            CurrentInstanceError::Malformed
        );

        std::fs::remove_dir_all(dir).unwrap();
    }
}
