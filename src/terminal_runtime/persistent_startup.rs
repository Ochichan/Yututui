use crate::persist;

pub use crate::persist::StartupStoreSet as PersistentStartupState;

pub struct TerminalStartupState {
    pub(super) config: crate::config::Config,
    pub(super) persistent: PersistentStartupState,
    pub(super) persistence_access: persist::PersistenceAccess,
}

impl TerminalStartupState {
    pub fn new(
        config: crate::config::Config,
        persistent: PersistentStartupState,
        persistence_access: persist::PersistenceAccess,
    ) -> Self {
        Self {
            config,
            persistent,
            persistence_access,
        }
    }
}

pub fn load_persistent_startup_state()
-> std::result::Result<PersistentStartupState, persist::StartupRecoveryError> {
    persist::load_startup_store_set()
}
