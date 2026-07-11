// Windows application of OS-neutral `DesktopApp` decisions.

impl WindowsTrayApp {
    fn dispatch_desktop_event(
        &mut self,
        event: DesktopEvent,
        target: &EventLoopWindowTarget<UserEvent>,
        mini_anchor: Option<(f64, f64)>,
    ) -> Option<FrontendReplay> {
        let transition = self.desktop_app.handle_event(event);
        self.apply_desktop_transition(transition, target, mini_anchor)
    }

    fn apply_desktop_transition(
        &mut self,
        transition: DesktopTransition,
        target: &EventLoopWindowTarget<UserEvent>,
        mut mini_anchor: Option<(f64, f64)>,
    ) -> Option<FrontendReplay> {
        let DesktopTransition { effects, replay } = transition;
        let mut failed_window = None;
        for effect in effects {
            match effect {
                DesktopEffect::EnsureTray => debug_assert!(self.tray.is_some()),
                DesktopEffect::EnsureMiniSurface => {
                    if !self.ensure_panel(target, mini_anchor.take()) {
                        failed_window = Some(WindowKind::Mini);
                    }
                }
                DesktopEffect::ShowMini => {
                    if !self.show_panel(target, mini_anchor.take()) {
                        failed_window = Some(WindowKind::Mini);
                    }
                }
                DesktopEffect::HideMini => self.hide_panel(),
                DesktopEffect::EnsureMainSurface => {
                    if !self.ensure_main_window(target) {
                        failed_window = Some(WindowKind::Main);
                    }
                }
                DesktopEffect::ShowMain => {
                    if !self.show_main_window(target) {
                        failed_window = Some(WindowKind::Main);
                    }
                }
                DesktopEffect::HideMain => self.hide_main_window(),
                // Windows activation is per-window: the mini is a TOOLWINDOW and the main is
                // the sole APPWINDOW. There is no process-wide policy call to make here.
                DesktopEffect::UseRegularActivation | DesktopEffect::UseAccessoryActivation => {}
                DesktopEffect::ApplyWindowPolicy {
                    kind: WindowKind::Mini,
                    policy,
                } => {
                    if let Some(panel) = &self.panel {
                        panel.set_pinned(policy.always_on_top);
                    }
                }
                DesktopEffect::ApplyWindowPolicy {
                    kind: WindowKind::Main,
                    ..
                } => {}
            }
        }
        if let Some(kind) = failed_window {
            let correction = self
                .desktop_app
                .handle_event(DesktopEvent::WindowEvent(DesktopWindowEvent::Hidden(kind)));
            let _ = self.apply_desktop_transition(correction, target, None);
        }
        replay.map(|(_, replay)| replay)
    }

    fn ensure_main_window(&mut self, target: &EventLoopWindowTarget<UserEvent>) -> bool {
        self.main_teardown_at = None;
        if let Some(main) = &self.main_window {
            return main.ensure_surface();
        }
        let boot = boot_json(&self.last_conn);
        let proxy = self.proxy.clone();
        match MainWindow::create(target, boot, None, move |generation, body| {
            let _ = proxy.send_event(UserEvent::Main(MainRequest::Ipc { generation, body }));
        }) {
            Ok(main) => {
                self.main_window = Some(main);
                true
            }
            Err(e) => {
                crate::desktop::native_error::show(
                    "YuTuTui! Desktop",
                    &format!("Could not create the main window: {e}"),
                );
                report_error(e);
                false
            }
        }
    }

    fn show_main_window(&mut self, target: &EventLoopWindowTarget<UserEvent>) -> bool {
        self.ensure_main_window(target) && self.main_window.as_ref().is_some_and(MainWindow::show)
    }
}
