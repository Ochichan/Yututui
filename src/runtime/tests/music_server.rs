#[test]
fn coherent_reload_resolves_remove_ambiguity_and_failure() {
    let store_error = crate::open_subsonic::ServiceError::Store(
        crate::open_subsonic::StoreError::StorageUnavailable,
    );
    assert_eq!(
        super::super::dispatch::resolve_music_server_remove(Some(store_error), Ok(false)),
        Ok(())
    );
    assert_eq!(
        super::super::dispatch::resolve_music_server_remove(Some(store_error), Ok(true)),
        Err(crate::app::MusicServerFailure::Storage)
    );
    assert_eq!(
        super::super::dispatch::resolve_music_server_remove(
            None,
            Err(crate::open_subsonic::ServiceError::ActorUnavailable),
        ),
        Err(crate::app::MusicServerFailure::Unavailable)
    );
}

#[test]
fn failed_reload_retains_only_a_live_global_owner() {
    assert!(super::super::open_subsonic_runtime::retain_after_reload_error(true));
    assert!(!super::super::open_subsonic_runtime::retain_after_reload_error(false));
}

#[test]
fn live_connection_does_not_hide_playback_reports_needing_a_decision() {
    use crate::app::MusicServerHealth;

    assert_eq!(
        super::super::dispatch::live_music_server_health(0, 0, 0, 0, 0),
        MusicServerHealth::UpToDate
    );
    assert_eq!(
        super::super::dispatch::live_music_server_health(1, 0, 0, 0, 0),
        MusicServerHealth::NeedsAttention
    );
    assert_eq!(
        super::super::dispatch::live_music_server_health(0, 1, 0, 0, 0),
        MusicServerHealth::NeedsAttention
    );
    assert_eq!(
        super::super::dispatch::live_music_server_health(0, 0, 1, 0, 0),
        MusicServerHealth::NeedsAttention
    );
    assert_eq!(
        super::super::dispatch::live_music_server_health(0, 0, 0, 1, 0),
        MusicServerHealth::NeedsAttention
    );
    assert_eq!(
        super::super::dispatch::live_music_server_health(0, 0, 0, 0, 1),
        MusicServerHealth::NeedsAttention
    );
}
