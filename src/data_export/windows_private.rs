//! Windows-only creation and verification of private personal-export files.
//!
//! A normal `OpenOptions::create_new` inherits the destination directory's DACL. Downloads may
//! be readable by other local accounts, so inheritance is not a safe default for an export that
//! contains listening history. This helper opens the new file without read/write sharing, replaces
//! its DACL through the file handle, protects that DACL from inheritance, and reads it back before
//! returning the handle. Any failure removes the new file instead of allowing an inherited ACL to
//! escape into the publish path.

#![cfg(windows)]

use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::mem::{align_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, GENERIC_ALL, GENERIC_WRITE, HANDLE,
    LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, AclSizeInformation,
    AddAccessAllowedAce, CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
    GetAclInformation, GetLengthSid, GetSecurityDescriptorControl, GetTokenInformation, IsValidAcl,
    IsValidSecurityDescriptor, IsValidSid, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PRESENT,
    SE_DACL_PROTECTED, SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER, TokenUser,
    WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandleEx, MoveFileW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

// Keep the security-specific values together here; these stable access-mask values are defined by
// winnt.h and are not exported from the Authorization module that provides SetSecurityInfo.
const READ_CONTROL_ACCESS: u32 = 0x0002_0000;
const WRITE_DAC_ACCESS: u32 = 0x0004_0000;
const FILE_ALL_ACCESS_MASK: u32 = 0x001f_01ff;
const FILE_SHARE_DELETE_ONLY: u32 = 0x0000_0004;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const INHERITED_ACE_FLAG: u8 = 0x10;
const INHERIT_ONLY_ACE_FLAG: u8 = 0x08;
const ACCESS_ALLOWED_COMPOUND_ACE_TYPE: u8 = 4;
const ACCESS_ALLOWED_OBJECT_ACE_TYPE: u8 = 5;
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
const ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE: u8 = 11;
const DESTINATION_DANGEROUS_ACCESS: u32 = 0x0000_0002 // FILE_ADD_FILE / FILE_WRITE_DATA
    | 0x0000_0004 // FILE_ADD_SUBDIRECTORY / FILE_APPEND_DATA
    | 0x0000_0040 // FILE_DELETE_CHILD
    | 0x0001_0000 // DELETE
    | 0x0004_0000 // WRITE_DAC
    | 0x0008_0000 // WRITE_OWNER
    | 0x0200_0000 // MAXIMUM_ALLOWED
    | GENERIC_WRITE
    | GENERIC_ALL;
// An intermediate directory may normally allow users to create unrelated siblings (the default
// C:\ ACL grants narrow FILE_ADD_FILE / FILE_ADD_SUBDIRECTORY rights). Those rights alone cannot
// redirect an already-existing next component, so they remain compatible. Delete/rename or ACL
// takeover rights do redirect it and fail closed. Metadata-only writes are likewise insufficient;
// a fully mapped FILE_GENERIC_WRITE is detected separately by its complete specific-rights set.
const ANCESTOR_DANGEROUS_ACCESS: u32 = 0x0000_0040 // FILE_DELETE_CHILD
    | 0x0001_0000 // DELETE
    | 0x0004_0000 // WRITE_DAC
    | 0x0008_0000 // WRITE_OWNER
    | 0x0200_0000 // MAXIMUM_ALLOWED
    | GENERIC_WRITE
    | GENERIC_ALL;
const MAPPED_FILE_GENERIC_WRITE_SPECIFIC: u32 = 0x0000_0002 // FILE_ADD_FILE
    | 0x0000_0004 // FILE_ADD_SUBDIRECTORY
    | 0x0000_0010 // FILE_WRITE_EA
    | 0x0000_0100; // FILE_WRITE_ATTRIBUTES
const TRUSTED_INSTALLER_SID: &str =
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectoryRole {
    Ancestor,
    Destination,
}

impl DirectoryRole {
    fn has_dangerous_access(self, mask: u32) -> bool {
        match self {
            Self::Ancestor => {
                mask & ANCESTOR_DANGEROUS_ACCESS != 0
                    || mask & MAPPED_FILE_GENERIC_WRITE_SPECIFIC
                        == MAPPED_FILE_GENERIC_WRITE_SPECIFIC
            }
            Self::Destination => mask & DESTINATION_DANGEROUS_ACCESS != 0,
        }
    }
}

/// Keeps every root-to-destination directory handle open without delete sharing.
///
/// The caller retains this guard through no-replace publication and its final directory sync, so
/// no ancestor name can be renamed or deleted after its identity and security descriptor have
/// been accepted.
#[must_use = "keep the destination-chain guard alive through publication and directory sync"]
pub(super) struct DestinationChainGuard {
    _directories: Vec<File>,
}

/// Stable identity captured from an open temp-file handle before it is closed for publication.
/// FILE_ID_INFO carries the full 128-bit identifier needed by ReFS, unlike the older 64-bit file
/// index exposed by BY_HANDLE_FILE_INFORMATION.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    volume_serial_number: u64,
    file_id: [u8; 16],
}

/// Create a new non-reparse file whose protected DACL grants access only to the current user.
///
/// Preparation that can fail (token/SID/ACL construction) happens before the path is created.
/// While the DACL is replaced, the handle denies read/write sharing; delete sharing is retained so
/// the path can be marked for deletion before closing if hardening or verification fails.
pub(crate) fn create_private_file(path: &Path) -> io::Result<File> {
    let current_user = current_user_sid()?;
    let private_acl = AclBuffer::for_sid(current_user.as_ptr())?;

    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .access_mode(GENERIC_WRITE | READ_CONTROL_ACCESS | WRITE_DAC_ACCESS)
        .share_mode(FILE_SHARE_DELETE_ONLY);
    let file = options.open(path)?;

    let hardened = (|| {
        apply_private_dacl(&file, private_acl.as_ptr())?;
        verify_private_dacl(&file, current_user.as_ptr())?;

        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "export temp path is not a regular non-reparse file",
            ));
        }
        Ok(())
    })();

    match hardened {
        Ok(()) => Ok(file),
        Err(error) => Err(remove_failed_file(path, file, error)),
    }
}

/// Verify the protected DACL and owner through an existing credential/key file handle.
pub(crate) fn verify_current_user_private_file(file: &File) -> io::Result<()> {
    let current_user = current_user_sid()?;
    verify_private_dacl(file, current_user.as_ptr())
}

/// Verify the complete volume-root-to-destination chain and pin it against rename/delete races.
///
/// Every component is opened without following a reparse point and without delete sharing. Normal
/// Windows roots may grant narrow sibling-creation rights to built-in users, so intermediate
/// components reject only grants that can delete/rename the next component or take over the ACL.
/// The destination retains the stricter create/delete/write check needed to protect export names.
/// Owners and dangerous writers are limited to the current user, LocalSystem, Administrators, and
/// the exact Windows Modules Installer (TrustedInstaller) service SID.
pub(super) fn verify_private_destination_chain(path: &Path) -> io::Result<DestinationChainGuard> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "export destination must be an absolute Windows path",
        ));
    }

    let trusted = TrustedSids::new()?;
    let mut paths: Vec<PathBuf> = path.ancestors().map(Path::to_path_buf).collect();
    paths.reverse();
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "export destination has no Windows volume-root chain",
        ));
    }

    let mut opened = Vec::with_capacity(paths.len());
    for (index, component) in paths.iter().enumerate() {
        let role = if index + 1 == paths.len() {
            DirectoryRole::Destination
        } else {
            DirectoryRole::Ancestor
        };
        let directory = open_chain_directory(component)?;
        verify_directory_security(&directory, &trusted, role)?;
        let identity = file_identity(&directory)?;
        opened.push((component.clone(), directory, identity));
    }

    // Reopen every name after the full chain is pinned. This catches a component that changed
    // between lexical enumeration and its original handle open; the original handles remain live
    // so their kernel identities can be retained through publication.
    for (component, _, expected) in &opened {
        revalidate_directory_path_identity(component, *expected)?;
    }

    Ok(DestinationChainGuard {
        _directories: opened
            .into_iter()
            .map(|(_, directory, _)| directory)
            .collect(),
    })
}

fn open_chain_directory(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .access_mode(READ_CONTROL_ACCESS)
        // Deliberately omit FILE_SHARE_DELETE so ordinary delete/rename opens conflict while the
        // export holds the verified directory object. POSIX-semantics renames may still move the
        // name, but the returned guard continues to own the verified kernel object.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let directory = options.open(path)?;
    require_real_directory(&directory)?;
    Ok(directory)
}

fn require_real_directory(directory: &File) -> io::Result<()> {
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "export path contains a non-directory or reparse-point component",
        ));
    }
    Ok(())
}

fn verify_directory_security(
    directory: &File,
    trusted: &TrustedSids,
    role: DirectoryRole,
) -> io::Result<()> {
    let mut owner: PSID = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: all output pointers are valid. On success `descriptor` is a LocalAlloc allocation
    // guarded by LocalDescriptor; owner and DACL point inside that live allocation.
    let result = unsafe {
        GetSecurityInfo(
            directory.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(win32_error(
            result,
            "could not inspect export path security",
        ));
    }
    if descriptor.is_null() {
        return Err(invalid_acl(
            "Windows returned no export path security descriptor",
        ));
    }
    let _descriptor = LocalDescriptor(descriptor);
    // SAFETY: descriptor is the live GetSecurityInfo allocation guarded above.
    if unsafe { IsValidSecurityDescriptor(descriptor) } == 0 {
        return Err(invalid_acl(
            "Windows returned an invalid export path security descriptor",
        ));
    }

    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: the descriptor is valid and both output pointers are initialized storage.
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
        return Err(last_error(
            "could not inspect export path ACL control flags",
        ));
    }
    if control & SE_DACL_PRESENT == 0 || dacl.is_null() {
        // An absent or null DACL grants access to everyone.
        return Err(invalid_acl("export path has an absent or null DACL"));
    }
    // SAFETY: owner points inside the valid descriptor returned with OWNER_SECURITY_INFORMATION.
    if owner.is_null() || unsafe { IsValidSid(owner) } == 0 || !trusted.contains(owner) {
        // A foreign owner has implicit WRITE_DAC and can grant itself replacement rights later.
        return Err(invalid_acl("export path is owned by an untrusted account"));
    }
    // SAFETY: dacl points inside the live descriptor and was returned as its DACL.
    if unsafe { IsValidAcl(dacl) } == 0 {
        return Err(invalid_acl("export path DACL is invalid"));
    }
    verify_directory_dacl(dacl, trusted, role)
}

fn verify_directory_dacl(
    dacl: *mut ACL,
    trusted: &TrustedSids,
    role: DirectoryRole,
) -> io::Result<()> {
    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: caller supplies a valid ACL; `size` matches the requested information class.
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(last_error("could not inspect export path ACL"));
    }

    for index in 0..size.AceCount {
        let mut ace_ptr: *mut c_void = null_mut();
        // SAFETY: the valid ACL reports AceCount entries and `index` is in that range.
        if unsafe { GetAce(dacl, index, &mut ace_ptr) } == 0 || ace_ptr.is_null() {
            return Err(last_error("could not inspect export path ACL entry"));
        }
        // SAFETY: every ACE returned from a valid ACL contains the common ACE_HEADER.
        let header = unsafe { &*(ace_ptr.cast::<ACE_HEADER>()) };
        if header.AceFlags & INHERIT_ONLY_ACE_FLAG != 0 || !is_allow_ace_type(header.AceType) {
            continue;
        }
        if usize::from(header.AceSize) < size_of::<ACE_HEADER>() + size_of::<u32>() {
            return Err(invalid_acl("export path allow ACE is truncated"));
        }
        // SAFETY: the ACE size check covers the u32 mask at byte offset four. read_unaligned is
        // used so the Rust reference rules do not assume more alignment than the raw ACL promises.
        let mask = unsafe { std::ptr::read_unaligned(ace_ptr.cast::<u8>().add(4).cast::<u32>()) };
        if !role.has_dangerous_access(mask) {
            continue;
        }
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
            return Err(invalid_acl(
                "export path has a conditional or object-specific dangerous grant",
            ));
        }
        let sid = standard_allow_ace_sid(ace_ptr, header)?;
        if !trusted.contains(sid) {
            return Err(invalid_acl(
                "another account can create, replace, delete, rename, or re-permission the export path",
            ));
        }
    }
    Ok(())
}

fn is_allow_ace_type(ace_type: u8) -> bool {
    matches!(
        ace_type,
        ACCESS_ALLOWED_ACE_TYPE
            | ACCESS_ALLOWED_COMPOUND_ACE_TYPE
            | ACCESS_ALLOWED_OBJECT_ACE_TYPE
            | ACCESS_ALLOWED_CALLBACK_ACE_TYPE
            | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE
    )
}

fn standard_allow_ace_sid(ace_ptr: *mut c_void, header: &ACE_HEADER) -> io::Result<PSID> {
    let sid_offset = size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>();
    let sid_bytes = usize::from(header.AceSize)
        .checked_sub(sid_offset)
        .filter(|bytes| *bytes >= 8)
        .ok_or_else(|| invalid_acl("export destination allow ACE SID is truncated"))?;
    // SAFETY: the ACE contains the complete fixed SID header checked immediately above.
    let sub_authority_count = unsafe { *ace_ptr.cast::<u8>().add(sid_offset + 1) };
    let expected = 8usize
        .checked_add(usize::from(sub_authority_count) * size_of::<u32>())
        .ok_or_else(|| invalid_acl("export destination SID length overflowed"))?;
    if sid_bytes != expected {
        return Err(invalid_acl(
            "export destination allow ACE SID length is invalid",
        ));
    }
    // SAFETY: the SID offset and self-described length are bounded by the valid ACE.
    let sid = unsafe { ace_ptr.cast::<u8>().add(sid_offset) }.cast::<c_void>();
    // SAFETY: the complete SID representation is contained within the ACE.
    if unsafe { IsValidSid(sid) } == 0 || unsafe { GetLengthSid(sid) } as usize != expected {
        return Err(invalid_acl(
            "export destination allow ACE contains an invalid SID",
        ));
    }
    Ok(sid)
}

struct TrustedSids {
    current_user: SidBuffer,
    local_system: SidBuffer,
    administrators: SidBuffer,
    trusted_installer: SidBuffer,
}

impl TrustedSids {
    fn new() -> io::Result<Self> {
        Ok(Self {
            current_user: current_user_sid()?,
            local_system: well_known_sid(WinLocalSystemSid)?,
            administrators: well_known_sid(WinBuiltinAdministratorsSid)?,
            trusted_installer: sid_from_string(TRUSTED_INSTALLER_SID)?,
        })
    }

    fn contains(&self, sid: PSID) -> bool {
        [
            self.current_user.as_ptr(),
            self.local_system.as_ptr(),
            self.administrators.as_ptr(),
            self.trusted_installer.as_ptr(),
        ]
        .into_iter()
        .any(|trusted| {
            // SAFETY: caller supplies a validated SID and all trusted buffers contain validated
            // SIDs that stay live for the duration of this comparison.
            (unsafe { EqualSid(sid, trusted) }) != 0
        })
    }
}

/// Atomically move `from` to a new `to` name without replacing an existing destination.
///
/// Export temp and final names are in the same directory, so this is a same-volume rename that
/// preserves the file's verified security descriptor. `MoveFileW` has no replace mode: Windows
/// fails the operation when `to` already exists.
pub(super) fn move_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let from = null_terminated_path(from)?;
    let to = null_terminated_path(to)?;

    // SAFETY: both vectors are live, NUL-terminated UTF-16 path strings. MoveFileW does not retain
    // either pointer after returning and, unlike MoveFileExW, has no replace-existing flag.
    if unsafe { MoveFileW(from.as_ptr(), to.as_ptr()) } != 0 {
        Ok(())
    } else {
        Err(last_error("could not publish the private export file"))
    }
}

/// Capture the volume plus 128-bit file identifier from an already-open temp file.
pub(crate) fn file_identity(file: &File) -> io::Result<FileIdentity> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the File handle is live, `info` is correctly aligned writable storage, and the
    // buffer size exactly matches the FileIdInfo structure requested from Windows.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(last_error(
            "could not identify the private export filesystem object",
        ));
    }
    Ok(FileIdentity {
        volume_serial_number: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

/// Reopen a path without following a reparse point and require the same captured file identity.
pub(crate) fn revalidate_path_identity(path: &Path, expected: FileIdentity) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    if file_identity(&file)? == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "export temp file identity changed before publish",
        ))
    }
}

fn revalidate_directory_path_identity(path: &Path, expected: FileIdentity) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let directory = options.open(path)?;
    require_real_directory(&directory)?;
    if file_identity(&directory)? == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "export path directory identity changed during verification",
        ))
    }
}

fn null_terminated_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows export path contains an interior NUL",
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn apply_private_dacl(file: &File, dacl: *const ACL) -> io::Result<()> {
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: `handle` belongs to the live `File`; `dacl` points to an initialized ACL that
    // outlives this call. Owner/group/SACL are intentionally unchanged via null pointers.
    let result = unsafe {
        SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null(),
        )
    };
    if result == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(win32_error(result, "could not apply private export ACL"))
    }
}

fn verify_private_dacl(file: &File, expected_sid: PSID) -> io::Result<()> {
    let handle = file.as_raw_handle() as HANDLE;
    let mut owner: PSID = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();

    // SAFETY: all output pointers are valid for the call. On success Windows allocates
    // `descriptor` with LocalAlloc; `LocalDescriptor` releases it with LocalFree below.
    let result = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(win32_error(result, "could not verify private export ACL"));
    }
    if descriptor.is_null() {
        return Err(invalid_acl(
            "Windows returned no export security descriptor",
        ));
    }
    let _descriptor = LocalDescriptor(descriptor);

    // SAFETY: `descriptor` is the live GetSecurityInfo allocation guarded above.
    if unsafe { IsValidSecurityDescriptor(descriptor) } == 0 {
        return Err(invalid_acl(
            "Windows returned an invalid export security descriptor",
        ));
    }
    if owner.is_null() {
        return Err(invalid_acl(
            "private file is not owned by the current account",
        ));
    }
    // SAFETY: `owner` points inside the live descriptor and was checked for null above.
    if unsafe { IsValidSid(owner) } == 0 {
        return Err(invalid_acl(
            "private file is not owned by the current account",
        ));
    }
    // SAFETY: `owner` is a valid SID in the live descriptor, and `expected_sid` is the valid
    // current-account SID retained by the caller for the duration of this check.
    if unsafe { EqualSid(owner, expected_sid) } == 0 {
        return Err(invalid_acl(
            "private file is not owned by the current account",
        ));
    }

    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: the descriptor is valid and both scalar output pointers are initialized storage.
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
        return Err(last_error("could not inspect export ACL control flags"));
    }
    if control & SE_DACL_PRESENT == 0 || control & SE_DACL_PROTECTED == 0 {
        return Err(invalid_acl("export DACL is absent or still inheritable"));
    }
    if dacl.is_null() {
        // A null DACL grants everyone full access, so this must be an explicit hard failure.
        return Err(invalid_acl("export has a null DACL"));
    }

    // SAFETY: `dacl` points inside the live descriptor and GetSecurityInfo identified it as the
    // object's DACL. IsValidAcl performs the structural check before any ACE is inspected.
    if unsafe { IsValidAcl(dacl) } == 0 {
        return Err(invalid_acl("export DACL is invalid"));
    }
    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: `dacl` is valid and `size` has exactly the layout/length requested by this class.
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(last_error("could not inspect export ACL entries"));
    }
    if size.AceCount != 1 {
        return Err(invalid_acl(
            "export DACL must contain exactly one current-user entry",
        ));
    }

    let mut ace_ptr: *mut c_void = null_mut();
    // SAFETY: the ACL is valid, reports one ACE, and index zero is therefore in bounds.
    if unsafe { GetAce(dacl, 0, &mut ace_ptr) } == 0 || ace_ptr.is_null() {
        return Err(last_error("could not inspect the export ACL entry"));
    }
    // SAFETY: GetAce returned the first structurally valid ACE, whose common header is present for
    // every ACE type. Type and size are checked before casting to the larger allow-ACE layout.
    let header = unsafe { &*(ace_ptr.cast::<ACE_HEADER>()) };
    if header.AceType != ACCESS_ALLOWED_ACE_TYPE || header.AceFlags & INHERITED_ACE_FLAG != 0 {
        return Err(invalid_acl(
            "export ACL entry is not one explicit allow entry",
        ));
    }
    if usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>() {
        return Err(invalid_acl("export ACL allow entry is truncated"));
    }

    // SAFETY: the common header reports enough initialized bytes for ACCESS_ALLOWED_ACE.
    let ace = unsafe { &*(ace_ptr.cast::<ACCESS_ALLOWED_ACE>()) };
    if ace.Mask != FILE_ALL_ACCESS_MASK {
        return Err(invalid_acl(
            "export ACL entry does not grant the current user full file access",
        ));
    }

    let sid_offset = size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>();
    let sid_bytes = usize::from(header.AceSize) - sid_offset;
    if sid_bytes < 8 {
        return Err(invalid_acl("export ACL SID is truncated"));
    }
    // A SID is an 8-byte fixed header followed by SubAuthorityCount u32 values. Bound that
    // self-described length to the ACE before asking Windows to inspect it.
    // SAFETY: the ACE contains at least the eight-byte SID header checked immediately above.
    let sub_authority_count = unsafe { *ace_ptr.cast::<u8>().add(sid_offset + 1) };
    let expected_sid_bytes = 8usize
        .checked_add(usize::from(sub_authority_count) * size_of::<u32>())
        .ok_or_else(|| invalid_acl("export ACL SID length overflowed"))?;
    if sid_bytes != expected_sid_bytes {
        return Err(invalid_acl("export ACL SID length is invalid"));
    }

    // SAFETY: SidStart is within the size-bounded, structurally valid allow ACE above.
    let ace_sid = unsafe { ace_ptr.cast::<u8>().add(sid_offset) }.cast::<c_void>();
    // SAFETY: the SID's complete self-described length is contained within the valid ACE.
    if unsafe { IsValidSid(ace_sid) } == 0 {
        return Err(invalid_acl("export ACL contains an invalid SID"));
    }
    // SAFETY: `ace_sid` was validated immediately above.
    if unsafe { GetLengthSid(ace_sid) } as usize != expected_sid_bytes {
        return Err(invalid_acl("export ACL SID has an unexpected length"));
    }
    // SAFETY: both pointers refer to validated SIDs that remain live for the duration of the call.
    if unsafe { EqualSid(ace_sid, expected_sid) } == 0 {
        return Err(invalid_acl(
            "export ACL grants access to an unexpected account",
        ));
    }
    Ok(())
}

fn current_user_sid() -> io::Result<SidBuffer> {
    let mut token: HANDLE = null_mut();
    // SAFETY: GetCurrentProcess returns a process pseudo-handle; `token` is valid output storage.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error("could not open the current process token"));
    }
    if token.is_null() {
        return Err(invalid_acl("Windows returned no process token"));
    }
    let _token = OwnedHandle(token);

    let mut required = 0u32;
    // SAFETY: the zero-length probe intentionally uses a null buffer and a valid length output.
    let probe = unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut required) };
    if probe != 0 || required == 0 {
        return Err(invalid_acl(
            "Windows returned an invalid token-user buffer size",
        ));
    }
    let probe_error = io::Error::last_os_error();
    if probe_error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        return Err(with_context(
            probe_error,
            "could not size the current-user SID",
        ));
    }

    let mut token_info = AlignedBuffer::new(required)?;
    // SAFETY: the aligned allocation is at least `required` bytes and stays live through all SID
    // reads below; the output length pointer is valid.
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            token_info.as_mut_ptr(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(last_error("could not read the current-user SID"));
    }

    // SAFETY: a successful TokenUser query returns a TOKEN_USER at the start of the aligned buffer.
    let source_sid = unsafe { (*(token_info.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    // SAFETY: `source_sid` came from the live, successful TokenUser response.
    if source_sid.is_null() || unsafe { IsValidSid(source_sid) } == 0 {
        return Err(invalid_acl("Windows returned an invalid current-user SID"));
    }
    // SAFETY: source_sid was validated immediately above.
    let sid_len = unsafe { GetLengthSid(source_sid) };
    if sid_len == 0 {
        return Err(last_error("could not size the current-user SID"));
    }

    let mut sid = SidBuffer {
        storage: AlignedBuffer::new(sid_len)?,
    };
    // SAFETY: destination is `sid_len` bytes, and source is the validated token SID of that size.
    if unsafe { windows_sys::Win32::Security::CopySid(sid_len, sid.as_mut_ptr(), source_sid) } == 0
    {
        return Err(last_error("could not copy the current-user SID"));
    }
    Ok(sid)
}

fn well_known_sid(kind: i32) -> io::Result<SidBuffer> {
    let mut sid = SidBuffer {
        storage: AlignedBuffer::new(SECURITY_MAX_SID_SIZE)?,
    };
    let mut length = SECURITY_MAX_SID_SIZE;
    // SAFETY: destination storage is SECURITY_MAX_SID_SIZE bytes, `length` is valid output, and a
    // null domain SID is required for the machine-independent LocalSystem/built-in group SIDs.
    if unsafe { CreateWellKnownSid(kind, null_mut(), sid.as_mut_ptr(), &mut length) } == 0 {
        return Err(last_error("could not construct a trusted Windows SID"));
    }
    // SAFETY: CreateWellKnownSid initialized `sid` on success.
    if unsafe { IsValidSid(sid.as_ptr()) } == 0 {
        return Err(invalid_acl("Windows constructed an invalid trusted SID"));
    }
    Ok(sid)
}

fn sid_from_string(value: &str) -> io::Result<SidBuffer> {
    let mut encoded: Vec<u16> = value.encode_utf16().collect();
    encoded.push(0);
    let mut source: PSID = null_mut();
    // SAFETY: `encoded` is a live NUL-terminated UTF-16 SID string and `source` is valid output
    // storage. ConvertStringSidToSidW returns a LocalAlloc allocation on success.
    if unsafe { ConvertStringSidToSidW(encoded.as_ptr(), &mut source) } == 0 {
        return Err(last_error("could not construct the TrustedInstaller SID"));
    }
    if source.is_null() {
        return Err(invalid_acl(
            "Windows returned no TrustedInstaller SID allocation",
        ));
    }
    let _source = LocalSid(source);
    // SAFETY: `source` is the live ConvertStringSidToSidW result.
    if unsafe { IsValidSid(source) } == 0 {
        return Err(invalid_acl(
            "Windows constructed an invalid TrustedInstaller SID",
        ));
    }
    // SAFETY: source was validated immediately above.
    let length = unsafe { GetLengthSid(source) };
    if length == 0 {
        return Err(last_error("could not size the TrustedInstaller SID"));
    }

    let mut sid = SidBuffer {
        storage: AlignedBuffer::new(length)?,
    };
    // SAFETY: destination is `length` bytes and source is the validated SID of that size.
    if unsafe { windows_sys::Win32::Security::CopySid(length, sid.as_mut_ptr(), source) } == 0 {
        return Err(last_error("could not copy the TrustedInstaller SID"));
    }
    Ok(sid)
}

fn remove_failed_file(path: &Path, file: File, cause: io::Error) -> io::Error {
    // If deletion itself is blocked, an empty protected DACL keeps any rejected leftover closed
    // to every account. The owner can still recover/delete it later through Windows owner rights.
    let deny_all = AclBuffer::empty()
        .and_then(|acl| apply_private_dacl(&file, acl.as_ptr()))
        .is_ok();
    // FILE_SHARE_DELETE allows this call to mark the path for deletion while `file` still denies
    // read/write opens. Once the handle drops, a successful call removes the file atomically.
    let first_remove = fs::remove_file(path);
    drop(file);
    if first_remove.is_ok() || fs::remove_file(path).is_ok() {
        return cause;
    }

    let cleanup = if deny_all {
        "rejected export temp file is deny-all but could not be removed"
    } else {
        "could not secure or remove the rejected export temp file"
    };
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{cause}; {cleanup}"),
    )
}

struct SidBuffer {
    storage: AlignedBuffer,
}

impl SidBuffer {
    fn as_ptr(&self) -> PSID {
        self.storage.as_ptr().cast_mut()
    }

    fn as_mut_ptr(&mut self) -> PSID {
        self.storage.as_mut_ptr()
    }
}

struct AclBuffer {
    storage: AlignedBuffer,
}

impl AclBuffer {
    fn empty() -> io::Result<Self> {
        let acl_len = u32::try_from(size_of::<ACL>())
            .map_err(|_| invalid_acl("empty ACL length is invalid"))?;
        let mut storage = AlignedBuffer::new(acl_len)?;
        let acl = storage.as_mut_ptr().cast::<ACL>();
        // SAFETY: storage is aligned, zeroed, and exactly large enough for an empty ACL header.
        if unsafe { windows_sys::Win32::Security::InitializeAcl(acl, acl_len, ACL_REVISION) } == 0 {
            return Err(last_error("could not initialize the deny-all export ACL"));
        }
        Ok(Self { storage })
    }

    fn for_sid(sid: PSID) -> io::Result<Self> {
        Self::for_sid_with_mask(sid, FILE_ALL_ACCESS_MASK)
    }

    fn for_sid_with_mask(sid: PSID, mask: u32) -> io::Result<Self> {
        // SAFETY: caller supplies a validated, live SID.
        let sid_len = unsafe { GetLengthSid(sid) } as usize;
        let ace_len = size_of::<ACCESS_ALLOWED_ACE>()
            .checked_sub(size_of::<u32>())
            .and_then(|base| base.checked_add(sid_len))
            .ok_or_else(|| invalid_acl("current-user SID is too large for an ACL"))?;
        let acl_len = size_of::<ACL>()
            .checked_add(ace_len)
            .and_then(|len| u32::try_from(len).ok())
            .ok_or_else(|| invalid_acl("current-user ACL is too large"))?;
        let mut storage = AlignedBuffer::new(acl_len)?;
        let acl = storage.as_mut_ptr().cast::<ACL>();

        // SAFETY: `storage` is aligned, zeroed, and at least `acl_len` bytes.
        if unsafe { windows_sys::Win32::Security::InitializeAcl(acl, acl_len, ACL_REVISION) } == 0 {
            return Err(last_error("could not initialize the private export ACL"));
        }
        // SAFETY: ACL and SID are initialized and live; AddAccessAllowedAce copies the SID.
        if unsafe { AddAccessAllowedAce(acl, ACL_REVISION, mask, sid) } == 0 {
            return Err(last_error(
                "could not add the current user to the export ACL",
            ));
        }
        // SAFETY: ACL points to the initialized buffer above.
        if unsafe { IsValidAcl(acl) } == 0 {
            return Err(invalid_acl("constructed export ACL is invalid"));
        }
        Ok(Self { storage })
    }

    fn as_ptr(&self) -> *const ACL {
        self.storage.as_ptr().cast::<ACL>()
    }

    #[cfg(test)]
    fn as_mut_ptr(&mut self) -> *mut ACL {
        self.storage.as_mut_ptr().cast::<ACL>()
    }
}

struct AlignedBuffer {
    words: Vec<usize>,
}

impl AlignedBuffer {
    fn new(byte_len: u32) -> io::Result<Self> {
        let byte_len = usize::try_from(byte_len)
            .map_err(|_| invalid_acl("Windows security buffer is too large"))?;
        let words = byte_len
            .checked_add(size_of::<usize>() - 1)
            .map(|len| len / size_of::<usize>())
            .filter(|len| *len > 0)
            .ok_or_else(|| invalid_acl("Windows returned an empty security buffer"))?;
        let buffer = Self {
            words: vec![0usize; words],
        };
        debug_assert!(
            (buffer.words.as_ptr() as usize).is_multiple_of(align_of::<usize>()),
            "security buffer must remain pointer-aligned"
        );
        Ok(buffer)
    }

    fn as_ptr(&self) -> *const c_void {
        self.words.as_ptr().cast()
    }

    fn as_mut_ptr(&mut self) -> *mut c_void {
        self.words.as_mut_ptr().cast()
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: OwnedHandle is constructed only from a successful OpenProcessToken result and
        // owns exactly one real token handle (never a process pseudo-handle).
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct LocalDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc and ownership was
        // transferred exactly once into this guard.
        let _ = unsafe { LocalFree(self.0) };
    }
}

struct LocalSid(PSID);

impl Drop for LocalSid {
    fn drop(&mut self) {
        // SAFETY: ConvertStringSidToSidW allocated this SID with LocalAlloc and ownership was
        // transferred exactly once into this guard.
        let _ = unsafe { LocalFree(self.0) };
    }
}

fn invalid_acl(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn last_error(context: &'static str) -> io::Error {
    with_context(io::Error::last_os_error(), context)
}

fn win32_error(code: u32, context: &'static str) -> io::Error {
    with_context(io::Error::from_raw_os_error(code as i32), context)
}

fn with_context(error: io::Error, context: &'static str) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_path(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "yututui-private-export-{label}-{}-{nonce}.tmp",
            std::process::id()
        ))
    }

    #[test]
    fn new_export_file_has_one_protected_current_user_ace() {
        let path = test_path("acl");
        let file = create_private_file(&path).unwrap();
        let sid = current_user_sid().unwrap();
        verify_private_dacl(&file, sid.as_ptr()).unwrap();
        let identity = file_identity(&file).unwrap();

        drop(file);
        revalidate_path_identity(&path, identity).unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn existing_path_is_never_replaced() {
        let path = test_path("existing");
        fs::write(&path, b"keep").unwrap();

        let error = create_private_file(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"keep");

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn move_keeps_private_acl_and_removes_source_name() {
        let source = test_path("move-source");
        let destination = test_path("move-destination");
        let file = create_private_file(&source).unwrap();
        drop(file);

        move_no_replace(&source, &destination).unwrap();

        assert!(!source.exists());
        let moved = File::open(&destination).unwrap();
        let sid = current_user_sid().unwrap();
        verify_private_dacl(&moved, sid.as_ptr()).unwrap();
        drop(moved);
        fs::remove_file(destination).unwrap();
    }

    #[test]
    fn move_never_replaces_an_existing_destination() {
        let source = test_path("move-collision-source");
        let destination = test_path("move-collision-destination");
        let file = create_private_file(&source).unwrap();
        drop(file);
        fs::write(&destination, b"keep").unwrap();

        let error = move_no_replace(&source, &destination).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(source.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"keep");
        fs::remove_file(source).unwrap();
        fs::remove_file(destination).unwrap();
    }

    #[test]
    fn move_path_encoding_preserves_extended_unc_long_paths() {
        let raw = format!(r"\\?\UNC\server\share\{}\export.json", "segment".repeat(48));
        assert!(raw.encode_utf16().count() > 260);
        let encoded = null_terminated_path(Path::new(&raw)).unwrap();

        assert_eq!(encoded.last(), Some(&0));
        assert!(!encoded[..encoded.len() - 1].contains(&0));
        assert_eq!(
            OsString::from_wide(&encoded[..encoded.len() - 1]),
            Path::new(&raw).as_os_str()
        );
    }

    #[test]
    fn path_identity_rejects_a_replacement_file() {
        let path = test_path("identity-replacement");
        let file = create_private_file(&path).unwrap();
        let identity = file_identity(&file).unwrap();
        drop(file);
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();

        let error = revalidate_path_identity(&path, identity).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn destination_acl_accepts_trusted_writers_and_untrusted_readers() {
        let trusted = TrustedSids::new().unwrap();

        let mut current_user = AclBuffer::for_sid(trusted.current_user.as_ptr()).unwrap();
        verify_directory_dacl(
            current_user.as_mut_ptr(),
            &trusted,
            DirectoryRole::Destination,
        )
        .unwrap();

        let mut system = AclBuffer::for_sid(trusted.local_system.as_ptr()).unwrap();
        verify_directory_dacl(system.as_mut_ptr(), &trusted, DirectoryRole::Destination).unwrap();

        let mut trusted_installer = AclBuffer::for_sid(trusted.trusted_installer.as_ptr()).unwrap();
        verify_directory_dacl(
            trusted_installer.as_mut_ptr(),
            &trusted,
            DirectoryRole::Destination,
        )
        .unwrap();

        let world = well_known_sid(windows_sys::Win32::Security::WinWorldSid).unwrap();
        let mut read_only = AclBuffer::for_sid_with_mask(world.as_ptr(), 0x0000_0001).unwrap();
        verify_directory_dacl(read_only.as_mut_ptr(), &trusted, DirectoryRole::Destination)
            .unwrap();
    }

    #[test]
    fn destination_acl_rejects_an_untrusted_create_or_delete_grant() {
        let trusted = TrustedSids::new().unwrap();
        let world = well_known_sid(windows_sys::Win32::Security::WinWorldSid).unwrap();
        for mask in [
            0x0000_0002,
            0x0000_0004,
            0x0000_0040,
            0x0001_0000,
            GENERIC_WRITE,
        ] {
            let mut acl = AclBuffer::for_sid_with_mask(world.as_ptr(), mask).unwrap();

            let error =
                verify_directory_dacl(acl.as_mut_ptr(), &trusted, DirectoryRole::Destination)
                    .unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn ancestor_acl_allows_narrow_root_creation_but_rejects_path_takeover() {
        let trusted = TrustedSids::new().unwrap();
        let world = well_known_sid(windows_sys::Win32::Security::WinWorldSid).unwrap();
        let mut narrow_creation =
            AclBuffer::for_sid_with_mask(world.as_ptr(), 0x0000_0002 | 0x0000_0004).unwrap();

        verify_directory_dacl(
            narrow_creation.as_mut_ptr(),
            &trusted,
            DirectoryRole::Ancestor,
        )
        .unwrap();

        let mut metadata_only =
            AclBuffer::for_sid_with_mask(world.as_ptr(), 0x0000_0010 | 0x0000_0100).unwrap();
        verify_directory_dacl(
            metadata_only.as_mut_ptr(),
            &trusted,
            DirectoryRole::Ancestor,
        )
        .unwrap();

        for mask in [
            0x0000_0040,
            0x0001_0000,
            WRITE_DAC_ACCESS,
            0x0008_0000,
            GENERIC_WRITE,
            GENERIC_ALL,
            MAPPED_FILE_GENERIC_WRITE_SPECIFIC,
        ] {
            let mut acl = AclBuffer::for_sid_with_mask(world.as_ptr(), mask).unwrap();
            let error = verify_directory_dacl(acl.as_mut_ptr(), &trusted, DirectoryRole::Ancestor)
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn destination_chain_guard_retains_the_open_directory_after_a_name_move() {
        let directory = test_path("chain-guard").with_extension("dir");
        let moved = directory.with_extension("moved");
        fs::create_dir(&directory).unwrap();

        let guard = verify_private_destination_chain(&directory).unwrap();
        assert!(guard._directories.len() >= 2);
        let expected = file_identity(guard._directories.last().unwrap()).unwrap();
        fs::rename(&directory, &moved).unwrap();
        assert_eq!(
            file_identity(guard._directories.last().unwrap()).unwrap(),
            expected
        );

        drop(guard);
        fs::remove_dir(moved).unwrap();
    }
}
