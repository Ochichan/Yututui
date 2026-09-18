//! The Atlas globe view: the Player content area in dedicated Radio mode while Atlas is open.
//!
//! The globe is packed into terminal cells from a dot raster (Braille 2×4 or ASCII 1×1);
//! no image protocol is involved, so it renders wherever the rest of the app does. The
//! raster is a pure function of [`RasterKey`], memoised in `App.bridges.atlas_raster`, so
//! redraws caused by unrelated state (toasts, ticks) are a plain blit.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::atlas::{
    AtlasFocus, AtlasTarget, GLOBE_MIN_HEIGHT, GLOBE_MIN_WIDTH, PANEL_MIN_WIDTH, PanelRows,
    PanelTab, land_mask,
};
use crate::app::{App, MouseTarget};
use crate::atlas::MarkerKind;
use crate::atlas::raster::{
    CellRect, DotClass, Geometry, MarkerCell, PackedCell, RasterParams, Renderer, pack,
    project_markers, rasterize,
};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::text::truncate_to_width;

/// Everything that changes the globe's bytes. Angles are quantised so sub-hundredth drift
/// (kinetic tails) does not force a re-raster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RasterKey {
    rect: CellRect,
    renderer: Renderer,
    lat_q: i32,
    lon_q: i32,
    scale_q: i32,
    active_country: Option<[u8; 2]>,
    grid: bool,
}

pub struct RasterCache {
    key: RasterKey,
    cells: Vec<PackedCell>,
}

const PANEL_WIDTH: u16 = 36;

fn cell_rect(r: Rect) -> CellRect {
    CellRect {
        x: r.x,
        y: r.y,
        width: r.width,
        height: r.height,
    }
}

fn raster_key(app: &App, rect: CellRect, renderer: Renderer) -> RasterKey {
    let atlas = &app.radio_mode.atlas;
    RasterKey {
        rect,
        renderer,
        lat_q: (atlas.camera.centre.lat * 100.0).round() as i32,
        lon_q: (atlas.camera.centre.lon * 100.0).round() as i32,
        scale_q: (atlas.camera.scale * 1000.0).round() as i32,
        active_country: atlas.active_country,
        grid: atlas.grid,
    }
}

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    app.bridges.atlas_globe.set(None);
    app.bridges.atlas_panel.set(None);
    if area.width < GLOBE_MIN_WIDTH || area.height < GLOBE_MIN_HEIGHT {
        let hint = Paragraph::new(t!(
            "Atlas needs a larger terminal",
            "아틀라스는 더 큰 터미널이 필요해요",
            "アトラスにはもっと大きな端末が必要です"
        ))
        .alignment(Alignment::Center)
        .style(app.theme.style(R::TextMuted));
        frame.render_widget(hint, area);
        return;
    }
    let panel_shown = app.atlas_panel_visible(area.width);
    let (globe_area, panel_area) = if panel_shown {
        let cols = Layout::horizontal([
            Constraint::Min(GLOBE_MIN_WIDTH),
            Constraint::Length(PANEL_WIDTH.max(PANEL_MIN_WIDTH)),
        ])
        .split(area);
        (cols[0], Some(cols[1]))
    } else {
        (area, None)
    };
    render_globe(frame, app, globe_area);
    if let Some(panel) = panel_area {
        render_panel(frame, app, panel);
    }
}

fn render_globe(frame: &mut Frame, app: &App, area: Rect) {
    let atlas = &app.radio_mode.atlas;
    let focused = atlas.focus == AtlasFocus::Globe;
    let title = format!(" {} ", t!("Atlas", "아틀라스", "アトラス"));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.style(if focused {
            R::BorderFocused
        } else {
            R::BorderMuted
        }))
        .title(Line::from(Span::styled(
            title,
            app.theme.style(R::TextPrimary).add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 4 || inner.height < 3 {
        return;
    }
    // Reserve the bottom row for the context / status line.
    let globe = Rect {
        height: inner.height - 1,
        ..inner
    };
    let status_row = Rect {
        y: inner.y + inner.height - 1,
        height: 1,
        ..inner
    };
    let rect = cell_rect(globe);
    app.bridges.atlas_globe.set(Some(rect));
    app.register_mouse_button(globe, MouseTarget::Atlas(AtlasTarget::Globe));

    let renderer = app.atlas_renderer();
    let key = raster_key(app, rect, renderer);
    let geom = Geometry::new(rect, renderer, atlas.camera.scale);
    {
        let mut cache = app.bridges.atlas_raster.borrow_mut();
        let stale = cache.as_ref().is_none_or(|c| c.key != key);
        if stale {
            let land = land_mask();
            let params = RasterParams {
                land,
                active: atlas
                    .active_mask
                    .as_ref()
                    .map(|m| m as &dyn crate::atlas::LandLookup),
                grid: atlas.grid,
            };
            let grid = rasterize(&geom, &atlas.camera, &params);
            let cells = pack(&geom, &grid, renderer);
            *cache = Some(RasterCache { key, cells });
        }
        let cells = &cache.as_ref().expect("filled above").cells;
        let buf = frame.buffer_mut();
        for cell in cells {
            let color = app.theme.color(match cell.class {
                DotClass::ActiveLand => R::Accent,
                DotClass::Coast => R::TextPrimary,
                DotClass::Land => R::TextMuted,
                DotClass::Limb => R::BorderMuted,
                _ => R::TextSubtle,
            });
            if let Some(c) = buf.cell_mut((cell.col, cell.row)) {
                c.set_char(cell.ch).set_fg(color);
            }
        }
    }

    let markers = project_markers(&geom, &atlas.camera, &app.atlas_markers());
    let ascii = renderer == Renderer::Ascii;
    let buf = frame.buffer_mut();
    for m in &markers {
        let (ch, role) = marker_glyph(m, ascii);
        if let Some(c) = buf.cell_mut((m.col, m.row)) {
            c.set_char(ch).set_fg(app.theme.color(role));
            if matches!(m.kind, MarkerKind::Selected | MarkerKind::Highlight) {
                c.set_style(
                    Style::default()
                        .fg(app.theme.color(role))
                        .add_modifier(Modifier::BOLD),
                );
                c.set_char(ch);
            }
        }
    }

    render_globe_status(frame, app, status_row, markers.len());
}

fn marker_glyph(m: &MarkerCell, ascii: bool) -> (char, R) {
    match (m.kind, ascii) {
        (MarkerKind::Selected, false) => ('◉', R::Accent),
        (MarkerKind::Selected, true) => ('☼', R::Accent),
        (MarkerKind::Highlight, false) => ('○', R::AccentAlt),
        (MarkerKind::Highlight, true) => ('○', R::AccentAlt),
        (MarkerKind::Favorite, _) => ('♥', R::Success),
        (MarkerKind::Normal, false) if m.count > 1 => ('●', R::TextPrimary),
        (MarkerKind::Normal, true) if m.count > 1 => ('■', R::TextPrimary),
        (MarkerKind::Normal, _) => ('•', R::TextPrimary),
        (MarkerKind::Estimated, _) => ('•', R::TextSubtle),
    }
}

fn render_globe_status(frame: &mut Frame, app: &App, row: Rect, visible: usize) {
    let atlas = &app.radio_mode.atlas;
    let hover_tip = app.atlas_hover_tip();
    let left = if let Some(err) = &atlas.error {
        err.clone()
    } else if let Some(tip) = hover_tip {
        tip
    } else if !atlas.context.is_empty() {
        atlas.context.clone()
    } else if atlas.catalog.is_empty() && atlas.loading {
        t!(
            "Loading stations…",
            "방송국 불러오는 중…",
            "局を読み込み中…"
        )
        .to_owned()
    } else {
        t!(
            "Drag to rotate · wheel to zoom · click a signal",
            "드래그로 회전 · 휠로 확대 · 방송국 클릭",
            "ドラッグで回転 · ホイールで拡大 · 局をクリック"
        )
        .to_owned()
    };
    let right = if atlas.loading {
        format!(
            "{} {}…",
            atlas.catalog.len(),
            t!("loading", "불러오는 중", "読み込み中")
        )
    } else {
        format!(
            "{}/{} {}",
            visible,
            atlas.catalog.len(),
            t!("signals", "개 방송국", "局")
        )
    };
    let right_w = crate::ui::buttons::text_width(&right);
    let left_w = row.width.saturating_sub(right_w + 1);
    let left = truncate_to_width(&left, usize::from(left_w));
    let left_style = if atlas.error.is_some() {
        app.theme.style(R::Error)
    } else {
        app.theme.style(R::TextMuted)
    };
    frame.render_widget(Paragraph::new(left).style(left_style), row);
    if right_w < row.width {
        let r = Rect {
            x: row.x + row.width - right_w,
            width: right_w,
            ..row
        };
        frame.render_widget(
            Paragraph::new(right).style(app.theme.style(R::TextSubtle)),
            r,
        );
    }
}

fn render_panel(frame: &mut Frame, app: &App, area: Rect) {
    let atlas = &app.radio_mode.atlas;
    let focused = atlas.focus == AtlasFocus::Panel;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.style(if focused {
            R::BorderFocused
        } else {
            R::BorderMuted
        }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.bridges.atlas_panel.set(Some(cell_rect(inner)));
    if inner.height < 3 || inner.width < 8 {
        return;
    }
    let rows = Layout::vertical([
        Constraint::Length(1), // tabs
        Constraint::Length(1), // search
        Constraint::Min(1),    // list
        Constraint::Length(1), // hint
    ])
    .split(inner);

    // Tabs.
    let mut x = rows[0].x;
    for tab in PanelTab::ALL {
        let label = format!(" {} ", tab.label());
        let w = crate::ui::buttons::text_width(&label);
        if x + w > rows[0].right() {
            break;
        }
        let r = Rect {
            x,
            y: rows[0].y,
            width: w,
            height: 1,
        };
        let style = if tab == atlas.panel_tab {
            app.theme
                .style(R::SelectionFg)
                .bg(app.theme.color(R::SelectionBg))
        } else {
            app.theme.style(R::TextMuted)
        };
        frame.render_widget(Paragraph::new(label).style(style), r);
        app.register_mouse_button(r, MouseTarget::Atlas(AtlasTarget::PanelTab(tab)));
        x += w + 1;
    }
    let close = "×";
    let close_rect = Rect {
        x: rows[0].right() - 1,
        y: rows[0].y,
        width: 1,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(close).style(app.theme.style(R::TextMuted)),
        close_rect,
    );
    app.register_mouse_button(close_rect, MouseTarget::Atlas(AtlasTarget::Close));

    // Search box.
    let query = if atlas.search_editing {
        format!("/ {}▏", atlas.search_query)
    } else if atlas.search_query.is_empty() {
        format!("/ {}", t!("search…", "검색…", "検索…"))
    } else {
        format!("/ {}", atlas.search_query)
    };
    let query = truncate_to_width(&query, usize::from(rows[1].width));
    frame.render_widget(
        Paragraph::new(query).style(app.theme.style(if atlas.search_editing {
            R::TextPrimary
        } else {
            R::TextMuted
        })),
        rows[1],
    );
    app.register_mouse_button(rows[1], MouseTarget::Atlas(AtlasTarget::Search));

    // Rows.
    let list = rows[2];
    type RowLabel<'a> = Box<dyn Fn(usize) -> Option<(String, String, bool)> + 'a>;
    let (len, label_of): (usize, RowLabel<'_>) = match &atlas.panel_rows {
        PanelRows::Catalog(idx) => (
            idx.len(),
            Box::new(move |i| {
                let st = atlas.catalog.get(*idx.get(i)?)?;
                Some((
                    st.name.to_string(),
                    st.meta_line(),
                    app.library.is_radio_favorite(&st.to_song().video_id),
                ))
            }),
        ),
        PanelRows::Library(uuids) => (
            uuids.len(),
            Box::new(move |i| {
                let uuid = uuids.get(i)?;
                let song = app
                    .library
                    .radio_favorites
                    .iter()
                    .chain(app.library.radios.iter())
                    .find(|s| s.video_id.ends_with(&**uuid))?;
                Some((
                    song.title.clone(),
                    song.artist.clone(),
                    app.library.is_radio_favorite(&song.video_id),
                ))
            }),
        ),
    };
    if len == 0 {
        let empty = match atlas.panel_tab {
            _ if !atlas.search_query.is_empty() => {
                t!("No matches", "일치하는 방송국 없음", "一致する局なし")
            }
            PanelTab::Favorites => t!(
                "No favorite stations yet (f)",
                "즐겨찾기한 방송국이 없어요 (f)",
                "お気に入りの局はまだありません (f)"
            ),
            PanelTab::Recent => t!(
                "Nothing tuned yet",
                "아직 들은 방송국이 없어요",
                "まだ再生した局はありません"
            ),
            PanelTab::World => t!("Loading…", "불러오는 중…", "読み込み中…"),
        };
        frame.render_widget(
            Paragraph::new(empty).style(app.theme.style(R::TextMuted)),
            list,
        );
    } else {
        let per_row = 2u16;
        let visible = usize::from(list.height / per_row).max(1);
        let selected = atlas.panel_selected.min(len - 1);
        let first = selected
            .saturating_sub(visible - 1)
            .min(len.saturating_sub(visible));
        let mut y = list.y;
        for i in first..len.min(first + visible) {
            let Some((name, meta, favorite)) = label_of(i) else {
                continue;
            };
            let is_selected = i == selected;
            let marker = if is_selected { "▸ " } else { "  " };
            let heart = if favorite { " ♥" } else { "" };
            let name_line = truncate_to_width(
                &format!("{marker}{name}"),
                usize::from(
                    list.width
                        .saturating_sub(crate::ui::buttons::text_width(heart)),
                ),
            );
            let meta_line = truncate_to_width(&format!("  {meta}"), usize::from(list.width));
            let row_rect = Rect {
                x: list.x,
                y,
                width: list.width,
                height: per_row.min(list.bottom().saturating_sub(y)),
            };
            let style = if is_selected && focused {
                app.theme
                    .style(R::SelectionFg)
                    .bg(app.theme.color(R::SelectionBg))
            } else if is_selected {
                app.theme
                    .style(R::SelectionInactiveFg)
                    .bg(app.theme.color(R::SelectionInactiveBg))
            } else {
                app.theme.style(R::TextPrimary)
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(name_line),
                    Span::styled(heart.to_owned(), app.theme.style(R::Success)),
                ]))
                .style(style),
                Rect {
                    height: 1,
                    ..row_rect
                },
            );
            if row_rect.height > 1 {
                frame.render_widget(
                    Paragraph::new(meta_line).style(if is_selected {
                        style
                    } else {
                        app.theme.style(R::TextMuted)
                    }),
                    Rect {
                        y: y + 1,
                        height: 1,
                        ..row_rect
                    },
                );
            }
            app.register_mouse_button(row_rect, MouseTarget::Atlas(AtlasTarget::PanelRow(i)));
            y += per_row;
            if y >= list.bottom() {
                break;
            }
        }
    }

    let hint = match atlas.active_country {
        Some(_) => format!(
            "{} · {}",
            atlas.active_country_name,
            t!("Enter plays", "Enter로 재생", "Enterで再生")
        ),
        None => t!(
            "Enter plays · n/p cycle · c country",
            "Enter 재생 · n/p 이동 · c 국가",
            "Enter再生 · n/p切替 · c国"
        )
        .to_owned(),
    };
    frame.render_widget(
        Paragraph::new(truncate_to_width(&hint, usize::from(rows[3].width)))
            .style(app.theme.style(R::TextSubtle)),
        rows[3],
    );
}

/// The one-line "open Atlas" affordance drawn under the radio set piece while Atlas is closed.
pub fn render_open_button(frame: &mut Frame, app: &App, row: Rect) {
    if row.width < 12 || row.height == 0 {
        return;
    }
    let chord = app
        .keymap
        .chord(
            crate::keymap::KeyContext::Player,
            crate::keymap::Action::ToggleAtlas,
        )
        .map_or_else(
            || "a".to_owned(),
            |c| crate::keymap::format_chord_for_display(c, app.retro_mode()),
        );
    let label = format!(
        "[{chord}] {}",
        t!("Atlas globe", "아틀라스 지구본", "アトラス地球儀")
    );
    let w = crate::ui::buttons::text_width(&label).min(row.width);
    let r = Rect {
        x: row.x + (row.width - w) / 2,
        width: w,
        ..row
    };
    frame.render_widget(
        Paragraph::new(label).style(app.theme.style(R::PlayerControl)),
        r,
    );
    app.register_mouse_button(r, MouseTarget::Atlas(AtlasTarget::Open));
}
