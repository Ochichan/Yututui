use super::*;

#[tokio::test]
async fn build_ctx_uses_anonymous_ytm_for_local_playlist_without_cookie() {
    let spec = file_spec(TransferDest::LocalPlaylist {
        name: Some("Imported".to_owned()),
    });
    let cfg = config_without_cookie();

    let ctx = build_ctx(&spec, &cfg).await.unwrap();

    assert!(matches!(
        ctx.ytm,
        Some(crate::api::ytmusic::YtMusicApi::Anonymous)
    ));
    assert!(ctx.spotify.is_none());
}

#[tokio::test]
async fn build_ctx_requires_cookie_for_account_writes() {
    let spec = file_spec(TransferDest::YtmLikes);
    let cfg = config_without_cookie();

    let err = match build_ctx(&spec, &cfg).await {
        Ok(_) => panic!("account write without a cookie should fail"),
        Err(err) => err,
    };

    assert!(err.contains("YouTube Music cookie"));
}

#[test]
fn run_reports_usage_without_starting_runtime_for_parse_failures() {
    assert_eq!(run(&[]), EXIT_USAGE);
    assert_eq!(run(&["--help".to_owned()]), EXIT_OK);
    assert_eq!(run(&["unknown".to_owned()]), EXIT_USAGE);
    assert_eq!(
        run(&["list".to_owned(), "unknown-side".to_owned()]),
        EXIT_USAGE
    );
    assert_eq!(
        run(&["import".to_owned(), "not-a-source".to_owned()]),
        EXIT_USAGE
    );
}
