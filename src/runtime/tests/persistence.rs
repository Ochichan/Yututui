use super::super::{durable_mutation_rejection_reason, read_only};
use crate::app::Cmd;

struct RecoveryLatchReset;

impl Drop for RecoveryLatchReset {
    fn drop(&mut self) {
        crate::persist::clear_startup_recovery_error_for_test();
    }
}

#[test]
fn late_recovery_latch_rejects_durable_transfer_before_actor_admission() {
    let _reset = RecoveryLatchReset;
    crate::persist::latch_startup_recovery_error_for_test(crate::persist::StoreKind::Config);

    let command = Cmd::Transfer(crate::transfer::actor::TransferCmd::Disconnect);
    assert_eq!(
        read_only::durable_mutation_component(&command),
        Some("transfer state")
    );
    let reason = durable_mutation_rejection_reason(None)
        .expect("the live recovery latch overrides a writable startup lease");
    assert!(
        reason.contains("unverifiable recovery artifact"),
        "{reason}"
    );
}
