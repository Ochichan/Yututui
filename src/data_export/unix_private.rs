//! Unix destination-chain checks for path-stable private publication.
//!
//! A safe target directory is insufficient when another account owns or can write an ancestor:
//! that account could rename the checked directory and replace it before a path-based publish.
//! Walking bottom-up proves that every component is controlled by the current user or root and
//! that shared writable components use sticky deletion semantics (for example `/tmp`).

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

const STICKY_BIT: u32 = 0o1000;
const SHARED_WRITE_BITS: u32 = 0o022;

pub(super) fn verify_destination_chain(destination: &Path, current_uid: u32) -> Result<(), String> {
    let mut component = Some(destination);
    while let Some(path) = component {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            format!(
                "cannot inspect export directory or ancestor {} ({error})",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "export directory chain changed at {}; choose a stable real directory",
                path.display()
            ));
        }
        verify_component(
            path,
            metadata.uid(),
            metadata.permissions().mode(),
            current_uid,
        )?;

        #[cfg(target_os = "macos")]
        super::macos_private::verify_destination_directory(path).map_err(|error| {
            format!(
                "directory ACL grants unsafe ancestor access; remove create, delete, or permission-change allow entries or choose a private directory: {} ({error})",
                path.display()
            )
        })?;

        component = path.parent();
    }
    Ok(())
}

fn verify_component(
    path: &Path,
    owner_uid: u32,
    mode: u32,
    current_uid: u32,
) -> Result<(), String> {
    if owner_uid != current_uid && owner_uid != 0 {
        return Err(format!(
            "directory or ancestor is owned by another account that can replace the export path; choose a directory under your home or a trusted sticky temporary directory: {}",
            path.display()
        ));
    }
    if mode & SHARED_WRITE_BITS != 0 && mode & STICKY_BIT == 0 {
        return Err(format!(
            "directory or ancestor is writable by another account without sticky protection; remove group/other write permission, add the sticky bit to a trusted temporary directory, or choose a private directory: {}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_policy_accepts_private_root_and_sticky_directories() {
        let path = Path::new("/fixture");
        assert!(verify_component(path, 501, 0o700, 501).is_ok());
        assert!(verify_component(path, 0, 0o755, 501).is_ok());
        assert!(verify_component(path, 0, 0o1777, 501).is_ok());
    }

    #[test]
    fn component_policy_rejects_untrusted_owner_and_nonsticky_shared_write() {
        let path = Path::new("/fixture");
        assert!(verify_component(path, 502, 0o700, 501).is_err());
        assert!(verify_component(path, 501, 0o770, 501).is_err());
        assert!(verify_component(path, 0, 0o777, 501).is_err());
    }
}
