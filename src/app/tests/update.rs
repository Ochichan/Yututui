use super::*;

fn update_status(available: bool, first_seen: bool) -> crate::update::UpdateStatus {
    crate::update::UpdateStatus {
        current: "1.0.0".to_owned(),
        latest: "v1.0.1".to_owned(),
        available,
        first_seen,
        method: crate::update::InstallMethod::Cargo,
    }
}

#[test]
fn first_seen_update_emits_desktop_notification_and_marks_seen() {
    let mut app = App::new(100);
    let status = update_status(true, true);

    let cmds = app.update(Msg::UpdateChecked(status));

    assert!(
        app.overlays
            .update_status
            .as_ref()
            .is_some_and(|s| { s.latest == "v1.0.1" && s.available && s.first_seen })
    );
    assert!(app.status.text.contains("Update available"));
    assert!(cmds.iter().any(|cmd| matches!(
        cmd,
        Cmd::DesktopNotify { title, body }
            if title.contains("1.0.1")
                && body.contains("Latest: v1.0.1")
                && body.contains("cargo install yututui --force")
    )));
    assert!(cmds.iter().any(|cmd| matches!(
        cmd,
        Cmd::UpdateSeen { tag } if tag == "v1.0.1"
    )));
}

#[test]
fn repeated_or_unavailable_update_only_stores_status() {
    for status in [update_status(true, false), update_status(false, true)] {
        let mut app = App::new(100);

        let cmds = app.update(Msg::UpdateChecked(status));

        assert!(app.overlays.update_status.is_some());
        assert!(
            cmds.iter()
                .all(|cmd| { !matches!(cmd, Cmd::DesktopNotify { .. } | Cmd::UpdateSeen { .. }) })
        );
    }
}
