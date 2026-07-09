use super::*;

#[test]
fn accounts_tab_places_spotify_import_mode_before_import_action() {
    let fields = SettingsTab::Accounts.fields();
    assert_eq!(
        fields,
        vec![
            Field::LastfmEnabled,
            Field::LastfmConnect,
            Field::LastfmLoveSync,
            Field::ListenBrainzEnabled,
            Field::ListenBrainzToken,
            Field::SpotifyClientId,
            Field::SpotifyRedirectPort,
            Field::SpotifyConnect,
            Field::SpotifyImportMode,
            Field::SpotifyImport,
            Field::ScrobbleLocalFiles,
        ]
    );
    assert_eq!(Field::SpotifyImportMode.kind(), FieldKind::Select);
}
