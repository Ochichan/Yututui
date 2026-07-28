use super::*;

/// Run inherited safety cleanup before best-effort panic-time disk writes.
///
/// The inherited hook kills media and restores the terminal. Persistence may block on filesystem
/// locks, so it deliberately runs second.
pub fn install_panic_flush(pending: PanicPending) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        match pending.shadow.seal_and_snapshot() {
            Ok(snapshot) => {
                for operation in snapshot.into_iter().flatten() {
                    let _ = operation.write();
                }
            }
            Err(PanicShadowSealed) => {
                // A concurrent or nested hook owns the one-shot persistence frontier.
            }
        }
    }));
}
