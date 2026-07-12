use std::io;
use std::path::Path;

pub(super) fn report_dir(label: &str, dir: &Path, kr: bool) -> bool {
    if dir_is_writable(dir) {
        println!("  ✓ {label} — {}", dir.display());
        true
    } else {
        let note = if kr { "쓰기 불가" } else { "not writable" };
        println!("  ✗ {label} — {} ({note})", dir.display());
        false
    }
}

pub(super) fn dir_is_writable(dir: &Path) -> bool {
    let mut anchor = dir;
    while !anchor.exists() {
        let Some(parent) = anchor.parent() else {
            return false;
        };
        anchor = parent;
    }
    // An observational CLI must remain byte-for-byte read-only. Permission metadata is less
    // authoritative than a probe, but it cannot bypass the process mutation guard.
    if crate::persist::persistence_access().is_read_only() {
        return anchor
            .metadata()
            .is_ok_and(|meta| !meta.permissions().readonly());
    }

    let mut random = [0_u8; 8];
    if getrandom::fill(&mut random).is_err() {
        return false;
    }
    let suffix = random
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let probe = anchor.join(format!(".ytt-doctor-write-probe-{suffix}"));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(file) => {
            drop(file);
            std::fs::remove_file(&probe).is_ok()
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
        Err(_) => false,
    }
}
