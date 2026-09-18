use super::*;
use crate::api::Song;
use crate::app::MouseTarget;

fn text_for<'a>(
    parts: &'a [(Option<MouseTarget>, std::borrow::Cow<'static, str>)],
    target: &MouseTarget,
) -> &'a str {
    parts
        .iter()
        .find(|(candidate, _)| candidate.as_ref() == Some(target))
        .map(|(_, text)| text.as_ref())
        .unwrap_or_else(|| panic!("missing {target:?}"))
}

#[test]
fn taste_chip_follows_streaming_mode_and_stays_off_when_the_station_is_off() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.queue
        .set(vec![Song::remote("a", "Night", "Nova", "3:00")], 0);
    app.autoplay_streaming = true;
    let song = app.queue.current().cloned().expect("current");
    assert_eq!(
        app.streaming
            .taste
            .apply(crate::streaming::TasteEdit::ban_track(&song).expect("id")),
        crate::streaming::TasteOutcome::Applied
    );
    assert_eq!(
        app.streaming
            .taste
            .apply(crate::streaming::TasteEdit::ban_artist(&song).expect("artist")),
        crate::streaming::TasteOutcome::Applied
    );
    assert_eq!(
        app.streaming
            .taste
            .apply(crate::streaming::TasteEdit::parse_seed("jazz").expect("term")),
        crate::streaming::TasteOutcome::Applied
    );

    let parts = status_line_parts(&app, "    ", false, false);
    assert_eq!(
        text_for(&parts, &MouseTarget::StationCard),
        "banned 2 · seeds 1"
    );
    let streaming_at = parts
        .iter()
        .position(|(target, _)| matches!(target, Some(MouseTarget::StreamingMenu)))
        .expect("streaming chip");
    let taste_at = parts
        .iter()
        .position(|(target, _)| matches!(target, Some(MouseTarget::StationCard)))
        .expect("taste chip");
    assert!(
        taste_at > streaming_at,
        "taste chip sits after streaming:<mode>"
    );

    app.autoplay_streaming = false;
    let parts = status_line_parts(&app, "    ", false, false);
    assert!(
        !parts
            .iter()
            .any(|(target, _)| matches!(target, Some(MouseTarget::StationCard)))
    );
}
