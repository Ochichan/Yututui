//! Small filesystem primitives for private app state.
//!
//! Unix gets the security properties the callers care about: private directories are `0700`,
//! private files are `0600`, and reads/appends use `O_NOFOLLOW` so a symlink is rejected instead
//! of followed. Windows keeps the existing per-user-profile behaviour behind the same API.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

#[cfg(unix)]
fn private_dir_mode() -> u32 {
    PRIVATE_DIR_MODE
}

#[cfg(unix)]
fn private_file_mode() -> u32 {
    PRIVATE_FILE_MODE
}

#[cfg(unix)]
fn is_owned_by_current_user(meta: &fs::Metadata) -> bool {
    meta.uid() == unsafe { libc::geteuid() }
}

#[cfg(unix)]
fn reject_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing symlink path {}", path.display()),
        )),
        Ok(_) | Err(_) => Ok(()),
    }
}

#[cfg(not(unix))]
fn reject_symlink(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Ensure `path` exists as a private app directory.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("refusing non-directory private path {}", path.display()),
            ));
        }
        if !is_owned_by_current_user(&meta) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "private path is not owned by current user: {}",
                    path.display()
                ),
            ));
        }
        let mode = meta.permissions().mode() & 0o777;
        if mode != private_dir_mode() {
            fs::set_permissions(path, fs::Permissions::from_mode(private_dir_mode()))?;
        }
    }
    Ok(())
}

fn random_suffix() -> io::Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let mut out = String::with_capacity(16);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('0'));
    }
    Ok(out)
}

fn temp_path_for(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("state.json");
    Ok(path.with_file_name(format!(
        ".{name}.tmp.{}.{}",
        std::process::id(),
        random_suffix()?
    )))
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true)
        .create_new(true)
        .mode(private_file_mode())
        .custom_flags(libc::O_NOFOLLOW);
    opts.open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(unix)]
fn open_private_append(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true)
        .append(true)
        .mode(private_file_mode())
        .custom_flags(libc::O_NOFOLLOW);
    let file = opts.open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(private_file_mode()))?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_private_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// Atomically replace `path` with private file contents.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    reject_symlink(path)?;
    let tmp = temp_path_for(path)?;
    let write_result = (|| {
        let mut file = create_private_file(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        #[cfg(unix)]
        fs::set_permissions(&tmp, fs::Permissions::from_mode(private_file_mode()))?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

/// Serialize `value` as pretty JSON and atomically replace `path` with a private file.
pub fn write_private_atomic_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    write_private_atomic(path, &json)
}

#[cfg(unix)]
fn open_no_symlink(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).custom_flags(libc::O_NOFOLLOW);
    opts.open(path)
}

#[cfg(not(unix))]
fn open_no_symlink(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

/// Read a regular file, rejecting symlinks on Unix.
pub fn read_no_symlink(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = open_no_symlink(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a regular file: {}", path.display()),
        ));
    }
    let mut out = Vec::new();
    file.read_to_end(&mut out)?;
    Ok(out)
}

pub fn read_to_string_no_symlink(path: &Path) -> io::Result<String> {
    String::from_utf8(read_no_symlink(path)?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Read at most `max_bytes`, rejecting symlinks on Unix and oversized regular files.
pub fn read_no_symlink_limited(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let file = open_no_symlink(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a regular file: {}", path.display()),
        ));
    }
    if meta.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file too large: {}", path.display()),
        ));
    }
    let mut out = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut out)?;
    if out.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file too large: {}", path.display()),
        ));
    }
    Ok(out)
}

/// Append one JSONL line to a private file.
pub fn append_private_jsonl(path: &Path, line: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    let mut file = open_private_append(path)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        std::env::temp_dir().join(format!("ytm-tui-{name}-{}-{suffix}", std::process::id()))
    }

    #[test]
    fn writes_and_reads_private_file() {
        let dir = temp_root("safe-fs");
        let path = dir.join("config.json");
        write_private_atomic(&path, br#"{"ok":true}"#).unwrap();
        assert_eq!(read_no_symlink(&path).unwrap(), br#"{"ok":true}"#);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn private_modes_are_enforced() {
        let dir = temp_root("modes");
        ensure_private_dir(&dir).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            private_dir_mode()
        );
        let path = dir.join("secret.json");
        write_private_atomic(&path, b"{}").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            private_file_mode()
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_reads_are_rejected() {
        use std::os::unix::fs::symlink;

        let dir = temp_root("symlink");
        fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real");
        let link = dir.join("link");
        fs::write(&real, b"secret").unwrap();
        symlink(&real, &link).unwrap();
        assert!(read_no_symlink(&link).is_err());
        let _ = fs::remove_dir_all(dir);
    }
}
