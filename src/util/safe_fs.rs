//! Small filesystem primitives for private app state.
//!
//! Unix gets the security properties the callers care about: private directories are `0700`,
//! private files are `0600`, and reads/appends use `O_NOFOLLOW` so a symlink is rejected instead
//! of followed. Windows rejects reparse-point final paths for the same private state APIs.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Recovery backups for secret-bearing files are useful, but old credentials should not
/// accumulate indefinitely.
pub const SECRET_BACKUP_RETENTION: usize = 3;
const RECOVERY_BACKUP_LABELS: &[&str] = &["corrupt", "too-large", "unreadable"];

/// Metadata for a recovery backup. Contents are intentionally never read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryBackup {
    pub path: PathBuf,
    pub label: String,
    pub len: u64,
    pub modified_unix: Option<u64>,
}

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

#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

#[cfg(windows)]
fn is_windows_reparse_point(meta: &fs::Metadata) -> bool {
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(windows)]
fn reject_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if is_windows_reparse_point(&meta) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing reparse-point path {}", path.display()),
        )),
        Ok(_) | Err(_) => Ok(()),
    }
}

#[cfg(not(any(unix, windows)))]
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
    #[cfg(windows)]
    {
        let meta = fs::symlink_metadata(path)?;
        if is_windows_reparse_point(&meta) || !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("refusing non-directory private path {}", path.display()),
            ));
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
fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn sync_parent_dir(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) => sync_dir(parent),
        None => Ok(()),
    }
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

#[cfg(windows)]
fn create_private_file(path: &Path) -> io::Result<File> {
    reject_symlink(path)?;
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(not(any(unix, windows)))]
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

#[cfg(windows)]
fn open_private_append(path: &Path) -> io::Result<File> {
    reject_symlink(path)?;
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(not(any(unix, windows)))]
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
        sync_parent_dir(path)?;
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

#[cfg(windows)]
fn open_no_symlink(path: &Path) -> io::Result<File> {
    reject_symlink(path)?;
    OpenOptions::new().read(true).open(path)
}

#[cfg(not(any(unix, windows)))]
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
    append_private_jsonl_inner(path, line, false)
}

/// Append one JSONL line and force the file and parent directory to storage before returning.
pub fn append_private_jsonl_durable(path: &Path, line: &str) -> io::Result<()> {
    append_private_jsonl_inner(path, line, true)
}

fn append_private_jsonl_inner(path: &Path, line: &str, durable: bool) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    let mut file = open_private_append(path)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    if durable {
        file.sync_all()?;
        sync_parent_dir(path)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Schema-drift-tolerant JSON loading
//
// Persisted state (config, play history, caches) is read by whatever build is
// installed today, but was written by whatever build was installed last time. A
// strict `from_str::<T>` fails the instant ONE field's stored value no longer
// deserializes — a renamed enum variant, a retyped field, an un-aliased rename —
// after which the caller falls back to `T::default()`, wiping the WHOLE file. On a
// `.app` reinstall (new binary reading the previous install's files) that reads as
// "all my settings and history reset again."
//
// `recover_lenient` bounds the blast radius: it keeps every field that still fits
// and defaults only the specific incompatible leaves (recursing into nested objects
// and pruning individual bad elements out of arrays), so a single schema change can
// never again reset an entire file.
// ---------------------------------------------------------------------------

/// Element-wise array recovery costs one `T` deserialization per element; skip it for
/// pathologically large arrays and keep the (empty) default instead of stalling startup.
const MAX_ELEMENTWISE_ARRAY: usize = 20_000;

/// Total budget of `T` deserialization attempts across a whole [`recover_lenient`] pass. The
/// per-array [`MAX_ELEMENTWISE_ARRAY`] cap bounds a single array; this bounds the cross-field
/// `O(fields × elements)` accumulation so a crafted-but-under-size-cap file (many fields, each
/// a large drifted array) can't burn startup CPU. Genuine schema-drift recovery only touches
/// the drifted leaves and stays far under this.
const MAX_RECOVERY_DESERIALIZES: usize = 50_000;

/// Load `T` from `path` with schema-drift recovery.
///
/// - missing / unreadable / not a regular file → `T::default()`
/// - valid JSON (even if schema-drifted) → recovered value, preserving all still-valid fields
/// - present but not valid JSON at all → the file is moved aside to `*.corrupt.bak` (so the
///   next `save` cannot silently destroy it) and `T::default()` is returned
pub fn load_json_or_default<T>(path: &Path) -> T
where
    T: DeserializeOwned + Serialize + Default,
{
    let text = match read_to_string_no_symlink(path) {
        Ok(text) => text,
        // A genuinely missing file has nothing to protect. A present-but-unreadable one
        // (permission error, invalid UTF-8, not a regular file, …) must not be silently
        // replaced by the next default `save`, so set it aside first.
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound {
                let _ = backup_with_label(path, "unreadable");
            }
            return T::default();
        }
    };
    // Fast path: byte-for-byte the previous behaviour when nothing drifted.
    if let Ok(value) = serde_json::from_str::<T>(&text) {
        return value;
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => recover_lenient::<T>(value),
        Err(_) => {
            let _ = backup_aside(path);
            T::default()
        }
    }
}

/// Like [`load_json_or_default`] but bounds the read at `max_bytes`. A file larger than the
/// cap is moved aside to `*.too-large.bak` (so the next `save` can't silently destroy it)
/// and `T::default()` is returned — an oversized or corrupted-huge state file can neither
/// stall startup nor spike memory before the schema-drift recovery even runs. The trust
/// boundary is low (same-user data dir), but sync tools, other same-user processes, and
/// user mistakes can all leave a giant file behind.
///
/// - missing / unreadable / symlink / not a regular file → `T::default()`
/// - larger than `max_bytes` → moved to `*.too-large.bak`, `T::default()`
/// - otherwise identical to [`load_json_or_default`] (fast path, then schema-drift recovery)
pub fn load_json_or_default_limited<T>(path: &Path, max_bytes: u64) -> T
where
    T: DeserializeOwned + Serialize + Default,
{
    let bytes = match read_no_symlink_limited(path, max_bytes) {
        Ok(bytes) => bytes,
        // `read_no_symlink_limited` reports oversize as `InvalidData`; set it aside so a fresh
        // default cannot clobber it, mirroring the unparseable-JSON path.
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            let _ = backup_with_label(path, "too-large");
            return T::default();
        }
        // Missing → nothing to protect. Present-but-unreadable (permission, not a regular
        // file, …) → set it aside so the next default save can't clobber intact data.
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound {
                let _ = backup_with_label(path, "unreadable");
            }
            return T::default();
        }
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return T::default();
    };
    if let Ok(value) = serde_json::from_str::<T>(&text) {
        return value;
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => recover_lenient::<T>(value),
        Err(_) => {
            let _ = backup_aside(path);
            T::default()
        }
    }
}

/// Deserialize `T` from an in-memory JSON `value`, preserving every field that still fits
/// and defaulting only the leaves that no longer deserialize. See the module note above.
pub fn recover_lenient<T>(value: Value) -> T
where
    T: DeserializeOwned + Serialize + Default,
{
    // Fast path — nothing drifted.
    if let Ok(value) = T::deserialize(&value) {
        return value;
    }
    // Graft the on-disk value onto defaults (always valid) key by key, keeping only what
    // still lets the whole thing deserialize as `T`.
    let Ok(mut root) = serde_json::to_value(T::default()) else {
        return T::default();
    };
    if let (Value::Object(_), Value::Object(incoming)) = (&root, &value) {
        let incoming = incoming.clone();
        let mut path: Vec<String> = Vec::new();
        let mut budget: usize = MAX_RECOVERY_DESERIALIZES;
        for (key, sub) in &incoming {
            path.push(key.clone());
            graft::<T>(&mut root, &mut path, sub, &mut budget);
            path.pop();
        }
    }
    T::deserialize(&root).unwrap_or_default()
}

/// Fold on-disk `incoming` into `root` at `path`. Invariant: on entry `root` deserializes as
/// `T`; on exit it still does, now holding the richest value at `path` that keeps that true.
/// Sibling keys are all still at their (valid) defaults or already-grafted-valid state, so
/// only the key at `path` can introduce invalidity — which is what makes the single-key /
/// single-element validity checks below accurate.
fn graft<T>(root: &mut Value, path: &mut Vec<String>, incoming: &Value, budget: &mut usize)
where
    T: DeserializeOwned,
{
    if *budget == 0 {
        return; // out of recovery budget: leave the (valid) default at this path
    }
    let current = path_get(root, path).cloned();

    // Try the whole subtree wholesale first.
    path_set(root, path, incoming.clone());
    *budget -= 1;
    if T::deserialize(&*root).is_ok() {
        return;
    }

    match (current.as_ref(), incoming) {
        // Both objects: restore the default subtree, then fold child keys individually so a
        // single bad leaf drops only itself instead of the whole section.
        (Some(Value::Object(_)), Value::Object(sub)) => {
            path_set(root, path, current.clone().unwrap());
            for (key, child) in sub {
                if *budget == 0 {
                    break;
                }
                path.push(key.clone());
                graft::<T>(root, path, child, budget);
                path.pop();
            }
        }
        // Both arrays: keep only the elements that individually deserialize, so a drifted
        // record drops out of a history log without taking the rest of the log with it.
        (Some(Value::Array(_)), Value::Array(items)) => {
            let mut kept: Vec<Value> = Vec::new();
            if items.len() <= MAX_ELEMENTWISE_ARRAY {
                let mut attempted = 0usize;
                for item in items {
                    if *budget == 0 {
                        break;
                    }
                    path_set(root, path, Value::Array(vec![item.clone()]));
                    *budget -= 1;
                    attempted += 1;
                    if T::deserialize(&*root).is_ok() {
                        kept.push(item.clone());
                    } else if attempted >= 256 && kept.len() * 10 < attempted {
                        // Overwhelmingly incompatible array (the element *type* drifted, not a
                        // few records): stop proving each element fails and keep the default.
                        path_set(root, path, current.clone().unwrap());
                        return;
                    }
                }
            }
            path_set(root, path, Value::Array(kept));
        }
        // Incompatible leaf or type mismatch (renamed enum, retyped scalar, …): keep default.
        _ => match current {
            Some(value) => path_set(root, path, value),
            None => path_remove(root, path),
        },
    }
}

fn path_get<'a>(root: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut cur = root;
    for key in path {
        cur = cur.as_object()?.get(key)?;
    }
    Some(cur)
}

/// Set `root` at `path` to `value`, creating intermediate objects as needed.
fn path_set(root: &mut Value, path: &[String], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        *root = value;
        return;
    };
    let mut cur = root;
    for key in parents {
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        cur = cur
            .as_object_mut()
            .unwrap()
            .entry(key.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    if !cur.is_object() {
        *cur = Value::Object(Map::new());
    }
    cur.as_object_mut().unwrap().insert(last.clone(), value);
}

fn path_remove(root: &mut Value, path: &[String]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut cur = root;
    for key in parents {
        match cur.as_object_mut().and_then(|obj| obj.get_mut(key)) {
            Some(next) => cur = next,
            None => return,
        }
    }
    if let Some(obj) = cur.as_object_mut() {
        obj.remove(last);
    }
}

/// Move an unparseable file aside so a fresh default won't silently destroy it. Never
/// overwrites an existing backup (numbered `*.corrupt.N.bak`).
pub fn backup_aside(path: &Path) -> io::Result<PathBuf> {
    backup_with_label(path, "corrupt")
}

/// Secret-bearing counterpart to [`backup_aside`]. Preserves the newest recovery copy and
/// prunes older secret backups beyond [`SECRET_BACKUP_RETENTION`].
pub fn backup_aside_secret(path: &Path) -> io::Result<PathBuf> {
    backup_with_label_retained(path, "corrupt", SECRET_BACKUP_RETENTION)
}

/// Move an oversized file aside (`*.too-large.bak`). Public counterpart to [`backup_aside`]
/// for callers (e.g. `Config::load`) that read with their own size cap outside
/// [`load_json_or_default_limited`].
pub fn backup_too_large(path: &Path) -> io::Result<PathBuf> {
    backup_with_label(path, "too-large")
}

/// Secret-bearing counterpart to [`backup_too_large`].
pub fn backup_too_large_secret(path: &Path) -> io::Result<PathBuf> {
    backup_with_label_retained(path, "too-large", SECRET_BACKUP_RETENTION)
}

/// List app-created recovery backups for `path` without reading their contents.
pub fn recovery_backups(path: &Path) -> io::Result<Vec<RecoveryBackup>> {
    recovery_backups_for_labels(path, RECOVERY_BACKUP_LABELS)
}

/// Enforce the default secret-backup retention for `path`.
pub fn enforce_secret_backup_retention(path: &Path) -> io::Result<usize> {
    rotate_recovery_backups(path, SECRET_BACKUP_RETENTION, None)
}

/// Move a file aside under a labeled, numbered backup (`*.<label>.bak`, then
/// `*.<label>.N.bak`) without ever overwriting an existing one. Shared by
/// [`backup_aside`] (unparseable JSON) and the oversized-file path in
/// [`load_json_or_default_limited`].
fn backup_with_label(path: &Path, label: &str) -> io::Result<PathBuf> {
    backup_with_label_inner(path, label)
}

fn backup_with_label_retained(path: &Path, label: &str, keep: usize) -> io::Result<PathBuf> {
    let bak = backup_with_label_inner(path, label)?;
    rotate_recovery_backups(path, keep, Some(&bak))?;
    Ok(bak)
}

fn backup_with_label_inner(path: &Path, label: &str) -> io::Result<PathBuf> {
    reject_symlink(path)?;
    for n in 0..1000 {
        let bak = backup_path(path, label, n);
        match fs::hard_link(path, &bak) {
            Ok(()) => {
                fs::remove_file(path)?;
                sync_parent_dir(path)?;
                return Ok(bak);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("too many backups"))
}

fn rotate_recovery_backups(
    path: &Path,
    keep: usize,
    protected: Option<&Path>,
) -> io::Result<usize> {
    let mut backups = recovery_backups(path)?;
    if backups.len() <= keep {
        return Ok(0);
    }
    backups.sort_by(|a, b| {
        a.modified_unix
            .cmp(&b.modified_unix)
            .then_with(|| a.path.cmp(&b.path))
    });

    let mut removed = 0usize;
    while backups.len() > keep {
        let Some(idx) = backups
            .iter()
            .position(|backup| protected != Some(backup.path.as_path()))
        else {
            break;
        };
        let backup = backups.remove(idx);
        match fs::remove_file(&backup.path) {
            Ok(()) => {
                removed += 1;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                removed += 1;
            }
            Err(e) => return Err(e),
        }
    }
    if removed > 0 {
        sync_parent_dir(path)?;
    }
    Ok(removed)
}

fn recovery_backups_for_labels(path: &Path, labels: &[&str]) -> io::Result<Vec<RecoveryBackup>> {
    let mut out = Vec::new();
    for label in labels {
        for n in 0..1000 {
            let backup = backup_path(path, label, n);
            let meta = match fs::symlink_metadata(&backup) {
                Ok(meta) if meta.is_file() => meta,
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let modified_unix = meta
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|age| age.as_secs());
            out.push(RecoveryBackup {
                path: backup,
                label: (*label).to_owned(),
                len: meta.len(),
                modified_unix,
            });
        }
    }
    Ok(out)
}

fn backup_path(path: &Path, label: &str, n: usize) -> PathBuf {
    if n == 0 {
        path.with_extension(format!("{label}.bak"))
    } else {
        path.with_extension(format!("{label}.{n}.bak"))
    }
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

    #[test]
    fn durable_append_writes_jsonl_line() {
        let dir = temp_root("append-durable");
        let path = dir.join("queue.jsonl");
        append_private_jsonl_durable(&path, r#"{"ok":true}"#).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"ok\":true}\n");
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

    #[cfg(windows)]
    #[test]
    fn reparse_point_reads_are_rejected() {
        use std::os::windows::fs::symlink_file;

        let dir = temp_root("reparse");
        fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real");
        let link = dir.join("link");
        fs::write(&real, b"secret").unwrap();
        if symlink_file(&real, &link).is_err() {
            let _ = fs::remove_dir_all(dir);
            return;
        }
        assert!(read_no_symlink(&link).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn limited_load_reads_within_cap_and_sets_oversized_aside() {
        use serde::{Deserialize, Serialize};
        #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
        struct S {
            n: u32,
        }
        let dir = temp_root("limited");
        let path = dir.join("s.json");

        // Under the cap → loads normally.
        write_private_atomic(&path, br#"{"n":7}"#).unwrap();
        assert_eq!(load_json_or_default_limited::<S>(&path, 1024), S { n: 7 });

        // Over the cap → default, and the offending file is moved to `*.too-large.bak`
        // (never destroyed, so nothing is silently lost).
        let big = format!(r#"{{"n":7,"pad":"{}"}}"#, "x".repeat(2048));
        write_private_atomic(&path, big.as_bytes()).unwrap();
        assert_eq!(load_json_or_default_limited::<S>(&path, 1024), S::default());
        assert!(!path.exists());
        assert!(path.with_extension("too-large.bak").exists());

        // Missing file → default (no panic, no backup).
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(load_json_or_default_limited::<S>(&path, 1024), S::default());
    }

    #[test]
    fn present_but_unreadable_file_is_backed_up_not_clobbered() {
        use serde::{Deserialize, Serialize};
        #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
        struct S {
            n: u32,
        }
        let dir = temp_root("unreadable");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        // Invalid UTF-8 is present-but-unreadable: preserve it, don't silently default+clobber.
        fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
        assert_eq!(load_json_or_default::<S>(&path), S::default());
        assert!(!path.exists(), "the unreadable file is moved aside");
        assert!(
            path.with_extension("unreadable.bak").exists(),
            "present-but-unreadable file must be preserved as a backup",
        );
        // A genuinely missing file leaves no backup.
        let missing = dir.join("missing.json");
        assert_eq!(load_json_or_default::<S>(&missing), S::default());
        assert!(!missing.with_extension("unreadable.bak").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn backup_aside_never_overwrites_existing_backup() {
        let dir = temp_root("backup-numbered");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        let first_backup = path.with_extension("corrupt.bak");
        let numbered_backup = path.with_extension("corrupt.1.bak");
        fs::write(&path, b"new-bad").unwrap();
        fs::write(&first_backup, b"old-bad").unwrap();

        let backed_up = backup_aside(&path).unwrap();

        assert_eq!(backed_up, numbered_backup);
        assert_eq!(fs::read(&first_backup).unwrap(), b"old-bad");
        assert_eq!(fs::read(&backed_up).unwrap(), b"new-bad");
        assert!(!path.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn secret_backup_retention_keeps_newest_and_prunes_old_recovery_files() {
        let dir = temp_root("secret-backup-retention");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let mut newest = PathBuf::new();

        for idx in 0..(SECRET_BACKUP_RETENTION + 2) {
            fs::write(&path, format!("secret-{idx}")).unwrap();
            newest = backup_aside_secret(&path).unwrap();
        }

        let backups = recovery_backups(&path).unwrap();
        assert_eq!(backups.len(), SECRET_BACKUP_RETENTION);
        assert!(backups.iter().any(|backup| backup.path == newest));
        assert_eq!(
            fs::read_to_string(&newest).unwrap(),
            format!("secret-{}", SECRET_BACKUP_RETENTION + 1)
        );
        assert!(!path.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn explicit_secret_backup_cleanup_prunes_to_retention() {
        let dir = temp_root("secret-backup-cleanup");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        for idx in 0..(SECRET_BACKUP_RETENTION + 1) {
            fs::write(backup_path(&path, "corrupt", idx), b"old").unwrap();
        }
        fs::write(&path, b"current").unwrap();

        let removed = enforce_secret_backup_retention(&path).unwrap();

        assert_eq!(removed, 1);
        assert_eq!(
            recovery_backups(&path).unwrap().len(),
            SECRET_BACKUP_RETENTION
        );
        assert_eq!(fs::read(&path).unwrap(), b"current");
        let _ = fs::remove_dir_all(dir);
    }

    mod recover {
        use super::*;
        use serde::{Deserialize, Serialize};
        use serde_json::json;

        #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Mode {
            #[default]
            Balanced,
            Aggressive,
        }

        #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(default)]
        struct Inner {
            mode: Mode,
            weight: f32,
            label: String,
        }

        #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(default)]
        struct Row {
            id: String,
            kind: Mode,
        }

        #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(default)]
        struct Store {
            volume: u8,
            name: String,
            inner: Inner,
            log: Vec<Row>,
        }

        #[test]
        fn valid_input_is_returned_unchanged() {
            let store = Store {
                volume: 42,
                name: "hi".into(),
                inner: Inner {
                    mode: Mode::Aggressive,
                    weight: 0.7,
                    label: "x".into(),
                },
                log: vec![Row {
                    id: "a".into(),
                    kind: Mode::Balanced,
                }],
            };
            let value = serde_json::to_value(&store).unwrap();
            assert_eq!(recover_lenient::<Store>(value), store);
        }

        #[test]
        fn unknown_nested_enum_defaults_only_that_field() {
            // `inner.mode` is a variant this build no longer knows; everything else survives.
            let value = json!({
                "volume": 55,
                "name": "keep me",
                "inner": { "mode": "wild", "weight": 0.9, "label": "still here" },
                "log": [],
            });
            let recovered = recover_lenient::<Store>(value);
            assert_eq!(recovered.volume, 55, "sibling scalar preserved");
            assert_eq!(recovered.name, "keep me");
            assert_eq!(recovered.inner.mode, Mode::Balanced, "bad enum -> default");
            assert_eq!(
                recovered.inner.weight, 0.9,
                "sibling in same object preserved"
            );
            assert_eq!(recovered.inner.label, "still here");
        }

        #[test]
        fn retyped_scalar_defaults_only_that_field() {
            let value = json!({ "volume": "loud", "name": "kept" });
            let recovered = recover_lenient::<Store>(value);
            assert_eq!(recovered.volume, 0, "string-where-u8-expected -> default");
            assert_eq!(recovered.name, "kept");
        }

        #[test]
        fn bad_array_element_is_dropped_not_the_whole_log() {
            let value = json!({
                "log": [
                    { "id": "a", "kind": "balanced" },
                    { "id": "b", "kind": "wild" },        // drifted enum
                    { "id": "c", "kind": "aggressive" },
                ],
            });
            let recovered = recover_lenient::<Store>(value);
            assert_eq!(
                recovered.log,
                vec![
                    Row {
                        id: "a".into(),
                        kind: Mode::Balanced
                    },
                    Row {
                        id: "c".into(),
                        kind: Mode::Aggressive
                    },
                ],
                "only the drifted record is dropped",
            );
        }

        #[test]
        fn overwhelmingly_incompatible_array_drops_to_default() {
            // 300 rows whose enum variant this build no longer knows. Element-wise recovery
            // would try each one; the early-abort keeps the default once the keep-ratio
            // collapses, bounding startup CPU on a crafted under-size-cap file.
            let rows: Vec<Value> = (0..300)
                .map(|i| json!({ "id": format!("r{i}"), "kind": "wild" }))
                .collect();
            let recovered = recover_lenient::<Store>(json!({ "log": rows, "volume": 9 }));
            assert!(
                recovered.log.is_empty(),
                "all-drifted array -> default (empty)"
            );
            assert_eq!(recovered.volume, 9, "sibling field is still preserved");
        }

        #[test]
        fn unrelated_json_falls_back_to_default() {
            assert_eq!(
                recover_lenient::<Store>(json!({ "nope": 1 })),
                Store::default()
            );
            assert_eq!(recover_lenient::<Store>(json!([1, 2, 3])), Store::default());
        }

        #[test]
        fn corrupt_file_is_backed_up_and_defaulted() {
            let dir = temp_root("recover-corrupt");
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("store.json");
            fs::write(&path, b"this is not { json").unwrap();

            let recovered = load_json_or_default::<Store>(&path);
            assert_eq!(recovered, Store::default());
            assert!(
                dir.join("store.corrupt.bak").exists(),
                "unparseable file must be preserved as a backup, not destroyed",
            );
            let _ = fs::remove_dir_all(dir);
        }

        #[test]
        fn missing_file_is_default_without_backup() {
            let dir = temp_root("recover-missing");
            let path = dir.join("store.json");
            assert_eq!(load_json_or_default::<Store>(&path), Store::default());
            assert!(!dir.join("store.corrupt.bak").exists());
        }
    }
}
