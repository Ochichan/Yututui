use super::*;
use crate::mousemap::{MouseAction, MouseContext, MouseGesture};
use crate::open_subsonic::{
    AccountScopeId, AlbumId, BackendId, ItemId, OpenSubsonicItemRef, ServerAlbum,
    ServerLibraryPage, ServerLibraryRow, ServerLibrarySection, ServerPlaylistAccess,
    ServerPlaylistId, ServerPlaylistLinkHealth, ServerPlaylistLinkSummary, ServerPlaylistSummary,
    ServerSong,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn server_song(id: &str) -> ServerSong {
    ServerSong {
        item: OpenSubsonicItemRef::new(
            BackendId::new("backend").unwrap(),
            AccountScopeId::new("account").unwrap(),
            ItemId::new(id).unwrap(),
        ),
        title: format!("Song {id}"),
        artist: "Artist".to_owned(),
        artists: vec!["Artist".to_owned()],
        album: None,
        album_id: None,
        album_artist: None,
        duration_secs: Some(120),
        track_number: None,
        disc_number: None,
        year: None,
        cover_art_id: None,
        content_type: None,
        suffix: None,
        starred: false,
        user_rating: None,
        play_count: None,
        played_at: None,
    }
}

fn server_album(id: &str) -> ServerAlbum {
    ServerAlbum {
        id: AlbumId::new(id).unwrap(),
        name: format!("Album {id}"),
        artist: "Artist".to_owned(),
        artist_id: None,
        song_count: Some(1),
        duration_secs: None,
        year: None,
        cover_art_id: None,
    }
}

fn server_playlist(id: &str, access: ServerPlaylistAccess) -> ServerPlaylistSummary {
    ServerPlaylistSummary {
        id: ServerPlaylistId::new(id).unwrap(),
        name: format!("Playlist {id}"),
        owner: Some("owner".to_owned()),
        song_count: Some(2),
        duration_secs: None,
        public: Some(false),
        cover_art_id: None,
        access,
        link: None,
        readonly_evidence: Some(access == ServerPlaylistAccess::ReadOnly),
        owner_evidence: Some("owner".to_owned()),
    }
}

fn linked_server_playlist(id: &str, health: ServerPlaylistLinkHealth) -> ServerPlaylistSummary {
    let mut playlist = server_playlist(id, ServerPlaylistAccess::Linked);
    playlist.link = Some(ServerPlaylistLinkSummary {
        local_playlist_id: crate::personal_state::PlaylistId::new(format!("local-{id}")).unwrap(),
        health,
    });
    playlist
}

fn server_app(rows: Vec<ServerLibraryRow>) -> App {
    let mut app = App::new(50);
    app.mode = Mode::Library;
    app.server.library.source = LibrarySource::OpenSubsonic;
    app.server.library.section = ServerLibrarySection::Songs;
    app.server.library.generation = 9;
    app.server.library.page = Some(ServerLibraryPage {
        section: ServerLibrarySection::Songs,
        rows,
        next_offset: None,
        warning: None,
    });
    app
}

fn register_server_row(app: &App, index: usize) -> (u16, u16) {
    app.register_mouse_button(
        Rect::new(1, index as u16 + 1, 20, 1),
        MouseTarget::ServerLibraryRow {
            generation: app.server.library.generation,
            index,
        },
    );
    (2, index as u16 + 1)
}

fn local_playlist_app(server_configured: bool) -> App {
    let mut app = App::new(50);
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Playlists;
    app.server.settings.summary.configured = server_configured;
    let (ledger, _) = crate::personal_state::append_external_operations(
        &app.personal_state.ledger,
        crate::personal_state::OperationOrigin::Imported,
        &[crate::personal_state::ExternalOperationInput {
            acknowledgement_id: "context-local-playlist".to_owned(),
            operation: crate::personal_state::Operation::UpsertPlaylist {
                playlist_id: crate::personal_state::PlaylistId::new("road-trip").unwrap(),
                name: "Road Trip".to_owned(),
            },
            recorded_at_unix: 1,
        }],
    )
    .unwrap();
    app.install_personal_state_runtime(ledger).unwrap();
    app.register_mouse_button(Rect::new(1, 1, 20, 1), MouseTarget::ListRow(0));
    app
}

#[test]
fn server_song_right_click_opens_only_safe_actions() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = server_app(vec![ServerLibraryRow::Song(server_song("one"))]);
    let (col, row) = register_server_row(&app, 0);

    assert!(app.on_mouse_right_click(col, row).is_empty());
    let menu = app.overlays.context_menu.as_ref().expect("server menu");
    let labels: Vec<String> = menu
        .items
        .iter()
        .map(|item| item.label(menu.target_count()))
        .collect();
    assert_eq!(
        labels,
        vec![
            "Play now",
            "Add to queue",
            "Favorite / unfavorite",
            "Add to playlist"
        ]
    );
    assert!(labels.iter().all(|label| !label.contains("Download")));

    assert!(app.activate_context_menu_item(3).is_empty());
    assert_eq!(
        app.playlist_picker
            .as_ref()
            .expect("local playlist picker")
            .songs[0]
            .title,
        "Song one"
    );
}

#[test]
fn server_right_click_obeys_disabled_enqueue_and_activate_mouse_actions() {
    let mut disabled = server_app(vec![
        ServerLibraryRow::Song(server_song("one")),
        ServerLibraryRow::Song(server_song("two")),
    ]);
    disabled.server.library.selected = 1;
    disabled
        .mousemap
        .set(
            MouseContext::Library,
            MouseGesture::RightClick,
            MouseAction::Disabled,
        )
        .unwrap();
    let (col, row) = register_server_row(&disabled, 0);
    assert!(disabled.on_mouse_right_click(col, row).is_empty());
    assert_eq!(disabled.server.library.selected, 1);
    assert!(disabled.overlays.context_menu.is_none());

    let mut enqueue = server_app(vec![ServerLibraryRow::Song(server_song("one"))]);
    enqueue
        .mousemap
        .set(
            MouseContext::Library,
            MouseGesture::RightClick,
            MouseAction::Enqueue,
        )
        .unwrap();
    let (col, row) = register_server_row(&enqueue, 0);
    assert!(!enqueue.on_mouse_right_click(col, row).is_empty());
    assert_eq!(enqueue.server.library.selected, 0);
    assert!(enqueue.overlays.context_menu.is_none());

    let mut activate = server_app(vec![ServerLibraryRow::Album(server_album("album"))]);
    activate
        .mousemap
        .set(
            MouseContext::Library,
            MouseGesture::RightClick,
            MouseAction::Activate,
        )
        .unwrap();
    let (col, row) = register_server_row(&activate, 0);
    let commands = activate.on_mouse_right_click(col, row);
    assert!(matches!(
        commands.as_slice(),
        [Cmd::ServerLibrary(ServerLibraryCommand::LoadDetail {
            target: ServerLibraryDetailTarget::Album(id),
            ..
        })] if id.as_str() == "album"
    ));
}

#[test]
fn delayed_server_menu_action_validates_generation_and_stable_identity() {
    let mut app = server_app(vec![ServerLibraryRow::Song(server_song("one"))]);
    let (col, row) = register_server_row(&app, 0);
    app.on_mouse_right_click(col, row);

    app.server.library.page.as_mut().unwrap().rows[0] =
        ServerLibraryRow::Song(server_song("replacement"));
    assert!(app.activate_context_menu_item(0).is_empty());
    assert!(app.overlays.context_menu.is_none());
    assert!(app.status.text.contains("list changed"));

    let (col, row) = register_server_row(&app, 0);
    app.on_mouse_right_click(col, row);
    app.server.library.generation += 1;
    assert!(app.activate_context_menu_item(0).is_empty());
    assert!(app.status.text.contains("list changed"));
}

#[test]
fn server_container_context_menu_exposes_activate_only() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = server_app(vec![ServerLibraryRow::Album(server_album("album"))]);
    let (col, row) = register_server_row(&app, 0);

    app.on_mouse_right_click(col, row);
    let menu = app.overlays.context_menu.as_ref().expect("server menu");
    assert_eq!(menu.items.len(), 1);
    assert_eq!(menu.items[0].label(1), "Activate");
}

#[test]
fn server_playlist_menu_offers_copy_and_only_exact_server_access_can_link() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);

    for access in [ServerPlaylistAccess::ReadOnly, ServerPlaylistAccess::Linked] {
        let mut app = server_app(vec![ServerLibraryRow::Playlist(server_playlist(
            "remote", access,
        ))]);
        let (col, row) = register_server_row(&app, 0);
        app.on_mouse_right_click(col, row);
        let menu = app.overlays.context_menu.as_ref().expect("playlist menu");
        let labels: Vec<_> = menu
            .items
            .iter()
            .map(|item| item.label(menu.target_count()))
            .collect();
        assert_eq!(labels, ["Activate", "Import a copy"]);
    }

    let mut app = server_app(vec![ServerLibraryRow::Playlist(server_playlist(
        "remote",
        ServerPlaylistAccess::Server,
    ))]);
    let (col, row) = register_server_row(&app, 0);
    app.on_mouse_right_click(col, row);
    let menu = app.overlays.context_menu.as_ref().expect("playlist menu");
    let labels: Vec<_> = menu
        .items
        .iter()
        .map(|item| item.label(menu.target_count()))
        .collect();
    assert_eq!(labels, ["Activate", "Import a copy", "Link and sync"]);

    let commands = app.activate_context_menu_item(2);
    assert!(matches!(
        commands.as_slice(),
        [Cmd::ServerLibrary(ServerLibraryCommand::PreparePlaylist {
            server_playlist_id,
            kind: ServerPlaylistPreviewKind::LinkAndSync,
            ..
        })] if server_playlist_id.as_str() == "remote"
    ));
}

#[test]
fn server_playlist_context_labels_are_localized() {
    let _guard = crate::i18n::lock_for_test();
    let original = crate::i18n::current();
    for (language, copy, link, restore, unlink_server, unlink_local, delete_both, delete_local) in [
        (
            crate::i18n::Language::English,
            "Import a copy",
            "Link and sync",
            "Restore server playlist",
            "Unlink; keep server",
            "Unlink; keep local",
            "Delete both",
            "Delete local too",
        ),
        (
            crate::i18n::Language::Korean,
            "복사본 가져오기",
            "연결 및 동기화",
            "서버 목록 복구",
            "연결 해제·서버 유지",
            "연결 해제·로컬 유지",
            "둘 다 삭제",
            "로컬 목록도 삭제",
        ),
        (
            crate::i18n::Language::Japanese,
            "コピーとしてインポート",
            "リンクして同期",
            "サーバー側を復元",
            "リンク解除・サーバー保持",
            "リンク解除・ローカル保持",
            "両方を削除",
            "ローカル側も削除",
        ),
    ] {
        crate::i18n::set_language(language);
        assert_eq!(
            ContextMenuItem::new(ContextCommand::ImportServerPlaylistCopy).label(1),
            copy
        );
        assert_eq!(
            ContextMenuItem::new(ContextCommand::LinkServerPlaylist).label(1),
            link
        );
        for (command, expected) in [
            (ContextCommand::RestoreServerPlaylist, restore),
            (ContextCommand::UnlinkKeepServerPlaylist, unlink_server),
            (ContextCommand::UnlinkKeepLocalPlaylist, unlink_local),
            (ContextCommand::DeleteLinkedPlaylistBoth, delete_both),
            (ContextCommand::DeleteLinkedPlaylistLocal, delete_local),
        ] {
            assert_eq!(ContextMenuItem::new(command).label(1), expected);
        }
    }
    crate::i18n::set_language(original);
}

#[test]
fn linked_playlist_context_choices_follow_remote_health() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);

    let mut linked = server_app(vec![ServerLibraryRow::Playlist(linked_server_playlist(
        "linked",
        ServerPlaylistLinkHealth::UpToDate,
    ))]);
    let (col, row) = register_server_row(&linked, 0);
    linked.on_mouse_right_click(col, row);
    let menu = linked.overlays.context_menu.as_ref().expect("linked menu");
    let labels: Vec<_> = menu
        .items
        .iter()
        .map(|item| item.label(menu.target_count()))
        .collect();
    assert_eq!(
        labels,
        [
            "Activate",
            "Import a copy",
            "Unlink; keep server",
            "Delete both",
        ]
    );

    let mut attention = server_app(vec![ServerLibraryRow::Playlist(linked_server_playlist(
        "attention",
        ServerPlaylistLinkHealth::NeedsAttention,
    ))]);
    let (col, row) = register_server_row(&attention, 0);
    attention.on_mouse_right_click(col, row);
    let menu = attention
        .overlays
        .context_menu
        .as_ref()
        .expect("attention menu");
    let labels: Vec<_> = menu
        .items
        .iter()
        .map(|item| item.label(menu.target_count()))
        .collect();
    assert_eq!(labels, ["Activate", "Import a copy", "Unlink; keep server"]);

    let mut missing = server_app(vec![ServerLibraryRow::Playlist(linked_server_playlist(
        "missing",
        ServerPlaylistLinkHealth::ServerMissing,
    ))]);
    let (col, row) = register_server_row(&missing, 0);
    missing.on_mouse_right_click(col, row);
    let menu = missing
        .overlays
        .context_menu
        .as_ref()
        .expect("missing menu");
    let labels: Vec<_> = menu
        .items
        .iter()
        .map(|item| item.label(menu.target_count()))
        .collect();
    assert_eq!(
        labels,
        [
            "Restore server playlist",
            "Unlink; keep local",
            "Delete local too",
        ]
    );
}

#[test]
fn local_playlist_create_link_action_requires_a_configured_server_and_opens_confirmation() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);

    let mut off = local_playlist_app(false);
    off.on_mouse_right_click(2, 1);
    let labels = off
        .overlays
        .context_menu
        .as_ref()
        .unwrap()
        .items
        .iter()
        .map(|item| item.label(1))
        .collect::<Vec<_>>();
    assert!(!labels.iter().any(|label| label.contains("server playlist")));

    let mut configured = local_playlist_app(true);
    configured.on_mouse_right_click(2, 1);
    let menu = configured.overlays.context_menu.as_ref().unwrap();
    let labels = menu
        .items
        .iter()
        .map(|item| item.label(1))
        .collect::<Vec<_>>();
    let create_index = labels
        .iter()
        .position(|label| label == "Create linked server playlist")
        .unwrap();
    assert!(
        configured
            .activate_context_menu_item(create_index)
            .is_empty()
    );
    assert!(matches!(
        configured.server.library.playlist_create.as_ref(),
        Some(crate::app::ServerPlaylistCreateModal {
            stage: crate::app::ServerPlaylistCreateStage::Confirming,
            snapshot,
            ..
        }) if snapshot.playlist_id.as_str() == "road-trip" && snapshot.name == "Road Trip"
    ));
}

#[test]
fn local_playlist_create_link_context_label_is_localized() {
    let _guard = crate::i18n::lock_for_test();
    let original = crate::i18n::current();
    for (language, expected) in [
        (
            crate::i18n::Language::English,
            "Create linked server playlist",
        ),
        (crate::i18n::Language::Korean, "서버 연결 목록 만들기"),
        (crate::i18n::Language::Japanese, "サーバー連携リストを作成"),
    ] {
        crate::i18n::set_language(language);
        assert_eq!(
            ContextMenuItem::new(ContextCommand::CreateLinkedServerPlaylist).label(1),
            expected
        );
    }
    crate::i18n::set_language(original);
}

#[test]
fn linked_playlist_recovery_choices_are_complete_at_thirty_columns_in_all_languages() {
    let _guard = crate::i18n::lock_for_test();
    let original = crate::i18n::current();
    for (language, keep_server, keep_local) in [
        (
            crate::i18n::Language::English,
            "Unlink; keep server",
            "Unlink; keep local",
        ),
        (
            crate::i18n::Language::Korean,
            "연결 해제·서버 유지",
            "연결 해제·로컬 유지",
        ),
        (
            crate::i18n::Language::Japanese,
            "リンク解除・サーバー保持",
            "リンク解除・ローカル保持",
        ),
    ] {
        crate::i18n::set_language(language);
        for (health, expected) in [
            (ServerPlaylistLinkHealth::UpToDate, keep_server),
            (ServerPlaylistLinkHealth::ServerMissing, keep_local),
        ] {
            let mut app = server_app(vec![ServerLibraryRow::Playlist(linked_server_playlist(
                "remote", health,
            ))]);
            let (col, row) = register_server_row(&app, 0);
            app.on_mouse_right_click(col, row);
            let item_count = app
                .overlays
                .context_menu
                .as_ref()
                .expect("linked playlist menu")
                .items
                .len();
            let backend = TestBackend::new(30, 30);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    crate::ui::views::context_menu::render(frame, &app, frame.area());
                })
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            let comparable: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
            let expected: String = expected.chars().filter(|ch| !ch.is_whitespace()).collect();
            assert!(
                comparable.contains(&expected),
                "{language:?} {health:?}: {text:?}"
            );
            for index in 0..item_count {
                let rect = app
                    .hits
                    .rect_of_target(MouseTarget::ContextMenuItem(index))
                    .expect("visible context-menu choice");
                assert!(rect.right() <= 30);
                assert!(rect.bottom() <= 30);
            }
        }
    }
    crate::i18n::set_language(original);
}

#[test]
fn linked_playlist_context_actions_preserve_confirmation_policy() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);

    for (health, item, action, immediate) in [
        (
            ServerPlaylistLinkHealth::UpToDate,
            2,
            ServerPlaylistRecoveryAction::UnlinkKeepServer,
            true,
        ),
        (
            ServerPlaylistLinkHealth::UpToDate,
            3,
            ServerPlaylistRecoveryAction::DeleteBoth,
            false,
        ),
        (
            ServerPlaylistLinkHealth::ServerMissing,
            1,
            ServerPlaylistRecoveryAction::UnlinkKeepLocal,
            true,
        ),
        (
            ServerPlaylistLinkHealth::ServerMissing,
            2,
            ServerPlaylistRecoveryAction::DeleteLocal,
            false,
        ),
    ] {
        let mut app = server_app(vec![ServerLibraryRow::Playlist(linked_server_playlist(
            "remote", health,
        ))]);
        let (col, row) = register_server_row(&app, 0);
        app.on_mouse_right_click(col, row);
        let commands = app.activate_context_menu_item(item);
        let modal = app
            .server
            .library
            .playlist_recovery
            .as_ref()
            .expect("recovery modal");
        assert_eq!(modal.action, action);
        assert_eq!(
            modal.stage,
            if immediate {
                ServerPlaylistRecoveryStage::Applying
            } else {
                ServerPlaylistRecoveryStage::Confirming
            }
        );
        if immediate {
            assert!(matches!(
                commands.as_slice(),
                [Cmd::ServerLibrary(ServerLibraryCommand::RecoverPlaylist {
                    action: command_action,
                    ..
                })] if *command_action == action
            ));
        } else {
            assert!(commands.is_empty());
        }
    }
}

#[test]
fn delayed_missing_playlist_recovery_rejects_a_changed_link_health() {
    let mut app = server_app(vec![ServerLibraryRow::Playlist(linked_server_playlist(
        "remote",
        ServerPlaylistLinkHealth::ServerMissing,
    ))]);
    let (col, row) = register_server_row(&app, 0);
    app.on_mouse_right_click(col, row);
    let Some(ServerLibraryRow::Playlist(playlist)) = app
        .server
        .library
        .page
        .as_mut()
        .and_then(|page| page.rows.first_mut())
    else {
        panic!("playlist row");
    };
    playlist.link.as_mut().unwrap().health = ServerPlaylistLinkHealth::UpToDate;

    assert!(app.activate_context_menu_item(0).is_empty());
    assert!(app.server.library.playlist_recovery.is_none());
    assert!(app.status.text.contains("list changed"));
}

#[test]
fn publish_to_server_is_complete_at_thirty_columns_in_all_languages() {
    let _guard = crate::i18n::lock_for_test();
    let original = crate::i18n::current();
    for (language, expected) in [
        (crate::i18n::Language::English, "Copy to music server"),
        (crate::i18n::Language::Korean, "음악 서버로 복사"),
        (crate::i18n::Language::Japanese, "音楽サーバーへコピー"),
    ] {
        crate::i18n::set_language(language);
        let label = ContextMenuItem::new(ContextCommand::PublishToServer).label(1);
        assert_eq!(label, expected);
        // The menu renders inside a bordered box, so the label has to fit well inside 30 columns
        // or the narrow layout truncates it into something the user cannot act on.
        let width: usize = unicode_width::UnicodeWidthStr::width(label.as_str());
        assert!(
            width <= 26,
            "{language:?} label is {width} columns: {label}"
        );
    }
    crate::i18n::set_language(original);
}

/// Put one downloaded track in the Library and register its row for a right-click.
fn downloads_app_with_one_track() -> App {
    let mut app = App::new(50);
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Downloads;
    let mut song = crate::api::Song::local_file(std::path::PathBuf::from(
        "/tmp/Verify Track [dQw4w9WgXcQ].m4a",
    ));
    song.video_id = "dQw4w9WgXcQ".to_owned();
    song.title = "Verify Track".to_owned();
    song.local_path = Some(std::path::PathBuf::from(
        "/tmp/Verify Track [dQw4w9WgXcQ].m4a",
    ));
    app.library_ui.downloaded = vec![song];
    app
}

fn right_click_first_library_row(app: &mut App) {
    app.register_mouse_button(Rect::new(1, 1, 20, 1), MouseTarget::ListRow(0));
    app.on_mouse_right_click(2, 1);
}

#[test]
fn publish_is_offered_for_a_downloaded_track_once_a_server_is_connected() {
    let mut app = downloads_app_with_one_track();
    app.server.settings.summary.configured = true;

    right_click_first_library_row(&mut app);

    let items = &app.overlays.context_menu.as_ref().expect("menu").items;
    assert!(
        items.contains(&ContextMenuItem::new(ContextCommand::PublishToServer)),
        "{items:?}"
    );
}

#[test]
fn publish_is_hidden_without_a_connected_server() {
    let mut app = downloads_app_with_one_track();
    app.server.settings.summary.configured = false;

    right_click_first_library_row(&mut app);

    // Offering it here would promise something that can only fail once the user picks it.
    let items = &app.overlays.context_menu.as_ref().expect("menu").items;
    assert!(
        !items.contains(&ContextMenuItem::new(ContextCommand::PublishToServer)),
        "{items:?}"
    );
}

#[test]
fn publish_is_hidden_for_a_track_that_was_never_downloaded() {
    let mut app = downloads_app_with_one_track();
    app.server.settings.summary.configured = true;
    app.library_ui.downloaded[0].local_path = None;

    right_click_first_library_row(&mut app);

    let items = &app.overlays.context_menu.as_ref().expect("menu").items;
    assert!(
        !items.contains(&ContextMenuItem::new(ContextCommand::PublishToServer)),
        "{items:?}"
    );
}

#[test]
fn publish_is_hidden_for_a_multi_row_selection() {
    let mut app = downloads_app_with_one_track();
    app.server.settings.summary.configured = true;
    let second = app.library_ui.downloaded[0].clone();
    app.library_ui.downloaded.push(second);
    app.library_ui.picked = [0, 1].into_iter().collect();
    app.library_ui.selected = 1;
    app.library_ui.anchor = 0;
    app.register_mouse_button(Rect::new(1, 1, 20, 1), MouseTarget::ListRow(0));
    app.on_mouse_right_click(2, 1);

    // Publishing is one track at a time; a bulk copy into someone's music library is a
    // different decision and is not what this entry does.
    let items = &app.overlays.context_menu.as_ref().expect("menu").items;
    assert!(
        !items.contains(&ContextMenuItem::new(ContextCommand::PublishToServer)),
        "{items:?}"
    );
}
