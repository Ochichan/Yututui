use crate::personal_state::{
    CausalStamp, DeviceId, DeviceRecord, Dot, Operation, OperationEnvelope, OperationOrigin,
    PersonalStateV2, PortableTrack, PortableTrackKey,
};
use crate::sync::DeviceSecretMaterial;

use super::*;

#[test]
fn detached_listening_rebase_preserves_remote_conflicts() {
    let local_secret = DeviceSecretMaterial::generate_for("rebase-local").unwrap();
    let remote_secret = DeviceSecretMaterial::generate_for("rebase-remote").unwrap();
    let local_device = DeviceId::new(local_secret.device_id()).unwrap();
    let remote_device = DeviceId::new(remote_secret.device_id()).unwrap();
    let local_record = DeviceRecord {
        device_id: local_device.clone(),
        name: "Local".to_owned(),
        revoked: false,
        public_identity: Some(local_secret.public_identity()),
    };
    let remote_record = DeviceRecord {
        device_id: remote_device.clone(),
        name: "Remote".to_owned(),
        revoked: false,
        public_identity: Some(remote_secret.public_identity()),
    };
    let mut observed = PersonalStateV2::empty("rebase-listening".to_owned()).unwrap();
    for (sequence, device) in [(1, local_record), (2, remote_record)] {
        let dot = Dot {
            device_id: local_device.clone(),
            sequence,
        };
        observed.operations.push(OperationEnvelope {
            operation_id: format!("rebase-local:{sequence}"),
            stamp: CausalStamp {
                dot: dot.clone(),
                observed: observed.version_vector.clone(),
                recorded_at_unix: 0,
            },
            origin: OperationOrigin::Local,
            operation: Operation::AddDevice { device },
        });
        observed.version_vector.observe(&dot);
    }
    crate::personal_state::refresh_device_registry(&mut observed).unwrap();
    observed.normalize().unwrap();

    let track = PortableTrack {
        key: PortableTrackKey::Catalog {
            provider: "ytmusic".to_owned(),
            exact_catalog_id: "rebase-track".to_owned(),
        },
        title: "Rebase track".to_owned(),
        artist: "Artist".to_owned(),
        album: None,
        duration_secs: Some(600),
        isrc: None,
    };
    let current = local_changes(&observed, &local_device, &track);
    let durable = remote_changes(&observed, &remote_device, &track);

    let rebased = rebase_local_operations(&durable, &observed, &current, &local_device).unwrap();
    let projection = crate::listening::ListeningProjection::from_ledger(&rebased).unwrap();
    assert_eq!(projection.resumes[&track.key].candidates.len(), 2);
    assert_eq!(projection.passport_notes["station"].len(), 2);
    assert_eq!(
        projection.dj_presets[&crate::listening::DjPresetId::new("preset").unwrap()].len(),
        2
    );
    let local_resume = rebased.operations.iter().find(|operation| {
        matches!(
            operation.operation,
            Operation::Listening {
                change: crate::listening::ListeningOperation::SetResume { .. }
            }
        )
    });
    assert_eq!(
        local_resume
            .unwrap()
            .stamp
            .observed
            .observed(&remote_device),
        0
    );

    let same_actor_durable = crate::personal_state::append_listening(
        &observed,
        Some(&local_device),
        crate::listening::ListeningOperation::ClearResume {
            clear: crate::listening::ResumeClear {
                track,
                provenance: crate::listening::ResumeProvenance {
                    playback_session_id: "recovered-same-actor".to_owned(),
                    device_id: local_device.clone(),
                },
            },
        },
        4,
    )
    .unwrap();
    assert_eq!(
        rebase_local_operations(&same_actor_durable, &observed, &current, &local_device)
            .unwrap_err(),
        SyncServiceError::LocalStateChanged
    );
}

fn local_changes(
    observed: &PersonalStateV2,
    local_device: &DeviceId,
    track: &PortableTrack,
) -> PersonalStateV2 {
    crate::personal_state::append_listening(
        observed,
        Some(local_device),
        crate::listening::ListeningOperation::SetResume {
            point: crate::listening::ResumePoint {
                track: track.clone(),
                position_ms: 120_000,
                provenance: crate::listening::ResumeProvenance {
                    playback_session_id: "local-session".to_owned(),
                    device_id: local_device.clone(),
                },
            },
        },
        1,
    )
    .and_then(|state| {
        crate::personal_state::append_listening(
            &state,
            Some(local_device),
            crate::listening::ListeningOperation::SetPassportNote {
                note: crate::listening::PassportNote {
                    station_uuid: "station".to_owned(),
                    note: "Local note".to_owned(),
                },
            },
            2,
        )
    })
    .and_then(|state| {
        crate::personal_state::append_listening(
            &state,
            Some(local_device),
            crate::listening::ListeningOperation::UpsertDjPreset {
                preset: crate::listening::DjPreset {
                    preset_id: crate::listening::DjPresetId::new("preset").unwrap(),
                    name: "Local preset".to_owned(),
                    snapshot: crate::streaming::TasteSnapshot::default(),
                },
            },
            3,
        )
    })
    .unwrap()
}

fn remote_changes(
    observed: &PersonalStateV2,
    remote_device: &DeviceId,
    track: &PortableTrack,
) -> PersonalStateV2 {
    crate::personal_state::append_listening(
        observed,
        Some(remote_device),
        crate::listening::ListeningOperation::ClearResume {
            clear: crate::listening::ResumeClear {
                track: track.clone(),
                provenance: crate::listening::ResumeProvenance {
                    playback_session_id: "remote-session".to_owned(),
                    device_id: remote_device.clone(),
                },
            },
        },
        1,
    )
    .and_then(|state| {
        crate::personal_state::append_listening(
            &state,
            Some(remote_device),
            crate::listening::ListeningOperation::SetPassportNote {
                note: crate::listening::PassportNote {
                    station_uuid: "station".to_owned(),
                    note: "Remote note".to_owned(),
                },
            },
            2,
        )
    })
    .and_then(|state| {
        crate::personal_state::append_listening(
            &state,
            Some(remote_device),
            crate::listening::ListeningOperation::UpsertDjPreset {
                preset: crate::listening::DjPreset {
                    preset_id: crate::listening::DjPresetId::new("preset").unwrap(),
                    name: "Remote preset".to_owned(),
                    snapshot: crate::streaming::TasteSnapshot::default(),
                },
            },
            3,
        )
    })
    .unwrap()
}
