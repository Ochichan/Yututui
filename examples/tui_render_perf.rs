//! Deterministic in-process render/allocation benchmark.
//!
//! This is intentionally an `example`, not a criterion benchmark: the exact same source can
//! be copied onto the frozen baseline tree, it needs no new dependency, and it emits one stable
//! JSON schema that `scripts/tui-perf.py compare` can pair across revisions.
//!
//! ## Interactive Player performance TODO
//!
//! `TODO(interactive-player-canvas-perf)`: recover the full-field Player canvas cost without
//! lowering configured FPS, density, animation phase, or visual cadence. Against frozen
//! `origin/main` `435e362c8c0bea05df8f2688a6b748609d841a83`, seven alternating release-mode
//! AB/BA pairs recorded median candidate/baseline draw-p95 ratios of 1.234 (art 100x30), 1.310
//! (art 160x50), 1.366 (art + lyrics 100x30), and 1.547 (art + lyrics 160x50). The acceptance
//! ceiling is 1.10; baseline mean-draw CV remained between 0.40% and 1.93%.
//!
//! The lyrics cases compare intentionally different work: the frozen baseline suppresses canvas
//! rendering while lyrics are visible, whereas the interactive layout requires one full-Player
//! canvas behind lyrics. Keep the four `canvas_art*` cases below until the debt is resolved, and
//! report them separately from the pre-existing general `render_and_interaction` gate. Resolution
//! requires a fresh seven-pair AB/BA run with baseline CV at most 10%, at least six passing pairs,
//! draw p95 at most 1.10, and the original one-sided ytt/process-tree CPU bounds at most 1.05.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::hash_map::DefaultHasher;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use image::DynamicImage;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui_image::picker::Picker;
use ratatui_image::thread::ThreadProtocol;
use serde::Serialize;
use sha2::{Digest, Sha256};
use yututui::api::Song;
use yututui::app::{
    AiMessage, AiRole, App, LibraryTab, LocalSection, Mode, Msg, SearchFocus, TrackLyrics,
};
use yututui::config::PlayerBarPosition;
use yututui::i18n::Language;
use yututui::local::{LocalIndex, LocalTrack};
use yututui::lyrics::LyricLine;

const SCHEMA: &str = "ytt.tui-perf.render.v1";
const LOCAL_SCROLL_TRACKS: usize = 180;
const LOCAL_LARGE_TRACKS: usize = 999;
const LOCAL_FILTER_STRIDE: usize = 17;

struct CountingAllocator;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static REALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED: AtomicU64 = AtomicU64::new(0);
static DEALLOCATED: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicU64 = AtomicU64::new(0);
static WINDOW_PEAK: AtomicU64 = AtomicU64::new(0);

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

// SAFETY: every operation is delegated unchanged to the process System allocator. The atomics
// only observe sizes and never influence the pointer, layout, or allocation result.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarding the caller-provided layout to System is the GlobalAlloc contract.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
            let live =
                LIVE.fetch_add(layout.size() as u64, Ordering::Relaxed) + layout.size() as u64;
            update_peak(live);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        DEALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: pointer/layout are the exact pair supplied by the GlobalAlloc caller.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarding the caller-provided allocation and size to System.
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            REALLOCS.fetch_add(1, Ordering::Relaxed);
            if new_size >= layout.size() {
                let growth = (new_size - layout.size()) as u64;
                ALLOCATED.fetch_add(growth, Ordering::Relaxed);
                let live = LIVE.fetch_add(growth, Ordering::Relaxed) + growth;
                update_peak(live);
            } else {
                let shrink = (layout.size() - new_size) as u64;
                DEALLOCATED.fetch_add(shrink, Ordering::Relaxed);
                LIVE.fetch_sub(shrink, Ordering::Relaxed);
            }
        }
        new_pointer
    }
}

fn update_peak(live: u64) {
    let mut peak = WINDOW_PEAK.load(Ordering::Relaxed);
    while live > peak {
        match WINDOW_PEAK.compare_exchange_weak(peak, live, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(current) => peak = current,
        }
    }
}

#[derive(Clone, Copy)]
struct AllocSnapshot {
    allocs: u64,
    reallocs: u64,
    allocated: u64,
    deallocated: u64,
    live: u64,
}

impl AllocSnapshot {
    fn now() -> Self {
        Self {
            allocs: ALLOCS.load(Ordering::Relaxed),
            reallocs: REALLOCS.load(Ordering::Relaxed),
            allocated: ALLOCATED.load(Ordering::Relaxed),
            deallocated: DEALLOCATED.load(Ordering::Relaxed),
            live: LIVE.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
struct Args {
    output: Option<PathBuf>,
    case: Option<String>,
    warmup: usize,
    batches: usize,
    draws: usize,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut output = None;
        let mut case = None;
        let mut warmup = 20;
        let mut batches = 20;
        let mut draws = 200;
        let mut raw = std::env::args().skip(1);
        while let Some(arg) = raw.next() {
            let next = |name: &str, it: &mut std::iter::Skip<std::env::Args>| {
                it.next().ok_or_else(|| format!("{name} requires a value"))
            };
            match arg.as_str() {
                "--output" => output = Some(PathBuf::from(next("--output", &mut raw)?)),
                "--case" => case = Some(next("--case", &mut raw)?),
                "--warmup" => warmup = positive_usize("--warmup", &next("--warmup", &mut raw)?)?,
                "--batches" => {
                    batches = positive_usize("--batches", &next("--batches", &mut raw)?)?
                }
                "--draws" => draws = positive_usize("--draws", &next("--draws", &mut raw)?)?,
                "-h" | "--help" => return Err(usage().to_string()),
                other => return Err(format!("unknown argument `{other}`\n\n{}", usage())),
            }
        }
        Ok(Self {
            output,
            case,
            warmup,
            batches,
            draws,
        })
    }
}

fn positive_usize(name: &str, raw: &str) -> Result<usize, String> {
    raw.parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{name} must be a positive integer"))
}

fn usage() -> &'static str {
    "Usage: tui_render_perf [--output FILE] [--case NAME] [--warmup N] [--batches N] [--draws N]\n\
     Cases: player, search50, library999, ai_long, reducer_input, retro160x50, normal80x24, \
     local_empty60x16, local_scroll72x18, local_max120x30, local_filter_ko90x24, \
     canvas_art100x30, canvas_art160x50, canvas_art_lyrics100x30, \
     canvas_art_lyrics160x50, animation_half100x30, animation_half_art_lyrics160x50, \
     animation_heavy_half100x30, animation_heavy_half_art_lyrics160x50"
}

#[derive(Serialize)]
struct LatencyBucket {
    ns: u128,
    count: usize,
}

#[derive(Serialize)]
struct BatchReport {
    batch: usize,
    draws: usize,
    total_ns: u128,
    mean_draw_ns: f64,
    p50_draw_ns: u128,
    p95_draw_ns: u128,
    max_draw_ns: u128,
    latency_histogram: Vec<LatencyBucket>,
    allocations: u64,
    reallocations: u64,
    allocated_bytes: u64,
    deallocated_bytes: u64,
    retained_bytes_delta: i64,
    peak_live_bytes_delta: u64,
}

#[derive(Serialize)]
struct CaseReport {
    name: String,
    update_path: &'static str,
    width: u16,
    height: u16,
    warmup_draws: usize,
    measured_draws: usize,
    total_draw_ns: u128,
    mean_draw_ns: f64,
    p50_draw_ns: u128,
    p95_draw_ns: u128,
    max_draw_ns: u128,
    latency_histogram: Vec<LatencyBucket>,
    batches: Vec<BatchReport>,
    buffer_style_digest: String,
    hit_map_digest: String,
    checkpoint_digest: String,
}

#[derive(Serialize)]
struct Report {
    schema: &'static str,
    kind: &'static str,
    binary: PathBuf,
    binary_sha256: String,
    scenario_sha256: Option<String>,
    run_id: String,
    started_unix_ns: u128,
    finished_unix_ns: u128,
    os: &'static str,
    arch: &'static str,
    batches_per_case: usize,
    draws_per_batch: usize,
    cases: Vec<CaseReport>,
}

fn main() {
    let args = match Args::parse() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("tui_render_perf: {message}");
            std::process::exit(2);
        }
    };
    let binary = std::env::current_exe().unwrap_or_else(|error| {
        eprintln!("tui_render_perf: locate current executable: {error}");
        std::process::exit(2);
    });
    let binary_sha256 = sha256_file(&binary).unwrap_or_else(|message| {
        eprintln!("tui_render_perf: {message}");
        std::process::exit(2);
    });
    let selected = args.case.as_deref();
    let run_id = std::env::var("TUI_PERF_RUN_ID").unwrap_or_else(|_| {
        eprintln!("tui_render_perf: TUI_PERF_RUN_ID is required");
        std::process::exit(2);
    });
    let started_unix_ns = unix_time_ns();
    let specs = case_specs();
    if let Some(case) = selected
        && !specs.iter().any(|spec| spec.name == case)
    {
        eprintln!("tui_render_perf: unknown case `{case}`\n\n{}", usage());
        std::process::exit(2);
    }

    let cases = specs
        .into_iter()
        .filter(|spec| selected.is_none_or(|case| spec.name == case))
        .map(|spec| run_case(spec, args.warmup, args.batches, args.draws))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|message| {
            eprintln!("tui_render_perf: {message}");
            std::process::exit(2);
        });
    let report = Report {
        schema: SCHEMA,
        kind: "render_summary",
        binary,
        binary_sha256,
        scenario_sha256: std::env::var("TUI_PERF_SCENARIO_SHA256").ok(),
        run_id,
        started_unix_ns,
        finished_unix_ns: unix_time_ns(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        batches_per_case: args.batches,
        draws_per_batch: args.draws,
        cases,
    };
    let encoded = serde_json::to_vec_pretty(&report).expect("serialize render report");
    match args.output {
        Some(path) => {
            if let Some(parent) = path.parent()
                && let Err(error) = std::fs::create_dir_all(parent)
            {
                eprintln!("tui_render_perf: create {}: {error}", parent.display());
                std::process::exit(2);
            }
            let mut out = BufWriter::new(File::create(&path).unwrap_or_else(|error| {
                eprintln!("tui_render_perf: create {}: {error}", path.display());
                std::process::exit(2);
            }));
            out.write_all(&encoded).expect("write render report");
            out.write_all(b"\n").expect("terminate render report");
        }
        None => {
            std::io::stdout()
                .write_all(&encoded)
                .expect("write render report");
            println!();
        }
    }
}

fn unix_time_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|error| {
            eprintln!("tui_render_perf: system clock before Unix epoch: {error}");
            std::process::exit(2);
        })
        .as_nanos()
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[derive(Clone, Copy)]
struct CaseSpec {
    name: &'static str,
    update_path: &'static str,
    width: u16,
    height: u16,
    language: Language,
    build: fn() -> App,
    update: fn(&mut App, usize),
}

fn case_specs() -> Vec<CaseSpec> {
    vec![
        CaseSpec {
            name: "player",
            update_path: "direct_fixture_state",
            width: 100,
            height: 30,
            language: Language::English,
            build: player_app,
            update: mutate_for_interaction,
        },
        CaseSpec {
            name: "search50",
            update_path: "direct_fixture_state",
            width: 100,
            height: 30,
            language: Language::English,
            build: search_app,
            update: mutate_for_interaction,
        },
        CaseSpec {
            name: "library999",
            update_path: "direct_fixture_state",
            width: 100,
            height: 30,
            language: Language::English,
            build: library_app,
            update: mutate_for_interaction,
        },
        CaseSpec {
            name: "ai_long",
            update_path: "direct_fixture_state",
            width: 100,
            height: 30,
            language: Language::English,
            build: ai_app,
            update: mutate_for_interaction,
        },
        CaseSpec {
            name: "reducer_input",
            update_path: "app_update_msg_key",
            width: 100,
            height: 30,
            language: Language::English,
            build: search_app,
            update: reducer_input,
        },
        CaseSpec {
            name: "retro160x50",
            update_path: "direct_fixture_state",
            width: 160,
            height: 50,
            language: Language::English,
            build: retro_app,
            update: mutate_for_interaction,
        },
        CaseSpec {
            name: "normal80x24",
            update_path: "direct_fixture_state",
            width: 80,
            height: 24,
            language: Language::English,
            build: player_app,
            update: mutate_for_interaction,
        },
        CaseSpec {
            name: "canvas_art100x30",
            update_path: "direct_fixture_state",
            width: 100,
            height: 30,
            language: Language::English,
            build: canvas_art_app,
            update: canvas_interaction,
        },
        CaseSpec {
            name: "canvas_art160x50",
            update_path: "direct_fixture_state",
            width: 160,
            height: 50,
            language: Language::English,
            build: canvas_art_app,
            update: canvas_interaction,
        },
        CaseSpec {
            name: "canvas_art_lyrics100x30",
            update_path: "direct_fixture_state",
            width: 100,
            height: 30,
            language: Language::English,
            build: canvas_art_lyrics_app,
            update: canvas_interaction,
        },
        CaseSpec {
            name: "canvas_art_lyrics160x50",
            update_path: "direct_fixture_state",
            width: 160,
            height: 50,
            language: Language::English,
            build: canvas_art_lyrics_app,
            update: canvas_interaction,
        },
        CaseSpec {
            name: "animation_half100x30",
            update_path: "app_update_msg_anim_tick",
            width: 100,
            height: 30,
            language: Language::English,
            build: animation_half_app,
            update: canvas_interaction,
        },
        CaseSpec {
            name: "animation_half_art_lyrics160x50",
            update_path: "app_update_msg_anim_tick",
            width: 160,
            height: 50,
            language: Language::English,
            build: animation_half_art_lyrics_app,
            update: canvas_interaction,
        },
        CaseSpec {
            name: "animation_heavy_half100x30",
            update_path: "app_update_msg_anim_tick",
            width: 100,
            height: 30,
            language: Language::English,
            build: animation_heavy_half_app,
            update: canvas_interaction,
        },
        CaseSpec {
            name: "animation_heavy_half_art_lyrics160x50",
            update_path: "app_update_msg_anim_tick",
            width: 160,
            height: 50,
            language: Language::English,
            build: animation_heavy_half_art_lyrics_app,
            update: canvas_interaction,
        },
        CaseSpec {
            name: "local_empty60x16",
            update_path: "direct_fixture_state",
            width: 60,
            height: 16,
            language: Language::English,
            build: local_empty_app,
            update: no_interaction,
        },
        CaseSpec {
            name: "local_scroll72x18",
            update_path: "direct_fixture_state",
            width: 72,
            height: 18,
            language: Language::English,
            build: local_scroll_app,
            update: local_scroll_interaction,
        },
        CaseSpec {
            name: "local_max120x30",
            update_path: "direct_fixture_state",
            width: 120,
            height: 30,
            language: Language::English,
            build: local_max_app,
            update: local_max_interaction,
        },
        CaseSpec {
            name: "local_filter_ko90x24",
            update_path: "direct_fixture_state",
            width: 90,
            height: 24,
            language: Language::Korean,
            build: local_filter_korean_app,
            update: local_filter_interaction,
        },
    ]
}

fn run_case(
    spec: CaseSpec,
    warmup: usize,
    batch_count: usize,
    draws_per_batch: usize,
) -> Result<CaseReport, String> {
    yututui::i18n::set_language(spec.language);
    let checkpoint_digest = checkpoint_digest(spec)?;
    let mut app = (spec.build)();
    let backend = TestBackend::new(spec.width, spec.height);
    let mut terminal = Terminal::new(backend).map_err(|e| format!("create backend: {e}"))?;
    for step in 0..warmup {
        (spec.update)(&mut app, step);
        terminal
            .draw(|frame| yututui::ui::render(frame, &app))
            .map_err(|e| format!("warm-up draw: {e}"))?;
    }

    let mut batches = Vec::with_capacity(batch_count);
    let mut case_latencies = Vec::with_capacity(batch_count.saturating_mul(draws_per_batch));
    for batch in 0..batch_count {
        let mut latencies = Vec::with_capacity(draws_per_batch);
        let before = AllocSnapshot::now();
        WINDOW_PEAK.store(before.live, Ordering::Relaxed);
        for draw in 0..draws_per_batch {
            let latency = measure_update_to_draw(|| {
                (spec.update)(&mut app, batch * draws_per_batch + draw);
                terminal
                    .draw(|frame| yututui::ui::render(frame, &app))
                    .map(|_| ())
                    .map_err(|e| format!("measured draw: {e}"))
            })?;
            latencies.push(latency);
        }
        let total_ns = latencies.iter().sum::<u128>();
        let after = AllocSnapshot::now();
        case_latencies.extend_from_slice(&latencies);
        latencies.sort_unstable();
        batches.push(BatchReport {
            batch,
            draws: draws_per_batch,
            total_ns,
            mean_draw_ns: total_ns as f64 / draws_per_batch as f64,
            p50_draw_ns: percentile(&latencies, 0.50),
            p95_draw_ns: percentile(&latencies, 0.95),
            max_draw_ns: *latencies.last().unwrap_or(&0),
            latency_histogram: latency_histogram(&latencies),
            allocations: after.allocs.saturating_sub(before.allocs),
            reallocations: after.reallocs.saturating_sub(before.reallocs),
            allocated_bytes: after.allocated.saturating_sub(before.allocated),
            deallocated_bytes: after.deallocated.saturating_sub(before.deallocated),
            retained_bytes_delta: signed_delta(after.live, before.live),
            peak_live_bytes_delta: WINDOW_PEAK
                .load(Ordering::Relaxed)
                .saturating_sub(before.live),
        });
    }

    let buffer_style_digest = digest(terminal.backend().buffer());
    let hit_map_digest = digest_hit_map(&app, spec.width, spec.height);
    case_latencies.sort_unstable();
    let total_draw_ns = case_latencies.iter().sum::<u128>();
    Ok(CaseReport {
        name: spec.name.to_string(),
        update_path: spec.update_path,
        width: spec.width,
        height: spec.height,
        warmup_draws: warmup,
        measured_draws: case_latencies.len(),
        total_draw_ns,
        mean_draw_ns: total_draw_ns as f64 / case_latencies.len().max(1) as f64,
        p50_draw_ns: percentile(&case_latencies, 0.50),
        p95_draw_ns: percentile(&case_latencies, 0.95),
        max_draw_ns: *case_latencies.last().unwrap_or(&0),
        latency_histogram: latency_histogram(&case_latencies),
        batches,
        buffer_style_digest,
        hit_map_digest,
        checkpoint_digest,
    })
}

fn checkpoint_digest(spec: CaseSpec) -> Result<String, String> {
    const CHECKPOINTS: [usize; 6] = [0, 1, 29, 30, 59, 119];
    let mut app = (spec.build)();
    let backend = TestBackend::new(spec.width, spec.height);
    let mut terminal =
        Terminal::new(backend).map_err(|e| format!("create checkpoint backend: {e}"))?;
    let mut hasher = DefaultHasher::new();
    for step in 0..=CHECKPOINTS[CHECKPOINTS.len() - 1] {
        if step > 0 {
            (spec.update)(&mut app, step - 1);
        }
        terminal
            .draw(|frame| yututui::ui::render(frame, &app))
            .map_err(|e| format!("checkpoint draw: {e}"))?;
        if CHECKPOINTS.contains(&step) {
            step.hash(&mut hasher);
            terminal.backend().buffer().hash(&mut hasher);
            digest_hit_map(&app, spec.width, spec.height).hash(&mut hasher);
        }
    }
    Ok(format!("{:016x}", hasher.finish()))
}

fn latency_histogram(sorted: &[u128]) -> Vec<LatencyBucket> {
    let mut histogram: Vec<LatencyBucket> = Vec::new();
    for &ns in sorted {
        if let Some(last) = histogram.last_mut()
            && last.ns == ns
        {
            last.count += 1;
            continue;
        }
        histogram.push(LatencyBucket { ns, count: 1 });
    }
    histogram
}

fn percentile(sorted: &[u128], quantile: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() - 1) as f64 * quantile).ceil() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn measure_update_to_draw<E>(action: impl FnOnce() -> Result<(), E>) -> Result<u128, E> {
    measure_update_to_draw_with_clock(action, Instant::now)
}

fn measure_update_to_draw_with_clock<E>(
    action: impl FnOnce() -> Result<(), E>,
    mut now: impl FnMut() -> Instant,
) -> Result<u128, E> {
    let started = now();
    action()?;
    Ok(now().saturating_duration_since(started).as_nanos())
}

fn signed_delta(after: u64, before: u64) -> i64 {
    let delta = i128::from(after) - i128::from(before);
    delta.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn digest(value: &impl Hash) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn digest_hit_map(app: &App, width: u16, height: u16) -> String {
    let mut hasher = DefaultHasher::new();
    app.hits.seekbar_rect().hash(&mut hasher);
    for y in 0..height {
        for x in 0..width {
            // MouseTarget deliberately has no Hash implementation; Debug is stable enough for
            // same-toolchain baseline/candidate equivalence and covers payload indices/actions.
            format!("{:?}", app.hits.target_at(x, y)).hash(&mut hasher);
        }
    }
    format!("{:016x}", hasher.finish())
}

fn songs(count: usize) -> Vec<Song> {
    (0..count)
        .map(|index| {
            Song::remote(
                format!("perf-{index:04}"),
                format!("Performance fixture track {index:04} 한글 제목"),
                format!("Fixture artist {:03}", index % 37),
                format!("{}:{:02}", 2 + index % 5, index % 60),
            )
        })
        .collect()
}

fn player_app() -> App {
    let mut app = App::new(100);
    app.queue.set(songs(20), 7);
    app.mode = Mode::Player;
    app.playback.duration = Some(245.0);
    app.playback.time_pos = Some(73.0);
    app.playback.volume = 67;
    app
}

fn canvas_art_app() -> App {
    let mut app = player_app();
    app.config.player_bar_position = Some(PlayerBarPosition::Bottom);
    app.config.album_art = Some(true);
    app.config.animations.master = true;
    app.config.animations.plasma = true;
    app.playback.paused = false;

    attach_art(&mut app);
    app
}

fn attach_art(app: &mut App) {
    let picker = Picker::halfblocks();
    let image = Arc::new(DynamicImage::new_rgba8(32, 32));
    let protocol = picker.new_resize_protocol_shared(image);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    app.art.dims = (32, 32);
    app.art.picker = Some(picker);
    *app.art.protocol.borrow_mut() = Some(ThreadProtocol::new(tx, Some(protocol)));
}

fn canvas_art_lyrics_app() -> App {
    let mut app = canvas_art_app();
    let video_id = app.queue.current().expect("fixture track").video_id.clone();
    app.lyrics.visible = true;
    app.lyrics.track = Some(TrackLyrics {
        video_id: video_id.into(),
        lines: vec![LyricLine {
            time: 0.0,
            text: "Deterministic performance fixture lyric".to_owned(),
        }]
        .into(),
    });
    app
}

fn attach_lyrics(app: &mut App) {
    let video_id = app.queue.current().expect("fixture track").video_id.clone();
    app.lyrics.visible = true;
    app.lyrics.track = Some(TrackLyrics {
        video_id: video_id.into(),
        lines: vec![
            LyricLine {
                time: 0.0,
                text: "Deterministic performance fixture lyric".to_owned(),
            },
            LyricLine {
                time: 60.0,
                text: "Second deterministic performance fixture lyric".to_owned(),
            },
        ]
        .into(),
    });
}

fn configure_animation_half(app: &mut App) {
    let a = &mut app.config.animations;
    a.master = true;
    a.fps = 30;
    a.pause_unfocused = true;
    a.track_intro = true;
    a.toast = true;
    a.volume_flash = true;
    a.seek_flash = true;
    a.selection = true;
    a.caret = true;
    a.activity = true;
    a.popup_fade = true;
    a.title = true;
    a.heart = true;
    a.seekbar = true;
    a.spinner = true;
    a.eq_bars = true;
    a.controls = true;
    a.border = true;
    a.time_glow = true;
    a.bounce = true;
    a.starfield = true;
    a.visualizer = true;
    a.rain = true;
    app.playback.paused = false;
}

fn configure_animation_heavy_half(app: &mut App) {
    let a = &mut app.config.animations;
    a.master = true;
    a.fps = 30;
    a.pause_unfocused = true;
    a.title = true;
    a.lyrics = true;
    a.seekbar = true;
    a.eq_bars = true;
    a.controls = true;
    a.border = true;
    a.rain = true;
    a.donut = true;
    a.visualizer = true;
    a.starfield = true;
    a.comets = true;
    a.snow = true;
    a.fireflies = true;
    a.cube = true;
    a.aquarium = true;
    a.waves = true;
    a.fireworks = true;
    a.life = true;
    a.pipes = true;
    a.plasma = true;
    app.playback.paused = false;
}

fn animation_half_app() -> App {
    let mut app = player_app();
    // The fixture mutates queue/volume directly. Let the ordinary reducer observe that
    // steady state while animations are still disabled so the first measured AnimTick does
    // not synthesize track-intro/volume-flash effects that real runtime setup already saw.
    let _ = app.update(Msg::Noop);
    configure_animation_half(&mut app);
    app
}

fn animation_half_art_lyrics_app() -> App {
    let mut app = animation_half_app();
    app.config.player_bar_position = Some(PlayerBarPosition::Bottom);
    app.config.album_art = Some(true);
    attach_art(&mut app);
    attach_lyrics(&mut app);
    app
}

fn animation_heavy_half_app() -> App {
    let mut app = player_app();
    let _ = app.update(Msg::Noop);
    configure_animation_heavy_half(&mut app);
    app
}

fn animation_heavy_half_art_lyrics_app() -> App {
    let mut app = animation_heavy_half_app();
    app.config.player_bar_position = Some(PlayerBarPosition::Bottom);
    app.config.album_art = Some(true);
    attach_art(&mut app);
    attach_lyrics(&mut app);
    app
}

fn search_app() -> App {
    let mut app = player_app();
    app.mode = Mode::Search;
    app.search.input = "performance fixture".to_string();
    app.search.results = songs(50);
    app.search.selected = 25;
    app.search.focus = SearchFocus::Results;
    app
}

fn library_app() -> App {
    let mut app = player_app();
    app.mode = Mode::Library;
    app.library.favorites = songs(999);
    app.library_ui.tab = LibraryTab::Favorites;
    app.library_ui.selected = 500;
    app.library_ui.anchor = 500;
    app
}

fn ai_app() -> App {
    let mut app = player_app();
    app.mode = Mode::Ai;
    app.ai.available = true;
    for index in 0..220 {
        app.ai.messages.push(AiMessage {
            role: if index % 2 == 0 {
                AiRole::User
            } else {
                AiRole::Ai
            },
            text: format!(
                "## Turn {index}\nThis is a **deterministic** long transcript line with `code`, \
                 Korean text 한글, and a list item:\n- recommendation {}\n- reason {}",
                index % 17,
                index % 11
            ),
        });
    }
    app.ai.suggestions = songs(12);
    app.ai.suggestions_selected = 5;
    app
}

fn retro_app() -> App {
    let mut app = player_app();
    app.config.retro_mode = true;
    app.queue.set(
        vec![Song::remote(
            "retro-fixture",
            "한글 제목 日本語 ✨ very long metadata",
            "가수 简体 ♥",
            "4:01",
        )],
        0,
    );
    app
}

fn local_tracks(count: usize) -> Vec<LocalTrack> {
    (0..count)
        .map(|index| {
            let mut track = LocalTrack::untagged(
                PathBuf::from(format!("/perf/local/track-{index:04}.flac")),
                4_000_000 + index as u64 * 1_337,
                1_700_000_000 + index as i64,
            );
            track.title = if index.is_multiple_of(LOCAL_FILTER_STRIDE) {
                format!("한글 바늘 트랙 {index:04}")
            } else {
                format!("Local performance track {index:04}")
            };
            track.artist = vec![format!("Fixture artist {:03}", index % 37)];
            track.album = Some(format!("Fixture album {:03}", index % 53));
            track.album_artist = Some(format!("Fixture artist {:03}", index % 37));
            track.genre = vec![if index.is_multiple_of(2) {
                "Electronic".to_owned()
            } else {
                "한국 인디".to_owned()
            }];
            track.year = Some(1990 + (index % 35) as i32);
            track.disc_no = Some(1 + (index % 2) as u32);
            track.track_no = Some(1 + (index % 20) as u32);
            track.duration_ms = Some(120_000 + (index % 240) as u64 * 1_000);
            track.bitrate = Some(900_000 + (index % 7) as u32 * 32_000);
            track.sample_rate = Some(if index.is_multiple_of(3) {
                96_000
            } else {
                48_000
            });
            if index.is_multiple_of(5) {
                track.embedded_art_key = Some(format!("fixture-art-{index:04}"));
            }
            track
        })
        .collect()
}

fn local_app(count: usize) -> App {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    app.local_dedicated_mode = true;
    app.local_mode.index.index = LocalIndex {
        tracks: local_tracks(count),
        updated_at: 1_700_000_000,
        ..LocalIndex::default()
    };
    app.local_mode.index.loaded = true;
    app.local_mode.ui.section = LocalSection::Tracks;
    app
}

fn local_empty_app() -> App {
    local_app(0)
}

fn local_scroll_app() -> App {
    let mut app = local_app(LOCAL_SCROLL_TRACKS);
    app.local_mode.ui.selected = 140;
    app.local_mode.ui.anchor = 140;
    app
}

fn local_max_app() -> App {
    let mut app = local_app(LOCAL_LARGE_TRACKS);
    app.local_mode.ui.selected = usize::MAX;
    app.local_mode.ui.anchor = usize::MAX;
    app
}

fn local_filter_korean_app() -> App {
    let mut app = local_app(LOCAL_SCROLL_TRACKS);
    app.local_mode.ui.filter_query = "한글 바늘".to_owned();
    app.local_mode.ui.filter_editing = true;
    app.local_mode.ui.selected = 5;
    app.local_mode.ui.anchor = 5;
    app
}

/// Deterministic real input path: key event -> `App::update(Msg::Key)` -> reducer -> render.
/// Home/End avoid wall-clock key-repeat acceleration while still exercising keymap lookup,
/// reducer dispatch, selection state, dirty tracking, overlay synchronization, and hit-map output.
fn reducer_input(app: &mut App, step: usize) {
    let code = if step.is_multiple_of(2) {
        KeyCode::End
    } else {
        KeyCode::Home
    };
    let _ = app.update(Msg::Key(KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }));
}

fn no_interaction(_app: &mut App, _step: usize) {}

fn local_scroll_interaction(app: &mut App, step: usize) {
    app.local_mode.ui.selected = 140 + step % (LOCAL_SCROLL_TRACKS - 140);
    app.local_mode.ui.anchor = app.local_mode.ui.selected;
}

fn local_max_interaction(app: &mut App, _step: usize) {
    app.local_mode.ui.selected = usize::MAX;
    app.local_mode.ui.anchor = usize::MAX;
}

fn local_filter_interaction(app: &mut App, step: usize) {
    let matching_rows = LOCAL_SCROLL_TRACKS.div_ceil(LOCAL_FILTER_STRIDE);
    app.local_mode.ui.selected = step % matching_rows;
    app.local_mode.ui.anchor = app.local_mode.ui.selected;
}

fn mutate_for_interaction(app: &mut App, step: usize) {
    match app.mode {
        Mode::Player => {
            app.playback.time_pos = Some((step % 244) as f64);
        }
        Mode::Search => {
            if !app.search.results.is_empty() {
                app.search.selected = step % app.search.results.len();
            }
        }
        Mode::Library => {
            if !app.library.favorites.is_empty() {
                app.library_ui.selected = step % app.library.favorites.len();
                app.library_ui.anchor = app.library_ui.selected;
            }
        }
        Mode::Ai => {
            if !app.ai.suggestions.is_empty() {
                app.ai.suggestions_selected = step % app.ai.suggestions.len();
            }
        }
        Mode::Settings => {}
    }
}

fn canvas_interaction(app: &mut App, step: usize) {
    app.update(Msg::AnimTick);
    app.playback.time_pos = Some((step % 244) as f64);
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    use super::{
        LOCAL_FILTER_STRIDE, LOCAL_LARGE_TRACKS, LOCAL_SCROLL_TRACKS, animation_half_app,
        animation_heavy_half_app, case_specs, checkpoint_digest, local_empty_app,
        local_filter_korean_app, local_max_app, local_scroll_app,
        measure_update_to_draw_with_clock, percentile, reducer_input, search_app,
    };
    use yututui::app::App;

    fn enabled_animation_effects(app: &App) -> Vec<&'static str> {
        let a = &app.config.animations;
        [
            ("title", a.title),
            ("heart", a.heart),
            ("seekbar", a.seekbar),
            ("spinner", a.spinner),
            ("eq_bars", a.eq_bars),
            ("controls", a.controls),
            ("border", a.border),
            ("track_intro", a.track_intro),
            ("lyrics", a.lyrics),
            ("toast", a.toast),
            ("volume_flash", a.volume_flash),
            ("like_burst", a.like_burst),
            ("seek_flash", a.seek_flash),
            ("selection", a.selection),
            ("stagger", a.stagger),
            ("caret", a.caret),
            ("tabs", a.tabs),
            ("popup_fade", a.popup_fade),
            ("activity", a.activity),
            ("about_fx", a.about_fx),
            ("time_glow", a.time_glow),
            ("progress_sparkle", a.progress_sparkle),
            ("border_chase", a.border_chase),
            ("pause_flash", a.pause_flash),
            ("error_shake", a.error_shake),
            ("rain", a.rain),
            ("donut", a.donut),
            ("visualizer", a.visualizer),
            ("starfield", a.starfield),
            ("bounce", a.bounce),
            ("comets", a.comets),
            ("snow", a.snow),
            ("fireflies", a.fireflies),
            ("cube", a.cube),
            ("aquarium", a.aquarium),
            ("waves", a.waves),
            ("fireworks", a.fireworks),
            ("life", a.life),
            ("pipes", a.pipes),
            ("plasma", a.plasma),
        ]
        .into_iter()
        .filter_map(|(name, enabled)| enabled.then_some(name))
        .collect()
    }

    #[test]
    fn percentile_uses_nearest_rank_upward() {
        assert_eq!(percentile(&[1, 2, 3, 4, 5], 0.50), 3);
        assert_eq!(percentile(&[1, 2, 3, 4, 5], 0.95), 5);
        assert_eq!(percentile(&[], 0.95), 0);
    }

    #[test]
    fn pooled_case_p95_is_not_the_mean_of_batch_p95s() {
        let mut first = vec![0; 95];
        first.extend([100; 5]);
        let second = vec![0; 100];
        let batch_mean = (percentile(&first, 0.95) + percentile(&second, 0.95)) / 2;
        let mut pooled = first;
        pooled.extend(second);
        pooled.sort_unstable();

        assert_eq!(batch_mean, 50);
        assert_eq!(percentile(&pooled, 0.95), 0);
    }

    #[test]
    fn update_to_draw_timer_starts_before_the_update() {
        let origin = Instant::now();
        let action_seen = Cell::new(false);
        let clock_calls = Cell::new(0usize);
        let measured = measure_update_to_draw_with_clock(
            || {
                action_seen.set(true);
                Ok::<(), ()>(())
            },
            || {
                let call = clock_calls.get();
                clock_calls.set(call + 1);
                match call {
                    0 => {
                        assert!(!action_seen.get());
                        origin
                    }
                    1 => {
                        assert!(action_seen.get());
                        origin + Duration::from_millis(7)
                    }
                    _ => panic!("timer sampled the clock more than twice"),
                }
            },
        )
        .expect("synthetic interaction succeeds");

        assert_eq!(measured, Duration::from_millis(7).as_nanos());
    }

    #[test]
    fn reducer_input_case_dispatches_real_key_messages_deterministically() {
        let mut app = search_app();
        assert_eq!(app.search.selected, 25);

        reducer_input(&mut app, 0);
        assert_eq!(app.search.selected, app.search.results.len() - 1);

        reducer_input(&mut app, 1);
        assert_eq!(app.search.selected, 0);
    }

    #[test]
    fn local_deck_fixtures_cover_empty_scroll_max_and_unicode_filter_states() {
        let empty = local_empty_app();
        assert_eq!(empty.local_rows_len(), 0);

        let scrolled = local_scroll_app();
        assert_eq!(scrolled.local_rows_len(), LOCAL_SCROLL_TRACKS);
        assert_eq!(scrolled.local_mode.ui.selected, 140);

        let max = local_max_app();
        assert_eq!(max.local_rows_len(), LOCAL_LARGE_TRACKS);
        assert_eq!(max.local_mode.ui.selected, usize::MAX);

        let filtered = local_filter_korean_app();
        assert_eq!(
            filtered.local_rows_len(),
            LOCAL_SCROLL_TRACKS.div_ceil(LOCAL_FILTER_STRIDE)
        );
        assert_eq!(filtered.local_mode.ui.filter_query, "한글 바늘");
        assert!(filtered.local_mode.ui.filter_editing);
    }

    #[test]
    fn interactive_player_performance_todo_cases_remain_in_the_harness() {
        let cases = case_specs();
        for (name, width, height) in [
            ("canvas_art100x30", 100, 30),
            ("canvas_art160x50", 160, 50),
            ("canvas_art_lyrics100x30", 100, 30),
            ("canvas_art_lyrics160x50", 160, 50),
        ] {
            let case = cases
                .iter()
                .find(|case| case.name == name)
                .unwrap_or_else(|| panic!("missing tracked performance TODO case {name}"));
            assert_eq!((case.width, case.height), (width, height));
        }
    }

    #[test]
    fn half_animation_cases_and_profiles_are_exact_and_checkpoint_repeatable() {
        let cases = case_specs();
        for (name, width, height) in [
            ("animation_half100x30", 100, 30),
            ("animation_half_art_lyrics160x50", 160, 50),
            ("animation_heavy_half100x30", 100, 30),
            ("animation_heavy_half_art_lyrics160x50", 160, 50),
        ] {
            let case = cases
                .iter()
                .find(|case| case.name == name)
                .unwrap_or_else(|| panic!("missing half-animation performance case {name}"));
            assert_eq!((case.width, case.height), (width, height));
            assert_eq!(case.update_path, "app_update_msg_anim_tick");
        }

        let balanced = animation_half_app();
        assert!(balanced.config.animations.master);
        assert_eq!(balanced.config.animations.fps, 30);
        assert!(balanced.config.animations.pause_unfocused);
        assert_eq!(
            enabled_animation_effects(&balanced),
            [
                "title",
                "heart",
                "seekbar",
                "spinner",
                "eq_bars",
                "controls",
                "border",
                "track_intro",
                "toast",
                "volume_flash",
                "seek_flash",
                "selection",
                "caret",
                "popup_fade",
                "activity",
                "time_glow",
                "rain",
                "visualizer",
                "starfield",
                "bounce",
            ]
        );

        let heavy = animation_heavy_half_app();
        assert!(heavy.config.animations.master);
        assert_eq!(heavy.config.animations.fps, 30);
        assert!(heavy.config.animations.pause_unfocused);
        assert_eq!(
            enabled_animation_effects(&heavy),
            [
                "title",
                "seekbar",
                "eq_bars",
                "controls",
                "border",
                "lyrics",
                "rain",
                "donut",
                "visualizer",
                "starfield",
                "comets",
                "snow",
                "fireflies",
                "cube",
                "aquarium",
                "waves",
                "fireworks",
                "life",
                "pipes",
                "plasma",
            ]
        );

        let case = *cases
            .iter()
            .find(|case| case.name == "animation_half100x30")
            .expect("balanced half-animation case");
        assert_eq!(
            checkpoint_digest(case).expect("first checkpoint digest"),
            checkpoint_digest(case).expect("second checkpoint digest")
        );
    }
}
