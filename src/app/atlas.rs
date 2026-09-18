//! Atlas globe: the Player-owned sub-surface of dedicated Radio mode. Keys, mouse, fetch
//! events and the kinetic tick all land here; the pure math lives in [`crate::atlas`].
//!
//! Invariants:
//! - Atlas is open only while `radio_dedicated_mode` holds and the UI tier is `Full`; every
//!   path that leaves Radio mode, goes home or enters Mini calls [`App::close_atlas`].
//! - Coast/autorotate request animation ticks only through [`App::atlas_motion_active`],
//!   which is false the moment motion stops, so the clock never idles on the globe.
//! - Fetch replies carry the generation they were requested under; anything older than
//!   `AtlasState::generation` is dropped, so closing and reopening never mixes catalogs.

use std::sync::OnceLock;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::api::radio_browser::RadioStation;
use crate::atlas::fetch::{AtlasCmd, AtlasErrorKind, AtlasEvent};
use crate::atlas::geometry::{Country, World, world};
use crate::atlas::mask::LandMask;
use crate::atlas::raster::{CellRect, Geometry, Renderer, nearest_marker, project_markers};
use crate::atlas::stations::{AtlasStation, Catalog, RecentTuned};
use crate::atlas::view::{Camera, Kinetic, VelocityTracker, degrees_per_dot};
use crate::atlas::{LatLon, Marker};
use crate::config::{AtlasPanel, AtlasRenderer};
use crate::keymap::{Action, Chord, KeyContext};
use crate::t;

use super::{App, Cmd, ContextCommand, Mode, PersistCmd, StatusKind, copy_to_clipboard};

/// Messages the atlas domain receives from the runtime.
#[derive(Debug)]
pub enum AtlasMsg {
    Event(AtlasEvent),
}

/// Mouse hit targets the Atlas view registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtlasTarget {
    Globe,
    PanelRow(usize),
    PanelTab(PanelTab),
    Search,
    Close,
    /// The "Atlas" button drawn in the radio set piece while Atlas is closed.
    Open,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelTab {
    #[default]
    World,
    Favorites,
    Recent,
}

impl PanelTab {
    pub const ALL: [PanelTab; 3] = [PanelTab::World, PanelTab::Favorites, PanelTab::Recent];

    pub fn label(self) -> &'static str {
        match self {
            PanelTab::World => t!("World", "세계", "世界"),
            PanelTab::Favorites => t!("Favorites", "즐겨찾기", "お気に入り"),
            PanelTab::Recent => t!("Recent", "최근", "最近"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AtlasFocus {
    #[default]
    Globe,
    Panel,
}

/// A left button held down on the globe. `moved` flips on the first drag event; a release
/// without movement is a click.
#[derive(Debug, Clone, Copy)]
pub struct PressSession {
    pub col: u16,
    pub row: u16,
    pub moved: bool,
    pub started: Instant,
    /// A press that caught a running coast only stops it; its release must not tune.
    pub caught_coast: bool,
}

/// Rows the panel currently lists: catalog indices (World / search) or library songs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelRows {
    Catalog(Vec<usize>),
    /// Favorite / recent uuids in library order; those present in the catalog map to markers.
    Library(Vec<Box<str>>),
}

impl Default for PanelRows {
    fn default() -> Self {
        PanelRows::Catalog(Vec::new())
    }
}

pub const PANEL_MIN_WIDTH: u16 = 34;
/// Content width from which the panel opens automatically (`AtlasPanel::Auto`).
pub const PANEL_AUTO_WIDTH: u16 = 96;
pub const GLOBE_MIN_WIDTH: u16 = 24;
pub const GLOBE_MIN_HEIGHT: u16 = 10;
const ROTATE_STEP_DEG: f32 = 4.0;
const ROTATE_BIG_STEP_DEG: f32 = 15.0;
const ZOOM_STEP: f32 = 1.25;
const WHEEL_ZOOM: f32 = 1.284_025_4; // e^0.25
const WORLD_PAGE: u32 = 500;
const MORE_PAGE_SLACK: u32 = 3;
const SEARCH_MAX: usize = 150;
const HIT_RADIUS_CELLS: f32 = 1.5;
const AUTOROTATE_DEG_PER_SEC: f32 = 3.0;
/// A tick gap longer than this means the clock was parked (unfocused, hidden), not slow.
const STALL_ABORT_SECS: f32 = 1.0;
/// Below this many stations the panel shows a hint instead of an empty list.
const RESOLVE_MAX: usize = 100;

#[derive(Default)]
pub struct AtlasState {
    pub open: bool,
    pub camera: Camera,
    pub kinetic: Kinetic,
    pub velocity: VelocityTracker,
    /// Seconds since the drag began, fed to the velocity tracker (deterministic in tests).
    pub drag_clock: f32,
    pub catalog: Catalog,
    pub generation: u64,
    /// The station last tuned from Atlas (marker `◉`).
    pub selected: Option<usize>,
    /// Keyboard cursor / landing highlight (marker `○`).
    pub highlight: Option<usize>,
    pub hover: Option<usize>,
    pub active_country: Option<[u8; 2]>,
    pub active_country_name: String,
    pub active_mask: Option<LandMask>,
    pub press: Option<PressSession>,
    /// Last clicked / hit cell; `n`/`p` cycle by distance from here (globe centre otherwise).
    pub crosshair: Option<(u16, u16)>,
    pub focus: AtlasFocus,
    /// User override of the panel visibility for this session (`None` = config).
    pub panel_override: Option<bool>,
    pub panel_tab: PanelTab,
    pub panel_rows: PanelRows,
    pub panel_selected: usize,
    pub search_query: String,
    pub search_editing: bool,
    pub recent: RecentTuned,
    pub loading: bool,
    pub error: Option<String>,
    pub pages_fetched: u32,
    pub grid: bool,
    pub autorotate: bool,
    pub last_tick: Option<Instant>,
    /// Centre on the playing station once it starts (config `follow_playing`).
    pub follow_uuid: Option<Box<str>>,
    /// Context line under the globe: what the last hit/landing found.
    pub context: String,
}

impl AtlasState {
    pub fn motion_active(&self) -> bool {
        self.open && (self.kinetic.active() || self.autorotate)
    }
}

static LAND: OnceLock<LandMask> = OnceLock::new();

/// The world land mask, built once per process on first Atlas open (a few ms).
pub fn land_mask() -> &'static LandMask {
    LAND.get_or_init(|| LandMask::build(world()))
}

fn country_code_str(code: &[u8; 2]) -> &str {
    std::str::from_utf8(code).unwrap_or("--")
}

/// A radio `Song` carries `rad:<uuid>` as its id; the catalog is keyed by the bare uuid.
fn song_uuid(video_id: &str) -> &str {
    video_id
        .strip_prefix(crate::search_source::SearchSource::RadioBrowser.id_prefix())
        .and_then(|rest| rest.strip_prefix(':'))
        .unwrap_or(video_id)
}

fn song_video_id(uuid: &str) -> String {
    format!(
        "{}:{uuid}",
        crate::search_source::SearchSource::RadioBrowser.id_prefix()
    )
}

pub(in crate::app) fn atlas_context_command_label(command: ContextCommand) -> String {
    match command {
        ContextCommand::CopyStreamUrl => t!(
            "Copy stream url",
            "스트림 URL 복사",
            "ストリームURLをコピー"
        )
        .to_owned(),
        ContextCommand::BrowseCountry => {
            t!("Open country list", "국가 목록 열기", "国リストを開く").to_owned()
        }
        _ => String::new(),
    }
}

impl App {
    /// Whether the Atlas surface owns the Player content area right now.
    pub fn atlas_active(&self) -> bool {
        self.radio_dedicated_mode
            && self.radio_mode.atlas.open
            && self.mode == Mode::Player
            && self.bridges.ui_tier.get() != crate::ui::layout::UiTier::Mini
    }

    /// Coast or autorotate is running: the animation clock must tick.
    pub fn atlas_motion_active(&self) -> bool {
        self.atlas_active() && self.radio_mode.atlas.motion_active() && self.animations().master
    }

    pub fn atlas_renderer(&self) -> Renderer {
        match self.config.atlas.renderer {
            AtlasRenderer::Braille => Renderer::Braille,
            AtlasRenderer::Ascii => Renderer::Ascii,
            AtlasRenderer::Auto if self.retro_mode() => Renderer::Ascii,
            AtlasRenderer::Auto => Renderer::Braille,
        }
    }

    /// Whether the station panel shows at `content_width` columns.
    pub fn atlas_panel_visible(&self, content_width: u16) -> bool {
        let atlas = &self.radio_mode.atlas;
        let wanted = atlas
            .panel_override
            .unwrap_or(match self.config.atlas.panel {
                AtlasPanel::Shown => true,
                AtlasPanel::Hidden => false,
                AtlasPanel::Auto => content_width >= PANEL_AUTO_WIDTH,
            });
        wanted && content_width >= GLOBE_MIN_WIDTH + PANEL_MIN_WIDTH
    }

    pub(in crate::app) fn toggle_atlas(&mut self) -> Vec<Cmd> {
        if self.radio_mode.atlas.open {
            self.close_atlas();
            self.dirty = true;
            return Vec::new();
        }
        if !self.radio_dedicated_mode {
            self.status.kind = StatusKind::Info;
            self.status.text = t!(
                "Atlas needs Radio mode (Alt+Shift+R)",
                "아틀라스는 라디오 모드에서 열 수 있어요 (Alt+Shift+R)",
                "アトラスはラジオモードで開けます (Alt+Shift+R)"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        }
        if self.bridges.ui_tier.get() == crate::ui::layout::UiTier::Mini {
            self.status.kind = StatusKind::Info;
            self.status.text = t!(
                "Atlas needs a larger terminal",
                "아틀라스는 더 큰 터미널이 필요해요",
                "アトラスにはもっと大きな端末が必要です"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        }
        self.open_atlas()
    }

    fn open_atlas(&mut self) -> Vec<Cmd> {
        let _ = land_mask();
        let atlas = &mut self.radio_mode.atlas;
        atlas.open = true;
        atlas.generation += 1;
        atlas.kinetic = Kinetic::default();
        atlas.press = None;
        atlas.hover = None;
        atlas.focus = AtlasFocus::Globe;
        atlas.search_editing = false;
        atlas.grid = self.config.atlas.grid;
        atlas.autorotate = self.config.atlas.autorotate;
        atlas.last_tick = None;
        atlas.error = None;
        atlas.context.clear();
        let generation = atlas.generation;
        let mut cmds = Vec::new();
        if atlas.catalog.is_empty() {
            atlas.loading = true;
            atlas.pages_fetched = 0;
            cmds.push(Cmd::Atlas(AtlasCmd::World {
                generation,
                limit: WORLD_PAGE,
            }));
        }
        self.atlas_refresh_panel_rows();
        self.atlas_follow_playing_now();
        self.dirty = true;
        cmds
    }

    /// Leave the globe: clears every transient so a later reopen starts from a known state.
    /// The catalog is kept for the session (cheap, and the reference keeps it too).
    pub(in crate::app) fn close_atlas(&mut self) {
        let atlas = &mut self.radio_mode.atlas;
        if !atlas.open {
            return;
        }
        atlas.open = false;
        atlas.generation += 1;
        atlas.kinetic = Kinetic::default();
        atlas.press = None;
        atlas.hover = None;
        atlas.search_editing = false;
        atlas.loading = false;
        atlas.follow_uuid = None;
        self.dirty = true;
    }

    fn atlas_toast(&mut self, text: String) {
        self.status.kind = StatusKind::Info;
        self.status.text = text;
        self.dirty = true;
    }

    fn atlas_globe_rect(&self) -> Option<CellRect> {
        self.bridges.atlas_globe.get()
    }

    fn atlas_geometry(&self) -> Option<Geometry> {
        let rect = self.atlas_globe_rect()?;
        Some(Geometry::new(
            rect,
            self.atlas_renderer(),
            self.radio_mode.atlas.camera.scale,
        ))
    }

    pub fn atlas_hover_chrome(&self) -> bool {
        self.atlas_active() && self.interaction.pointer_motion && self.animations().master
    }

    pub fn atlas_hover_tip(&self) -> Option<String> {
        if !self.atlas_hover_chrome() {
            return None;
        }
        let idx = self.radio_mode.atlas.hover?;
        let st = self.radio_mode.atlas.catalog.get(idx)?;
        let country = if !st.country.is_empty() {
            st.country.as_ref()
        } else {
            country_code_str(&st.country_code)
        };
        Some(format!("{} · {}", st.name, country))
    }

    pub fn atlas_markers(&self) -> Vec<Marker> {
        let atlas = &self.radio_mode.atlas;
        let highlight = if self.atlas_hover_chrome() {
            atlas.hover.or(atlas.highlight)
        } else {
            atlas.highlight
        };
        let library = &self.library;
        atlas
            .catalog
            .markers(atlas.selected, highlight, &|uuid: &str| {
                library.is_radio_favorite(&song_video_id(uuid))
            })
    }

    fn atlas_marker_cells(&self) -> Vec<crate::atlas::raster::MarkerCell> {
        let Some(geom) = self.atlas_geometry() else {
            return Vec::new();
        };
        project_markers(&geom, &self.radio_mode.atlas.camera, &self.atlas_markers())
    }

    fn atlas_crosshair_cell(&self) -> Option<(u16, u16)> {
        if let Some(cell) = self.radio_mode.atlas.crosshair {
            return Some(cell);
        }
        let rect = self.atlas_globe_rect()?;
        Some((rect.x + rect.width / 2, rect.y + rect.height / 2))
    }

    /// The coordinate under a cell, if the cell is on the globe.
    fn atlas_cell_latlon(&self, col: u16, row: u16) -> Option<LatLon> {
        let geom = self.atlas_geometry()?;
        let (nx, ny) = geom.cell_to_disc(col, row);
        self.radio_mode.atlas.camera.unproject(nx, ny)
    }

    fn atlas_country_at_cell(&self, col: u16, row: u16) -> Option<&'static Country> {
        let at = self.atlas_cell_latlon(col, row)?;
        world().country_at(at)
    }

    fn atlas_station_at_cell(&self, col: u16, row: u16) -> Option<usize> {
        let cells = self.atlas_marker_cells();
        nearest_marker(&cells, col, row, HIT_RADIUS_CELLS).map(|c| c.index)
    }

    fn atlas_set_context(&mut self, text: String) {
        self.radio_mode.atlas.context = text;
        self.dirty = true;
    }

    fn atlas_station_context(&self, idx: usize) -> String {
        let Some(st) = self.radio_mode.atlas.catalog.get(idx) else {
            return String::new();
        };
        let mut text = format!("{} · {}", st.name, st.meta_line());
        if st.estimated {
            text.push_str(" · ");
            text.push_str(t!("approximate", "대략적 위치", "おおよその位置"));
        }
        text
    }

    pub(in crate::app) fn on_key_atlas(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let chord = Chord::from(k);
        if self.radio_mode.atlas.search_editing {
            return self.on_key_atlas_search(k, chord);
        }
        if let Some(action) = self.keymap.context_action(KeyContext::Atlas, chord) {
            return self.on_atlas_action(action);
        }
        // Fixed aliases (not remappable): vi rotation, `=` zoom, Shift+arrows, Esc ladder.
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        match k.code {
            KeyCode::Esc => return self.atlas_escape(),
            KeyCode::Char('h') if k.modifiers.is_empty() => {
                return self.on_atlas_action(Action::AtlasRotateLeft);
            }
            KeyCode::Char('l') if k.modifiers.is_empty() => {
                return self.on_atlas_action(Action::AtlasRotateRight);
            }
            KeyCode::Char('k') if k.modifiers.is_empty() => {
                return self.on_atlas_action(Action::AtlasRotateUp);
            }
            KeyCode::Char('j') if k.modifiers.is_empty() => {
                return self.on_atlas_action(Action::AtlasRotateDown);
            }
            KeyCode::Char('=') if k.modifiers.is_empty() => {
                return self.on_atlas_action(Action::AtlasZoomIn);
            }
            KeyCode::Left if shift => return self.atlas_rotate(0.0, -ROTATE_BIG_STEP_DEG),
            KeyCode::Right if shift => return self.atlas_rotate(0.0, ROTATE_BIG_STEP_DEG),
            KeyCode::Up if shift => return self.atlas_rotate(ROTATE_BIG_STEP_DEG, 0.0),
            KeyCode::Down if shift => return self.atlas_rotate(-ROTATE_BIG_STEP_DEG, 0.0),
            _ => {}
        }
        // Transport pass-through: the keys a listener expects to keep working over the globe.
        match self.keymap.context_action(KeyContext::Player, chord) {
            Some(
                action @ (Action::TogglePause
                | Action::ToggleMute
                | Action::NextTrack
                | Action::PrevTrack
                | Action::SpeedUp
                | Action::SpeedDown
                | Action::IdentifyNowPlaying
                | Action::ToggleRadioMode
                | Action::ToggleRecordings
                | Action::ToggleAtlas
                | Action::OpenSettings
                | Action::OpenSearch
                | Action::SleepTimer),
            ) => self.on_player_action(action),
            _ => Vec::new(),
        }
    }

    fn on_key_atlas_search(&mut self, k: KeyEvent, chord: Chord) -> Vec<Cmd> {
        match k.code {
            KeyCode::Esc => {
                self.radio_mode.atlas.search_editing = false;
                if !self.radio_mode.atlas.search_query.is_empty() {
                    self.radio_mode.atlas.search_query.clear();
                    self.atlas_refresh_panel_rows();
                }
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Enter => {
                self.radio_mode.atlas.search_editing = false;
                self.radio_mode.atlas.focus = AtlasFocus::Panel;
                let query = self.radio_mode.atlas.search_query.trim().to_owned();
                self.dirty = true;
                if query.is_empty() {
                    return Vec::new();
                }
                vec![Cmd::Atlas(AtlasCmd::Search {
                    generation: self.radio_mode.atlas.generation,
                    query,
                })]
            }
            KeyCode::Backspace => {
                if self.keymap.text_edit_action(chord) == Some(Action::DeleteWord) {
                    let q = &mut self.radio_mode.atlas.search_query;
                    let trimmed = q.trim_end();
                    let cut = trimmed.rfind(' ').map_or(0, |i| i + 1);
                    q.truncate(cut);
                } else {
                    self.radio_mode.atlas.search_query.pop();
                }
                self.atlas_refresh_panel_rows();
                Vec::new()
            }
            KeyCode::Char(c)
                if !k
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if self.radio_mode.atlas.search_query.chars().count() < 80 {
                    self.radio_mode.atlas.search_query.push(c);
                    self.atlas_refresh_panel_rows();
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn atlas_escape(&mut self) -> Vec<Cmd> {
        let atlas = &mut self.radio_mode.atlas;
        if !atlas.search_query.is_empty() {
            atlas.search_query.clear();
            self.atlas_refresh_panel_rows();
            return Vec::new();
        }
        if atlas.highlight.is_some() {
            atlas.highlight = None;
            atlas.context.clear();
            self.dirty = true;
            return Vec::new();
        }
        if atlas.active_country.is_some() {
            self.atlas_clear_country();
            return Vec::new();
        }
        self.close_atlas();
        Vec::new()
    }

    /// Back to the world view: the country tint, its panel filter and the mask go away.
    fn atlas_clear_country(&mut self) {
        let atlas = &mut self.radio_mode.atlas;
        atlas.active_country = None;
        atlas.active_country_name.clear();
        atlas.active_mask = None;
        atlas.context.clear();
        self.atlas_refresh_panel_rows();
    }

    pub(in crate::app) fn on_atlas_action(&mut self, action: Action) -> Vec<Cmd> {
        match action {
            Action::AtlasRotateLeft => self.atlas_rotate(0.0, -ROTATE_STEP_DEG),
            Action::AtlasRotateRight => self.atlas_rotate(0.0, ROTATE_STEP_DEG),
            Action::AtlasRotateUp => self.atlas_rotate(ROTATE_STEP_DEG, 0.0),
            Action::AtlasRotateDown => self.atlas_rotate(-ROTATE_STEP_DEG, 0.0),
            Action::AtlasZoomIn => self.atlas_zoom(ZOOM_STEP),
            Action::AtlasZoomOut => self.atlas_zoom(1.0 / ZOOM_STEP),
            Action::AtlasResetView => {
                self.atlas_stop_motion();
                self.atlas_clear_country();
                let atlas = &mut self.radio_mode.atlas;
                atlas.camera = Camera::new();
                let playing = self.atlas_playing_index();
                if let Some(idx) = playing
                    && let Some(st) = self.radio_mode.atlas.catalog.get(idx)
                {
                    self.radio_mode.atlas.camera.focus(st.pos);
                }
                self.dirty = true;
                Vec::new()
            }
            Action::AtlasNextSignal => self.atlas_cycle_signal(true),
            Action::AtlasPrevSignal => self.atlas_cycle_signal(false),
            Action::AtlasActivate => self.atlas_activate(),
            Action::AtlasBrowseCountry => self.atlas_browse_country_at_cursor(),
            Action::AtlasRandom => self.atlas_random(),
            Action::AtlasFavorite => self.atlas_favorite(),
            Action::AtlasGoToPlaying => {
                match self.atlas_playing_index() {
                    Some(idx) => {
                        self.atlas_stop_motion();
                        if let Some(st) = self.radio_mode.atlas.catalog.get(idx) {
                            self.radio_mode.atlas.camera.focus(st.pos);
                        }
                        self.radio_mode.atlas.highlight = Some(idx);
                        let text = self.atlas_station_context(idx);
                        self.atlas_set_context(text);
                    }
                    None => self.atlas_toast(
                        t!(
                            "Nothing is playing from Atlas",
                            "아틀라스에서 재생 중인 방송국이 없어요",
                            "アトラスから再生中の局はありません"
                        )
                        .to_owned(),
                    ),
                }
                Vec::new()
            }
            Action::AtlasSearch => {
                self.radio_mode.atlas.search_editing = true;
                self.radio_mode.atlas.focus = AtlasFocus::Panel;
                self.radio_mode.atlas.panel_override = Some(true);
                self.dirty = true;
                Vec::new()
            }
            Action::AtlasFocusNext | Action::AtlasFocusPrev => {
                let atlas = &mut self.radio_mode.atlas;
                atlas.focus = match atlas.focus {
                    AtlasFocus::Globe => AtlasFocus::Panel,
                    AtlasFocus::Panel => AtlasFocus::Globe,
                };
                if atlas.focus == AtlasFocus::Panel {
                    atlas.panel_override = Some(true);
                } else if self.config.atlas.panel == AtlasPanel::Hidden {
                    atlas.panel_override = None;
                }
                self.dirty = true;
                Vec::new()
            }
            Action::AtlasToggleGrid => {
                self.radio_mode.atlas.grid = !self.radio_mode.atlas.grid;
                self.dirty = true;
                Vec::new()
            }
            Action::AtlasToggleAutorotate => {
                let atlas = &mut self.radio_mode.atlas;
                atlas.autorotate = !atlas.autorotate;
                atlas.last_tick = None;
                if atlas.autorotate && !self.animations().master {
                    self.atlas_toast(
                        t!(
                            "Autorotate needs animations on (A)",
                            "자동 회전은 애니메이션이 켜져야 해요 (A)",
                            "自動回転にはアニメーションが必要です (A)"
                        )
                        .to_owned(),
                    );
                }
                self.dirty = true;
                Vec::new()
            }
            Action::AtlasClose => {
                self.close_atlas();
                Vec::new()
            }
            Action::VolUp | Action::VolDown => self.on_player_action(action),
            _ => Vec::new(),
        }
    }

    fn atlas_rotate(&mut self, dlat: f32, dlon: f32) -> Vec<Cmd> {
        self.atlas_stop_motion();
        let atlas = &mut self.radio_mode.atlas;
        if atlas.focus == AtlasFocus::Panel && dlat != 0.0 {
            return self.atlas_panel_move(dlat < 0.0);
        }
        // With the panel focused, left/right walk its tabs so favorites/recent stay reachable
        // without a mouse.
        if atlas.focus == AtlasFocus::Panel && dlon != 0.0 {
            let tabs = PanelTab::ALL;
            let pos = tabs.iter().position(|t| *t == atlas.panel_tab).unwrap_or(0);
            let next = if dlon > 0.0 {
                (pos + 1) % tabs.len()
            } else {
                (pos + tabs.len() - 1) % tabs.len()
            };
            return self.atlas_mouse_target(AtlasTarget::PanelTab(tabs[next]), 0, 0);
        }
        atlas.camera.rotate_by(dlat, dlon);
        self.dirty = true;
        Vec::new()
    }

    fn atlas_zoom(&mut self, factor: f32) -> Vec<Cmd> {
        self.atlas_stop_motion();
        self.radio_mode.atlas.camera.zoom_by(factor);
        self.dirty = true;
        Vec::new()
    }

    fn atlas_stop_motion(&mut self) {
        let atlas = &mut self.radio_mode.atlas;
        atlas.kinetic = Kinetic::default();
        atlas.last_tick = None;
    }

    fn atlas_panel_move(&mut self, down: bool) -> Vec<Cmd> {
        let atlas = &mut self.radio_mode.atlas;
        let len = match &atlas.panel_rows {
            PanelRows::Catalog(v) => v.len(),
            PanelRows::Library(v) => v.len(),
        };
        if len == 0 {
            return Vec::new();
        }
        atlas.panel_selected = if down {
            (atlas.panel_selected + 1).min(len - 1)
        } else {
            atlas.panel_selected.saturating_sub(1)
        };
        if let Some(idx) = self.atlas_panel_catalog_index(self.radio_mode.atlas.panel_selected) {
            self.radio_mode.atlas.highlight = Some(idx);
            let text = self.atlas_station_context(idx);
            self.atlas_set_context(text);
        }
        self.dirty = true;
        Vec::new()
    }

    /// Cycle the highlight through visible markers ordered by distance from the crosshair.
    fn atlas_cycle_signal(&mut self, forward: bool) -> Vec<Cmd> {
        let cells = self.atlas_marker_cells();
        if cells.is_empty() {
            self.atlas_toast(
                t!(
                    "No signals in view",
                    "화면에 방송국이 없어요",
                    "表示範囲に局がありません"
                )
                .to_owned(),
            );
            return Vec::new();
        }
        let (col, row) = self.atlas_crosshair_cell().unwrap_or((0, 0));
        let order = crate::atlas::raster::markers_by_distance(&cells, col, row);
        let current = self.radio_mode.atlas.highlight;
        let pos = current.and_then(|h| order.iter().position(|&i| cells[i].index == h));
        let next = match (pos, forward) {
            (None, _) => 0,
            (Some(p), true) => (p + 1) % order.len(),
            (Some(p), false) => (p + order.len() - 1) % order.len(),
        };
        let idx = cells[order[next]].index;
        self.radio_mode.atlas.highlight = Some(idx);
        let text = self.atlas_station_context(idx);
        self.atlas_set_context(text);
        Vec::new()
    }

    fn atlas_panel_catalog_index(&self, row: usize) -> Option<usize> {
        let atlas = &self.radio_mode.atlas;
        match &atlas.panel_rows {
            PanelRows::Catalog(v) => v.get(row).copied(),
            PanelRows::Library(v) => v.get(row).and_then(|uuid| atlas.catalog.index_of(uuid)),
        }
    }

    fn atlas_activate(&mut self) -> Vec<Cmd> {
        let atlas = &self.radio_mode.atlas;
        if atlas.focus == AtlasFocus::Panel {
            if let Some(idx) = self.atlas_panel_catalog_index(atlas.panel_selected) {
                return self.atlas_tune(idx);
            }
            if let PanelRows::Library(v) = &atlas.panel_rows
                && let Some(uuid) = v.get(atlas.panel_selected)
                && let Some(song) = self
                    .library
                    .radio_favorites
                    .iter()
                    .chain(self.library.radios.iter())
                    .find(|s| song_uuid(&s.video_id) == &**uuid)
                    .cloned()
            {
                self.atlas_toast(format!("{} {}", t!("Tuning", "재생", "再生"), song.title));
                return self.play_now_many(vec![song]);
            }
            return Vec::new();
        }
        match atlas.highlight {
            Some(idx) => self.atlas_tune(idx),
            None => self.atlas_cycle_signal(true),
        }
    }

    fn atlas_browse_country_at_cursor(&mut self) -> Vec<Cmd> {
        let country = match self.radio_mode.atlas.highlight {
            Some(idx) => self
                .radio_mode
                .atlas
                .catalog
                .get(idx)
                .and_then(|st| world().by_code(country_code_str(&st.country_code))),
            None => self
                .atlas_crosshair_cell()
                .and_then(|(c, r)| self.atlas_country_at_cell(c, r)),
        };
        match country {
            Some(country) => self.atlas_browse_country(country),
            None => {
                self.atlas_toast(
                    t!(
                        "Point at a country first",
                        "먼저 국가를 가리켜 주세요",
                        "先に国を指してください"
                    )
                    .to_owned(),
                );
                Vec::new()
            }
        }
    }

    fn atlas_browse_country(&mut self, country: &'static Country) -> Vec<Cmd> {
        self.atlas_stop_motion();
        let atlas = &mut self.radio_mode.atlas;
        atlas.active_country = Some(country.code);
        atlas.active_country_name = country.name.to_string();
        atlas.active_mask = Some(LandMask::build_country(country));
        atlas.camera.focus(country.centroid);
        atlas.panel_tab = PanelTab::World;
        atlas.panel_override = Some(true);
        atlas.loading = true;
        let generation = atlas.generation;
        let code = country_code_str(&country.code).to_owned();
        self.atlas_refresh_panel_rows();
        self.atlas_set_context(format!(
            "{} · {}",
            country.name,
            t!(
                "loading stations…",
                "방송국 불러오는 중…",
                "局を読み込み中…"
            )
        ));
        vec![Cmd::Atlas(AtlasCmd::Country { generation, code })]
    }

    fn atlas_random(&mut self) -> Vec<Cmd> {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
            ^ (self.radio_mode.atlas.generation << 20)
            ^ self.anim.anim_frame;
        let atlas = &mut self.radio_mode.atlas;
        let pick = atlas.catalog.random_avoiding(atlas.recent.as_slice(), seed);
        match pick {
            Some(idx) => {
                self.atlas_stop_motion();
                if let Some(st) = self.radio_mode.atlas.catalog.get(idx) {
                    self.radio_mode.atlas.camera.focus(st.pos);
                }
                self.atlas_tune(idx)
            }
            None => {
                self.atlas_toast(
                    t!(
                        "No stations loaded yet",
                        "아직 불러온 방송국이 없어요",
                        "まだ局が読み込まれていません"
                    )
                    .to_owned(),
                );
                Vec::new()
            }
        }
    }

    fn atlas_favorite(&mut self) -> Vec<Cmd> {
        let atlas = &self.radio_mode.atlas;
        let idx = match atlas.focus {
            AtlasFocus::Panel => self.atlas_panel_catalog_index(atlas.panel_selected),
            AtlasFocus::Globe => atlas.highlight.or(atlas.selected),
        };
        match idx {
            Some(idx) => self.atlas_favorite_at(idx),
            None => Vec::new(),
        }
    }

    fn atlas_favorite_at(&mut self, idx: usize) -> Vec<Cmd> {
        let Some(song) = self
            .radio_mode
            .atlas
            .catalog
            .get(idx)
            .map(AtlasStation::to_song)
        else {
            return Vec::new();
        };
        let now_favorite = std::sync::Arc::make_mut(&mut self.library).toggle_radio_favorite(&song);
        self.atlas_toast(if now_favorite {
            format!("♥ {}", song.title)
        } else {
            format!(
                "{} {}",
                t!("Unfavorited", "즐겨찾기 해제", "お気に入り解除"),
                song.title
            )
        });
        self.atlas_refresh_panel_rows();
        vec![Cmd::Persist(PersistCmd::Library)]
    }

    fn atlas_playing_index(&self) -> Option<usize> {
        let current = self.queue.current()?;
        if !current.is_radio_station() {
            return None;
        }
        self.radio_mode
            .atlas
            .catalog
            .index_of(song_uuid(&current.video_id))
    }

    /// Tune a catalog station through the ordinary radio playback path.
    pub(in crate::app) fn atlas_tune(&mut self, idx: usize) -> Vec<Cmd> {
        let Some(st) = self.radio_mode.atlas.catalog.get(idx).cloned() else {
            return Vec::new();
        };
        let already = self
            .queue
            .current()
            .is_some_and(|c| song_uuid(&c.video_id) == &*st.uuid);
        let atlas = &mut self.radio_mode.atlas;
        atlas.selected = Some(idx);
        atlas.highlight = None;
        atlas.recent.push(&st.uuid);
        if already {
            self.dirty = true;
            return Vec::new();
        }
        if self.config.atlas.follow_playing {
            atlas.follow_uuid = Some(st.uuid.clone());
        }
        let song = st.to_song();
        self.atlas_toast(format!("{} {}", t!("Tuning", "재생", "再生"), st.name));
        let mut cmds = self.play_now_many(vec![song]);
        cmds.push(Cmd::Atlas(AtlasCmd::Click {
            uuid: st.uuid.to_string(),
        }));
        cmds
    }

    /// Centre on the playing station once, when `follow_playing` armed it. Called after
    /// player admission via [`App::atlas_follow_playing_now`] and on open.
    pub(in crate::app) fn atlas_follow_playing_now(&mut self) {
        if !self.radio_mode.atlas.open || self.radio_mode.atlas.press.is_some() {
            return;
        }
        let Some(current) = self.queue.current() else {
            return;
        };
        let wanted = self.radio_mode.atlas.follow_uuid.as_deref();
        if wanted != Some(song_uuid(&current.video_id)) {
            return;
        }
        if let Some(idx) = self
            .radio_mode
            .atlas
            .catalog
            .index_of(song_uuid(&current.video_id))
            && let Some(st) = self.radio_mode.atlas.catalog.get(idx)
        {
            let pos = st.pos;
            self.atlas_stop_motion();
            self.radio_mode.atlas.camera.focus(pos);
            self.radio_mode.atlas.selected = Some(idx);
        }
        self.radio_mode.atlas.follow_uuid = None;
        self.dirty = true;
    }

    pub(in crate::app) fn atlas_mouse_target(
        &mut self,
        target: AtlasTarget,
        col: u16,
        row: u16,
    ) -> Vec<Cmd> {
        match target {
            AtlasTarget::Open => self.toggle_atlas(),
            AtlasTarget::Close => {
                self.close_atlas();
                Vec::new()
            }
            AtlasTarget::Globe => {
                let caught_coast = self.radio_mode.atlas.kinetic.active();
                self.atlas_stop_motion();
                let atlas = &mut self.radio_mode.atlas;
                atlas.focus = AtlasFocus::Globe;
                atlas.search_editing = false;
                atlas.velocity = VelocityTracker::default();
                atlas.drag_clock = 0.0;
                atlas.hover = None;
                atlas.press = Some(PressSession {
                    col,
                    row,
                    moved: false,
                    started: Instant::now(),
                    caught_coast,
                });
                self.dirty = true;
                Vec::new()
            }
            AtlasTarget::PanelRow(i) => {
                let atlas = &mut self.radio_mode.atlas;
                atlas.focus = AtlasFocus::Panel;
                atlas.search_editing = false;
                atlas.panel_selected = i;
                if let Some(idx) = self.atlas_panel_catalog_index(i) {
                    return self.atlas_tune(idx);
                }
                self.atlas_activate()
            }
            AtlasTarget::PanelTab(tab) => {
                let atlas = &mut self.radio_mode.atlas;
                atlas.panel_tab = tab;
                atlas.focus = AtlasFocus::Panel;
                atlas.panel_selected = 0;
                atlas.search_editing = false;
                self.atlas_refresh_panel_rows();
                self.atlas_resolve_library_rows()
            }
            AtlasTarget::Search => self.on_atlas_action(Action::AtlasSearch),
        }
    }

    /// Returns `None` when Atlas does not own this drag.
    pub(in crate::app) fn atlas_mouse_drag(&mut self, col: u16, row: u16) -> Option<Vec<Cmd>> {
        if !self.atlas_active() {
            return None;
        }
        let geom = self.atlas_geometry()?;
        let (dpx, dpy) = self.atlas_renderer().dots_per_cell();
        let atlas = &mut self.radio_mode.atlas;
        let press = atlas.press.as_mut()?;
        let (dx_cells, dy_cells) = (
            f32::from(col) - f32::from(press.col),
            f32::from(row) - f32::from(press.row),
        );
        if dx_cells == 0.0 && dy_cells == 0.0 {
            return Some(Vec::new());
        }
        press.moved = true;
        press.col = col;
        press.row = row;
        // Dots with rows growing downward — the convention `Kinetic` and `rotate_by` share.
        let dx = dx_cells * f32::from(dpx);
        let dy = dy_cells * f32::from(dpy);
        let dt = press.started.elapsed().as_secs_f32() - atlas.drag_clock;
        atlas.drag_clock += dt;
        let clock = atlas.drag_clock;
        atlas.velocity.record(dx, dy, dt.max(1e-3), clock);
        let deg = degrees_per_dot(geom.radius_dots());
        atlas.camera.rotate_by(dy * deg, -dx * deg);
        self.dirty = true;
        Some(Vec::new())
    }

    pub(in crate::app) fn atlas_mouse_left_up(&mut self) -> Option<Vec<Cmd>> {
        if !self.atlas_active() {
            return None;
        }
        let press = self.radio_mode.atlas.press.take()?;
        self.dirty = true;
        if press.caught_coast {
            return Some(Vec::new());
        }
        if !press.moved {
            return Some(self.atlas_click(press.col, press.row));
        }
        if self.config.atlas.coast && self.animations().master {
            let atlas = &mut self.radio_mode.atlas;
            let clock = press.started.elapsed().as_secs_f32();
            let (vx, vy) = atlas.velocity.release(clock);
            if let Some(k) = Kinetic::launch(vx, vy) {
                atlas.kinetic = k;
                atlas.last_tick = Some(Instant::now());
            }
        }
        Some(Vec::new())
    }

    fn atlas_click(&mut self, col: u16, row: u16) -> Vec<Cmd> {
        self.radio_mode.atlas.crosshair = Some((col, row));
        if let Some(idx) = self.atlas_station_at_cell(col, row) {
            let text = self.atlas_station_context(idx);
            self.atlas_set_context(text);
            return self.atlas_tune(idx);
        }
        match self.atlas_country_at_cell(col, row) {
            Some(country) => self.atlas_browse_country(country),
            None => {
                self.radio_mode.atlas.highlight = None;
                self.atlas_clear_country();
                Vec::new()
            }
        }
    }

    pub(in crate::app) fn atlas_mouse_scroll(
        &mut self,
        up: bool,
        col: u16,
        row: u16,
    ) -> Option<Vec<Cmd>> {
        if !self.atlas_active() {
            return None;
        }
        let rect = self.atlas_globe_rect()?;
        let inside = col >= rect.x
            && col < rect.x + rect.width
            && row >= rect.y
            && row < rect.y + rect.height;
        if inside {
            return Some(self.atlas_zoom(if up { WHEEL_ZOOM } else { 1.0 / WHEEL_ZOOM }));
        }
        if self.bridges.atlas_panel.get().is_some_and(|p| {
            col >= p.x && col < p.x + p.width && row >= p.y && row < p.y + p.height
        }) {
            self.radio_mode.atlas.focus = AtlasFocus::Panel;
            return Some(self.atlas_panel_move(!up));
        }
        None
    }

    pub(in crate::app) fn atlas_focus_lost(&mut self) {
        let atlas = &mut self.radio_mode.atlas;
        if atlas.press.take().is_some() {
            self.dirty = true;
        }
    }

    /// Advance coast / autorotate by the wall-clock time since the last tick.
    pub(in crate::app) fn atlas_tick(&mut self) {
        if !self.atlas_motion_active() {
            return;
        }
        let now = Instant::now();
        let radius = self.atlas_geometry().map_or(100.0, |g| g.radius_dots());
        let atlas = &mut self.radio_mode.atlas;
        let dt = atlas
            .last_tick
            .map_or(1.0 / 30.0, |t| now.duration_since(t).as_secs_f32());
        atlas.last_tick = Some(now);
        // A slow animation clock (5 fps is allowed) must not read as a stalled one: clamp the
        // integration step, and only a real pause (unfocused, mode switch) aborts the coast.
        let dt = if dt > STALL_ABORT_SECS {
            f32::INFINITY
        } else {
            dt.min(crate::atlas::view::KINETIC_MAX_FRAME)
        };
        if atlas.kinetic.active() {
            let still = atlas.kinetic.step(&mut atlas.camera, dt, radius);
            if !still {
                atlas.kinetic = Kinetic::default();
                self.atlas_land();
            }
        } else if atlas.autorotate {
            atlas
                .camera
                .rotate_by(0.0, AUTOROTATE_DEG_PER_SEC * dt.min(0.1));
        }
        self.dirty = true;
    }

    /// The coast has stopped: highlight the deepest visible station (reference "landing").
    fn atlas_land(&mut self) {
        let axis = self.radio_mode.atlas.camera.centre.to_unit();
        let atlas = &mut self.radio_mode.atlas;
        atlas.last_tick = None;
        if let Some(idx) = atlas.catalog.nearest_visible(axis, atlas.selected) {
            atlas.highlight = Some(idx);
            let text = format!(
                "{} · {}",
                t!("Landed near", "근처에 도착", "近くに到着"),
                self.atlas_station_context(idx)
            );
            self.atlas_set_context(text);
        }
    }

    pub(in crate::app) fn on_atlas_msg(&mut self, msg: AtlasMsg) -> Vec<Cmd> {
        match msg {
            AtlasMsg::Event(event) => self.on_atlas_event(event),
        }
    }

    fn atlas_event_generation(event: &AtlasEvent) -> u64 {
        match event {
            AtlasEvent::World { generation, .. }
            | AtlasEvent::Country { generation, .. }
            | AtlasEvent::Search { generation, .. }
            | AtlasEvent::Resolved { generation, .. }
            | AtlasEvent::Error { generation, .. } => *generation,
        }
    }

    fn atlas_convert(rows: Vec<RadioStation>) -> Vec<AtlasStation> {
        let world: &World = world();
        let estimator = |code: &str, seed: &str| -> Option<LatLon> {
            world.by_code(code).map(|c| c.estimated_location(seed))
        };
        rows.into_iter()
            .filter_map(|r| AtlasStation::from_radio(r, &estimator))
            .collect()
    }

    fn on_atlas_event(&mut self, event: AtlasEvent) -> Vec<Cmd> {
        if Self::atlas_event_generation(&event) != self.radio_mode.atlas.generation {
            return Vec::new();
        }
        let mut cmds = Vec::new();
        match event {
            AtlasEvent::World {
                page,
                rows,
                from_cache,
                refreshing,
                ..
            } => {
                let converted = Self::atlas_convert(rows);
                let atlas = &mut self.radio_mode.atlas;
                atlas.catalog.merge_world(converted);
                if !from_cache {
                    atlas.pages_fetched = atlas.pages_fetched.max(page + 1);
                }
                atlas.loading = refreshing;
                atlas.error = None;
                let limit = self.config.atlas.effective_station_limit();
                let atlas = &mut self.radio_mode.atlas;
                // Progressive loading continues once no network refresh is pending, whether the
                // page came from the directory or from a fresh cache.
                // Random pages overlap, so allow a few beyond the arithmetic minimum.
                let page_budget = limit.div_ceil(WORLD_PAGE) + MORE_PAGE_SLACK;
                if !refreshing
                    && (atlas.catalog.len() as u32) < limit
                    && atlas.pages_fetched < page_budget
                {
                    atlas.loading = true;
                    let page = atlas.pages_fetched.max(1);
                    cmds.push(Cmd::Atlas(AtlasCmd::More {
                        generation: atlas.generation,
                        page,
                        limit: WORLD_PAGE,
                    }));
                }
                self.atlas_refresh_panel_rows();
                self.atlas_follow_playing_now();
            }
            AtlasEvent::Country { code, rows, .. } => {
                let converted = Self::atlas_convert(rows);
                let atlas = &mut self.radio_mode.atlas;
                let count = converted.len();
                atlas.catalog.merge_country(converted);
                atlas.loading = false;
                atlas.error = None;
                if atlas
                    .active_country
                    .map(|c| country_code_str(&c).to_owned())
                    == Some(code)
                {
                    let name = atlas.active_country_name.clone();
                    self.atlas_set_context(format!(
                        "{name} · {count} {}",
                        t!("stations", "개 방송국", "局")
                    ));
                }
                self.atlas_refresh_panel_rows();
            }
            AtlasEvent::Search { query, rows, .. } => {
                if query.trim() == self.radio_mode.atlas.search_query.trim() {
                    let converted = Self::atlas_convert(rows);
                    self.radio_mode.atlas.catalog.merge_world(converted);
                    self.atlas_refresh_panel_rows();
                }
            }
            AtlasEvent::Resolved { rows, .. } => {
                let converted = Self::atlas_convert(rows);
                self.radio_mode.atlas.catalog.merge_world(converted);
                self.atlas_refresh_panel_rows();
            }
            AtlasEvent::Error { kind, message, .. } => {
                let atlas = &mut self.radio_mode.atlas;
                atlas.loading = false;
                atlas.error = Some(match kind {
                    AtlasErrorKind::Network => t!(
                        "Radio Browser is unreachable",
                        "Radio Browser에 연결할 수 없어요",
                        "Radio Browserに接続できません"
                    )
                    .to_owned(),
                    _ => message,
                });
                self.dirty = true;
            }
        }
        cmds
    }

    /// Recompute the panel's row list from the tab, the search query and the catalog.
    pub(in crate::app) fn atlas_refresh_panel_rows(&mut self) {
        let atlas = &mut self.radio_mode.atlas;
        let query = atlas.search_query.trim();
        atlas.panel_rows = if !query.is_empty() {
            PanelRows::Catalog(atlas.catalog.search(query, SEARCH_MAX))
        } else {
            match atlas.panel_tab {
                PanelTab::World => match atlas.active_country {
                    Some(code) => PanelRows::Catalog(
                        atlas
                            .catalog
                            .by_country(country_code_str(&code), SEARCH_MAX),
                    ),
                    None => PanelRows::Catalog((0..atlas.catalog.len().min(SEARCH_MAX)).collect()),
                },
                PanelTab::Favorites => PanelRows::Library(
                    self.library
                        .radio_favorites
                        .iter()
                        .map(|s| Box::from(song_uuid(&s.video_id)))
                        .collect(),
                ),
                PanelTab::Recent => PanelRows::Library(
                    self.library
                        .radios
                        .iter()
                        .map(|s| Box::from(song_uuid(&s.video_id)))
                        .collect(),
                ),
            }
        };
        let len = match &atlas.panel_rows {
            PanelRows::Catalog(v) => v.len(),
            PanelRows::Library(v) => v.len(),
        };
        atlas.panel_selected = atlas.panel_selected.min(len.saturating_sub(1));
        self.dirty = true;
    }

    /// Favorites/recent rows without a catalog entry get resolved once so they can be plotted.
    fn atlas_resolve_library_rows(&mut self) -> Vec<Cmd> {
        let atlas = &self.radio_mode.atlas;
        let PanelRows::Library(rows) = &atlas.panel_rows else {
            return Vec::new();
        };
        let missing: Vec<String> = rows
            .iter()
            .filter(|uuid| atlas.catalog.index_of(uuid).is_none())
            .take(RESOLVE_MAX)
            .map(|u| u.to_string())
            .collect();
        if missing.is_empty() {
            return Vec::new();
        }
        vec![Cmd::Atlas(AtlasCmd::Resolve {
            generation: atlas.generation,
            uuids: missing,
        })]
    }

    pub(in crate::app) fn on_mouse_move(&mut self, col: u16, row: u16) -> Vec<Cmd> {
        self.interaction.pointer_motion = true;
        if !self.atlas_active() || self.radio_mode.atlas.press.is_some() {
            return Vec::new();
        }
        if !self.animations().master {
            if self.radio_mode.atlas.hover.take().is_some() {
                self.dirty = true;
            }
            return Vec::new();
        }
        let on_globe = self.atlas_globe_rect().is_some_and(|rect| {
            col >= rect.x
                && col < rect.x + rect.width
                && row >= rect.y
                && row < rect.y + rect.height
        });
        let next = on_globe
            .then(|| self.atlas_station_at_cell(col, row))
            .flatten();
        if self.radio_mode.atlas.hover != next {
            self.radio_mode.atlas.hover = next;
            self.dirty = true;
        }
        Vec::new()
    }

    pub(in crate::app) fn atlas_pin_at(&self, col: u16, row: u16) -> Option<(usize, Box<str>)> {
        if !self.atlas_active() {
            return None;
        }
        let index = self.atlas_station_at_cell(col, row)?;
        let uuid = self.radio_mode.atlas.catalog.get(index)?.uuid.clone();
        Some((index, uuid))
    }

    pub(in crate::app) fn execute_atlas_context_command(
        &mut self,
        index: usize,
        uuid: Box<str>,
        command: ContextCommand,
    ) -> Vec<Cmd> {
        let Some(st) = self.radio_mode.atlas.catalog.get(index) else {
            return Vec::new();
        };
        if st.uuid != uuid {
            return Vec::new();
        }
        match command {
            ContextCommand::PlayNow => self.atlas_tune(index),
            ContextCommand::ToggleFavorite => self.atlas_favorite_at(index),
            ContextCommand::CopyStreamUrl => self.atlas_copy_stream_url(index),
            ContextCommand::BrowseCountry => self.atlas_browse_station_country(index),
            _ => Vec::new(),
        }
    }

    fn atlas_copy_stream_url(&mut self, idx: usize) -> Vec<Cmd> {
        let Some(url) = self
            .radio_mode
            .atlas
            .catalog
            .get(idx)
            .map(|st| st.url.to_string())
        else {
            return Vec::new();
        };
        copy_to_clipboard(&url);
        self.atlas_toast(
            t!(
                "✓ Stream URL copied to clipboard",
                "✓ 스트림 URL이 클립보드에 복사됐어요",
                "✓ ストリームURLをクリップボードにコピーしました"
            )
            .to_owned(),
        );
        Vec::new()
    }

    fn atlas_browse_station_country(&mut self, idx: usize) -> Vec<Cmd> {
        let Some(code) = self
            .radio_mode
            .atlas
            .catalog
            .get(idx)
            .map(|st| st.country_code)
        else {
            return Vec::new();
        };
        match world().by_code(country_code_str(&code)) {
            Some(country) => self.atlas_browse_country(country),
            None => Vec::new(),
        }
    }
}
