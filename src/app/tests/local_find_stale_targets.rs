use super::*;
use ratatui::layout::Rect;

fn find_app(indexed: bool) -> App {
    let tracks = indexed.then(|| {
        vec![super::local::local_deck_track(
            "/music/Pointer.flac",
            "Pointer",
            &["Local"],
            Some("Safety"),
            Some("Local"),
            &["Test"],
            1,
        )]
    });
    let mut app = super::local::app_with_local_deck_index(tracks.unwrap_or_default());
    app.mode = Mode::Search;
    app.local_mode.find.corpus_generation = 11;
    app.local_mode.find.request_id = 17;
    app
}

fn rendered_launch_target(app: &App, wanted: usize) -> MouseTarget {
    render_app(app);
    app.hits
        .regions()
        .iter()
        .find_map(|region| match &region.target {
            target @ MouseTarget::LocalFindLaunchpad { index, .. } if *index == wanted => {
                Some(target.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("Local Find action {wanted} was not rendered"))
}

#[test]
fn delayed_launchpad_and_recovery_targets_reject_new_generations() {
    let mut launchpad = find_app(true);
    let old_launch = rendered_launch_target(&launchpad, 0);
    assert!(matches!(
        &old_launch,
        MouseTarget::LocalFindLaunchpad { stamp, .. }
            if stamp == &launchpad.local_find_pointer_stamp()
    ));
    launchpad.local_mode.find.request_id += 1;
    assert!(launchpad.on_mouse_target(old_launch).is_empty());
    assert!(launchpad.local_mode.find.query.is_empty());

    let mut recovery = find_app(false);
    let old_rescan = rendered_launch_target(&recovery, 6);
    recovery.local_mode.find.corpus_generation += 1;
    assert!(recovery.on_mouse_target(old_rescan).is_empty());
    assert!(!recovery.local_mode.index.scanning);
}

#[test]
fn local_find_scrollbar_target_and_drag_fail_closed_after_view_change() {
    let mut app = find_app(true);
    let bar = Rect::new(4, 10, 1, 5);
    let stamp = app.local_find_pointer_stamp();

    let backend = TestBackend::new(6, 16);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            crate::ui::buttons::render_list_scrollbar(
                frame,
                &app,
                bar,
                ScrollSurface::LocalFind,
                7,
                0,
                2,
            );
        })
        .unwrap();
    assert!(app.hits.regions().iter().any(|region| {
        matches!(
            &region.target,
            MouseTarget::LocalFindScrollbar { stamp: rendered } if rendered == &stamp
        )
    }));

    // Record the same viewport geometry the real Local Find render records.
    app.bridges
        .local_find_scroll
        .resolve(0, 2, 7, crate::ui::scroll::SCROLLOFF);
    app.on_mouse_click(bar.x, bar.y, false);
    assert!(app.interaction.drag_scrollbar.is_some());
    app.on_mouse_drag(bar.x, bar.bottom() - 1);
    assert_eq!(app.bridges.local_find_scroll.offset(), 5);

    app.on_mouse_left_up();
    app.bridges.local_find_scroll.reset();
    app.bridges
        .local_find_scroll
        .resolve(0, 2, 7, crate::ui::scroll::SCROLLOFF);
    app.on_mouse_click(bar.x, bar.y, false);
    assert!(app.interaction.drag_scrollbar.is_some());

    // A query/result replacement while the button is held invalidates the captured drag.
    app.local_mode.find.request_id += 1;
    app.on_mouse_drag(bar.x, bar.bottom() - 1);
    assert_eq!(app.bridges.local_find_scroll.offset(), 0);
    assert!(app.interaction.drag_scrollbar.is_none());

    // The old hit map also cannot begin another drag before the next frame is rendered.
    app.on_mouse_click(bar.x, bar.y, false);
    assert!(app.interaction.drag_scrollbar.is_none());
    assert_eq!(app.bridges.local_find_scroll.offset(), 0);
}
