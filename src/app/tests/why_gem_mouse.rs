use super::*;

fn add_why_gem(app: &mut App, video_id: &str) {
    app.why_gem.upsert(
        video_id.to_owned(),
        why_gem::streaming_origin_model(crate::streaming::StreamingMode::Balanced),
    );
}

#[test]
fn paired_double_click_does_not_close_a_card_opened_from_a_queue_row() {
    let mut app = app_playing(10, 0);
    add_why_gem(&mut app, "id9");
    app.open_queue_popup();
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::QueueWhyGem(9));

    app.update(Msg::MouseClick {
        col,
        row,
        multi: false,
    });
    assert_eq!(app.overlays.why_gem_video_id.as_deref(), Some("id9"));
    assert_eq!(app.overlays.why_gem_queue_index, Some(9));

    render_app(&app);
    let cmds = app.update(Msg::MouseDoubleClick { col, row });
    assert!(cmds.is_empty());
    assert_eq!(app.overlays.why_gem_video_id.as_deref(), Some("id9"));
    assert_eq!(current(&app), "id0");
}

#[test]
fn paired_outside_double_click_cannot_activate_the_exposed_queue_row() {
    let mut app = app_playing(10, 0);
    add_why_gem(&mut app, "id0");
    app.open_queue_popup();
    app.open_why_gem_at(0);
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::QueueRow(9));
    assert_eq!(
        app.mouse_target_at(col, row),
        Some(MouseTarget::QueueRow(9)),
        "fixture row must sit outside the centered WhyGem card"
    );

    let first = app.update(Msg::MouseClick {
        col,
        row,
        multi: false,
    });
    assert!(first.is_empty());
    assert!(app.overlays.why_gem_video_id.is_none());

    let second = app.update(Msg::MouseDoubleClick { col, row });
    assert!(second.is_empty(), "paired second press must be consumed");
    assert_eq!(current(&app), "id0");
    assert!(app.queue_popup.open);
}

#[test]
fn outside_dismiss_gesture_cannot_drag_the_exposed_queue_selection() {
    let mut app = app_playing(10, 0);
    add_why_gem(&mut app, "id0");
    app.open_queue_popup();
    app.open_why_gem_at(0);
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::QueueRow(9));
    assert_eq!(app.queue_popup.cursor, 0);

    assert!(
        app.update(Msg::MouseClick {
            col,
            row,
            multi: false,
        })
        .is_empty()
    );
    assert!(app.overlays.why_gem_video_id.is_none());

    assert!(app.update(Msg::MouseDrag { col, row }).is_empty());
    assert_eq!(
        app.queue_popup.cursor, 0,
        "the press that dismissed WhyGem must own its entire drag gesture"
    );
}
