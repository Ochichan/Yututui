use crate::persist;

pub use crate::persist::StartupStoreSet as PersistentStartupState;

pub struct TerminalStartupState {
    pub(super) config: crate::config::Config,
    pub(super) persistent: PersistentStartupState,
    pub(super) persistence_access: persist::PersistenceAccess,
    pub(super) keyboard_input_mode: crate::terminal_keyboard::KeyboardInputMode,
    pub(super) shutdown: crate::player::lifetime::ShutdownLatch,
}

impl TerminalStartupState {
    pub fn new(
        config: crate::config::Config,
        persistent: PersistentStartupState,
        persistence_access: persist::PersistenceAccess,
        keyboard_input_mode: crate::terminal_keyboard::KeyboardInputMode,
        shutdown: crate::player::lifetime::ShutdownLatch,
    ) -> Self {
        Self {
            config,
            persistent,
            persistence_access,
            keyboard_input_mode,
            shutdown,
        }
    }
}

pub fn load_persistent_startup_state()
-> std::result::Result<PersistentStartupState, persist::StartupRecoveryError> {
    persist::load_startup_store_set()
}
