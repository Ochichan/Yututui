use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::*;

const PROPERTY_SEEDS: u64 = 512;
const MAX_TRACE_EVENTS: usize = 96;

#[derive(Clone, Debug, PartialEq, Eq)]
enum OperationView {
    Replace { sidecar: String, sha256: String },
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CandidateView {
    order: Option<JournalOrder>,
    operation: OperationView,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ProtocolState {
    committed_through: Option<JournalOrder>,
    candidate: Option<CandidateView>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ModelOrder {
    process_epoch: u64,
    sequence: u128,
    generation: [u8; 16],
}

impl ModelOrder {
    fn from_journal(order: JournalOrder) -> Self {
        Self {
            process_epoch: order.process_epoch,
            sequence: order.sequence,
            generation: order.generation.0,
        }
    }

    fn into_journal(self) -> JournalOrder {
        JournalOrder {
            process_epoch: self.process_epoch,
            sequence: self.sequence,
            generation: JournalGeneration(self.generation),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelCandidate {
    order: Option<ModelOrder>,
    operation: OperationView,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ModelState {
    committed_through: Option<ModelOrder>,
    candidate: Option<ModelCandidate>,
}

#[derive(Clone, Copy, Debug)]
enum InvalidLine {
    TornJson,
    WrongVersion,
    WrongKind,
    IncompleteOrder,
    UnknownOperation,
}

#[derive(Clone, Debug)]
enum GeneratedEvent {
    Commit(JournalOrder),
    OrderedCandidate(CandidateView),
    TransitionalCandidate(CandidateView),
    LegacyCandidate(OperationView),
    Invalid(InvalidLine),
    Blank,
}

fn operation_view(seed: u64, step: usize, replace: bool) -> OperationView {
    if replace {
        OperationView::Replace {
            sidecar: format!("config.json.intent.model-{seed}-{step}.json"),
            sha256: format!(
                "{:064x}",
                seed.rotate_left((step % 64) as u32) ^ step as u64
            ),
        }
    } else {
        OperationView::Delete
    }
}

fn generation_hex(generation: JournalGeneration) -> String {
    generation
        .0
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn render_operation(
    kind: StoreKind,
    order: Option<JournalOrder>,
    operation: &OperationView,
) -> String {
    let mut record = match operation {
        OperationView::Replace { sidecar, sha256 } => serde_json::json!({
            "v": 1,
            "op": "replace",
            "kind": kind.label(),
            "sidecar": sidecar,
            "sha256": sha256,
        }),
        OperationView::Delete => serde_json::json!({
            "v": 1,
            "op": "delete",
            "kind": kind.label(),
        }),
    };
    if let Some(order) = order {
        let object = record
            .as_object_mut()
            .expect("generated journal record is an object");
        object.insert(
            "generation".to_owned(),
            serde_json::Value::String(generation_hex(order.generation)),
        );
        object.insert(
            "process_epoch".to_owned(),
            serde_json::Value::String(order.process_epoch.to_string()),
        );
        object.insert(
            "sequence".to_owned(),
            serde_json::Value::String(order.sequence.to_string()),
        );
    }
    record.to_string()
}

fn render_event(kind: StoreKind, event: &GeneratedEvent) -> String {
    match event {
        GeneratedEvent::Commit(order) => serde_json::json!({
            "v": 1,
            "op": "commit",
            "kind": kind.label(),
            "generation": generation_hex(order.generation),
            "process_epoch": order.process_epoch.to_string(),
            "sequence": order.sequence.to_string(),
        })
        .to_string(),
        GeneratedEvent::OrderedCandidate(candidate) => {
            render_operation(kind, candidate.order, &candidate.operation)
        }
        GeneratedEvent::TransitionalCandidate(candidate) => {
            let order = candidate
                .order
                .expect("transitional candidate carries an order");
            let mut record = serde_json::from_str::<serde_json::Value>(&render_operation(
                kind,
                None,
                &candidate.operation,
            ))
            .expect("generated transitional record is valid JSON");
            let object = record
                .as_object_mut()
                .expect("generated transitional record is an object");
            object.insert(
                "generation".to_owned(),
                serde_json::Value::String(generation_hex(order.generation)),
            );
            object.insert(
                "accepted_unix_nanos".to_owned(),
                serde_json::Value::String(order.sequence.to_string()),
            );
            record.to_string()
        }
        GeneratedEvent::LegacyCandidate(operation) => render_operation(kind, None, operation),
        GeneratedEvent::Invalid(InvalidLine::TornJson) => "{\"v\":1".to_owned(),
        GeneratedEvent::Invalid(InvalidLine::WrongVersion) => serde_json::json!({
            "v": 2,
            "op": "delete",
            "kind": kind.label(),
        })
        .to_string(),
        GeneratedEvent::Invalid(InvalidLine::WrongKind) => serde_json::json!({
            "v": 1,
            "op": "delete",
            "kind": StoreKind::Library.label(),
        })
        .to_string(),
        GeneratedEvent::Invalid(InvalidLine::IncompleteOrder) => serde_json::json!({
            "v": 1,
            "op": "delete",
            "kind": kind.label(),
            "generation": generation_hex(JournalGeneration([0x5a; 16])),
            "process_epoch": "7",
        })
        .to_string(),
        GeneratedEvent::Invalid(InvalidLine::UnknownOperation) => serde_json::json!({
            "v": 1,
            "op": "truncate",
            "kind": kind.label(),
        })
        .to_string(),
        GeneratedEvent::Blank => String::new(),
    }
}

fn generated_trace(seed: u64) -> Vec<GeneratedEvent> {
    let mut rng = fastrand::Rng::with_seed(seed);
    let event_count = 24 + rng.usize(0..(MAX_TRACE_EVENTS - 23));
    (0..event_count)
        .map(|step| {
            let order =
                journal_order_in_epoch(rng.u64(0..5), u128::from(rng.u64(0..32)), rng.u8(..));
            match rng.usize(0..14) {
                0..=2 => GeneratedEvent::OrderedCandidate(CandidateView {
                    order: Some(order),
                    operation: operation_view(seed, step, rng.bool()),
                }),
                3 => GeneratedEvent::TransitionalCandidate(CandidateView {
                    order: Some(JournalOrder {
                        process_epoch: 0,
                        sequence: order.sequence,
                        generation: order.generation,
                    }),
                    operation: operation_view(seed, step, rng.bool()),
                }),
                4..=5 => GeneratedEvent::Commit(order),
                6..=7 => GeneratedEvent::LegacyCandidate(operation_view(seed, step, rng.bool())),
                8 => GeneratedEvent::Invalid(InvalidLine::TornJson),
                9 => GeneratedEvent::Invalid(InvalidLine::WrongVersion),
                10 => GeneratedEvent::Invalid(InvalidLine::WrongKind),
                11 => GeneratedEvent::Invalid(if rng.bool() {
                    InvalidLine::IncompleteOrder
                } else {
                    InvalidLine::UnknownOperation
                }),
                12..=13 => GeneratedEvent::Blank,
                _ => unreachable!("generated choice is bounded"),
            }
        })
        .collect()
}

fn reference_state(events: &[GeneratedEvent]) -> ModelState {
    let committed_through = events
        .iter()
        .filter_map(|event| match event {
            GeneratedEvent::Commit(order) => Some(ModelOrder::from_journal(*order)),
            _ => None,
        })
        .max();
    let latest_ordered_line = events.iter().rposition(|event| {
        matches!(
            event,
            GeneratedEvent::Commit(_)
                | GeneratedEvent::OrderedCandidate(_)
                | GeneratedEvent::TransitionalCandidate(_)
        )
    });
    let invalid_after_latest_ordered = latest_ordered_line.is_some_and(|line_index| {
        events[line_index + 1..]
            .iter()
            .any(|event| matches!(event, GeneratedEvent::Invalid(_)))
    });
    let latest_legacy =
        events
            .iter()
            .enumerate()
            .rev()
            .find_map(|(line_index, event)| match event {
                GeneratedEvent::LegacyCandidate(operation) => Some((
                    line_index,
                    ModelCandidate {
                        order: None,
                        operation: operation.clone(),
                    },
                )),
                _ => None,
            });
    let ordered_winner = events
        .iter()
        .enumerate()
        .filter_map(|(line_index, event)| match event {
            GeneratedEvent::OrderedCandidate(candidate)
            | GeneratedEvent::TransitionalCandidate(candidate) => Some((
                line_index,
                ModelCandidate {
                    order: candidate.order.map(ModelOrder::from_journal),
                    operation: candidate.operation.clone(),
                },
            )),
            _ => None,
        })
        .max_by(|(left_line, left), (right_line, right)| {
            left.order
                .cmp(&right.order)
                // The protocol keeps the first record when an impossible conflicting duplicate
                // order is encountered, so an earlier physical line wins an exact tie.
                .then_with(|| right_line.cmp(left_line))
        })
        .map(|(_, candidate)| candidate)
        .filter(|candidate| {
            committed_through.is_none_or(|frontier| {
                candidate
                    .order
                    .is_some_and(|candidate_order| candidate_order > frontier)
            })
        });
    let later_legacy = latest_legacy.and_then(|(line_index, candidate)| {
        (latest_ordered_line.is_none_or(|ordered_line| line_index > ordered_line)
            && !invalid_after_latest_ordered)
            .then_some(candidate)
    });
    ModelState {
        committed_through,
        candidate: later_legacy.or(ordered_winner),
    }
}

fn implementation_state(state: &JournalState) -> ProtocolState {
    ProtocolState {
        committed_through: state.committed_through,
        candidate: state.candidate.as_ref().map(|candidate| CandidateView {
            order: candidate.order,
            operation: match &candidate.operation {
                JournalOperation::Replace { sidecar, sha256 } => OperationView::Replace {
                    sidecar: sidecar.clone(),
                    sha256: sha256.clone(),
                },
                JournalOperation::Delete => OperationView::Delete,
            },
        }),
    }
}

fn implementation_model_state(state: &JournalState) -> ModelState {
    let observed = implementation_state(state);
    ModelState {
        committed_through: observed.committed_through.map(ModelOrder::from_journal),
        candidate: observed.candidate.map(|candidate| ModelCandidate {
            order: candidate.order.map(ModelOrder::from_journal),
            operation: candidate.operation,
        }),
    }
}

fn event_order(event: &GeneratedEvent) -> Option<JournalOrder> {
    match event {
        GeneratedEvent::Commit(order) => Some(*order),
        GeneratedEvent::OrderedCandidate(candidate)
        | GeneratedEvent::TransitionalCandidate(candidate) => candidate.order,
        GeneratedEvent::LegacyCandidate(_) | GeneratedEvent::Invalid(_) | GeneratedEvent::Blank => {
            None
        }
    }
}

fn assert_verification_matches(
    parsed: &JournalState,
    modeled: &ModelState,
    order: JournalOrder,
    seed: u64,
) {
    let model_order = ModelOrder::from_journal(order);
    let expected = if modeled
        .candidate
        .as_ref()
        .is_some_and(|candidate| candidate.order == Some(model_order))
    {
        Some(IntentState::Current)
    } else if modeled
        .committed_through
        .is_some_and(|frontier| frontier >= model_order)
        || modeled.candidate.as_ref().is_some_and(|candidate| {
            candidate
                .order
                .is_none_or(|candidate_order| candidate_order > model_order)
        })
    {
        Some(IntentState::Superseded)
    } else {
        None
    };
    match expected {
        Some(expected) => assert_eq!(
            verify_intent_state(parsed, order).expect("modeled retained order is classifiable"),
            expected,
            "intent verification mismatch for seed {seed} and order {order:?}"
        ),
        None => assert!(
            verify_intent_state(parsed, order).is_err(),
            "unobserved order was incorrectly classified for seed {seed}: {order:?}"
        ),
    }
}

fn render_trace(kind: StoreKind, events: &[GeneratedEvent], terminal_newline: bool) -> String {
    let mut text = events
        .iter()
        .map(|event| render_event(kind, event))
        .collect::<Vec<_>>()
        .join("\n");
    if terminal_newline {
        text.push('\n');
    }
    text
}

#[test]
fn generated_journal_traces_match_the_reference_model_and_compact_safely() {
    let kind = StoreKind::Config;
    for seed in 0..PROPERTY_SEEDS {
        let events = generated_trace(seed);
        for prefix_len in 0..=events.len() {
            let prefix = &events[..prefix_len];
            let prefix_text = render_trace(kind, prefix, prefix_len % 2 == 0);
            assert_eq!(
                implementation_model_state(&parse_journal_state(kind, &prefix_text)),
                reference_state(prefix),
                "prefix mismatch for seed {seed} at step {prefix_len}: {prefix:#?}"
            );
        }
        let text = render_trace(kind, &events, seed % 2 == 0);
        let parsed = parse_journal_state(kind, &text);
        let expected = reference_state(&events);

        assert_eq!(
            implementation_model_state(&parsed),
            expected,
            "reference-model mismatch for seed {seed}: {events:#?}"
        );

        let compacted = compacted_journal_text(kind, &parsed);
        assert!(
            compacted.lines().count() <= 2,
            "compaction exceeded the protocol bound for seed {seed}"
        );
        assert_eq!(
            implementation_model_state(&parse_journal_state(kind, &compacted)),
            expected,
            "compaction changed the accepted state for seed {seed}: {events:#?}"
        );
        let reparsed_compacted = parse_journal_state(kind, &compacted);
        assert_eq!(
            compacted_journal_text(kind, &reparsed_compacted),
            compacted,
            "compaction was not byte-idempotent for seed {seed}"
        );

        let mut probes = vec![
            journal_order_in_epoch(0, 0, 0),
            journal_order_in_epoch(9, u128::from(seed), 0xff),
        ];
        probes.extend(events.iter().filter_map(event_order).take(6));
        if let Some(frontier) = expected.committed_through {
            probes.push(frontier.into_journal());
        }
        if let Some(candidate_order) = expected
            .candidate
            .as_ref()
            .and_then(|candidate| candidate.order)
        {
            probes.push(candidate_order.into_journal());
        }
        for probe in probes {
            assert_verification_matches(&parsed, &expected, probe, seed);
        }

        // Journal replacement always appends an ordered record after compaction. Prove that
        // compacting an arbitrary valid/corrupt prefix cannot change that append's meaning.
        let append = if seed % 3 == 0 {
            GeneratedEvent::Commit(journal_order_in_epoch(6, u128::from(seed % 41), 0xe1))
        } else {
            GeneratedEvent::OrderedCandidate(CandidateView {
                order: Some(journal_order_in_epoch(6, u128::from(seed % 41), 0xe2)),
                operation: operation_view(seed, MAX_TRACE_EVENTS, seed % 2 == 0),
            })
        };
        let mut appended_events = events.clone();
        appended_events.push(append.clone());
        let expected_after_append = reference_state(&appended_events);
        let append_line = render_event(kind, &append);
        let original_then_append = format!("{text}\n{append_line}\n");
        let compacted_then_append = format!("{compacted}{append_line}\n");
        assert_eq!(
            implementation_model_state(&parse_journal_state(kind, &original_then_append)),
            expected_after_append,
            "uncompacted append mismatch for seed {seed}"
        );
        assert_eq!(
            implementation_model_state(&parse_journal_state(kind, &compacted_then_append)),
            expected_after_append,
            "compacted append mismatch for seed {seed}"
        );
    }
}

#[derive(Clone, Copy, Debug)]
enum CandidateSpec {
    Replace(u8),
    Delete,
}

#[derive(Clone, Copy, Debug)]
enum InitialJournal {
    Ordered(CandidateSpec),
    LegacyReplace,
    FrontierOnly,
    FrontierAndCandidate,
}

#[derive(Clone, Copy, Debug)]
enum IncomingRecord {
    Candidate(CandidateSpec),
    Commit,
}

#[derive(Clone, Copy, Debug)]
enum PublishFault {
    BeforeVisible,
    AfterVisible,
}

#[derive(Clone, Copy, Debug)]
enum FullWriterPath {
    Actor,
    Panic,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct TinyState {
    value: u8,
}

fn payload(spec: CandidateSpec) -> Option<Vec<u8>> {
    match spec {
        CandidateSpec::Replace(value) => {
            Some(serde_json::to_vec(&TinyState { value }).expect("serialize test state"))
        }
        CandidateSpec::Delete => None,
    }
}

fn candidate_intent(spec: CandidateSpec, order: JournalOrder, path: &Path) -> JournalIntent {
    match payload(spec) {
        Some(bytes) => JournalIntent::Replace {
            order,
            kind: StoreKind::RomanizedTitles,
            path: path.to_path_buf(),
            bytes,
        },
        None => JournalIntent::Delete {
            order,
            kind: StoreKind::RomanizedTitles,
            path: path.to_path_buf(),
        },
    }
}

fn candidate_view_for(spec: CandidateSpec, order: JournalOrder, path: &Path) -> CandidateView {
    let operation = match payload(spec) {
        Some(bytes) => OperationView::Replace {
            sidecar: unique_intent_sidecar_path(path, order)
                .and_then(|sidecar| {
                    sidecar
                        .file_name()
                        .map(|name| name.to_string_lossy().into())
                })
                .expect("test order produces a sidecar path"),
            sha256: sha256_hex(&bytes),
        },
        None => OperationView::Delete,
    };
    CandidateView {
        order: Some(order),
        operation,
    }
}

fn apply_ordered_record(
    mut state: ProtocolState,
    incoming: IncomingRecord,
    order: JournalOrder,
    path: &Path,
) -> ProtocolState {
    match incoming {
        IncomingRecord::Candidate(spec) => {
            let candidate = candidate_view_for(spec, order, path);
            if state
                .candidate
                .as_ref()
                .and_then(|current| current.order)
                .is_none_or(|current| order > current)
            {
                state.candidate = Some(candidate);
            }
        }
        IncomingRecord::Commit => {
            if state
                .candidate
                .as_ref()
                .is_some_and(|candidate| candidate.order.is_none())
            {
                state.candidate = None;
            }
            state.committed_through = Some(
                state
                    .committed_through
                    .map_or(order, |current| current.max(order)),
            );
        }
    }
    if state.committed_through.is_some_and(|frontier| {
        state
            .candidate
            .as_ref()
            .and_then(|candidate| candidate.order)
            .is_some_and(|candidate_order| candidate_order <= frontier)
    }) {
        state.candidate = None;
    }
    state
}

fn prepare_incoming(
    incoming: IncomingRecord,
    order: JournalOrder,
    path: &Path,
) -> PreparedJournalRecord {
    match incoming {
        IncomingRecord::Candidate(spec) => {
            prepare_journal_record(&candidate_intent(spec, order, path))
                .expect("prepare incoming candidate")
        }
        IncomingRecord::Commit => {
            PreparedJournalRecord::without_sidecar(commit_record(StoreKind::RomanizedTitles, order))
        }
    }
}

fn retry_incoming(incoming: IncomingRecord, order: JournalOrder, path: &Path) {
    match incoming {
        IncomingRecord::Candidate(spec) => {
            write_journal_intent(&candidate_intent(spec, order, path))
                .expect("candidate retry converges");
        }
        IncomingRecord::Commit => {
            commit_journal_generation(StoreKind::RomanizedTitles, path, order)
                .expect("commit retry converges");
        }
    }
}

fn referenced_sidecars(state: &ProtocolState) -> BTreeSet<String> {
    state
        .candidate
        .iter()
        .filter_map(|candidate| match &candidate.operation {
            OperationView::Replace { sidecar, .. } => Some(sidecar.clone()),
            OperationView::Delete => None,
        })
        .collect()
}

fn assert_replayable(path: &Path, state: &ProtocolState) {
    let expected = match state
        .candidate
        .as_ref()
        .map(|candidate| &candidate.operation)
    {
        Some(OperationView::Replace { sidecar, sha256 }) => {
            let bytes = std::fs::read(path.parent().expect("test path has parent").join(sidecar))
                .expect("every accepted replace candidate retains its sidecar");
            assert_eq!(sha256_hex(&bytes), *sha256);
            serde_json::from_slice(&bytes).expect("generated sidecar decodes")
        }
        Some(OperationView::Delete) => TinyState::default(),
        None => TinyState { value: 0xfe },
    };
    assert_eq!(
        replay_journaled_snapshot(
            StoreKind::RomanizedTitles,
            path,
            TinyState { value: 0xfe },
            1024,
        ),
        expected
    );
}

fn assert_referenced_sidecar_available(path: &Path, state: &ProtocolState) {
    let Some(CandidateView {
        operation: OperationView::Replace { sidecar, sha256 },
        ..
    }) = &state.candidate
    else {
        return;
    };
    let bytes = std::fs::read(path.parent().expect("test path has parent").join(sidecar))
        .expect("every durable possibility retains its replace sidecar");
    assert_eq!(sha256_hex(&bytes), *sha256);
}

fn atomic_temp_artifacts(directory: &Path) -> Vec<String> {
    std::fs::read_dir(directory)
        .expect("test directory is readable")
        .map(|entry| {
            entry
                .expect("test directory entry is readable")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.contains(".tmp."))
        .collect()
}

fn seed_initial_journal(initial: InitialJournal, path: &Path) {
    let frontier = journal_order_in_epoch(4, 20, 0x41);
    match initial {
        InitialJournal::Ordered(spec) => {
            write_journal_intent(&candidate_intent(spec, frontier, path))
                .expect("seed ordered candidate");
        }
        InitialJournal::LegacyReplace => {
            let bytes = serde_json::to_vec(&TinyState { value: 1 }).unwrap();
            let sidecar = intent_sidecar_path(path).expect("legacy sidecar path");
            crate::util::safe_fs::write_private_atomic(&sidecar, &bytes).unwrap();
            append_journal_record(
                path,
                &serde_json::json!({
                    "v": 1,
                    "op": "replace",
                    "kind": StoreKind::RomanizedTitles.label(),
                    "sidecar": sidecar.file_name().unwrap().to_string_lossy(),
                    "sha256": sha256_hex(&bytes),
                }),
            )
            .unwrap();
        }
        InitialJournal::FrontierOnly => {
            commit_journal_generation(StoreKind::RomanizedTitles, path, frontier)
                .expect("seed committed frontier");
        }
        InitialJournal::FrontierAndCandidate => {
            commit_journal_generation(StoreKind::RomanizedTitles, path, frontier)
                .expect("seed committed frontier");
            let candidate = journal_order_in_epoch(4, 30, 0x42);
            write_journal_intent(&candidate_intent(
                CandidateSpec::Replace(1),
                candidate,
                path,
            ))
            .expect("seed candidate above frontier");
        }
    }
}

fn write_base_with_one_shot_fault(
    path: &Path,
    bytes: &[u8],
    fault: PublishFault,
    fail_once: &std::sync::atomic::AtomicBool,
) -> std::io::Result<()> {
    if !fail_once.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return crate::util::safe_fs::write_private_atomic(path, bytes);
    }
    match fault {
        PublishFault::BeforeVisible => Err(std::io::Error::other(
            "fault injection before store base publish",
        )),
        PublishFault::AfterVisible => {
            crate::util::safe_fs::write_private_atomic(path, bytes)?;
            Err(std::io::Error::other(
                "fault injection after store base publish",
            ))
        }
    }
}

fn remove_base_with_one_shot_fault(
    path: &Path,
    fault: PublishFault,
    fail_once: &std::sync::atomic::AtomicBool,
) -> std::io::Result<()> {
    if !fail_once.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return remove_store_file(path).map(|_| ());
    }
    match fault {
        PublishFault::BeforeVisible => Err(std::io::Error::other(
            "fault injection before store base unlink",
        )),
        PublishFault::AfterVisible => {
            remove_store_file(path)?;
            Err(std::io::Error::other(
                "fault injection after store base unlink",
            ))
        }
    }
}

#[test]
fn actor_and_panic_full_transactions_never_commit_a_failed_base_write() {
    for (case, (writer_path, fault)) in [
        (FullWriterPath::Actor, PublishFault::BeforeVisible),
        (FullWriterPath::Actor, PublishFault::AfterVisible),
        (FullWriterPath::Panic, PublishFault::BeforeVisible),
        (FullWriterPath::Panic, PublishFault::AfterVisible),
    ]
    .into_iter()
    .enumerate()
    {
        clear_startup_recovery_error_for_test();
        let directory = temp_dir(&format!("full-writer-fault-{case}"));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.json");
        let old_bytes = br#"{"value":1}"#;
        let recovered_bytes = serde_json::to_vec(&serde_json::Value::Null).unwrap();
        crate::util::safe_fs::write_private_atomic(&path, old_bytes).unwrap();
        let order = journal_order_in_epoch(12, case as u128 + 1, 0x81 + case as u8);
        let fail_once = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

        match writer_path {
            FullWriterPath::Actor => {
                let writer_path = path.clone();
                let writer_bytes = recovered_bytes.clone();
                let writer_fail_once = std::sync::Arc::clone(&fail_once);
                let operation = PendingOperation::save(
                    Snapshot::Test {
                        kind: StoreKind::Config,
                        label: "fault-injected actor replace",
                        storage_path: Some(path.clone()),
                        writer: std::sync::Arc::new(move || {
                            write_base_with_one_shot_fault(
                                &writer_path,
                                &writer_bytes,
                                fault,
                                &writer_fail_once,
                            )
                        }),
                    },
                    accepted_order(order),
                );
                assert_eq!(
                    write_operation_durable(&operation).unwrap_err().kind(),
                    std::io::ErrorKind::Other
                );
                assert_failed_base_write_remains_pending(
                    &path,
                    order,
                    fault,
                    old_bytes,
                    &recovered_bytes,
                );
                write_operation_durable(&operation).expect("actor retry commits");
            }
            FullWriterPath::Panic => {
                let writer_fail_once = std::sync::Arc::clone(&fail_once);
                let operation = PanicOperation::replace_with_writer_for_test(
                    order,
                    StoreKind::Config,
                    path.clone(),
                    recovered_bytes.clone(),
                    std::sync::Arc::new(move |path, bytes| {
                        write_base_with_one_shot_fault(path, bytes, fault, &writer_fail_once)
                    }),
                );
                assert_eq!(
                    write_panic_operation(&operation).unwrap_err().kind(),
                    std::io::ErrorKind::Other
                );
                assert_failed_base_write_remains_pending(
                    &path,
                    order,
                    fault,
                    old_bytes,
                    &recovered_bytes,
                );
                write_panic_operation(&operation).expect("panic retry commits");
            }
        }

        let settled = read_journal_state(StoreKind::Config, &path).unwrap();
        assert_eq!(settled.committed_through, Some(order));
        assert!(settled.candidate.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), recovered_bytes);
        assert!(!unique_intent_sidecar_path(&path, order).unwrap().exists());
        assert!(atomic_temp_artifacts(&directory).is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }
    clear_startup_recovery_error_for_test();
}

fn assert_failed_base_write_remains_pending(
    path: &Path,
    order: JournalOrder,
    fault: PublishFault,
    old_bytes: &[u8],
    recovered_bytes: &[u8],
) {
    let pending = read_journal_state(StoreKind::Config, path).unwrap();
    assert_eq!(pending.committed_through, None);
    assert_eq!(
        pending
            .candidate
            .as_ref()
            .and_then(|candidate| candidate.order),
        Some(order)
    );
    assert!(unique_intent_sidecar_path(path, order).unwrap().exists());
    let expected_base = match fault {
        PublishFault::BeforeVisible => old_bytes,
        PublishFault::AfterVisible => recovered_bytes,
    };
    assert_eq!(std::fs::read(path).unwrap(), expected_base);
    drop(acquire_intent_lock(path).expect("failed full transaction released its lock"));
}

#[test]
fn actor_and_panic_delete_transactions_never_commit_a_failed_unlink() {
    const BASE_BYTES: &[u8] = br#"{"cached":true}"#;
    for (case, (writer_path, fault)) in [
        (FullWriterPath::Actor, PublishFault::BeforeVisible),
        (FullWriterPath::Actor, PublishFault::AfterVisible),
        (FullWriterPath::Panic, PublishFault::BeforeVisible),
        (FullWriterPath::Panic, PublishFault::AfterVisible),
    ]
    .into_iter()
    .enumerate()
    {
        clear_startup_recovery_error_for_test();
        let directory = temp_dir(&format!("delete-writer-fault-{case}"));
        std::fs::create_dir_all(&directory).unwrap();
        let override_value = directory.to_string_lossy().into_owned();
        crate::test_util::env::with_var("YTM_DATA_DIR", Some(&override_value), || {
            let path = crate::romanize::cache_path().expect("test data override resolves cache");
            crate::util::safe_fs::write_private_atomic(&path, BASE_BYTES).unwrap();
            let order = journal_order_in_epoch(13, case as u128 + 1, 0x91 + case as u8);
            let fail_once = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

            match writer_path {
                FullWriterPath::Actor => {
                    let operation = PendingOperation::new(
                        PendingAction::DeleteRomanizedTitles,
                        accepted_order(order),
                    );
                    let remove_fail_once = std::sync::Arc::clone(&fail_once);
                    assert_eq!(
                        write_operation_durable_using(&operation, || {
                            remove_base_with_one_shot_fault(&path, fault, &remove_fail_once)
                        })
                        .unwrap_err()
                        .kind(),
                        std::io::ErrorKind::Other
                    );
                    assert_failed_delete_remains_pending(&path, order, fault, BASE_BYTES);
                    let remove_fail_once = std::sync::Arc::clone(&fail_once);
                    write_operation_durable_using(&operation, || {
                        remove_base_with_one_shot_fault(&path, fault, &remove_fail_once)
                    })
                    .expect("actor delete retry commits");
                }
                FullWriterPath::Panic => {
                    let remove_fail_once = std::sync::Arc::clone(&fail_once);
                    let operation = PanicOperation::delete_with_remover_for_test(
                        order,
                        path.clone(),
                        std::sync::Arc::new(move |path| {
                            remove_base_with_one_shot_fault(path, fault, &remove_fail_once)
                        }),
                    );
                    assert_eq!(
                        write_panic_operation(&operation).unwrap_err().kind(),
                        std::io::ErrorKind::Other
                    );
                    assert_failed_delete_remains_pending(&path, order, fault, BASE_BYTES);
                    write_panic_operation(&operation).expect("panic delete retry commits");
                }
            }

            let settled = read_journal_state(StoreKind::RomanizedTitles, &path).unwrap();
            assert_eq!(settled.committed_through, Some(order));
            assert!(settled.candidate.is_none());
            assert!(!path.exists());
            assert!(atomic_temp_artifacts(&directory).is_empty());
        });
        std::fs::remove_dir_all(directory).unwrap();
    }
    clear_startup_recovery_error_for_test();
}

fn assert_failed_delete_remains_pending(
    path: &Path,
    order: JournalOrder,
    fault: PublishFault,
    base_bytes: &[u8],
) {
    let pending = read_journal_state(StoreKind::RomanizedTitles, path).unwrap();
    assert_eq!(pending.committed_through, None);
    assert_eq!(
        pending
            .candidate
            .as_ref()
            .and_then(|candidate| candidate.order),
        Some(order)
    );
    assert!(matches!(
        pending
            .candidate
            .as_ref()
            .map(|candidate| &candidate.operation),
        Some(JournalOperation::Delete)
    ));
    match fault {
        PublishFault::BeforeVisible => assert_eq!(std::fs::read(path).unwrap(), base_bytes),
        PublishFault::AfterVisible => {
            assert!(!path.exists());
            crate::util::safe_fs::write_private_atomic(path, base_bytes)
                .expect("simulate crash rollback restoring the unlinked base name");
        }
    }
    assert_eq!(
        replay_journaled_snapshot(
            StoreKind::RomanizedTitles,
            path,
            TinyState { value: 0xfe },
            1024,
        ),
        TinyState::default(),
        "pending delete remains authoritative if the base name reappears"
    );
    drop(acquire_intent_lock(path).expect("failed delete transaction released its lock"));
}

#[test]
fn journal_publish_fault_matrix_preserves_all_recoverable_frontiers() {
    let mut case = 0_u64;
    for initial in [
        InitialJournal::Ordered(CandidateSpec::Replace(1)),
        InitialJournal::Ordered(CandidateSpec::Delete),
        InitialJournal::LegacyReplace,
        InitialJournal::FrontierOnly,
        InitialJournal::FrontierAndCandidate,
    ] {
        for incoming in [
            IncomingRecord::Candidate(CandidateSpec::Replace(2)),
            IncomingRecord::Candidate(CandidateSpec::Delete),
            IncomingRecord::Commit,
        ] {
            for (sequence, marker) in [(10_u128, 0x31_u8), (40, 0x51)] {
                let incoming_order = journal_order_in_epoch(4, sequence, marker);
                for fault in [PublishFault::BeforeVisible, PublishFault::AfterVisible] {
                    case += 1;
                    clear_startup_recovery_error_for_test();
                    let directory = temp_dir(&format!("protocol-fault-{case}"));
                    std::fs::create_dir_all(&directory).unwrap();
                    let path = directory.join("romanized.json");
                    seed_initial_journal(initial, &path);
                    let before = implementation_state(
                        &read_journal_state(StoreKind::RomanizedTitles, &path).unwrap(),
                    );
                    let journal_path = intent_journal_path(&path).unwrap();
                    let before_journal = std::fs::read(&journal_path).unwrap();
                    let desired =
                        apply_ordered_record(before.clone(), incoming, incoming_order, &path);

                    {
                        let _lock = acquire_intent_lock(&path).unwrap();
                        let record = prepare_incoming(incoming, incoming_order, &path);
                        let error = replace_journal_with_record_locked_by(
                            StoreKind::RomanizedTitles,
                            &path,
                            &record,
                            |journal_path, bytes| match fault {
                                PublishFault::BeforeVisible => Err(std::io::Error::other(
                                    "fault injection before journal publish",
                                )),
                                PublishFault::AfterVisible => {
                                    crate::util::safe_fs::write_private_atomic(
                                        journal_path,
                                        bytes,
                                    )?;
                                    Err(std::io::Error::other(
                                        "fault injection after journal publish",
                                    ))
                                }
                            },
                        )
                        .err()
                        .expect("injected journal publication fault surfaces");
                        assert_eq!(error.kind(), std::io::ErrorKind::Other);
                    }

                    let observed = implementation_state(
                        &read_journal_state(StoreKind::RomanizedTitles, &path).unwrap(),
                    );
                    let observed_journal = std::fs::read(&journal_path).unwrap();
                    let expected_observed = match fault {
                        PublishFault::BeforeVisible => &before,
                        PublishFault::AfterVisible => &desired,
                    };
                    assert_eq!(
                        &observed, expected_observed,
                        "wrong visible frontier for {initial:?} -> {incoming:?} at {fault:?}"
                    );

                    // A failed atomic replacement has two possible durable outcomes. Every
                    // sidecar referenced by either one must remain until a confirmed retry.
                    assert_referenced_sidecar_available(&path, &before);
                    assert_referenced_sidecar_available(&path, &observed);
                    assert_replayable(&path, &observed);
                    {
                        let _lock = acquire_intent_lock(&path).unwrap();
                        crate::util::safe_fs::write_private_atomic(&journal_path, &before_journal)
                            .expect("simulate crash rollback to the previous journal");
                    }
                    assert_replayable(&path, &before);
                    {
                        let _lock = acquire_intent_lock(&path).unwrap();
                        crate::util::safe_fs::write_private_atomic(
                            &journal_path,
                            &observed_journal,
                        )
                        .expect("restore the observed journal before retry");
                    }
                    assert_replayable(&path, &observed);
                    let ambiguous_sidecars = referenced_sidecars(&before)
                        .into_iter()
                        .chain(referenced_sidecars(&observed))
                        .collect::<BTreeSet<_>>();
                    assert_eq!(
                        intent_sidecar_count(&directory, "romanized.json"),
                        ambiguous_sidecars.len(),
                        "ambiguous publication retained the wrong sidecar set"
                    );
                    assert!(atomic_temp_artifacts(&directory).is_empty());
                    drop(acquire_intent_lock(&path).expect("intent lock is reacquirable"));

                    retry_incoming(incoming, incoming_order, &path);
                    let settled = implementation_state(
                        &read_journal_state(StoreKind::RomanizedTitles, &path).unwrap(),
                    );
                    assert_eq!(settled, desired, "retry did not converge");
                    assert_replayable(&path, &settled);
                    assert_eq!(
                        intent_sidecar_count(&directory, "romanized.json"),
                        referenced_sidecars(&settled).len(),
                        "confirmed retry did not reclaim obsolete sidecars"
                    );
                    let journal = std::fs::read_to_string(intent_journal_path(&path).unwrap())
                        .expect("settled journal remains readable");
                    assert!(journal.lines().count() <= 2);
                    assert!(atomic_temp_artifacts(&directory).is_empty());
                    std::fs::remove_dir_all(directory).unwrap();
                }
            }
        }
    }
    clear_startup_recovery_error_for_test();
}

#[test]
fn config_base_publish_faults_keep_the_exact_candidate_for_restart_retry() {
    let old_value = serde_json::json!({"theme": "old"});
    let recovered_value = serde_json::json!({"theme": "recovered"});
    for (case, fault) in [PublishFault::BeforeVisible, PublishFault::AfterVisible]
        .into_iter()
        .enumerate()
    {
        clear_startup_recovery_error_for_test();
        let directory = temp_dir(&format!("config-base-fault-{case}"));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.json");
        crate::util::safe_fs::write_private_atomic_json(&path, &old_value).unwrap();
        let order = journal_order_in_epoch(11, case as u128 + 1, 0x71 + case as u8);
        write_journal_intent(&JournalIntent::Replace {
            order,
            kind: StoreKind::Config,
            path: path.clone(),
            bytes: serde_json::to_vec(&recovered_value).unwrap(),
        })
        .unwrap();
        let sidecar = unique_intent_sidecar_path(&path, order).unwrap();

        let transaction = begin_config_recovery(&path).replay(old_value.clone(), 1024);
        assert!(transaction.has_replayed_candidate());
        let (value, result) = transaction.install_and_settle_with(|path, value| match fault {
            PublishFault::BeforeVisible => Err(std::io::Error::other(
                "fault injection before config base publish",
            )),
            PublishFault::AfterVisible => {
                crate::util::safe_fs::write_private_atomic_json(path, value)?;
                Err(std::io::Error::other(
                    "fault injection after config base publish",
                ))
            }
        });
        assert_eq!(value, recovered_value);
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Other);
        let expected_base = match fault {
            PublishFault::BeforeVisible => &old_value,
            PublishFault::AfterVisible => &recovered_value,
        };
        let visible_base = serde_json::from_slice::<serde_json::Value>(
            &crate::util::safe_fs::read_no_symlink_limited(&path, 1024).unwrap(),
        )
        .unwrap();
        assert_eq!(&visible_base, expected_base);
        let pending = read_journal_state(StoreKind::Config, &path).unwrap();
        assert_eq!(
            pending
                .candidate
                .as_ref()
                .and_then(|candidate| candidate.order),
            Some(order)
        );
        assert!(sidecar.exists());
        drop(acquire_intent_lock(&path).expect("failed transaction released its lock"));

        // A restart may observe either base image, but the exact pending receipt remains
        // authoritative and converges both possibilities to the same installed value.
        let current = serde_json::from_slice::<serde_json::Value>(
            &crate::util::safe_fs::read_no_symlink_limited(&path, 1024).unwrap(),
        )
        .unwrap();
        let retry = begin_config_recovery(&path).replay(current, 1024);
        assert!(retry.has_replayed_candidate());
        let (installed, result) = retry.install_and_settle();
        result.expect("restart retry installs and settles the candidate");
        assert_eq!(installed, recovered_value);
        let on_disk = serde_json::from_slice::<serde_json::Value>(
            &crate::util::safe_fs::read_no_symlink_limited(&path, 1024).unwrap(),
        )
        .unwrap();
        assert_eq!(on_disk, recovered_value);
        let settled = read_journal_state(StoreKind::Config, &path).unwrap();
        assert_eq!(settled.committed_through, Some(order));
        assert!(settled.candidate.is_none());
        assert!(!sidecar.exists());
        assert!(atomic_temp_artifacts(&directory).is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }
    clear_startup_recovery_error_for_test();
}
