//! macOS-only removal and verification of inherited extended ACL entries.
//!
//! `fchmod(0600)` controls BSD mode bits but does not remove NFSv4-style ACEs inherited from a
//! parent directory. Personal exports therefore install an empty `ACL_TYPE_EXTENDED` ACL through
//! the already-open file descriptor and read it back before any private data is written. A file
//! system that cannot perform or verify these operations is rejected fail-closed.

#![cfg(target_os = "macos")]

use std::ffi::{c_char, c_int, c_void};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::ptr::null_mut;

type Acl = *mut c_void;
type AclEntry = *mut c_void;

// From <sys/acl.h>; macOS supports ACL_TYPE_EXTENDED rather than POSIX access/default ACLs.
const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
const ACL_FIRST_ENTRY: c_int = 0;
const ACL_NEXT_ENTRY: c_int = -1;
const ACL_EXTENDED_ALLOW: c_int = 1;
const ACL_EXTENDED_DENY: c_int = 2;
const DANGEROUS_DIRECTORY_PERMISSIONS: u64 = (1 << 2) // add file / write data
    | (1 << 4) // delete this directory
    | (1 << 5) // add subdirectory / append data
    | (1 << 6) // delete child
    | (1 << 8) // write attributes
    | (1 << 10) // write extended attributes
    | (1 << 12) // write ACL/security
    | (1 << 13); // change owner

unsafe extern "C" {
    fn acl_init(count: c_int) -> Acl;
    fn acl_free(object: *mut c_void) -> c_int;
    fn acl_set_fd_np(fd: c_int, acl: Acl, acl_type: c_int) -> c_int;
    fn acl_get_fd_np(fd: c_int, acl_type: c_int) -> Acl;
    fn acl_get_entry(acl: Acl, entry_id: c_int, entry: *mut AclEntry) -> c_int;
    fn acl_valid(acl: Acl) -> c_int;
    fn acl_get_tag_type(entry: AclEntry, tag_type: *mut c_int) -> c_int;
    fn acl_get_permset_mask_np(entry: AclEntry, mask: *mut u64) -> c_int;
    fn acl_to_text(acl: Acl, length: *mut isize) -> *mut c_char;
}

pub(super) fn clear_and_verify_acl(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    let empty = OwnedAllocation::empty_acl()?;

    // SAFETY: `fd` belongs to the live File and `empty` owns a valid empty ACL allocation for the
    // duration of the call. ACL_TYPE_EXTENDED is the supported macOS file ACL type.
    if unsafe { acl_set_fd_np(fd, empty.as_acl(), ACL_TYPE_EXTENDED) } != 0 {
        return Err(last_error("could not clear inherited export ACL"));
    }
    verify_empty_acl(fd, "export file retained inherited ACL entries")
}

/// Reject a directory ACL that grants create/delete/re-permission rights.
///
/// Deny entries and read/search-only allow entries are safe for path integrity. In particular,
/// standard macOS home directories commonly carry `everyone deny delete`; rejecting all extended
/// ACLs would make the normal Downloads path unusable.
pub(super) fn verify_destination_directory(path: &Path) -> io::Result<()> {
    let directory = File::open(path)?;
    if !directory.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "export destination is no longer a directory",
        ));
    }
    verify_safe_directory_acl(directory.as_raw_fd())
}

fn verify_safe_directory_acl(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` belongs to the caller's live directory File. On success libc returns an
    // independently allocated ACL that OwnedAllocation releases with acl_free.
    let acl = unsafe { acl_get_fd_np(fd, ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(with_context(error, "could not inspect directory ACL"))
        };
    }
    let acl = OwnedAllocation(acl);
    // SAFETY: `acl` is the live allocation returned by acl_get_fd_np. Validating first lets an
    // EINVAL from acl_get_entry unambiguously mean that an empty ACL or the end was reached.
    if unsafe { acl_valid(acl.as_acl()) } != 0 {
        return Err(last_error("directory ACL is invalid"));
    }
    let mut entry: AclEntry = null_mut();
    let mut entry_id = ACL_FIRST_ENTRY;
    loop {
        // SAFETY: `acl` is live and valid; `entry` is writable output storage. Darwin returns
        // zero for an entry and EINVAL after the last entry (including an empty ACL).
        if unsafe { acl_get_entry(acl.as_acl(), entry_id, &mut entry) } != 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::EINVAL) {
                Ok(())
            } else {
                Err(with_context(error, "could not enumerate directory ACL"))
            };
        }
        if entry.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory ACL returned a null entry",
            ));
        }

        let mut tag_type = 0;
        // SAFETY: `entry` was returned by acl_get_entry from the live ACL and tag_type is valid
        // output storage.
        if unsafe { acl_get_tag_type(entry, &mut tag_type) } != 0 {
            return Err(last_error("could not inspect directory ACL entry type"));
        }
        if tag_type == ACL_EXTENDED_ALLOW {
            let mut permissions = 0u64;
            // SAFETY: `entry` is live and permissions is valid mask output storage.
            if unsafe { acl_get_permset_mask_np(entry, &mut permissions) } != 0 {
                return Err(last_error("could not inspect directory ACL permissions"));
            }
            if permissions & DANGEROUS_DIRECTORY_PERMISSIONS != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "directory ACL grants create, delete, or permission-change access",
                ));
            }
        } else if tag_type != ACL_EXTENDED_DENY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory ACL has an unknown entry type",
            ));
        }
        entry_id = ACL_NEXT_ENTRY;
    }
}

fn verify_empty_acl(fd: RawFd, nonempty_message: &'static str) -> io::Result<()> {
    // SAFETY: `fd` belongs to the caller's live File. On success libc returns an independently
    // allocated ACL that OwnedAllocation releases with acl_free.
    let acl = unsafe { acl_get_fd_np(fd, ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            // macOS removes the extended-ACL object entirely when an empty ACL is installed, so
            // acl_get_fd_np reports ENOENT. The descriptor is still live, making this unambiguous:
            // the file exists but has no extended ACL to return.
            return Ok(());
        }
        return Err(with_context(error, "could not read back the export ACL"));
    }
    let acl = OwnedAllocation(acl);
    let mut text_len = -1isize;
    // SAFETY: `acl` is the live allocation returned by acl_get_fd_np and `text_len` is valid output
    // storage. libc allocates the returned text, which OwnedAllocation releases below.
    let text = unsafe { acl_to_text(acl.as_acl(), &mut text_len) };
    if text.is_null() {
        return Err(last_error("could not inspect the export ACL"));
    }
    let _text = OwnedAllocation(text.cast());
    if text_len != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            nonempty_message,
        ));
    }
    Ok(())
}

struct OwnedAllocation(*mut c_void);

impl OwnedAllocation {
    fn empty_acl() -> io::Result<Self> {
        // SAFETY: zero asks libc for a valid ACL working allocation containing no entries.
        let acl = unsafe { acl_init(0) };
        if acl.is_null() {
            Err(last_error("could not allocate an empty export ACL"))
        } else {
            Ok(Self(acl))
        }
    }

    fn as_acl(&self) -> Acl {
        self.0
    }
}

impl Drop for OwnedAllocation {
    fn drop(&mut self) {
        // SAFETY: every OwnedAllocation is constructed from exactly one successful libc ACL/text
        // allocation and owns it until this single acl_free call.
        let _ = unsafe { acl_free(self.0) };
        self.0 = null_mut();
    }
}

fn last_error(context: &'static str) -> io::Error {
    with_context(io::Error::last_os_error(), context)
}

fn with_context(error: io::Error, context: &'static str) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::process::Command;

    use super::*;

    #[test]
    fn empty_extended_acl_round_trips_through_the_open_descriptor() {
        let path = std::env::temp_dir().join(format!(
            "yututui-export-acl-{}-{}",
            std::process::id(),
            super::super::random_suffix().expect("random suffix")
        ));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("create ACL fixture");

        clear_and_verify_acl(&file).expect("clear and verify ACL");

        drop(file);
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn ordinary_private_destination_has_no_extended_acl() {
        let path = std::env::temp_dir().join(format!(
            "yututui-export-directory-acl-{}-{}",
            std::process::id(),
            super::super::random_suffix().expect("random suffix")
        ));
        fs::create_dir(&path).expect("create directory fixture");

        verify_destination_directory(&path).expect("verify destination ACL");

        fs::remove_dir(path).expect("cleanup");
    }

    #[test]
    fn destination_with_a_dangerous_allow_acl_is_rejected() {
        let path = std::env::temp_dir().join(format!(
            "yututui-export-directory-nonempty-acl-{}-{}",
            std::process::id(),
            super::super::random_suffix().expect("random suffix")
        ));
        fs::create_dir(&path).expect("create directory fixture");
        let status = Command::new("/bin/chmod")
            .args(["+a", "everyone allow add_file,delete_child"])
            .arg(&path)
            .status()
            .expect("run macOS ACL fixture command");
        assert!(status.success(), "install ACL fixture");

        let error = verify_destination_directory(&path).expect_err("reject nonempty ACL");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        fs::remove_dir(path).expect("cleanup");
    }

    #[test]
    fn standard_deny_delete_acl_is_accepted() {
        let path = std::env::temp_dir().join(format!(
            "yututui-export-directory-deny-acl-{}-{}",
            std::process::id(),
            super::super::random_suffix().expect("random suffix")
        ));
        fs::create_dir(&path).expect("create directory fixture");
        let status = Command::new("/bin/chmod")
            .args(["+a", "everyone deny delete"])
            .arg(&path)
            .status()
            .expect("run macOS ACL fixture command");
        assert!(status.success(), "install standard deny ACL fixture");

        verify_destination_directory(&path).expect("deny-only ACL is path-safe");

        let status = Command::new("/bin/chmod")
            .arg("-N")
            .arg(&path)
            .status()
            .expect("remove ACL fixture");
        assert!(status.success(), "remove standard deny ACL fixture");
        fs::remove_dir(path).expect("cleanup");
    }
}
