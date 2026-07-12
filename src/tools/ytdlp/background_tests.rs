use super::*;

#[tokio::test]
async fn disabled_maintainer_returns_explicit_noop_owner() {
    let cfg = ToolsConfig {
        ytdlp_managed: Some(false),
        ..ToolsConfig::default()
    };

    let task = spawn_maintainer(cfg, |_| {});

    assert!(!task.is_enabled());
}
