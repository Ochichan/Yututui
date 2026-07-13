/// Clear the per-file dedup/latch state when a file ends for ANY reason, so the next
/// load's first `time-pos`/cache second and duration are never dedup-suppressed.
fn cache_io_failure_exit(
    state: &DispatchState,
    reason: crate::player::long_form_seek::CacheReason,
) -> ActorExit {
    let file_generation = state
        .cache
        .as_ref()
        .and_then(|cache| cache.status().file_generation)
        .or(state.active_file_generation)
        .unwrap_or(state.issued_file_generation);
    ActorExit::CacheEmergency {
        file_generation,
        position_secs: state.last_confirmed_time,
        paused: state.paused,
        reason,
    }
}

fn poll_cache_persistence(state: &mut DispatchState) {
    let action = state
        .cache
        .as_mut()
        .and_then(CacheRuntime::poll_persistence);
    queue_cache_action(state, action);
}

fn prepare_cache_action(state: &mut DispatchState, action: CacheAction) -> Option<CacheAction> {
    let action = state
        .cache
        .as_mut()
        .map_or(Some(action), |cache| cache.prepare_cache_action(action));
    publish_cache_status(state);
    action
}

fn reset_file_state(state: &mut DispatchState) {
    super::diagnostics::paused_for_cache(state.active_file_generation, false);
    state.last_sent_time_sec = None;
    state.last_sent_cache_sec = None;
    state.duration_known = false;
    state.eof_emitted = false;
    state.legacy_pending_end_generation = None;
    state.last_confirmed_time = 0.0;
    state.media_seekable = None;
    state.resume_telemetry = None;
    state.resume_post_load_generation = None;
    state.post_load_commands.clear();
    state.file_loaded_generation = None;
    state.playback_ready_generation = None;
    state.pending_load_restart_generation = None;
    state.uncorrelated_file_loaded = false;
    state.uncorrelated_load_restart = false;
}

fn reset_file_state_preserving_resume(state: &mut DispatchState, preserve_generation: Option<u64>) {
    let resume = state
        .resume_telemetry
        .take()
        .filter(|gate| Some(gate.file_generation) == preserve_generation);
    let post_load = (state.resume_post_load_generation == preserve_generation
        && preserve_generation.is_some())
    .then(|| std::mem::take(&mut state.post_load_commands));
    reset_file_state(state);
    if let Some(resume) = resume {
        state.last_confirmed_time = resume.target_secs;
        state.resume_telemetry = Some(resume);
    }
    if let (Some(generation), Some(post_load)) = (preserve_generation, post_load) {
        state.resume_post_load_generation = Some(generation);
        state.post_load_commands = post_load;
    }
}

fn reset_file_state_for_start(state: &mut DispatchState) {
    // `start-file` is the expected lifecycle boundary for the just-dispatched recovery load.
    // Keep its exact-seek quarantine until that same generation is correlated; stale gates were
    // already removed synchronously by a replacing Load/Stop/transport intent.
    reset_file_state_preserving_resume(state, Some(state.issued_file_generation));
}

const RESUME_TELEMETRY_FLOOR_TOLERANCE_SECS: f64 = 2.0;

#[derive(Clone, Debug)]
struct ResumeTelemetryGate {
    file_generation: u64,
    target_secs: f64,
    expected_request_id: Option<u64>,
    exact_completed: bool,
    buffered_near_target: Option<f64>,
    latest_seek_position: Option<f64>,
    terminal_deadline: Option<Instant>,
    source_recovery: bool,
}

fn record_resume_outcome(
    resume: Option<&super::recovery::LoadWithResume>,
    outcome: super::diagnostics::SourceRecoveryOutcome,
) {
    if resume.is_some_and(|resume| resume.episode_id.get() != 0) {
        super::diagnostics::source_recovery_outcome(outcome);
    }
}

fn finish_load_validation(
    emit: &EventSink,
    state: &mut DispatchState,
    pending: PendingLoadValidation,
    validated: LoadValidationOutcome,
) -> Option<PendingLoadBoundary> {
    match validated {
        LoadValidationOutcome::Validated(url) => {
            let wait_for_cache_reset = prepare_load_replacement(state);
            Some(PendingLoadBoundary::Validated(ValidatedLoad {
                request_id: pending.request_id,
                file_generation: pending.file_generation,
                url,
                resume: pending.resume,
                restore_transport: pending.restore_transport,
                source_context: pending.source_context,
                wait_for_cache_reset,
            }))
        }
        LoadValidationOutcome::Superseded => {
            record_resume_outcome(
                pending.resume.as_ref(),
                super::diagnostics::SourceRecoveryOutcome::Superseded,
            );
            None
        }
        LoadValidationOutcome::Rejected(error) => {
            record_resume_outcome(
                pending.resume.as_ref(),
                super::diagnostics::SourceRecoveryOutcome::ValidationRejected,
            );
            tracing::warn!(%error, "blocked unsafe playback URL");
            emit(PlayerEvent::file_scoped(
                pending.file_generation,
                PlayerEvent::Error(format!("blocked playback URL: {error}")),
            ));
            let wait_for_cache_reset = prepare_load_replacement(state);
            Some(PendingLoadBoundary::RejectedStop {
                file_generation: pending.file_generation,
                wait_for_cache_reset,
            })
        }
    }
}

fn prepare_load_replacement(state: &mut DispatchState) -> bool {
    let reset_action = state
        .cache
        .as_mut()
        .and_then(CacheRuntime::prepare_replacement);
    let wait_for_cache_reset = reset_action.is_some();
    if let Some(action) = reset_action {
        queue_cache_action(state, Some(action));
    }
    publish_cache_status(state);
    wait_for_cache_reset
}

fn install_resume_telemetry(
    state: &mut DispatchState,
    file_generation: u64,
    request: &super::recovery::LoadWithResume,
) {
    state.last_confirmed_time = request.position_secs;
    state.resume_telemetry = (request.position_secs > 0.0).then_some(ResumeTelemetryGate {
        file_generation,
        target_secs: request.position_secs,
        expected_request_id: None,
        exact_completed: false,
        buffered_near_target: None,
        latest_seek_position: None,
        terminal_deadline: None,
        source_recovery: request.episode_id.get() != 0,
    });
}

fn cancel_resume_post_load_if_superseded(state: &mut DispatchState, command: &PlayerCmd) {
    if !supersedes_pending_resume(command) {
        return;
    }
    let source_recovery = state
        .pending_resume
        .as_ref()
        .is_some_and(|pending| pending.request.episode_id.get() != 0)
        || state
            .resume_telemetry
            .as_ref()
            .is_some_and(|gate| gate.source_recovery);
    let pending_owned = state.pending_resume.take().is_some();
    let post_load_owned = state.resume_post_load_generation.take().is_some();
    let owned = pending_owned || post_load_owned || !state.post_load_commands.is_empty();
    if owned {
        state.post_load_commands.clear();
        state.resume_telemetry = None;
    }
    if owned && source_recovery {
        super::diagnostics::source_recovery_outcome(
            super::diagnostics::SourceRecoveryOutcome::Superseded,
        );
    }
}

fn pop_next_actor_command(
    state: &mut DispatchState,
    backlog: &mut VecDeque<PlayerCmd>,
) -> Option<PlayerCmd> {
    if let Some(command) = state.post_load_commands.pop_front() {
        Some(command)
    } else {
        backlog.pop_front()
    }
}

fn begin_resume_seek_observation(state: &mut DispatchState, file_generation: u64, request_id: u64) {
    if let Some(gate) = state
        .resume_telemetry
        .as_mut()
        .filter(|gate| gate.file_generation == file_generation)
    {
        gate.expected_request_id = Some(request_id);
        gate.exact_completed = false;
        gate.buffered_near_target = None;
        gate.latest_seek_position = None;
        gate.terminal_deadline = None;
    }
}

fn complete_resume_telemetry(
    emit: &EventSink,
    state: &mut DispatchState,
    file_generation: u64,
    request_id: u64,
) {
    let buffered = {
        let Some(gate) = state.resume_telemetry.as_mut().filter(|gate| {
            gate.file_generation == file_generation && gate.expected_request_id == Some(request_id)
        }) else {
            return;
        };
        gate.exact_completed = true;
        gate.latest_seek_position = None;
        if let Some(position) = gate.buffered_near_target.take() {
            Some((position, gate.source_recovery))
        } else {
            gate.terminal_deadline = Some(Instant::now() + RESUME_POSITION_TERMINAL_TIMEOUT);
            None
        }
    };
    if let Some((position, source_recovery)) = buffered {
        state.resume_telemetry = None;
        if source_recovery {
            super::diagnostics::source_recovery_outcome(
                super::diagnostics::SourceRecoveryOutcome::ResumeDispatched,
            );
        }
        forward_time_pos(emit, state, position);
    }
}

fn resume_position_terminal_deadline(state: &DispatchState) -> Option<Instant> {
    state
        .resume_telemetry
        .as_ref()
        .and_then(|gate| gate.terminal_deadline)
}

fn observe_time_pos(emit: &EventSink, state: &mut DispatchState, position: f64) {
    let position = crate::playback_policy::norm_position(position);
    if let Some(gate) = state.resume_telemetry.as_mut()
        && (state.active_file_generation.is_none()
            || state.active_file_generation == Some(gate.file_generation))
    {
        gate.latest_seek_position = Some(position);
        if gate.expected_request_id.is_none() {
            return;
        }
        let near_target = (gate.target_secs == 0.0 || position > 0.0)
            && (position - gate.target_secs).abs() <= RESUME_TELEMETRY_FLOOR_TOLERANCE_SECS;
        if !gate.exact_completed {
            if near_target {
                gate.buffered_near_target = Some(position);
            }
            return;
        }
        if !near_target {
            return;
        }
        let source_recovery = gate.source_recovery;
        state.resume_telemetry = None;
        if source_recovery {
            super::diagnostics::source_recovery_outcome(
                super::diagnostics::SourceRecoveryOutcome::ResumeDispatched,
            );
        }
    }
    forward_time_pos(emit, state, position);
}

const RESUME_POSITION_TERMINAL_TIMEOUT: Duration = Duration::from_millis(250);

fn forward_time_pos(emit: &EventSink, state: &mut DispatchState, position: f64) {
    state.last_confirmed_time = position;
    if let Some(cache) = state.cache.as_mut() {
        cache.observe_transport(position, state.paused);
    }
    let second = position as i64;
    if state.last_sent_time_sec != Some(second) {
        state.last_sent_time_sec = Some(second);
        emit_file_event(emit, state, PlayerEvent::TimePos(position));
        record_numeric_forward(state, "time-pos");
    }
}

fn emit_file_event(emit: &EventSink, state: &mut DispatchState, event: PlayerEvent) {
    if let Some(generation) = state.active_file_generation {
        emit(PlayerEvent::file_scoped(generation, event));
        return;
    }
    if state.quarantined_file_events.len() == FILE_EVENT_QUARANTINE_CAPACITY {
        state.quarantined_file_events.pop_front();
        tracing::warn!(
            capacity = FILE_EVENT_QUARANTINE_CAPACITY,
            "mpv file event quarantine full; dropped oldest uncorrelated property"
        );
    }
    state.quarantined_file_events.push_back(event);
}

fn emit_file_event_for_generation(
    emit: &EventSink,
    state: &mut DispatchState,
    playlist_entry_id: Option<u64>,
    generation: Option<u64>,
    event: PlayerEvent,
) {
    if let Some(generation) = generation {
        emit(PlayerEvent::file_scoped(generation, event));
    } else if playlist_entry_id.is_some() && playlist_entry_id != state.active_playlist_entry_id {
        tracing::debug!(
            ?playlist_entry_id,
            active_playlist_entry_id = ?state.active_playlist_entry_id,
            "ignored terminal event for an uncorrelated inactive playlist entry"
        );
    } else {
        // Old mpv does not include playlist IDs; an explicitly active but not-yet-correlated
        // entry can also end before its load reply. Quarantine only those plausible current
        // terminals. Never relabel a known-different entry with the current generation.
        emit_file_event(emit, state, event);
    }
}

fn cache_facts_belong_to_current_generation(state: &DispatchState) -> bool {
    state.active_file_generation == Some(state.issued_file_generation)
}

fn observe_cache_duration(state: &mut DispatchState, duration: Option<f64>) {
    if cache_facts_belong_to_current_generation(state) {
        let action = state
            .cache
            .as_mut()
            .and_then(|cache| cache.observe_duration(duration));
        queue_cache_action(state, action);
    } else if state.active_file_generation.is_none() {
        state.quarantined_cache_facts.duration_secs = Some(duration);
    }
}

fn observe_cache_network(state: &mut DispatchState, via_network: Option<bool>) {
    if cache_facts_belong_to_current_generation(state) {
        let action = state
            .cache
            .as_mut()
            .and_then(|cache| cache.observe_network(via_network));
        queue_cache_action(state, action);
    } else if state.active_file_generation.is_none() {
        state.quarantined_cache_facts.via_network = Some(via_network);
    }
}

fn observe_cache_seekable(state: &mut DispatchState, seekable: Option<bool>) {
    if cache_facts_belong_to_current_generation(state) {
        state.media_seekable = seekable;
        let action = state
            .cache
            .as_mut()
            .and_then(|cache| cache.observe_seekable(seekable));
        queue_cache_action(state, action);
    } else if state.active_file_generation.is_none() {
        state.quarantined_cache_facts.seekable = Some(seekable);
    }
}

fn observe_cache_partially_seekable(state: &mut DispatchState, partially_seekable: Option<bool>) {
    if cache_facts_belong_to_current_generation(state) {
        let action = state
            .cache
            .as_mut()
            .and_then(|cache| cache.observe_partially_seekable(partially_seekable));
        queue_cache_action(state, action);
    } else if state.active_file_generation.is_none() {
        state.quarantined_cache_facts.partially_seekable = Some(partially_seekable);
    }
}

fn replay_quarantined_cache_facts(state: &mut DispatchState) {
    let facts = std::mem::take(&mut state.quarantined_cache_facts);
    if let Some(duration) = facts.duration_secs {
        observe_cache_duration(state, duration);
    }
    if let Some(via_network) = facts.via_network {
        observe_cache_network(state, via_network);
    }
    if let Some(seekable) = facts.seekable {
        observe_cache_seekable(state, seekable);
    }
    if let Some(partially_seekable) = facts.partially_seekable {
        observe_cache_partially_seekable(state, partially_seekable);
    }
}

fn associate_entry_generation(
    emit: &EventSink,
    state: &mut DispatchState,
    entry_id: u64,
    generation: u64,
) {
    insert_entry_generation(state, entry_id, generation);
    if let Some((first, count)) = state.pending_redirects.remove(&entry_id) {
        associate_redirect_range(emit, state, first, count, generation);
    }
    if state.active_playlist_entry_id == Some(entry_id) {
        activate_correlated_entry(emit, state, generation);
    }
}

fn associate_redirect_range(
    emit: &EventSink,
    state: &mut DispatchState,
    first_entry_id: u64,
    count: u64,
    generation: u64,
) {
    for offset in 0..count.min(REDIRECT_ENTRY_RANGE_CAPACITY) {
        if let Some(entry_id) = first_entry_id.checked_add(offset) {
            insert_entry_generation(state, entry_id, generation);
            if state.active_playlist_entry_id == Some(entry_id) {
                activate_correlated_entry(emit, state, generation);
            }
        }
    }
}

fn insert_entry_generation(state: &mut DispatchState, entry_id: u64, generation: u64) {
    if !state.entry_generations.contains_key(&entry_id)
        && state.entry_generations.len() >= ENTRY_GENERATION_CAPACITY
        && let Some(victim) = state
            .entry_generations
            .keys()
            .filter(|candidate| Some(**candidate) != state.active_playlist_entry_id)
            .min()
            .copied()
    {
        state.entry_generations.remove(&victim);
        state.pending_redirects.remove(&victim);
        tracing::debug!(victim, "evicted stale mpv playlist correlation");
    }
    if state.entry_generations.len() < ENTRY_GENERATION_CAPACITY
        || state.entry_generations.contains_key(&entry_id)
    {
        state.entry_generations.insert(entry_id, generation);
    }
}

fn remember_pending_redirect(
    state: &mut DispatchState,
    old_entry_id: u64,
    first_entry_id: u64,
    count: u64,
) {
    if !state.pending_redirects.contains_key(&old_entry_id)
        && state.pending_redirects.len() >= PENDING_REDIRECT_CAPACITY
        && let Some(victim) = state
            .pending_redirects
            .keys()
            .filter(|candidate| Some(**candidate) != state.active_playlist_entry_id)
            .min()
            .copied()
    {
        state.pending_redirects.remove(&victim);
        tracing::debug!(victim, "evicted stale pending mpv redirect correlation");
    }
    if state.pending_redirects.len() < PENDING_REDIRECT_CAPACITY
        || state.pending_redirects.contains_key(&old_entry_id)
    {
        state
            .pending_redirects
            .insert(old_entry_id, (first_entry_id, count));
    }
}

fn mark_file_loaded(state: &mut DispatchState, generation: u64) {
    state.file_loaded_generation = Some(generation);
    if state.pending_load_restart_generation == Some(generation) {
        state.pending_load_restart_generation = None;
        state.playback_ready_generation = Some(generation);
    }
}

fn observe_playback_restart(state: &mut DispatchState) {
    if let Some(generation) = state.active_file_generation {
        if state.playback_ready_generation == Some(generation) {
            state.seek_observation = Some(SeekObservation::PlaybackRestart);
        } else if state.file_loaded_generation == Some(generation) {
            state.playback_ready_generation = Some(generation);
        } else {
            state.pending_load_restart_generation = Some(generation);
        }
    } else if state.active_playlist_entry_id.is_some() || state.legacy_start_waiting {
        state.uncorrelated_load_restart = true;
    }
}

fn activate_correlated_entry(emit: &EventSink, state: &mut DispatchState, generation: u64) {
    let generation_changed = state.active_file_generation != Some(generation);
    state.active_file_generation = Some(generation);
    if state.uncorrelated_load_restart {
        state.pending_load_restart_generation = Some(generation);
        state.uncorrelated_load_restart = false;
    }
    if state.uncorrelated_file_loaded {
        mark_file_loaded(state, generation);
        state.uncorrelated_file_loaded = false;
    }
    if generation_changed && generation == state.issued_file_generation {
        // Missing source context is deliberately fail-closed. Normal owner paths install an
        // explicit value before `loadfile`; this fallback only covers malformed correlation.
        let source_context = state
            .media_source_contexts
            .get(&generation)
            .copied()
            .unwrap_or(MediaSourceContext::Live);
        let action = state
            .cache
            .as_mut()
            .and_then(|cache| cache.begin_media(generation, source_context));
        queue_cache_action(state, action);
        replay_quarantined_cache_facts(state);
    } else if generation != state.issued_file_generation {
        // Owners still need correctly tagged stale telemetry, but an older rapid replacement
        // must never arm cache policy for the newest admitted media.
        state.quarantined_cache_facts = QuarantinedCacheFacts::default();
    }
    release_pending_resume_if_ready(state);
    while let Some(event) = state.quarantined_file_events.pop_front() {
        emit(PlayerEvent::file_scoped(generation, event));
    }
}

fn release_pending_resume_if_ready(state: &mut DispatchState) {
    let ready = state.pending_resume.as_ref().is_some_and(|pending| {
        pending.file_loaded && state.active_file_generation == Some(pending.file_generation)
    });
    if !ready {
        return;
    }
    let pending = state
        .pending_resume
        .take()
        .expect("ready recovery remains installed");
    state.resume_post_load_generation = Some(pending.file_generation);
    if pending.request.position_secs > 0.0 {
        state
            .post_load_commands
            .push_back(PlayerCmd::exact_seek(pending.request.position_secs));
    }
    state.post_load_commands.push_back(PlayerCmd::SetProperty {
        name: "pause".to_owned(),
        value: serde_json::Value::Bool(pending.request.paused),
    });
}

fn use_entry_id_protocol(state: &mut DispatchState) {
    state.playlist_identity_mode = PlaylistIdentityMode::EntryIds;
    state.legacy_loads.clear();
    state.legacy_redirect_generation = None;
    state.legacy_latest_playlist_filename = None;
    state.legacy_start_waiting = false;
    state.eof_emitted = false;
    state.legacy_pending_end_generation = None;
}

fn remove_legacy_load_generation(state: &mut DispatchState, generation: u64) {
    if let Some(index) = state
        .legacy_loads
        .iter()
        .position(|pending| pending.generation == generation)
    {
        state.legacy_loads.remove(index);
    }
}

fn mark_legacy_load_replied(state: &mut DispatchState, generation: u64) {
    state.playlist_identity_mode = PlaylistIdentityMode::Legacy;
    while state
        .legacy_loads
        .front()
        .is_some_and(|pending| pending.generation < generation)
    {
        state.legacy_loads.pop_front();
    }
    if let Some(pending) = state
        .legacy_loads
        .iter_mut()
        .find(|pending| pending.generation == generation)
    {
        pending.replied = true;
    }
}

struct PlaylistSelection<'a> {
    entry_id: Option<u64>,
    filename: Option<&'a str>,
}

fn playlist_selection(data: Option<&serde_json::Value>) -> Option<PlaylistSelection<'_>> {
    let entries = data?.as_array()?;
    let selected = entries
        .iter()
        .find(|entry| {
            entry.get("current").and_then(serde_json::Value::as_bool) == Some(true)
                || entry.get("playing").and_then(serde_json::Value::as_bool) == Some(true)
        })
        .or_else(|| (entries.len() == 1).then(|| &entries[0]))?;
    Some(PlaylistSelection {
        entry_id: selected.get("id").and_then(serde_json::Value::as_u64),
        filename: selected.get("filename").and_then(serde_json::Value::as_str),
    })
}

fn correlate_legacy_playlist(emit: &EventSink, state: &mut DispatchState, filename: Option<&str>) {
    if let Some(filename) = filename {
        state.legacy_latest_playlist_filename = Some(filename.to_owned());
    }
    if !state.legacy_start_waiting {
        return;
    }

    let selected_filename = state.legacy_latest_playlist_filename.as_deref();
    let selected_matches_pending = selected_filename.is_some_and(|filename| {
        state
            .legacy_loads
            .iter()
            .any(|pending| pending.url == filename)
    });
    let matched = selected_filename.and_then(|filename| {
        state
            .legacy_loads
            .iter()
            .rposition(|pending| pending.replied && pending.url == filename)
    });
    if let Some(index) = matched {
        let generation = state.legacy_loads[index].generation;
        let newer_direct_load_is_pending = state.legacy_loads.len() > index + 1;
        state.legacy_loads.drain(..=index);
        if newer_direct_load_is_pending {
            // The owner has already admitted a newer replacement. Even if this older file
            // reaches start-file, its quarantined properties are stale and must not be flushed
            // into the newer reducer generation. Keep waiting for the newest direct start.
            state.active_file_generation = None;
            state.quarantined_file_events.clear();
            state.legacy_redirect_generation = None;
            return;
        }
        state.legacy_start_waiting = false;
        state.legacy_redirect_generation = None;
        activate_correlated_entry(emit, state, generation);
    } else if selected_filename.is_some()
        && !selected_matches_pending
        && state.legacy_loads.is_empty()
        && let Some(generation) = state.legacy_redirect_generation.take()
    {
        state.legacy_start_waiting = false;
        activate_correlated_entry(emit, state, generation);
    } else if selected_filename.is_none()
        && let Some(index) = state
            .legacy_loads
            .iter()
            .position(|pending| pending.replied)
    {
        let generation = state.legacy_loads[index].generation;
        state.legacy_loads.drain(..=index);
        state.legacy_start_waiting = false;
        state.legacy_redirect_generation = None;
        activate_correlated_entry(emit, state, generation);
    } else if state.legacy_loads.is_empty()
        && let Some(generation) = state.legacy_redirect_generation.take()
    {
        state.legacy_start_waiting = false;
        activate_correlated_entry(emit, state, generation);
    }
}

/// Borrowed view of the two high-rate numeric mpv properties. Parsing into this (zero-copy
/// `&str` fields, `data` as a plain optional number) skips the full `serde_json::Value` tree that
/// `proto::parse_line` builds. `None` deliberately covers both JSON null and a missing `data`
/// field, matching the general parser's `Value::Null` fallback.
#[derive(serde::Deserialize)]
struct NumericPropertyLine<'a> {
    event: &'a str,
    name: &'a str,
    data: Option<f64>,
}

fn json_nonnegative_u64(value: &serde_json::Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        let number = value.as_f64()?;
        (number.is_finite() && number >= 0.0 && number <= u64::MAX as f64)
            .then_some(number.round() as u64)
    })
}

/// Translate one mpv line into a player event for the runtime.
fn dispatch_incoming(line: &str, emit: &EventSink, state: &mut DispatchState) {
    // Flush the previous complete measurement window before accounting this line, so forwarded
    // counts and raw counts always describe the same set of fully-dispatched events.
    log_numeric_perf(state, false);
    // Fast path: normalize and dedup the two high-rate progress properties before allocating.
    // Other property shapes fail or fall through to the general parser unchanged.
    if let Ok(property) = serde_json::from_str::<NumericPropertyLine>(line.trim())
        && property.event == "property-change"
    {
        match property.name {
            "time-pos" => {
                record_numeric_input(state, property.name, true);
                if let Some(t) = property.data {
                    observe_time_pos(emit, state, t);
                }
                return;
            }
            "demuxer-cache-time" => {
                record_numeric_input(state, property.name, true);
                match property.data {
                    Some(t) => {
                        let t = crate::playback_policy::norm_position(t);
                        let sec = t as i64;
                        if state.last_sent_cache_sec != Some(sec) {
                            state.last_sent_cache_sec = Some(sec);
                            emit_file_event(emit, state, PlayerEvent::CacheTime(Some(t)));
                            record_numeric_forward(state, property.name);
                        }
                    }
                    None => {
                        if state.last_sent_cache_sec.take().is_some() {
                            emit_file_event(emit, state, PlayerEvent::CacheTime(None));
                            record_numeric_forward(state, property.name);
                        }
                    }
                }
                return;
            }
            _ => {}
        }
    }
    let Some(incoming) = proto::parse_line(line) else {
        return;
    };
    match incoming {
        MpvIncoming::PropertyChange { name, value, .. } => match name.as_str() {
            "time-pos" => {
                record_numeric_input(state, &name, false);
                if let Some(t) = value.as_f64() {
                    observe_time_pos(emit, state, t);
                }
            }
            "duration" => match value.as_f64() {
                Some(d) => {
                    let d = crate::playback_policy::norm_duration(d);
                    state.duration_known = d > 0.0;
                    observe_cache_duration(state, Some(d));
                    emit_file_event(emit, state, PlayerEvent::Duration(Some(d)));
                }
                // Null after a real value = the property became unavailable (live edge,
                // mid-file teardown) — forward the loss ONCE so a reducer that missed the
                // load-time clear (late event from the old file) can't keep a stale length.
                None => {
                    observe_cache_duration(state, None);
                    if std::mem::take(&mut state.duration_known) {
                        emit_file_event(emit, state, PlayerEvent::Duration(None));
                    }
                }
            },
            "pause" => {
                if let Some(p) = value.as_bool() {
                    state.paused = p;
                    if let Some(cache) = state.cache.as_mut() {
                        cache.observe_transport(state.last_confirmed_time, p);
                    }
                    emit_file_event(emit, state, PlayerEvent::Paused(p));
                }
            }
            "seeking" => {
                if let Some(seeking) = value.as_bool() {
                    state.seek_observation = Some(SeekObservation::Seeking(seeking));
                }
            }
            "paused-for-cache" => {
                if let Some(paused) = value.as_bool() {
                    super::diagnostics::paused_for_cache(state.active_file_generation, paused);
                }
            }
            "demuxer-via-network" => {
                observe_cache_network(state, value.as_bool());
            }
            "seekable" => {
                observe_cache_seekable(state, value.as_bool());
            }
            "partially-seekable" => {
                observe_cache_partially_seekable(state, value.as_bool());
            }
            "file-cache-bytes" => {
                if let Some(bytes) = json_nonnegative_u64(&value) {
                    let action = state
                        .cache
                        .as_mut()
                        .and_then(|cache| cache.observe_file_cache_bytes(bytes));
                    queue_cache_action(state, action);
                }
            }
            "cache-speed" => {
                let action = state
                    .cache
                    .as_mut()
                    .and_then(|cache| cache.observe_cache_speed(json_nonnegative_u64(&value)));
                queue_cache_action(state, action);
            }
            "raw-input-rate" => {
                let action = state
                    .cache
                    .as_mut()
                    .and_then(|cache| cache.observe_raw_input_rate(json_nonnegative_u64(&value)));
                queue_cache_action(state, action);
            }
            "volume" => {
                if let Some(v) = value.as_f64() {
                    emit(PlayerEvent::Volume(v));
                }
            }
            "metadata" => {
                emit_file_event(emit, state, PlayerEvent::Metadata(value));
            }
            "audio-codec-name" => {
                emit_file_event(
                    emit,
                    state,
                    PlayerEvent::AudioCodec(value.as_str().map(str::to_owned)),
                );
            }
            "file-format" => {
                emit_file_event(
                    emit,
                    state,
                    PlayerEvent::FileFormat(value.as_str().map(str::to_owned)),
                );
            }
            property if audio_output::dispatch_property(property, &value, emit) => {}
            "playlist" => {
                if let Some(selection) = playlist_selection(Some(&value)) {
                    if selection.entry_id.is_some() {
                        use_entry_id_protocol(state);
                    } else if state.playlist_identity_mode != PlaylistIdentityMode::EntryIds {
                        state.playlist_identity_mode = PlaylistIdentityMode::Legacy;
                        correlate_legacy_playlist(emit, state, selection.filename);
                    }
                }
            }
            "eof-reached" => {
                if value.as_bool() == Some(true) && !state.eof_emitted {
                    state.eof_emitted = true;
                    if let Some(generation) = state.legacy_pending_end_generation.take() {
                        emit(PlayerEvent::file_scoped(generation, PlayerEvent::Eof));
                    } else {
                        emit_file_event(emit, state, PlayerEvent::Eof);
                    }
                }
            }
            "demuxer-cache-time" => match value.as_f64() {
                // High-rate like time-pos → dedup to whole seconds.
                Some(t) => {
                    record_numeric_input(state, &name, false);
                    let t = crate::playback_policy::norm_position(t);
                    let sec = t as i64;
                    if state.last_sent_cache_sec != Some(sec) {
                        state.last_sent_cache_sec = Some(sec);
                        emit_file_event(emit, state, PlayerEvent::CacheTime(Some(t)));
                        record_numeric_forward(state, &name);
                    }
                }
                // Unlike time-pos, a null here is a signal the reducer needs: the
                // property became unavailable (stream teardown, cache-less demuxer).
                None => {
                    record_numeric_input(state, &name, false);
                    if state.last_sent_cache_sec.take().is_some() {
                        emit_file_event(emit, state, PlayerEvent::CacheTime(None));
                        record_numeric_forward(state, &name);
                    }
                }
            },
            _ => {}
        },
        MpvIncoming::StartFile { playlist_entry_id } => {
            reset_file_state_for_start(state);
            state.quarantined_file_events.clear();
            state.quarantined_cache_facts = QuarantinedCacheFacts::default();
            let previous_generation = state.active_file_generation;
            state.active_playlist_entry_id = playlist_entry_id;
            state.active_file_generation = None;
            if let Some(entry_id) = playlist_entry_id {
                use_entry_id_protocol(state);
                if let Some(generation) = state.entry_generations.get(&entry_id).copied() {
                    activate_correlated_entry(emit, state, generation);
                }
            } else if state.playlist_identity_mode != PlaylistIdentityMode::EntryIds {
                state.playlist_identity_mode = PlaylistIdentityMode::Legacy;
                state.legacy_start_waiting = true;
                state.legacy_redirect_generation = previous_generation;
                correlate_legacy_playlist(emit, state, None);
            }
            if state.active_file_generation.is_none() {
                tracing::debug!(
                    ?playlist_entry_id,
                    "quarantining mpv file events until playlist entry is correlated"
                );
            }
        }
        MpvIncoming::FileLoaded { playlist_entry_id } => {
            let event_generation = playlist_entry_id
                .and_then(|entry_id| state.entry_generations.get(&entry_id).copied())
                .or_else(|| {
                    (playlist_entry_id.is_none()
                        || playlist_entry_id == state.active_playlist_entry_id)
                        .then_some(state.active_file_generation)
                        .flatten()
                });
            if let Some(generation) = event_generation {
                mark_file_loaded(state, generation);
            } else if playlist_entry_id == state.active_playlist_entry_id {
                state.uncorrelated_file_loaded = true;
            }
            if let Some(pending) = state.pending_resume.as_mut()
                && (event_generation == Some(pending.file_generation)
                    || (event_generation.is_none()
                        && playlist_entry_id == state.active_playlist_entry_id))
            {
                pending.file_loaded = true;
            }
            release_pending_resume_if_ready(state);
        }
        MpvIncoming::EndFile {
            reason,
            file_error,
            playlist_entry_id,
            playlist_insert_id,
            playlist_insert_num_entries,
        } => {
            let eof_was_emitted = state.eof_emitted;
            let generation = playlist_entry_id
                .and_then(|entry_id| state.entry_generations.get(&entry_id).copied())
                .or_else(|| {
                    (playlist_entry_id.is_none()
                        || playlist_entry_id == state.active_playlist_entry_id)
                        .then_some(state.active_file_generation)
                        .flatten()
                });
            let legacy_reasonless_error_candidate = reason.is_empty()
                && playlist_entry_id.is_none()
                && state.playlist_identity_mode == PlaylistIdentityMode::Legacy
                && !eof_was_emitted
                && generation.is_some_and(|generation| generation == state.issued_file_generation);
            if playlist_entry_id.is_none()
                && state.playlist_identity_mode == PlaylistIdentityMode::Legacy
            {
                state.legacy_redirect_generation = generation;
            }
            if reason == "redirect"
                && let (Some(first), Some(count)) =
                    (playlist_insert_id, playlist_insert_num_entries)
                && count > 0
            {
                if let Some(generation) = generation {
                    associate_redirect_range(emit, state, first, count, generation);
                } else if let Some(old_entry_id) = playlist_entry_id {
                    remember_pending_redirect(state, old_entry_id, first, count);
                }
            }
            if reason == "error"
                && state.resume_telemetry.as_ref().is_some_and(|gate| {
                    gate.source_recovery
                        && gate.file_generation
                            == generation.unwrap_or(state.issued_file_generation)
                })
            {
                super::diagnostics::source_recovery_outcome(
                    super::diagnostics::SourceRecoveryOutcome::LoadRejected,
                );
            }
            if state.pending_resume.as_ref().is_some_and(|pending| {
                generation.is_none_or(|value| value == pending.file_generation)
            }) {
                state.pending_resume = None;
            }
            let preserve_generation = generation
                .is_some_and(|generation| generation != state.issued_file_generation)
                .then_some(state.issued_file_generation);
            reset_file_state_preserving_resume(state, preserve_generation);
            match reason.as_str() {
                "eof" => {
                    if !eof_was_emitted {
                        emit_file_event_for_generation(
                            emit,
                            state,
                            playlist_entry_id,
                            generation,
                            PlayerEvent::Eof,
                        );
                    }
                    // Property changes and lifecycle events are delivered asynchronously.
                    // Keep the latch armed if the explicit fallback wins that race.
                    state.eof_emitted = true;
                }
                "error" => {
                    // Current mpv normally exposes only `loading failed`; 0.32 exposes no
                    // detail. Keep the owner signal URL-free in either case. The owner allows
                    // one generic refresh only with confirmed same-item mid-track evidence.
                    let _ = file_error;
                    emit_file_event_for_generation(
                        emit,
                        state,
                        playlist_entry_id,
                        generation,
                        PlayerEvent::Error(super::recovery::GENERIC_LOADING_FAILURE.to_owned()),
                    );
                }
                "" if legacy_reasonless_error_candidate => {
                    state.legacy_pending_end_generation = generation;
                    tracing::debug!(
                        ?generation,
                        "waiting for mpv 0.32 idle/start/eof terminal boundary"
                    );
                }
                // `stop` (our own Stop/replace), `quit`, `redirect`, and any future reason:
                // no event (the reducers own those transitions), but the per-file dedup
                // state must not survive into the next load's first second.
                reason => tracing::debug!(reason, "mpv end-file"),
            }
            if let Some(entry_id) = playlist_entry_id {
                state.entry_generations.remove(&entry_id);
            }
        }
        MpvIncoming::Idle => {
            if state.playlist_identity_mode == PlaylistIdentityMode::Legacy
                && let Some(generation) = state.legacy_pending_end_generation.take()
                && generation == state.issued_file_generation
            {
                emit(PlayerEvent::file_scoped(
                    generation,
                    PlayerEvent::Error(super::recovery::GENERIC_LOADING_FAILURE.to_owned()),
                ));
            }
        }
        MpvIncoming::PlaybackRestart => {
            observe_playback_restart(state);
        }
        MpvIncoming::CommandReply {
            request_id,
            error,
            data,
        } => {
            let accepted = error == "success" || error.is_empty();
            state.seek_observation = Some(SeekObservation::CommandReply {
                request_id,
                accepted,
            });
            let cache_action = state
                .cache
                .as_mut()
                .and_then(|cache| cache.handle_reply(request_id, accepted, data.as_ref()));
            queue_cache_action(state, cache_action);
            let Some(mut pending) = state.pending.remove(&request_id) else {
                return;
            };
            if audio_output::dispatch_reply(state, &mut pending, &error, data.as_ref(), emit) {
                return;
            }
            if !accepted && let Some(contract) = pending.terminal_contract {
                state.terminal_failure = Some(ActorExit::InternalCommandFailed {
                    operation: contract.operation,
                    rejected: true,
                });
            }
            if let Some(acknowledgement) = &pending.acknowledgement {
                if error == "success" || error.is_empty() {
                    acknowledgement.succeed();
                } else {
                    acknowledgement.fail(format!("mpv rejected {} ({error})", pending.label));
                }
            }
            if pending.label == "loadfile" {
                let file_generation = pending
                    .file_generation
                    .unwrap_or(state.issued_file_generation);
                if error == "success" || error.is_empty() {
                    if let Some(entry_id) = data
                        .as_ref()
                        .and_then(|value| value.get("playlist_entry_id"))
                        .and_then(serde_json::Value::as_u64)
                    {
                        use_entry_id_protocol(state);
                        associate_entry_generation(emit, state, entry_id, file_generation);
                    } else if state.playlist_identity_mode != PlaylistIdentityMode::EntryIds {
                        mark_legacy_load_replied(state, file_generation);
                        if state.legacy_latest_playlist_filename.is_some() {
                            correlate_legacy_playlist(emit, state, None);
                        }
                    }
                } else {
                    record_resume_outcome(
                        state.pending_resume.as_ref().map(|resume| &resume.request),
                        super::diagnostics::SourceRecoveryOutcome::LoadRejected,
                    );
                    state.failed_load_generations.insert(file_generation);
                    state.media_source_contexts.remove(&file_generation);
                    remove_legacy_load_generation(state, file_generation);
                    emit(PlayerEvent::file_scoped(
                        file_generation,
                        PlayerEvent::Error(super::recovery::GENERIC_LOADING_FAILURE.to_owned()),
                    ));
                }
            } else if pending.label == "loadfile identity" {
                let file_generation = pending
                    .file_generation
                    .unwrap_or(state.issued_file_generation);
                if state.failed_load_generations.remove(&file_generation) {
                    return;
                }
                let selection = (error == "success" || error.is_empty())
                    .then(|| playlist_selection(data.as_ref()))
                    .flatten();
                if let Some(entry_id) = selection.as_ref().and_then(|value| value.entry_id) {
                    use_entry_id_protocol(state);
                    associate_entry_generation(emit, state, entry_id, file_generation);
                } else if state.playlist_identity_mode != PlaylistIdentityMode::EntryIds {
                    mark_legacy_load_replied(state, file_generation);
                    correlate_legacy_playlist(
                        emit,
                        state,
                        selection.and_then(|value| value.filename),
                    );
                } else {
                    emit(PlayerEvent::file_scoped(
                        file_generation,
                        PlayerEvent::Error(
                            "mpv could not correlate loadfile with a playlist entry".to_owned(),
                        ),
                    ));
                }
            } else if error != "success" && !error.is_empty() {
                tracing::warn!(command = %pending.label, error = %error, "mpv command failed");
            }
        }
        // Script messages are a video-overlay concern ([`super::video`]); the audio
        // engine has no keys to press.
        MpvIncoming::ClientMessage { .. } | MpvIncoming::Other => {}
    }
}
