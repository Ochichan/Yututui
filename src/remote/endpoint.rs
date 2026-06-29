//! Per-user resolution of the control socket + instance descriptor, and token minting.
//!
//! The socket lives in a **per-user runtime dir** — `$XDG_RUNTIME_DIR` when set (Linux,
//! `0700`), otherwise the temp dir. On macOS the temp dir is already a per-user `0700`
//! directory, so it is itself the boundary. On Linux *without* `$XDG_RUNTIME_DIR` the
//! fallback is shared, world-writable `/tmp`: the `$USER` suffix there only avoids name
//! collisions — it is **not** a security boundary, since another local user can pre-create
//! the path. `$XDG_RUNTIME_DIR` (present on any systemd login session) is the real boundary;
//! the `/tmp` fallback trades isolation for "it still runs" on minimal setups. The instance
//! descriptor sits next to it, so a `ytt -r` client and the running instance always agree on
//! its location without the data dir.
//!
//! The path is a filesystem path used with `GenericFilePath` (not the namespaced/abstract
//! form): on Linux the abstract namespace bypasses filesystem permissions, which would
//! let any local user connect — the `0700` dir is exactly the boundary we want.

use std::io;
use std::path::PathBuf;

use super::proto::InstanceFile;

/// Base directory for the runtime socket + instance file.
fn runtime_base() -> PathBuf {
    #[cfg(unix)]
    {
        if let Some(x) = std::env::var_os("XDG_RUNTIME_DIR") {
            let p = PathBuf::from(x);
            if p.is_absolute() {
                return p;
            }
        }
    }
    std::env::temp_dir()
}

/// A short, filesystem-safe per-user tag to disambiguate shared namespaces (Linux `/tmp`
/// and the global Windows named-pipe namespace).
fn user_tag() -> String {
    let raw = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let tag: String = raw.chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect();
    if tag.is_empty() { "default".to_string() } else { tag }
}

/// The primary control-socket endpoint name (Unix path / Windows pipe name).
pub fn socket_endpoint() -> String {
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\ytm-tui-remote-{}", user_tag())
    }
    #[cfg(unix)]
    {
        runtime_base()
            .join(format!("ytm-tui-remote-{}.sock", user_tag()))
            .to_string_lossy()
            .into_owned()
    }
}

/// Endpoint for an explicit secondary instance (`--new-instance`): pid-qualified so it can
/// never collide with or displace the primary.
pub fn alt_socket_endpoint(pid: u32) -> String {
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\ytm-tui-remote-{}-{}", user_tag(), pid)
    }
    #[cfg(unix)]
    {
        runtime_base()
            .join(format!("ytm-tui-remote-{}-{}.sock", user_tag(), pid))
            .to_string_lossy()
            .into_owned()
    }
}

/// Path to the instance descriptor (endpoint + token), beside the socket.
pub fn instance_path() -> PathBuf {
    runtime_base().join(format!("ytm-tui-remote-{}.json", user_tag()))
}

/// Publish the instance descriptor so `ytt -r` clients can find + authenticate to us.
///
/// Written atomically (sibling temp file + rename) so a concurrent `ytt -r` reading the
/// descriptor never observes a half-written file — rename is atomic within one filesystem,
/// and the temp file is always beside the target, so it is.
pub fn write_instance(file: &InstanceFile) -> io::Result<()> {
    let json = serde_json::to_vec(file).map_err(io::Error::other)?;
    let final_path = instance_path();
    let mut tmp = final_path.clone().into_os_string();
    tmp.push(format!(".tmp.{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &final_path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Read the published descriptor, if any (absent / corrupt → `None`).
pub fn read_instance() -> Option<InstanceFile> {
    let data = std::fs::read(instance_path()).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Best-effort removal of the descriptor on shutdown.
pub fn remove_instance() {
    let _ = std::fs::remove_file(instance_path());
}

/// A 128-bit random token, hex-encoded. `fastrand` is already a dependency (queue
/// shuffling). The threat model is accident / cross-user prevention, not a same-user
/// attacker — the runtime dir's `0700` perms are the real boundary.
pub fn gen_token() -> String {
    let mut s = String::with_capacity(32);
    for _ in 0..16 {
        let byte = fastrand::u8(..);
        s.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_32_hex_chars() {
        let t = gen_token();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn endpoints_are_distinct_and_short() {
        let primary = socket_endpoint();
        let alt = alt_socket_endpoint(std::process::id());
        assert_ne!(primary, alt);
        // Unix sun_path caps at ~104 bytes; keep well under it. The alt (pid-qualified)
        // endpoint is the longer of the two, so it's the one that matters most.
        #[cfg(unix)]
        {
            assert!(primary.len() < 104, "endpoint too long: {primary} ({} bytes)", primary.len());
            assert!(alt.len() < 104, "alt endpoint too long: {alt} ({} bytes)", alt.len());
        }
    }

    #[test]
    fn user_tag_is_filesystem_safe() {
        let tag = user_tag();
        assert!(!tag.is_empty());
        assert!(tag.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
