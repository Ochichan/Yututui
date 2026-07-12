use super::*;

fn search_app() -> App {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Results;
    app.search.results = songs(3);
    app
}

fn open_search_menu(app: &mut App, index: usize) -> (u16, u16) {
    render_app(app);
    let (col, row) = button_center(app, MouseTarget::ListRow(index));
    let cmds = app.update(Msg::MouseRightClick { col, row });
    assert!(cmds.is_empty());
    assert!(app.overlays.context_menu.is_some());
    (col, row)
}

#[test]
fn search_right_click_opens_menu_without_legacy_enqueue() {
    let mut app = search_app();

    open_search_menu(&mut app, 1);

    assert_eq!(app.search.selected, 1);
    assert_eq!(
        app.queue.len(),
        0,
        "opening a menu must not enqueue the row"
    );
    assert!(app.queue.video_ids().all(|id| id != "id1"));
}

#[test]
fn immediate_right_double_click_activates_original_row_under_menu_hit_region() {
    let mut app = search_app();
    let (col, row) = open_search_menu(&mut app, 1);

    // A render after the first press publishes menu-item regions above the original search row.
    // Register the overlap explicitly so this test remains focused on reducer routing.
    app.register_mouse_button(Rect::new(col, row, 1, 1), MouseTarget::ContextMenuItem(0));
    assert_eq!(
        app.mouse_target_at(col, row),
        Some(MouseTarget::ContextMenuItem(0))
    );

    let mut cmds = app.update(Msg::MouseRightDoubleClick { col, row });

    assert_loads_video(&cmds, "id1");
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(current(&app), "id1");
    assert!(app.overlays.context_menu.is_none());
}

#[test]
fn outside_left_click_closes_menu_and_is_consumed() {
    let mut app = search_app();
    open_search_menu(&mut app, 1);
    let (col, row) = button_center(&app, MouseTarget::Nav(Mode::Library));

    let cmds = app.update(Msg::MouseClick { col, row });

    assert!(cmds.is_empty());
    assert!(app.overlays.context_menu.is_none());
    assert_eq!(
        app.mode,
        Mode::Search,
        "the covered nav target must not fire"
    );
    assert_eq!(app.search.selected, 1);
}

#[test]
fn queue_right_click_preserves_existing_range_and_menu_removes_it() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    app.queue_popup.anchor = 1;
    app.queue_popup.cursor = 3;
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::QueueRow(2));

    let cmds = app.update(Msg::MouseRightClick { col, row });

    assert!(cmds.is_empty());
    assert!(app.overlays.context_menu.is_some());
    assert_eq!((app.queue_popup.anchor, app.queue_popup.cursor), (1, 3));

    // Queue menus contain "Play now" followed by "Remove".
    app.register_mouse_button(Rect::new(col, row, 1, 1), MouseTarget::ContextMenuItem(1));
    let cmds = app.update(Msg::MouseClick { col, row });

    assert_no_load(&cmds);
    assert!(app.overlays.context_menu.is_none());
    let ids: Vec<&str> = app
        .queue
        .ordered()
        .iter()
        .map(|song| song.video_id.as_str())
        .collect();
    assert_eq!(ids, vec!["id0", "id4"]);
}

#[test]
fn queue_menu_play_from_here_preserves_the_existing_queue() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = app_playing(6, 0);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    app.queue_popup.anchor = 2;
    app.queue_popup.cursor = 4;
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::QueueRow(3));

    let cmds = app.update(Msg::MouseRightClick { col, row });

    assert!(cmds.is_empty());
    let menu = app
        .overlays
        .context_menu
        .as_ref()
        .expect("queue menu should open");
    assert_eq!(menu.target_count(), 3);
    assert_eq!(menu.items[0].label(menu.target_count()), "Play from here");

    app.register_mouse_button(Rect::new(col, row, 1, 1), MouseTarget::ContextMenuItem(0));
    let mut cmds = app.update(Msg::MouseClick { col, row });

    assert_loads_video(&cmds, "id2");
    assert_eq!(current(&app), "id0");
    assert!(app.queue_popup.open);
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(current(&app), "id2");
    assert!(!app.queue_popup.open);
    let ids: Vec<&str> = app
        .queue
        .ordered()
        .iter()
        .map(|song| song.video_id.as_str())
        .collect();
    assert_eq!(ids, vec!["id0", "id1", "id2", "id3", "id4", "id5"]);
}

#[test]
fn same_right_click_on_open_menu_keeps_selection() {
    let mut app = search_app();
    let (col, row) = open_search_menu(&mut app, 1);
    app.overlays.context_menu.as_mut().unwrap().selected = 2;

    let cmds = app.update(Msg::MouseRightClick { col, row });

    assert!(cmds.is_empty());
    let menu = app
        .overlays
        .context_menu
        .as_ref()
        .expect("same right click should keep the menu open");
    assert_eq!(menu.selected, 2);
}

#[test]
fn disabled_right_double_click_keeps_open_menu() {
    let mut app = search_app();
    app.mousemap
        .set(
            crate::mousemap::MouseContext::Search,
            crate::mousemap::MouseGesture::RightDoubleClick,
            crate::mousemap::MouseAction::Disabled,
        )
        .unwrap();
    let (col, row) = open_search_menu(&mut app, 1);
    app.overlays.context_menu.as_mut().unwrap().selected = 2;

    let cmds = app.update(Msg::MouseRightDoubleClick { col, row });

    assert!(cmds.is_empty());
    assert_eq!(app.queue.len(), 0);
    let menu = app
        .overlays
        .context_menu
        .as_ref()
        .expect("disabled double-click should leave the menu open");
    assert_eq!(menu.selected, 2);
}

#[test]
fn stale_search_identity_prevents_menu_action_and_reports_status() {
    let mut app = search_app();
    let (col, row) = open_search_menu(&mut app, 1);
    app.search.results[1] = Song::remote("replacement", "Replacement", "A", "0:10");
    app.register_mouse_button(Rect::new(col, row, 1, 1), MouseTarget::ContextMenuItem(0));

    let cmds = app.update(Msg::MouseClick { col, row });

    assert_no_load(&cmds);
    assert_eq!(app.queue.len(), 0);
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(
        app.status.text.contains("list changed") || app.status.text.contains("목록이 바뀌었어요"),
        "unexpected stale-target status: {:?}",
        app.status.text
    );
}

#[test]
fn shift_f10_opens_menu_beside_selected_visible_search_row() {
    let mut app = search_app();
    app.search.selected = 2;
    app.mousemap
        .set(
            crate::mousemap::MouseContext::Search,
            crate::mousemap::MouseGesture::RightClick,
            crate::mousemap::MouseAction::Disabled,
        )
        .unwrap();
    render_app(&app);
    let row_rect = app
        .hits
        .regions()
        .iter()
        .find(|region| region.target == MouseTarget::ListRow(2))
        .map(|region| region.rect)
        .expect("selected search row should be visible");

    let cmds = app.update(Msg::Key(shift(KeyCode::F(10))));

    assert!(cmds.is_empty());
    let menu = app
        .overlays
        .context_menu
        .as_ref()
        .expect("Shift+F10 should open the selected row menu");
    assert_eq!(menu.anchor_row, row_rect.y);
    assert_eq!(menu.anchor_col, row_rect.x.saturating_add(1));
}

#[test]
fn paired_left_double_click_does_not_leak_into_menu_opened_picker() {
    let mut app = search_app();
    let (col, row) = open_search_menu(&mut app, 1);
    // Search track menus: play, enqueue, favorite, add to playlist, download.
    app.register_mouse_button(Rect::new(col, row, 1, 1), MouseTarget::ContextMenuItem(3));

    app.update(Msg::MouseClick { col, row });
    assert!(app.playlist_picker.is_some());

    app.update(Msg::MouseDoubleClick { col, row });
    assert!(
        app.playlist_picker.is_some(),
        "the paired second press must not dismiss or confirm the newly opened picker"
    );
}

#[test]
fn outside_menu_press_owns_following_queue_drag_until_button_up() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    render_app(&app);
    let (open_col, open_row) = button_center(&app, MouseTarget::QueueRow(1));
    app.update(Msg::MouseRightClick {
        col: open_col,
        row: open_row,
    });
    assert!(app.overlays.context_menu.is_some());

    let (close_col, close_row) = button_center(&app, MouseTarget::QueueRow(3));
    app.update(Msg::MouseClick {
        col: close_col,
        row: close_row,
    });
    assert!(app.overlays.context_menu.is_none());
    assert_eq!(app.queue_popup.cursor, 1);

    let (drag_col, drag_row) = button_center(&app, MouseTarget::QueueRow(4));
    app.update(Msg::MouseDrag {
        col: drag_col,
        row: drag_row,
    });
    assert_eq!(
        app.queue_popup.cursor, 1,
        "drag motion from the consumed closing press must not reach the queue"
    );
    app.update(Msg::MouseLeftUp);
}
