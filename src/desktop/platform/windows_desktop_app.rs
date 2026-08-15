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
                DesktopEffect::ApplyWindowPolicy {
                    kind: WindowKind::Mini,
                    policy,
                } => {
                    if let Some(panel) = &self.panel {
                        panel.set_pinned(policy.always_on_top);
                    }
                }
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
}
