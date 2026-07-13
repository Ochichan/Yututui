use super::*;
use serde_json::json;

fn eligible_facts() -> MediaFacts {
    MediaFacts {
        duration_secs: Some(LONG_FORM_MIN_DURATION_SECS),
        via_network: Some(true),
        seekable: Some(true),
        partially_seekable: Some(false),
        live: false,
    }
}
fn budget() -> StorageBudget {
    StorageBudget {
        available_bytes: 8 * 1024 * 1024 * 1024,
        volume_bytes: 100 * 1024 * 1024 * 1024,
        session_written_bytes: 0,
        rolling_written_bytes: 0,
        control_latency_ms: 1_000,
        rate: RateEvidence {
            fixture_max_bytes_per_sec: Some(1024 * 1024),
            ..RateEvidence::default()
        },
    }
}

fn active_storage() -> Result<ActiveStorageGuard, CacheReason> {
    let budget = budget();
    Ok(ActiveStorageGuard {
        available_bytes: budget.available_bytes,
        admission: budget.admission().unwrap(),
    })
}

fn controller(mode: LongFormSeekOptimization) -> LongFormCacheController {
    LongFormCacheController::new(
        ControllerCapability::Available(CacheOptionFamily::Modern),
        mode,
    )
}

fn active_on_controller() -> LongFormCacheController {
    let mut policy = controller(LongFormSeekOptimization::On);
    let CacheAction::SetCacheOnDisk { token, .. } = policy
        .begin_media(1, eligible_facts(), 0, Some(budget()))
        .expect("eligible On media starts activation")
    else {
        panic!("enable expected");
    };
    assert!(matches!(
        policy.set_reply(token, true, true, 1),
        Some(CacheAction::ReadBackCacheOnDisk { expected: true, .. })
    ));
    assert!(policy.readback_reply(token, true, Some(true), 2).is_none());
    assert!(
        policy
            .file_cache_sample(1, 3, active_storage(), 1, 1)
            .is_none()
    );
    assert_eq!(policy.status().effective, CacheEffectiveState::DiskActive);
    policy
}

#[test]
fn stable_public_ids_are_exact() {
    assert_eq!(CacheEffectiveState::DiskActive.id(), "disk_active");
    assert_eq!(CacheReason::AutoUncachedSeek.id(), "auto_uncached_seek");
    assert_eq!(CacheReason::DisableFailed.id(), "disable_failed");
}

#[test]
fn policy_and_fact_updates_without_media_remain_no_media() {
    let mut policy = controller(LongFormSeekOptimization::Off);
    assert!(
        policy
            .update_media_facts(eligible_facts(), 0, Some(budget()))
            .is_none()
    );
    assert!(
        policy
            .update_requested(LongFormSeekOptimization::On, 1, Some(budget()))
            .is_none()
    );
    assert_eq!(policy.status().requested, LongFormSeekOptimization::On);
    assert_eq!(policy.status().effective, CacheEffectiveState::NoMedia);
    assert_eq!(policy.status().reason, CacheReason::NoMedia);
}

#[test]
fn normalizes_unsorted_overlapping_and_touching_ranges() {
    let ranges = normalize_seekable_ranges(&json!({
        "seekable-ranges": [
            {"start": 20.0, "end": 30.0},
            {"start": 0.0, "end": 10.0},
            {"start": 9.99, "end": 21.0},
            {"start": 40.0, "end": 41.0}
        ]
    }))
    .unwrap();
    assert_eq!(ranges.len(), 2);
    assert_eq!(
        ranges[0],
        CachedRange {
            start: 0.0,
            end: 30.0
        }
    );
    assert!(target_is_cached(30.049, &ranges));
    assert!(!target_is_cached(30.051, &ranges));
}

#[test]
fn malformed_range_state_fails_closed() {
    for value in [
        json!({}),
        json!({"seekable-ranges": null}),
        json!({"seekable-ranges": [{"start": -1, "end": 2}]}),
        json!({"seekable-ranges": [{"start": 2, "end": 1}]}),
        json!({"seekable-ranges": [{"start": 1}]}),
    ] {
        assert_eq!(
            normalize_seekable_ranges(&value),
            Err(CacheReason::InvalidRangeState)
        );
    }
}

#[test]
fn duration_and_jump_boundaries_are_inclusive() {
    let mut short = controller(LongFormSeekOptimization::Auto);
    let mut facts = eligible_facts();
    facts.duration_secs = Some(LONG_FORM_MIN_DURATION_SECS - 0.001);
    short.begin_media(1, facts, 0, Some(budget()));
    assert_eq!(short.status().reason, CacheReason::ShortMedia);

    let mut exact = controller(LongFormSeekOptimization::Auto);
    exact.begin_media(1, eligible_facts(), 0, Some(budget()));
    assert!(
        exact
            .committed_interactive_seek(0.0, AUTO_MIN_JUMP_SECS - 0.001, 1)
            .is_none()
    );
    assert_eq!(exact.status().reason, CacheReason::SeekBelowThreshold);
    assert!(matches!(
        exact.committed_interactive_seek(0.0, AUTO_MIN_JUMP_SECS, 2),
        Some(CacheAction::QueryRanges(_))
    ));
}

#[test]
fn auto_probe_is_immutable_and_stale_reply_cannot_enable() {
    let mut policy = controller(LongFormSeekOptimization::Auto);
    policy.begin_media(7, eligible_facts(), 0, Some(budget()));
    let CacheAction::QueryRanges(probe) = policy
        .committed_interactive_seek(0.0, 600.0, 5_000)
        .unwrap()
    else {
        panic!("query expected");
    };
    policy.begin_media(8, eligible_facts(), 6_000, Some(budget()));
    assert!(
        policy
            .range_reply(
                probe,
                Ok(&json!({"seekable-ranges": []})),
                Some(budget()),
                6_001,
            )
            .is_none()
    );
    assert_eq!(policy.status().effective, CacheEffectiveState::RamOnly);
}

#[test]
fn auto_probe_cannot_outlive_eligibility_facts() {
    let mut policy = controller(LongFormSeekOptimization::Auto);
    policy.begin_media(7, eligible_facts(), 0, Some(budget()));
    let CacheAction::QueryRanges(probe) = policy
        .committed_interactive_seek(0.0, 600.0, 5_000)
        .expect("eligible miss starts a range probe")
    else {
        panic!("query expected");
    };

    let mut live = eligible_facts();
    live.live = true;
    assert!(
        policy
            .update_media_facts(live, 5_001, Some(budget()))
            .is_none()
    );
    assert_eq!(policy.status().effective, CacheEffectiveState::RamOnly);
    assert_eq!(policy.status().reason, CacheReason::LiveSource);
    assert!(
        policy
            .range_reply(
                probe,
                Ok(&json!({ "seekable-ranges": [] })),
                Some(budget()),
                5_002,
            )
            .is_none()
    );
    assert_eq!(policy.status().effective, CacheEffectiveState::RamOnly);

    // The reply path independently rechecks facts, so even a caller that cannot deliver the
    // intervening policy action never turns a revoked candidate into a true write.
    let mut direct = controller(LongFormSeekOptimization::Auto);
    direct.begin_media(8, eligible_facts(), 0, Some(budget()));
    let CacheAction::QueryRanges(probe) = direct
        .committed_interactive_seek(0.0, 600.0, 5_000)
        .expect("query expected")
    else {
        panic!("query expected");
    };
    direct.facts.seekable = Some(false);
    assert!(
        direct
            .range_reply(
                probe,
                Ok(&json!({ "seekable-ranges": [] })),
                Some(budget()),
                5_001,
            )
            .is_none()
    );
    assert_eq!(direct.status().reason, CacheReason::UnseekableSource);
    assert_eq!(direct.status().effective, CacheEffectiveState::RamOnly);
}

#[test]
fn auto_cached_target_stays_ram_only() {
    let mut policy = controller(LongFormSeekOptimization::Auto);
    policy.begin_media(1, eligible_facts(), 0, Some(budget()));
    let CacheAction::QueryRanges(probe) = policy
        .committed_interactive_seek(0.0, 600.0, 5_000)
        .unwrap()
    else {
        panic!("query expected");
    };
    assert!(
        policy
            .range_reply(
                probe,
                Ok(&json!({"seekable-ranges": [{"start": 599.0, "end": 601.0}]})),
                Some(budget()),
                5_001,
            )
            .is_none()
    );
    assert_eq!(policy.status().effective, CacheEffectiveState::RamOnly);
    assert_eq!(policy.status().reason, CacheReason::SeekWithinCachedRange);
}

#[test]
fn on_waits_for_facts_then_enables_once() {
    let mut policy = controller(LongFormSeekOptimization::On);
    assert!(
        policy
            .begin_media(1, MediaFacts::default(), 0, Some(budget()))
            .is_none()
    );
    assert_eq!(policy.status().reason, CacheReason::AwaitingMediaFacts);
    assert!(matches!(
        policy.update_media_facts(eligible_facts(), 1, Some(budget())),
        Some(CacheAction::SetCacheOnDisk { enabled: true, .. })
    ));
    assert!(
        policy
            .update_media_facts(eligible_facts(), 2, Some(budget()))
            .is_none()
    );
}

#[test]
fn on_requires_an_explicit_partial_seekability_fact_before_enabling() {
    let mut policy = controller(LongFormSeekOptimization::On);
    let mut facts = eligible_facts();
    facts.partially_seekable = None;
    assert!(policy.begin_media(1, facts, 0, Some(budget())).is_none());
    assert_eq!(policy.status().reason, CacheReason::AwaitingMediaFacts);

    facts.partially_seekable = Some(true);
    assert!(
        policy
            .update_media_facts(facts, 1, Some(budget()))
            .is_none()
    );
    assert_eq!(
        policy.status().reason,
        CacheReason::PartiallySeekableUnproven
    );

    facts.partially_seekable = Some(false);
    assert!(matches!(
        policy.update_media_facts(facts, 2, Some(budget())),
        Some(CacheAction::SetCacheOnDisk { enabled: true, .. })
    ));
}

#[test]
fn eligibility_loss_disables_pending_or_active_writers() {
    let mut pending = controller(LongFormSeekOptimization::On);
    let CacheAction::SetCacheOnDisk { token: old, .. } = pending
        .begin_media(1, eligible_facts(), 0, Some(budget()))
        .expect("enable expected")
    else {
        panic!("enable expected");
    };
    let mut short = eligible_facts();
    short.duration_secs = Some(LONG_FORM_MIN_DURATION_SECS - 0.001);
    assert!(matches!(
        pending.update_media_facts(short, 1, Some(budget())),
        Some(CacheAction::SetCacheOnDisk { enabled: false, .. })
    ));
    assert_eq!(pending.status().reason, CacheReason::ShortMedia);
    assert!(pending.set_reply(old, true, true, 2).is_none());

    for (facts, reason) in [
        (
            MediaFacts {
                via_network: Some(false),
                ..eligible_facts()
            },
            CacheReason::LocalSource,
        ),
        (
            MediaFacts {
                seekable: Some(false),
                ..eligible_facts()
            },
            CacheReason::UnseekableSource,
        ),
        (
            MediaFacts {
                partially_seekable: Some(true),
                ..eligible_facts()
            },
            CacheReason::PartiallySeekableUnproven,
        ),
        (
            MediaFacts {
                live: true,
                ..eligible_facts()
            },
            CacheReason::LiveSource,
        ),
    ] {
        let mut active = active_on_controller();
        assert!(matches!(
            active.update_media_facts(facts, 10, Some(budget())),
            Some(CacheAction::SetCacheOnDisk { enabled: false, .. })
        ));
        assert_eq!(
            active.status().effective,
            CacheEffectiveState::DisablePending
        );
        assert_eq!(active.status().reason, reason);
    }
}

#[test]
fn on_to_auto_disables_both_in_flight_and_active_on_writers() {
    let mut pending = controller(LongFormSeekOptimization::On);
    let CacheAction::SetCacheOnDisk { token: old, .. } = pending
        .begin_media(1, eligible_facts(), 0, Some(budget()))
        .expect("enable expected")
    else {
        panic!("enable expected");
    };
    assert!(matches!(
        pending.update_requested(LongFormSeekOptimization::Auto, 1, Some(budget())),
        Some(CacheAction::SetCacheOnDisk { enabled: false, .. })
    ));
    assert_eq!(pending.status().requested, LongFormSeekOptimization::Auto);
    assert_eq!(
        pending.status().effective,
        CacheEffectiveState::DisablePending
    );
    assert!(pending.set_reply(old, true, true, 2).is_none());

    let mut active = active_on_controller();
    assert!(matches!(
        active.update_requested(LongFormSeekOptimization::Auto, 10, Some(budget())),
        Some(CacheAction::SetCacheOnDisk { enabled: false, .. })
    ));
    assert_eq!(active.status().requested, LongFormSeekOptimization::Auto);
    assert_eq!(
        active.status().effective,
        CacheEffectiveState::DisablePending
    );
    assert_eq!(active.status().reason, CacheReason::SequentialPlayback);
}

#[test]
fn activation_requires_reply_readback_and_file_growth() {
    let mut policy = controller(LongFormSeekOptimization::On);
    let CacheAction::SetCacheOnDisk { token, .. } = policy
        .begin_media(1, eligible_facts(), 0, Some(budget()))
        .unwrap()
    else {
        panic!("enable expected");
    };
    assert!(matches!(
        policy.set_reply(token, true, true, 1),
        Some(CacheAction::ReadBackCacheOnDisk { expected: true, .. })
    ));
    policy.readback_reply(token, true, Some(true), 2);
    policy.file_cache_sample(0, 100, active_storage(), 0, 0);
    assert_eq!(
        policy.status().effective,
        CacheEffectiveState::EnablePending
    );
    policy.file_cache_sample(1, 200, active_storage(), 0, 0);
    assert_eq!(policy.status().effective, CacheEffectiveState::DiskActive);
}

#[test]
fn rejected_enable_is_compensated_and_false_is_fully_proven() {
    let mut policy = controller(LongFormSeekOptimization::On);
    let CacheAction::SetCacheOnDisk { token, .. } = policy
        .begin_media(1, eligible_facts(), 0, Some(budget()))
        .unwrap()
    else {
        panic!("enable expected");
    };
    let CacheAction::SetCacheOnDisk {
        token: disable,
        enabled: false,
    } = policy
        .set_reply(token, true, false, 100)
        .expect("even a rejected true write is ambiguous and must be compensated")
    else {
        panic!("compensating disable expected");
    };
    assert_eq!(
        policy.status().effective,
        CacheEffectiveState::DisablePending
    );
    assert_eq!(policy.status().reason, CacheReason::PropertyRejected);

    assert!(matches!(
        policy.set_reply(disable, false, true, 101),
        Some(CacheAction::ReadBackCacheOnDisk {
            expected: false,
            ..
        })
    ));
    assert!(
        policy
            .readback_reply(disable, false, Some(false), 102)
            .is_none()
    );
    policy.file_cache_sample(0, 102, active_storage(), 0, 0);
    policy.file_cache_sample(0, 1_102, active_storage(), 0, 0);
    policy.file_cache_sample(0, 2_102, active_storage(), 0, 0);
    assert_eq!(
        policy.status().effective,
        CacheEffectiveState::LatchedUntilClose
    );
    assert_eq!(policy.status().reason, CacheReason::PropertyRejected);
}

#[test]
fn enable_timeout_and_ambiguous_readback_both_compensate() {
    let mut timed_out = controller(LongFormSeekOptimization::On);
    let CacheAction::SetCacheOnDisk { token, .. } = timed_out
        .begin_media(1, eligible_facts(), 0, Some(budget()))
        .unwrap()
    else {
        panic!("enable expected");
    };
    assert!(matches!(
        timed_out.property_timeout(token, true, ENABLE_DEADLINE_MS),
        Some(CacheAction::SetCacheOnDisk { enabled: false, .. })
    ));
    assert_eq!(timed_out.status().reason, CacheReason::PropertyTimeout);

    let mut ambiguous = controller(LongFormSeekOptimization::On);
    let CacheAction::SetCacheOnDisk { token, .. } = ambiguous
        .begin_media(1, eligible_facts(), 0, Some(budget()))
        .unwrap()
    else {
        panic!("enable expected");
    };
    assert!(matches!(
        ambiguous.set_reply(token, true, true, 1),
        Some(CacheAction::ReadBackCacheOnDisk { expected: true, .. })
    ));
    assert!(matches!(
        ambiguous.readback_reply(token, true, None, 2),
        Some(CacheAction::SetCacheOnDisk { enabled: false, .. })
    ));
    assert_eq!(
        ambiguous.status().reason,
        CacheReason::PropertyVerificationFailed
    );
}

#[test]
fn file_evidence_has_a_total_enable_deadline_and_late_growth_cannot_promote() {
    let mut policy = controller(LongFormSeekOptimization::On);
    let CacheAction::SetCacheOnDisk { token, .. } = policy
        .begin_media(1, eligible_facts(), 0, Some(budget()))
        .unwrap()
    else {
        panic!("enable expected");
    };
    policy.set_reply(token, true, true, 1);
    policy.readback_reply(token, true, Some(true), 2);
    assert!(matches!(
        policy.file_cache_sample(1, ENABLE_DEADLINE_MS, active_storage(), 0, 0,),
        Some(CacheAction::SetCacheOnDisk { enabled: false, .. })
    ));
    assert_eq!(
        policy.status().effective,
        CacheEffectiveState::DisablePending
    );
    assert_eq!(
        policy.status().reason,
        CacheReason::PropertyVerificationFailed
    );
}

#[test]
fn active_storage_and_watchdog_failures_begin_disable() {
    let mut pending = controller(LongFormSeekOptimization::On);
    pending.begin_media(1, eligible_facts(), 0, Some(budget()));
    assert!(matches!(
        pending.file_cache_sample(0, 1, Err(CacheReason::CacheRootUnavailable), 0, 0),
        Some(CacheAction::SetCacheOnDisk { enabled: false, .. })
    ));
    assert_eq!(pending.status().reason, CacheReason::CacheRootUnavailable);

    let mut active = controller(LongFormSeekOptimization::On);
    let CacheAction::SetCacheOnDisk { token, .. } = active
        .begin_media(1, eligible_facts(), 0, Some(budget()))
        .unwrap()
    else {
        panic!("enable expected");
    };
    active.set_reply(token, true, true, 1);
    active.readback_reply(token, true, Some(true), 2);
    active.file_cache_sample(1, 3, active_storage(), 0, 0);
    assert_eq!(active.status().effective, CacheEffectiveState::DiskActive);
    assert!(matches!(
        active.file_cache_sample(2, 4, Err(CacheReason::InsufficientFreeSpace), 0, 0),
        Some(CacheAction::SetCacheOnDisk { enabled: false, .. })
    ));
    assert_eq!(active.status().reason, CacheReason::InsufficientFreeSpace);

    let mut watchdog = controller(LongFormSeekOptimization::On);
    watchdog.begin_media(1, eligible_facts(), 0, Some(budget()));
    assert!(matches!(
        watchdog.active_watchdog_failure(1, CacheReason::PropertyTimeout),
        Some(CacheAction::SetCacheOnDisk { enabled: false, .. })
    ));
}

#[test]
fn disable_pending_monitor_failure_escalates_instead_of_becoming_unowned() {
    let mut policy = active_on_controller();
    let disable = policy
        .update_requested(LongFormSeekOptimization::Off, 10, Some(budget()))
        .expect("Off begins disable");
    assert!(matches!(
        disable,
        CacheAction::SetCacheOnDisk { enabled: false, .. }
    ));
    assert!(matches!(
        policy.active_watchdog_failure(11, CacheReason::PropertyTimeout),
        Some(CacheAction::EmergencyCloseAndResume {
            reason: CacheReason::PropertyTimeout,
            ..
        })
    ));

    let mut stat_failure = active_on_controller();
    stat_failure.update_requested(LongFormSeekOptimization::Off, 10, Some(budget()));
    assert!(matches!(
        stat_failure.file_cache_sample(1, 11, Err(CacheReason::CacheRootUnavailable), 0, 0,),
        Some(CacheAction::EmergencyCloseAndResume {
            reason: CacheReason::CacheRootUnavailable,
            ..
        })
    ));
}

#[test]
fn forced_ram_only_applies_to_one_media_without_changing_requested_policy() {
    let mut policy = controller(LongFormSeekOptimization::On);
    policy.force_next_media_ram_only();
    assert!(
        policy
            .begin_media(1, eligible_facts(), 0, Some(budget()))
            .is_none()
    );
    assert_eq!(policy.status().requested, LongFormSeekOptimization::On);
    assert_eq!(policy.status().effective, CacheEffectiveState::RamOnly);
    assert_eq!(policy.status().reason, CacheReason::DisableFailed);

    let mut changed_facts = eligible_facts();
    changed_facts.duration_secs = Some(LONG_FORM_MIN_DURATION_SECS * 2.0);
    assert!(
        policy
            .update_media_facts(changed_facts, 1, Some(budget()))
            .is_none()
    );
    assert!(
        policy
            .committed_interactive_seek(0.0, AUTO_MIN_JUMP_SECS, 2)
            .is_none()
    );
    assert!(
        policy
            .update_requested(LongFormSeekOptimization::Off, 3, Some(budget()))
            .is_none()
    );
    assert!(
        policy
            .update_requested(LongFormSeekOptimization::On, 4, Some(budget()))
            .is_none()
    );
    assert_eq!(policy.status().requested, LongFormSeekOptimization::On);
    assert_eq!(policy.status().effective, CacheEffectiveState::RamOnly);
    assert_eq!(policy.status().reason, CacheReason::DisableFailed);

    policy.close_media();
    assert!(matches!(
        policy.begin_media(2, eligible_facts(), 5, Some(budget())),
        Some(CacheAction::SetCacheOnDisk { enabled: true, .. })
    ));
}

#[test]
fn off_during_enable_compensates_and_stale_success_cannot_promote() {
    let mut policy = controller(LongFormSeekOptimization::On);
    let CacheAction::SetCacheOnDisk { token: old, .. } = policy
        .begin_media(1, eligible_facts(), 0, Some(budget()))
        .unwrap()
    else {
        panic!("enable expected");
    };
    let action = policy
        .update_requested(LongFormSeekOptimization::Off, 10, Some(budget()))
        .unwrap();
    assert!(matches!(
        action,
        CacheAction::SetCacheOnDisk { enabled: false, .. }
    ));
    assert!(policy.set_reply(old, true, true, 11).is_none());
    assert_eq!(
        policy.status().effective,
        CacheEffectiveState::DisablePending
    );
}

#[test]
fn disable_latches_only_after_ack_readback_and_three_stable_samples_over_two_seconds() {
    let mut policy = controller(LongFormSeekOptimization::On);
    let CacheAction::SetCacheOnDisk { token, .. } = policy
        .begin_media(1, eligible_facts(), 0, Some(budget()))
        .unwrap()
    else {
        panic!("enable expected");
    };
    policy.set_reply(token, true, true, 0);
    policy.readback_reply(token, true, Some(true), 0);
    policy.file_cache_sample(10, 1, active_storage(), 0, 0);
    let CacheAction::SetCacheOnDisk {
        token: disable,
        enabled: false,
    } = policy
        .update_requested(LongFormSeekOptimization::Off, 100, Some(budget()))
        .unwrap()
    else {
        panic!("disable expected");
    };
    policy.set_reply(disable, false, true, 100);
    policy.file_cache_sample(10, 100, active_storage(), 0, 0);
    policy.file_cache_sample(10, 1_100, active_storage(), 0, 0);
    policy.file_cache_sample(10, 2_100, active_storage(), 0, 0);
    policy.readback_reply(disable, false, Some(false), 2_100);
    policy.file_cache_sample(10, 2_100, active_storage(), 0, 0);
    policy.file_cache_sample(10, 3_100, active_storage(), 0, 0);
    assert_eq!(
        policy.status().effective,
        CacheEffectiveState::DisablePending
    );
    policy.file_cache_sample(10, 4_100, active_storage(), 0, 0);
    assert_eq!(
        policy.status().effective,
        CacheEffectiveState::LatchedUntilClose
    );
}

#[test]
fn policy_change_during_safety_disable_keeps_the_existing_false_proof_owned() {
    let mut policy = active_on_controller();
    let CacheAction::SetCacheOnDisk {
        token: disable,
        enabled: false,
    } = policy
        .file_cache_sample(CACHE_SOFT_TARGET_BYTES, 10, active_storage(), 0, 0)
        .expect("soft cap begins disable")
    else {
        panic!("disable expected");
    };

    assert!(
        policy
            .update_requested(LongFormSeekOptimization::Off, 11, Some(budget()))
            .is_none()
    );
    assert_eq!(
        policy.status().effective,
        CacheEffectiveState::DisablePending
    );
    assert!(matches!(
        policy.set_reply(disable, false, true, 12),
        Some(CacheAction::ReadBackCacheOnDisk {
            expected: false,
            ..
        })
    ));
    assert!(
        policy
            .readback_reply(disable, false, Some(false), 13)
            .is_none()
    );
}

#[test]
fn disable_timeout_requests_position_preserving_emergency_close() {
    let mut policy = controller(LongFormSeekOptimization::On);
    let CacheAction::SetCacheOnDisk { token, .. } = policy
        .begin_media(1, eligible_facts(), 0, Some(budget()))
        .unwrap()
    else {
        panic!("enable expected");
    };
    policy.set_reply(token, true, true, 0);
    policy.readback_reply(token, true, Some(true), 0);
    policy.file_cache_sample(10, 1, active_storage(), 0, 0);
    policy.observe_transport(913.25, true);
    policy.update_requested(LongFormSeekOptimization::Off, 100, Some(budget()));
    assert!(matches!(
        policy.tick(5_100),
        Some(CacheAction::EmergencyCloseAndResume {
            file_generation: 1,
            position_secs: 913.25,
            paused: true,
            reason: CacheReason::DisableFailed,
        })
    ));
}

#[test]
fn replacement_cannot_clear_an_existing_emergency_recycle() {
    let mut policy = LongFormCacheController::new(
        ControllerCapability::Available(CacheOptionFamily::Modern),
        LongFormSeekOptimization::On,
    );
    let enable = policy
        .begin_media(9, eligible_facts(), 0, Some(budget()))
        .expect("On policy starts activation");
    let CacheAction::SetCacheOnDisk { token, .. } = enable else {
        panic!("expected enable action");
    };
    policy.observe_transport(3_600.25, true);
    let readback = policy
        .set_reply(token, true, true, 1)
        .expect("accepted enable requires readback");
    assert!(matches!(
        readback,
        CacheAction::ReadBackCacheOnDisk {
            token: readback_token,
            expected: true,
        } if readback_token == token
    ));
    assert!(policy.readback_reply(token, true, Some(true), 1).is_none());
    assert!(
        policy
            .file_cache_sample(1, 1, active_storage(), 1, 1)
            .is_none()
    );
    assert_eq!(policy.status().effective, CacheEffectiveState::DiskActive);
    let disable = policy.update_requested(LongFormSeekOptimization::Off, 10, Some(budget()));
    let CacheAction::SetCacheOnDisk {
        token: disable_token,
        enabled: false,
    } = disable.expect("Off disables active cache")
    else {
        panic!("expected disable action");
    };
    let emergency = policy
        .property_timeout(disable_token, false, 11)
        .expect("disable timeout requires recycle");
    assert!(matches!(
        emergency,
        CacheAction::EmergencyCloseAndResume {
            file_generation: 9,
            position_secs,
            paused: true,
            ..
        } if (position_secs - 3_600.25).abs() < f64::EPSILON
    ));
    assert!(matches!(
        policy.prepare_replacement(20),
        Some(CacheAction::EmergencyCloseAndResume {
            file_generation: 9,
            position_secs,
            paused: true,
            ..
        }) if (position_secs - 3_600.25).abs() < f64::EPSILON
    ));
    assert_eq!(
        policy.status().effective,
        CacheEffectiveState::EmergencyClosePending
    );
}

#[test]
fn admission_is_fail_closed_and_inclusive_at_boundary() {
    let mut no_rate = budget();
    no_rate.rate = RateEvidence::default();
    assert_eq!(no_rate.admission(), Err(CacheReason::UnsafeRateBound));
    no_rate.rate.cache_speed_bytes_per_sec = Some(16 * 1024 * 1024);
    assert_eq!(
        no_rate.admission().unwrap().rate_bound_bytes_per_sec,
        32 * 1024 * 1024
    );

    let mut raw_only = budget();
    raw_only.rate = RateEvidence {
        raw_input_rate_bytes_per_sec: Some(64 * 1024 * 1024),
        ..RateEvidence::default()
    };
    assert_eq!(raw_only.admission(), Err(CacheReason::UnsafeRateBound));

    let mut measured = budget();
    measured.rate = RateEvidence {
        measured_file_delta_bytes_per_sec: Some(24 * 1024 * 1024),
        raw_input_rate_bytes_per_sec: Some(16 * 1024 * 1024),
        ..RateEvidence::default()
    };
    assert_eq!(
        measured.admission().unwrap().rate_bound_bytes_per_sec,
        32 * 1024 * 1024
    );

    let admission = budget().admission().unwrap();
    let mut exact = budget();
    exact.available_bytes = admission.required_available_bytes;
    assert!(exact.admission().is_ok());
    exact.available_bytes -= 1;
    assert_eq!(exact.admission(), Err(CacheReason::InsufficientFreeSpace));
}

#[test]
fn on_can_activate_from_proven_runtime_cache_speed_without_a_fixture_ceiling() {
    let mut runtime_budget = budget();
    runtime_budget.rate = RateEvidence {
        cache_speed_bytes_per_sec: Some(2 * 1024 * 1024),
        ..RateEvidence::default()
    };
    assert!(matches!(
        controller(LongFormSeekOptimization::On).begin_media(
            1,
            eligible_facts(),
            0,
            Some(runtime_budget),
        ),
        Some(CacheAction::SetCacheOnDisk { enabled: true, .. })
    ));
}

#[test]
fn active_guard_consumes_admitted_headroom_until_the_reserve_boundary() {
    let admission = budget().admission().unwrap();
    let mut active = budget();
    active.available_bytes = admission.required_available_bytes - 1;
    let guard = active
        .active_guard()
        .expect("active use does not re-run fresh admission");
    assert_eq!(guard.admission, admission);

    active.available_bytes = admission.reserve_bytes + admission.overshoot_bytes;
    let guard = active.active_guard().unwrap();
    assert_eq!(
        guard.available_bytes,
        guard.admission.reserve_bytes + guard.admission.overshoot_bytes
    );
}

#[test]
fn reserve_uses_larger_of_one_gib_and_five_percent() {
    let small = budget().admission().unwrap();
    assert_eq!(small.reserve_bytes, 5 * 1024 * 1024 * 1024);
    let mut tiny_volume = budget();
    tiny_volume.volume_bytes = 10 * 1024 * 1024 * 1024;
    let tiny = tiny_volume.admission().unwrap();
    assert_eq!(tiny.reserve_bytes, FREE_SPACE_RESERVE_BYTES);
}

#[test]
fn partial_seekability_requires_same_generation_proof() {
    let mut policy = controller(LongFormSeekOptimization::On);
    let mut facts = eligible_facts();
    facts.partially_seekable = Some(true);
    policy.begin_media(1, facts, 0, Some(budget()));
    assert_eq!(
        policy.status().reason,
        CacheReason::PartiallySeekableUnproven
    );
    let query = policy
        .committed_interactive_seek(0.0, AUTO_MIN_JUMP_SECS, 1)
        .expect("large partial-source seek queries its pre-seek range");
    let CacheAction::QueryRanges(probe) = query else {
        panic!("expected range query");
    };
    assert!(
        policy
            .range_reply(
                probe,
                Ok(&json!({ "seekable-ranges": [] })),
                Some(budget()),
                2,
            )
            .is_none()
    );
    assert!(matches!(
        policy.mark_off_cache_seek_succeeded(AUTO_MIN_JUMP_SECS, Some(budget()), 3),
        Some(CacheAction::SetCacheOnDisk { enabled: true, .. })
    ));
    policy.begin_media(2, facts, 2, Some(budget()));
    assert_eq!(
        policy.status().reason,
        CacheReason::PartiallySeekableUnproven
    );
}

#[test]
fn unconfirmed_interactive_target_never_becomes_emergency_resume_position() {
    let mut policy = controller(LongFormSeekOptimization::Auto);
    policy.begin_media(4, eligible_facts(), 0, Some(budget()));
    policy.observe_transport(120.5, false);
    assert!(matches!(
        policy.committed_interactive_seek(120.5, 900.0, 0),
        Some(CacheAction::QueryRanges(_))
    ));
    assert!(matches!(
        policy.emergency(CacheReason::DisableFailed),
        Some(CacheAction::EmergencyCloseAndResume {
            position_secs,
            paused: false,
            ..
        }) if (position_secs - 120.5).abs() < f64::EPSILON
    ));
}

#[test]
fn unsupported_override_and_read_only_are_truthful() {
    for (capability, effective, reason) in [
        (
            ControllerCapability::Unavailable(CacheReason::UnsupportedMpv),
            CacheEffectiveState::Unavailable,
            CacheReason::UnsupportedMpv,
        ),
        (
            ControllerCapability::Unavailable(CacheReason::ReadOnlyInstance),
            CacheEffectiveState::Unavailable,
            CacheReason::ReadOnlyInstance,
        ),
        (
            ControllerCapability::Overridden,
            CacheEffectiveState::Overridden,
            CacheReason::CustomMpvOverride,
        ),
    ] {
        let mut policy = LongFormCacheController::new(capability, LongFormSeekOptimization::On);
        policy.begin_media(1, eligible_facts(), 0, Some(budget()));
        assert_eq!(policy.status().effective, effective);
        assert_eq!(policy.status().reason, reason);
        for requested in [
            LongFormSeekOptimization::Off,
            LongFormSeekOptimization::Auto,
            LongFormSeekOptimization::On,
        ] {
            assert!(
                policy
                    .update_requested(requested, 1, Some(budget()))
                    .is_none()
            );
            assert_eq!(policy.status().requested, requested);
            assert_eq!(policy.status().effective, effective);
            assert_eq!(policy.status().reason, reason);
        }
    }
}
