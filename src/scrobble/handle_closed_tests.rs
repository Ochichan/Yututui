use super::*;

fn settings() -> ScrobbleSettings {
    ScrobbleSettings {
        lastfm_app: None,
        lastfm: None,
        listenbrainz: None,
        local_files: false,
    }
}

#[test]
fn observe_preserves_admission_state_after_closed_queue() {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
    drop(rx);
    drop(shutdown_rx);
    let mut handle = ScrobbleHandle::new(tx, shutdown_tx);

    assert_eq!(
        handle.observe(&crate::media::MediaSnapshot::idle()),
        Err(DeliveryError::Closed)
    );
    assert!(handle.last_fingerprint.is_none());
    assert!(handle.last_sent.is_none());
}

#[tokio::test]
async fn control_commands_report_closed_queue() {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
    drop(rx);
    drop(shutdown_rx);
    let handle = ScrobbleHandle::new(tx, shutdown_tx);

    assert_eq!(handle.reconfigure(settings()), Err(DeliveryError::Closed));
    assert_eq!(handle.auth_start(), Err(DeliveryError::Closed));
    assert_eq!(handle.shutdown_flush().await, Err(DeliveryError::Closed));
}
