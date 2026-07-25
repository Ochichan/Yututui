async fn begin_or_dispatch_command(
    conn: &Stream,
    emit: &EventSink,
    state: &mut DispatchState,
    request_id: &mut u64,
    file_generation_rx: &watch::Receiver<u64>,
    route_provider: &crate::playback_target::PlaybackRouteProviderHandle,
    cmd: PlayerCmd,
) -> io::Result<Option<PendingLoadValidation>> {
    if let PlayerCmd::SetLongFormSeekOptimization(requested) = &cmd {
        let action = state
            .cache
            .as_mut()
            .and_then(|cache| cache.update_requested(*requested));
        queue_cache_action(state, action);
        return Ok(None);
    }
    let load = match cmd {
        PlayerCmd::Load(load) => Some((load.destination().clone(), load.source_context(), None)),
        PlayerCmd::LoadWithResume(resume) => Some((
            resume.destination.clone(),
            resume.source_context,
            Some(resume),
        )),
        cmd => {
            if matches!(cmd, PlayerCmd::Stop) {
                state.issued_file_generation = reserve_file_generation(state);
            }
            dispatch_command(conn, emit, state, request_id, cmd, None).await?;
            None
        }
    };
    if let Some((destination, source_context, resume)) = load {
        *request_id = request_id.wrapping_add(1);
        // The public handle has already admitted this generation, but the actor does not publish
        // it as issued until validation has succeeded and `loadfile` is actually dispatched.
        // A seek/pause that supersedes recovery validation can therefore keep using the ready
        // current generation instead of waiting forever for a file that was never sent to mpv.
        let file_generation = reserve_file_generation(state);
        let load_request_id = *request_id;
        let task = tokio::spawn(validate_load_until_superseded(
            destination,
            file_generation,
            file_generation_rx.clone(),
            route_provider.clone(),
        ));
        return Ok(Some(PendingLoadValidation {
            request_id: load_request_id,
            file_generation,
            task,
            resume: resume::ResumeLoad::from_request(resume),
            source_context,
        }));
    }
    Ok(None)
}

fn reserve_file_generation(state: &mut DispatchState) -> u64 {
    state.admitted_file_generation = state
        .admitted_file_generation
        .max(state.issued_file_generation)
        .wrapping_add(1);
    state.admitted_file_generation
}

async fn validate_load_until_superseded(
    destination: crate::playback_target::PlaybackDestination,
    file_generation: u64,
    mut file_generation_rx: watch::Receiver<u64>,
    route_provider: crate::playback_target::PlaybackRouteProviderHandle,
) -> LoadValidationOutcome {
    let validation = async move {
        match destination {
            crate::playback_target::PlaybackDestination::Direct(target) => {
                crate::playback_target::validate_playback_target_for_handoff(&target)
                    .await
                    .map(|url| (url, None))
                    .map_err(|error| error.handoff_reason())
            }
            crate::playback_target::PlaybackDestination::Credentialed(target) => route_provider
                .open_route(target, file_generation)
                .await
                .map(|route| {
                    let (url, lease) = route.into_parts();
                    (url.into_string(), Some(lease))
                })
                .map_err(|error| error.reason()),
        }
    };
    tokio::pin!(validation);

    loop {
        if *file_generation_rx.borrow_and_update() > file_generation {
            return LoadValidationOutcome::Superseded;
        }
        tokio::select! {
            changed = file_generation_rx.changed() => {
                if changed.is_err() {
                    return LoadValidationOutcome::Superseded;
                }
            }
            result = &mut validation => {
                // An admission can race the validation future becoming ready. Re-read the
                // watch value at the commit boundary so unbiased select fairness cannot revive
                // a superseded URL.
                if *file_generation_rx.borrow_and_update() > file_generation {
                    return LoadValidationOutcome::Superseded;
                }
                return match result {
                    Ok((url, route_lease)) => {
                        LoadValidationOutcome::Validated { url, route_lease }
                    }
                    Err(reason) => LoadValidationOutcome::Rejected(reason.to_owned()),
                };
            }
        }
    }
}

fn install_resume_state(
    state: &mut DispatchState,
    file_generation: u64,
    request: super::recovery::LoadWithResume,
) {
    state.last_confirmed_time = request.position_secs;
    state.resume.install(file_generation, request);
}

async fn dispatch_validated_load(
    conn: &Stream,
    state: &mut DispatchState,
    request_id: &mut u64,
    load: ValidatedLoad,
) -> io::Result<()> {
    if let Some(lease) = load.route_lease {
        let installed = state
            .route_revocations
            .install(load.file_generation, lease.revocation_handle());
        if !installed {
            record_resume_outcome(
                load.resume.owned_request(),
                super::diagnostics::SourceRecoveryOutcome::Superseded,
            );
            return Ok(());
        }
        state
            .playback_route_leases
            .insert(load.file_generation, lease);
    }
    if load
        .resume
        .request()
        .is_some_and(super::recovery::LoadWithResume::forces_ram_only)
        && let Some(cache) = state.cache.as_mut()
    {
        cache.force_next_media_ram_only();
    }
    if state.media_source_contexts.len() >= ENTRY_GENERATION_CAPACITY {
        let oldest = state
            .media_source_contexts
            .keys()
            .filter(|generation| **generation != load.file_generation)
            .min()
            .copied();
        if let Some(oldest) = oldest {
            state.media_source_contexts.remove(&oldest);
        }
    }
    state
        .media_source_contexts
        .insert(load.file_generation, load.source_context);
    reset_file_state(state);
    if state.playlist_identity_mode != PlaylistIdentityMode::EntryIds
        && state.legacy_loads.len() >= LEGACY_LOAD_CAPACITY
    {
        return Err(io::Error::other(
            "legacy mpv load correlation queue saturated",
        ));
    }
    let needs_identity_query = state.playlist_identity_mode != PlaylistIdentityMode::EntryIds;
    let load_reserved =
        remember_pending_load(state, load.request_id, load.file_generation, "loadfile");
    let identity_request_id = if needs_identity_query {
        *request_id = request_id.wrapping_add(1);
        Some(*request_id)
    } else {
        None
    };
    let identity_reserved = identity_request_id.is_none_or(|identity_request_id| {
        remember_pending_load(
            state,
            identity_request_id,
            load.file_generation,
            "loadfile identity",
        )
    });
    if !load_reserved || !identity_reserved {
        state.pending.remove(&load.request_id);
        if let Some(identity_request_id) = identity_request_id {
            state.pending.remove(&identity_request_id);
        }
        return Err(io::Error::other(
            "mpv load identity correlation queue saturated",
        ));
    }
    if load.resume.is_some() {
        *request_id = request_id.wrapping_add(1);
        let pause_request_id = *request_id;
        remember_pending_command(state, pause_request_id, "recovery pre-load pause");
        write_json(
            conn,
            &proto::cmd_set_property("pause", &serde_json::Value::Bool(true), pause_request_id),
        )
        .await?;
    }
    state.issued_file_generation = load.file_generation;
    write_json(
        conn,
        &proto::cmd_loadfile(&load.url, "replace", load.request_id),
    )
    .await?;
    if let Some(request) = load.resume.into_request() {
        install_resume_state(state, load.file_generation, request);
    }
    state.legacy_latest_playlist_filename = None;
    state.legacy_loads.push_back(LegacyLoad {
        generation: load.file_generation,
        url: load.url,
        replied: false,
    });

    // mpv 0.33+ exposes stable IDs. Legacy mpv 0.32 exposes only the selected filename, so its
    // ordered snapshot remains the barrier that distinguishes a direct rapid replacement from
    // a redirect child. Once stable event IDs are proven, the redundant query is omitted.
    if let Some(identity_request_id) = identity_request_id {
        write_json(
            conn,
            &proto::cmd_get_property("playlist", identity_request_id),
        )
        .await?;
    }
    Ok(())
}

fn install_rejected_load_stop_boundary(state: &mut DispatchState, file_generation: u64) {
    reset_file_state(state);
    state.issued_file_generation = file_generation;
    state.admitted_file_generation = state.admitted_file_generation.max(file_generation);
    // The old physical file may emit a final stop event, but no later uncorrelated property may
    // be presented as the rejected owner's media while the internal Stop is in flight.
    state.active_file_generation = None;
}

async fn dispatch_rejected_load_stop(
    conn: &Stream,
    emit: &EventSink,
    state: &mut DispatchState,
    request_id: &mut u64,
    file_generation: u64,
) -> io::Result<()> {
    install_rejected_load_stop_boundary(state, file_generation);
    dispatch_command(
        conn,
        emit,
        state,
        request_id,
        PlayerCmd::Stop,
        Some("rejected-load stop"),
    )
    .await
}
