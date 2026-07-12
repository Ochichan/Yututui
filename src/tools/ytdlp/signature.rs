//! Detached-signature verification for managed yt-dlp releases.
//!
//! GPG is an external dependency and can stall while reading its keyring or waiting on a helper.
//! Keep both the key import and signature check asynchronous, wall-clock bounded, and owned by
//! the same cancellation-safe process-tree guard as yt-dlp itself.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::util::process::{self, ProcessProfile};
use crate::util::safe_fs;

const GPG_VERIFY_TIMEOUT: Duration = Duration::from_secs(30);
const VERIFIER_STDOUT_MAX: usize = 8 * 1024;

/// Pinned from https://github.com/yt-dlp/yt-dlp/blob/master/public.key.
/// The current signing identity is:
/// Simon Sawicki (yt-dlp signing key) <contact@grub4k.xyz>
const YTDLP_PUBLIC_KEY: &str = r#"-----BEGIN PGP PUBLIC KEY BLOCK-----

mQINBGP78C4BEAD0rF9zjGPAt0thlt5C1ebzccAVX7Nb1v+eqQjk+WEZdTETVCg3
WAM5ngArlHdm/fZqzUgO+pAYrB60GKeg7ffUDf+S0XFKEZdeRLYeAaqqKhSibVal
DjvOBOztu3W607HLETQAqA7wTPuIt2WqmpL60NIcyr27LxqmgdN3mNvZ2iLO+bP0
nKR/C+PgE9H4ytywDa12zMx6PmZCnVOOOu6XZEFmdUxxdQ9fFDqd9LcBKY2LDOcS
Yo1saY0YWiZWHtzVoZu1kOzjnS5Fjq/yBHJLImDH7pNxHm7s/PnaurpmQFtDFruk
t+2lhDnpKUmGr/I/3IHqH/X+9nPoS4uiqQ5HpblB8BK+4WfpaiEg75LnvuOPfZIP
KYyXa/0A7QojMwgOrD88ozT+VCkKkkJ+ijXZ7gHNjmcBaUdKK7fDIEOYI63Lyc6Q
WkGQTigFffSUXWHDCO9aXNhP3ejqFWgGMtCUsrbkcJkWuWY7q5ARy/05HbSM3K4D
U9eqtnxmiV1WQ8nXuI9JgJQRvh5PTkny5LtxqzcmqvWO9TjHBbrs14BPEO9fcXxK
L/CFBbzXDSvvAgArdqqlMoncQ/yicTlfL6qzJ8EKFiqW14QMTdAn6SuuZTodXCTi
InwoT7WjjuFPKKdvfH1GP4bnqdzTnzLxCSDIEtfyfPsIX+9GI7Jkk/zZjQARAQAB
tDdTaW1vbiBTYXdpY2tpICh5dC1kbHAgc2lnbmluZyBrZXkpIDxjb250YWN0QGdy
dWI0ay54eXo+iQJOBBMBCgA4FiEErAy75oSNaoc0ZK9OV89lkztadYEFAmP78C4C
GwMFCwkIBwIGFQoJCAsCBBYCAwECHgECF4AACgkQV89lkztadYEVqQ//cW7TxhXg
7Xbh2EZQzXml0egn6j8QaV9KzGragMiShrlvTO2zXfLXqyizrFP4AspgjSn/4NrI
8mluom+Yi+qr7DXT4BjQqIM9y3AjwZPdywe912Lxcw52NNoPZCm24I9T7ySc8lmR
FQvZC0w4H/VTNj/2lgJ1dwMflpwvNRiWa5YzcFGlCUeDIPskLx9++AJE+xwU3LYm
jQQsPBqpHHiTBEJzMLl+rfd9Fg4N+QNzpFkTDW3EPerLuvJniSBBwZthqxeAtw4M
UiAXh6JvCc2hJkKCoygRfM281MeolvmsGNyQm+axlB0vyldiPP6BnaRgZlx+l6MU
cPqgHblb7RW5j9lfr6OYL7SceBIHNv0CFrt1OnkGo/tVMwcs8LH3Ae4a7UJlIceL
V54aRxSsZU7w4iX+PB79BWkEsQzwKrUuJVOeL4UDwWajp75OFaUqbS/slDDVXvK5
OIeuth3mA/adjdvgjPxhRQjA3l69rRWIJDrqBSHldmRsnX6cvXTDy8wSXZgy51lP
m4IVLHnCy9m4SaGGoAsfTZS0cC9FgjUIyTyrq9M67wOMpUxnuB0aRZgJE1DsI23E
qdvcSNVlO+39xM/KPWUEh6b83wMn88QeW+DCVGWACQq5N3YdPnAJa50617fGbY6I
gXIoRHXkDqe23PZ/jURYCv0sjVtjPoVC+bg=
=bJkn
-----END PGP PUBLIC KEY BLOCK-----
"#;

struct GpgTools {
    gpg: PathBuf,
    gpgv: Option<PathBuf>,
}

fn resolve_gpg_tools() -> Result<GpgTools, String> {
    let Some(gpg) = crate::deps::resolve_on_path("gpg") else {
        return Err("GnuPG `gpg` is required to verify yt-dlp release signatures".to_owned());
    };
    Ok(GpgTools {
        gpg,
        gpgv: crate::deps::resolve_on_path("gpgv"),
    })
}

/// Owns the verifier directory from creation through the final awaited process.
///
/// In particular, aborting a maintainer future first drops the nested process-tree guard and then
/// this guard, so no partial keyring or fetched signature is left behind.
struct SignatureWorkDir {
    path: PathBuf,
}

impl SignatureWorkDir {
    fn create() -> Result<Self, String> {
        Self::create_in(&signature_temp_base())
    }

    fn create_in(base: &Path) -> Result<Self, String> {
        for attempt in 0..16u8 {
            let path = base.join(format!(
                "yututui-ytdlp-gpg-{}-{}-{attempt}",
                std::process::id(),
                super::now_unix()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    let guard = Self { path };
                    safe_fs::ensure_private_dir(guard.path())
                        .map_err(|e| format!("cannot prepare signature verifier dir: {e}"))?;
                    return Ok(guard);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!("cannot prepare signature verifier dir: {error}"));
                }
            }
        }
        Err("cannot allocate signature verifier temp dir".to_owned())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SignatureWorkDir {
    fn drop(&mut self) {
        match remove_work_dir(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                path = %self.path.display(),
                %error,
                "failed to remove yt-dlp signature verifier directory"
            ),
        }
    }
}

#[cfg(not(windows))]
fn remove_work_dir(path: &Path) -> std::io::Result<()> {
    std::fs::remove_dir_all(path)
}

#[cfg(windows)]
fn remove_work_dir(path: &Path) -> std::io::Result<()> {
    let mut last_error = None;
    for attempt in 0..5 {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                if attempt < 4 {
                    // Closing a kill-on-drop Job Object terminates the verifier synchronously,
                    // but Windows can retain its file handles for a short scheduling interval.
                    std::thread::sleep(Duration::from_millis(25 * (attempt + 1)));
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| std::io::Error::other("verifier cleanup failed")))
}

fn signature_temp_base() -> PathBuf {
    #[cfg(unix)]
    {
        let tmp = PathBuf::from("/tmp");
        if tmp.is_dir() {
            return tmp;
        }
    }
    std::env::temp_dir()
}

pub(super) async fn verify_sha256sums_signature(
    sums: &[u8],
    signature: &[u8],
) -> Result<(), String> {
    let tools = resolve_gpg_tools()?;
    let work = SignatureWorkDir::create()?;
    verify_sha256sums_signature_in(&tools, sums, signature, work.path()).await
}

async fn verify_sha256sums_signature_in(
    tools: &GpgTools,
    sums: &[u8],
    signature: &[u8],
    dir: &Path,
) -> Result<(), String> {
    let home = dir.join("gnupg-home");
    let sums_path = dir.join("SHA2-256SUMS");
    let sig_path = dir.join("SHA2-256SUMS.sig");
    let key_path = dir.join("yt-dlp-public.key");
    let keyring = "trustedkeys.kbx";

    std::fs::create_dir(&home).map_err(|e| format!("cannot prepare GPG home: {e}"))?;
    safe_fs::ensure_private_dir(&home).map_err(|e| format!("cannot prepare GPG home: {e}"))?;
    std::fs::write(&sums_path, sums).map_err(|e| format!("cannot write checksum manifest: {e}"))?;
    std::fs::write(&sig_path, signature)
        .map_err(|e| format!("cannot write checksum signature: {e}"))?;
    std::fs::write(&key_path, YTDLP_PUBLIC_KEY.as_bytes())
        .map_err(|e| format!("cannot write yt-dlp public key: {e}"))?;

    let home_arg = gpg_path_arg(tools, &home);
    let sums_arg = gpg_path_arg(tools, &sums_path);
    let sig_arg = gpg_path_arg(tools, &sig_path);
    let key_arg = gpg_path_arg(tools, &key_path);

    let mut import = process::tokio_command(&tools.gpg.to_string_lossy(), ProcessProfile::YtDlp);
    import
        .arg("--batch")
        .arg("--quiet")
        .arg("--homedir")
        .arg(&home_arg)
        .arg("--no-default-keyring")
        .arg("--keyring")
        .arg(keyring)
        .arg("--import")
        .arg(&key_arg)
        .stdin(Stdio::null())
        .current_dir(&home);
    run_verifier(import, "import yt-dlp signing key", GPG_VERIFY_TIMEOUT).await?;

    if let Some(gpgv) = &tools.gpgv {
        let mut verify = process::tokio_command(&gpgv.to_string_lossy(), ProcessProfile::YtDlp);
        verify
            .arg("--homedir")
            .arg(&home_arg)
            .arg("--keyring")
            .arg(keyring)
            .arg(&sig_arg)
            .arg(&sums_arg)
            .stdin(Stdio::null())
            .current_dir(&home);
        run_verifier(
            verify,
            "verify yt-dlp checksum signature",
            GPG_VERIFY_TIMEOUT,
        )
        .await
    } else {
        let mut verify =
            process::tokio_command(&tools.gpg.to_string_lossy(), ProcessProfile::YtDlp);
        verify
            .arg("--batch")
            .arg("--quiet")
            .arg("--homedir")
            .arg(&home_arg)
            .arg("--no-default-keyring")
            .arg("--keyring")
            .arg(keyring)
            .arg("--verify")
            .arg(&sig_arg)
            .arg(&sums_arg)
            .stdin(Stdio::null())
            .current_dir(&home);
        run_verifier(
            verify,
            "verify yt-dlp checksum signature",
            GPG_VERIFY_TIMEOUT,
        )
        .await
    }
}

#[cfg(not(windows))]
fn gpg_path_arg(_tools: &GpgTools, path: &Path) -> OsString {
    path.as_os_str().to_os_string()
}

#[cfg(windows)]
fn gpg_path_arg(tools: &GpgTools, path: &Path) -> OsString {
    if !gpg_uses_msys_paths(tools) {
        return path.as_os_str().to_os_string();
    }
    let normalized = path.to_string_lossy().replace('\\', "/");
    let bytes = normalized.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' && bytes[2] == b'/' && bytes[0].is_ascii_alphabetic() {
        let drive = (bytes[0] as char).to_ascii_lowercase();
        return format!("/{drive}/{}", &normalized[3..]).into();
    }
    normalized.into()
}

#[cfg(windows)]
fn gpg_uses_msys_paths(tools: &GpgTools) -> bool {
    let exe = tools
        .gpg
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    exe.contains("/git/usr/bin/")
        || exe.contains("/msys64/")
        || exe.contains("/mingw64/")
        || exe.contains("/mingw32/")
}

async fn run_verifier(
    cmd: tokio::process::Command,
    label: &str,
    deadline: Duration,
) -> Result<(), String> {
    // The outer timeout caps the complete helper lifecycle. The inner runner has the same
    // deadline for its pipe/read and wait phases, and its RAII tree guard kills synchronously if
    // the outer timeout or owner cancellation drops this future.
    let output = match tokio::time::timeout(
        deadline,
        process::tokio_output_limited(cmd, ProcessProfile::YtDlp, deadline, VERIFIER_STDOUT_MAX),
    )
    .await
    {
        Ok(Ok(output)) => output,
        Ok(Err(error)) if error.to_string().contains("timed out") => {
            return Err(format!("{label} timed out"));
        }
        Ok(Err(error)) => return Err(format!("{label} failed: {error:#}")),
        Err(_) => return Err(format!("{label} timed out")),
    };

    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr_tail);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        Err(format!("{label} failed with {}", output.status))
    } else {
        Err(format!("{label} failed with {}: {stderr}", output.status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_public_key_is_yt_dlp_signing_key() {
        assert!(YTDLP_PUBLIC_KEY.contains("BEGIN PGP PUBLIC KEY BLOCK"));
        assert!(YTDLP_PUBLIC_KEY.contains("tDdTaW1vbiBTYXdpY2tp"));
        assert!(YTDLP_PUBLIC_KEY.contains("dWI0ay54eXo+"));
    }

    #[tokio::test]
    async fn signature_verifier_rejects_invalid_signature_when_gpg_is_available() {
        let Ok(tools) = resolve_gpg_tools() else {
            return;
        };
        let work = SignatureWorkDir::create().unwrap();

        let err = verify_sha256sums_signature_in(
            &tools,
            b"not a sums file\n",
            b"not a signature",
            work.path(),
        )
        .await
        .expect_err("invalid detached signature must be rejected");

        assert!(
            err.contains("verify yt-dlp checksum signature"),
            "unexpected error: {err}"
        );
    }

    #[cfg(unix)]
    fn test_root(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let root = std::env::temp_dir().join(format!(
            "ytt-ytdlp-signature-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[cfg(unix)]
    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
    }

    #[cfg(unix)]
    async fn assert_process_exits(pid: libc::pid_t) {
        for _ in 0..40 {
            if !crate::util::process::process_exists_for_test(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("verifier fixture process {pid} survived cancellation");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_verifier_kills_tree_and_removes_work_dir_without_blocking_runtime() {
        use std::os::unix::fs::PermissionsExt;

        let root = test_root("cancel");
        let pid_file = root.join("verifier.pids");
        let pid_file_pending = root.join("verifier.pids.pending");
        let fixture = root.join("fake-gpg");
        let script = format!(
            "#!/bin/sh\nsleep 30 &\nprintf '%s\\n%s\\n' \"$$\" \"$!\" > {}\nmv {} {}\nwait\n",
            shell_quote(&pid_file_pending),
            shell_quote(&pid_file_pending),
            shell_quote(&pid_file)
        );
        std::fs::write(&fixture, script).unwrap();
        std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755)).unwrap();

        let tools = GpgTools {
            gpg: fixture,
            gpgv: None,
        };
        let work = SignatureWorkDir::create_in(&root).unwrap();
        let work_path = work.path().to_path_buf();
        let task = tokio::spawn(async move {
            verify_sha256sums_signature_in(&tools, b"manifest\n", b"signature", work.path()).await
        });

        // On a current-thread runtime this waiter can only run if verifier polling is async.
        // The fixture publishes both PIDs with an atomic rename: observing mere creation of a
        // redirected file is not sufficient because the shell creates it before `printf` writes.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !pid_file.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake verifier did not start without blocking the runtime");
        let pids: Vec<libc::pid_t> = std::fs::read_to_string(&pid_file)
            .unwrap()
            .lines()
            .map(|line| line.parse().unwrap())
            .collect();
        assert_eq!(pids.len(), 2);

        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        assert!(
            !work_path.exists(),
            "cancelling verification must remove its temporary keyring"
        );
        for pid in pids {
            assert_process_exits(pid).await;
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
