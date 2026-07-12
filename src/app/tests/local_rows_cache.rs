use super::local::{
    app_with_local_deck_index, assert_local_cached_render_matches_legacy, local_deck_track,
    local_song,
};
use super::*;

#[test]
fn local_rows_cache_reuses_arc_and_invalidates_for_revision_query_locale_and_fixture_content() {
    let _language_guard = crate::i18n::lock_for_test();
    let mut app = app_with_local_deck_index(vec![
        local_deck_track(
            "/tmp/music/a.flac",
            "Alpha",
            &["Artist"],
            Some("Album"),
            Some("Artist"),
            &["Rock"],
            1,
        ),
        local_deck_track(
            "/tmp/music/b.flac",
            "Beta",
            &["Artist"],
            Some("Album"),
            Some("Artist"),
            &["Rock"],
            2,
        ),
        local_deck_track(
            "/tmp/music/c.flac",
            "Gamma",
            &["Artist"],
            Some("Album"),
            Some("Artist"),
            &["Rock"],
            3,
        ),
    ]);

    let first = app.local_visible_rows();
    let second = app.local_visible_rows();
    assert!(std::sync::Arc::ptr_eq(&first, &second));

    app.local_mode.ui.filter_query = "beta".to_owned();
    let filtered = app.local_visible_rows();
    assert!(!std::sync::Arc::ptr_eq(&first, &filtered));
    assert_eq!(filtered.len(), 1);

    app.local_mode.ui.filter_query.clear();
    let before_direct_fixture_edit = app.local_visible_rows();
    let updated_at = app.local_mode.index.index.updated_at;
    app.local_mode.index.index.tracks[1].title = "Fixture changed".to_owned();
    assert_eq!(app.local_mode.index.index.updated_at, updated_at);
    let after_direct_fixture_edit = app.local_visible_rows();
    assert!(!std::sync::Arc::ptr_eq(
        &before_direct_fixture_edit,
        &after_direct_fixture_edit
    ));

    app.local_mode
        .ui
        .drill
        .push(LocalDrill::Genre("Rock".to_owned()));
    let drilled = app.local_visible_rows();
    assert!(!std::sync::Arc::ptr_eq(
        &after_direct_fixture_edit,
        &drilled
    ));
    app.local_mode.ui.drill.clear();

    let before_production_revision = app.local_visible_rows();
    let same_index = app.local_mode.index.index.clone();
    app.update(Msg::Local(LocalMsg::ScanFinished {
        index_path: None,
        result: crate::local::LocalScanResult {
            summary: crate::local::LocalScanSummary {
                indexed: same_index.tracks().len(),
                ..crate::local::LocalScanSummary::default()
            },
            index: same_index,
            errors: Vec::new(),
        },
    }));
    let after_production_revision = app.local_visible_rows();
    assert!(!std::sync::Arc::ptr_eq(
        &before_production_revision,
        &after_production_revision
    ));

    let old_language = crate::i18n::current();
    crate::i18n::set_language(crate::i18n::Language::Korean);
    let korean = app.local_visible_rows();
    assert!(!std::sync::Arc::ptr_eq(&after_production_revision, &korean));
    crate::i18n::set_language(old_language);
}

#[test]
fn local_rows_cache_guards_direct_download_and_error_fixture_mutations() {
    let mut downloads = App::new(100);
    downloads.mode = Mode::Library;
    downloads.library_ui.downloaded = vec![
        local_song("download-a"),
        local_song("download-b"),
        local_song("download-c"),
    ];
    downloads.apply_local_mode_confirm(LocalModeConfirm::Enter);
    let before_download_edit = downloads.local_visible_rows();
    downloads.library_ui.downloaded[1].title = "Changed fixture title".to_owned();
    let after_download_edit = downloads.local_visible_rows();
    assert!(!std::sync::Arc::ptr_eq(
        &before_download_edit,
        &after_download_edit
    ));
    downloads.library_ui.downloaded_rev = downloads.library_ui.downloaded_rev.wrapping_add(1);
    let after_download_revision = downloads.local_visible_rows();
    assert!(!std::sync::Arc::ptr_eq(
        &after_download_edit,
        &after_download_revision
    ));

    let mut errors = app_with_local_deck_index(Vec::new());
    errors.local_mode.index.errors = vec![crate::local::ScanError {
        path: PathBuf::from("/tmp/broken.flac"),
        message: "first fixture error".to_owned(),
    }];
    errors.switch_local_section(LocalSection::ScanErrors);
    let before_error_edit = errors.local_visible_rows();
    errors.local_mode.index.errors[0].message = "changed fixture error".to_owned();
    let after_error_edit = errors.local_visible_rows();
    assert!(!std::sync::Arc::ptr_eq(
        &before_error_edit,
        &after_error_edit
    ));
}

#[test]
fn local_cached_and_legacy_buffers_and_hits_match_across_layout_filter_scroll_and_locale() {
    let _language_guard = crate::i18n::lock_for_test();
    let tracks = (0..180)
        .map(|index| {
            let path = format!(
                "/tmp/music/Artist {}/Album {}/track {index:03}.flac",
                index % 9,
                index % 13
            );
            let title = if index % 17 == 0 {
                format!("Needle track {index:03}")
            } else {
                format!("Track {index:03}")
            };
            local_deck_track(
                &path,
                &title,
                &["Test Artist"],
                Some("Test Album"),
                Some("Test Artist"),
                &["Rock"],
                index,
            )
        })
        .collect();
    let mut app = app_with_local_deck_index(tracks);

    assert_local_cached_render_matches_legacy(&app, 120, 30);
    app.local_mode.ui.selected = usize::MAX;
    assert_local_cached_render_matches_legacy(&app, 60, 16);

    app.local_mode.ui.selected = 0;
    app.local_mode.ui.filter_query = "needle".to_owned();
    assert_local_cached_render_matches_legacy(&app, 90, 24);

    app.local_mode.ui.filter_query.clear();
    app.local_mode.ui.selected = 140;
    assert_local_cached_render_matches_legacy(&app, 72, 18);

    let old_language = crate::i18n::current();
    crate::i18n::set_language(crate::i18n::Language::Korean);
    assert_local_cached_render_matches_legacy(&app, 110, 24);
    crate::i18n::set_language(old_language);

    let empty = app_with_local_deck_index(Vec::new());
    assert_local_cached_render_matches_legacy(&empty, 60, 16);
}
