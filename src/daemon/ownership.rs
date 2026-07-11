//! Persisted-data ownership for the foreground daemon runtime.

use crate::data_ownership::OwnerGuard;

pub(super) fn acquire_for(
    command: &super::DaemonCommand,
) -> Result<Option<OwnerGuard>, crate::data_ownership::AcquireError> {
    if !matches!(command, super::DaemonCommand::Serve { .. }) {
        return Ok(None);
    }
    crate::data_ownership::acquire_owner()
        .map(Some)
        .inspect_err(|_| {
            eprintln!("ytt daemon: offline personal-data export is active; retry when it finishes");
        })
}

pub(super) fn finish_runtime(
    runtime: tokio::runtime::Runtime,
    owner: Option<OwnerGuard>,
    result: i32,
) -> i32 {
    drop(runtime);
    drop(owner);
    result
}
