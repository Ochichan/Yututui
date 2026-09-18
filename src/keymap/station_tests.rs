use super::*;

#[test]
fn station_may_shadow_global_and_cannot_clash_inside_itself() {
    assert_eq!(
        crate::keymap::PRE_GLOBAL_CONTEXTS,
        &[
            KeyContext::LocalDeck,
            KeyContext::Station,
            KeyContext::StationCard,
        ]
    );
    let km = KeyMap::default();
    let ban = parse_chord("B").unwrap();
    assert_eq!(km.chord(KeyContext::Station, Action::BanTrack), Some(ban));
    assert_eq!(
        km.chord(KeyContext::Global, Action::ToggleControlBox),
        Some(ban)
    );

    let mut km = KeyMap::default();
    let err = km
        .rebind(KeyContext::Global, Action::ToggleHelp, ban)
        .expect_err("Global B is already ToggleControlBox");
    assert_eq!(err.ctx, KeyContext::Global);
    assert_eq!(err.existing, Action::ToggleControlBox);

    let err = km
        .rebind(KeyContext::Station, Action::BanArtist, ban)
        .expect_err("Station B is BanTrack");
    assert_eq!(err.ctx, KeyContext::Station);
    assert_eq!(err.existing, Action::BanTrack);

    km.rebind(
        KeyContext::Station,
        Action::BanTrack,
        parse_chord("f9").unwrap(),
    )
    .expect("unused Station chord");
    km.rebind(KeyContext::Station, Action::BanTrack, ban)
        .expect("Station may reclaim a Global chord");
}
