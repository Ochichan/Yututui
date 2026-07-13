use super::*;

fn cache_runtime_for_correlation_test() -> CacheRuntime {
    CacheRuntime::new(
        crate::player::cache_support::CacheSpawnSupport {
            capability: crate::player::long_form_seek::ControllerCapability::Available(
                crate::player::long_form_seek::CacheOptionFamily::Modern,
            ),
            option_family: Some(crate::player::long_form_seek::CacheOptionFamily::Modern),
            override_source: None,
            cache_dir: None,
            spawn_args: Vec::new(),
        },
        crate::config::LongFormSeekOptimization::On,
    )
}

fn observe_eligible_long_form_facts(emit: &EventSink, state: &mut DispatchState) {
    for line in [
        r#"{"event":"property-change","name":"duration","data":3600.0}"#,
        r#"{"event":"property-change","name":"demuxer-via-network","data":true}"#,
        r#"{"event":"property-change","name":"seekable","data":true}"#,
        r#"{"event":"property-change","name":"partially-seekable","data":false}"#,
    ] {
        dispatch_incoming(line, emit, state);
    }
}

#[test]
fn cache_replays_current_media_facts_observed_before_load_reply_correlation() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let mut state = DispatchState {
        issued_file_generation: 7,
        cache: Some(cache_runtime_for_correlation_test()),
        ..DispatchState::default()
    };
    state
        .media_source_contexts
        .insert(7, crate::player::MediaSourceContext::OnDemand);
    assert!(remember_pending_load(&mut state, 11, 7, "loadfile"));

    dispatch_incoming(
        r#"{"event":"start-file","playlist_entry_id":42}"#,
        &emit,
        &mut state,
    );
    observe_eligible_long_form_facts(&emit, &mut state);
    assert_eq!(
        state.cache.as_ref().expect("cache runtime").status().reason,
        crate::player::long_form_seek::CacheReason::NoMedia
    );

    dispatch_incoming(
        r#"{"error":"success","request_id":11,"data":{"playlist_entry_id":42}}"#,
        &emit,
        &mut state,
    );

    let status = state.cache.as_ref().expect("cache runtime").status();
    assert_eq!(status.file_generation, Some(7));
    assert_ne!(
        status.reason,
        crate::player::long_form_seek::CacheReason::AwaitingMediaFacts
    );
}

#[test]
fn finite_seekable_live_context_stays_ram_only_after_fact_replay() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let mut state = DispatchState {
        issued_file_generation: 8,
        cache: Some(cache_runtime_for_correlation_test()),
        ..DispatchState::default()
    };
    state
        .media_source_contexts
        .insert(8, crate::player::MediaSourceContext::Live);
    assert!(remember_pending_load(&mut state, 18, 8, "loadfile"));

    dispatch_incoming(
        r#"{"event":"start-file","playlist_entry_id":81}"#,
        &emit,
        &mut state,
    );
    observe_eligible_long_form_facts(&emit, &mut state);
    dispatch_incoming(
        r#"{"error":"success","request_id":18,"data":{"playlist_entry_id":81}}"#,
        &emit,
        &mut state,
    );

    let status = state.cache.as_ref().expect("cache runtime").status();
    assert_eq!(
        status.effective,
        crate::player::long_form_seek::CacheEffectiveState::RamOnly
    );
    assert_eq!(
        status.reason,
        crate::player::long_form_seek::CacheReason::LiveSource
    );
    assert!(state.cache_actions.is_empty());
}

#[test]
fn stale_generation_facts_cannot_activate_cache_for_newer_admission() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let mut state = DispatchState {
        issued_file_generation: 2,
        cache: Some(cache_runtime_for_correlation_test()),
        ..DispatchState::default()
    };
    state
        .media_source_contexts
        .insert(1, crate::player::MediaSourceContext::OnDemand);
    state
        .media_source_contexts
        .insert(2, crate::player::MediaSourceContext::Live);
    assert!(remember_pending_load(&mut state, 11, 1, "loadfile"));

    dispatch_incoming(
        r#"{"event":"start-file","playlist_entry_id":41}"#,
        &emit,
        &mut state,
    );
    observe_eligible_long_form_facts(&emit, &mut state);
    dispatch_incoming(
        r#"{"error":"success","request_id":11,"data":{"playlist_entry_id":41}}"#,
        &emit,
        &mut state,
    );
    assert_eq!(
        state
            .cache
            .as_ref()
            .expect("cache runtime")
            .status()
            .file_generation,
        None
    );

    assert!(remember_pending_load(&mut state, 12, 2, "loadfile"));
    dispatch_incoming(
        r#"{"event":"start-file","playlist_entry_id":42}"#,
        &emit,
        &mut state,
    );
    observe_eligible_long_form_facts(&emit, &mut state);
    dispatch_incoming(
        r#"{"error":"success","request_id":12,"data":{"playlist_entry_id":42}}"#,
        &emit,
        &mut state,
    );

    let status = state.cache.as_ref().expect("cache runtime").status();
    assert_eq!(status.file_generation, Some(2));
    assert_eq!(
        status.reason,
        crate::player::long_form_seek::CacheReason::LiveSource
    );
}

#[test]
fn reply_before_start_file_still_begins_correlated_cache_generation() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let mut state = DispatchState {
        issued_file_generation: 9,
        cache: Some(cache_runtime_for_correlation_test()),
        ..DispatchState::default()
    };
    state
        .media_source_contexts
        .insert(9, crate::player::MediaSourceContext::Live);
    assert!(remember_pending_load(&mut state, 19, 9, "loadfile"));
    dispatch_incoming(
        r#"{"error":"success","request_id":19,"data":{"playlist_entry_id":91}}"#,
        &emit,
        &mut state,
    );

    dispatch_incoming(
        r#"{"event":"start-file","playlist_entry_id":91}"#,
        &emit,
        &mut state,
    );

    let status = state.cache.as_ref().expect("cache runtime").status();
    assert_eq!(status.file_generation, Some(9));
    assert_eq!(
        status.reason,
        crate::player::long_form_seek::CacheReason::LiveSource
    );
}
