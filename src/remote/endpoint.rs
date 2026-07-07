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

use std::io;
use std::path::{Path, PathBuf};

use super::proto::InstanceFile;
use crate::util::{runtime, safe_fs};

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
pub fn read_instance() -> Option<InstanceFile> {
    let data = instance_path()
        .ok()
        .and_then(|path| read_instance_bytes(path.as_path()))
        .or_else(|| read_instance_bytes(legacy_instance_path().as_path()))?;
    serde_json::from_slice(&data).ok()
}

fn read_instance_bytes(path: &Path) -> Option<Vec<u8>> {
    safe_fs::read_no_symlink_limited(path, 8 * 1024).ok()
}

/// Best-effort removal of the descriptor on shutdown.
pub fn remove_instance() {
    if let Ok(path) = instance_path() {
        let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_file(legacy_instance_path());
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
}
