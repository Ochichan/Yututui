// Kept as an include so support helpers and tests can use the backend's private event-loop
// types without widening the platform module's public surface.

impl MacTrayApp {
    fn reconcile_window_placements(&mut self) {
        let mut persist = false;
        if let Some(panel) = &self.panel {
            let changed = panel.reconcile_live_placement();
            persist |= changed && panel.is_pinned();
        }
        if persist {
            self.geometry_dirty_at = Some(Instant::now());
        }
    }

    fn handle_scale_factor_changed(&mut self, window_id: tao::window::WindowId) {
        if let Some(panel) = &self.panel
            && panel.window_id() == window_id
        {
            panel.reconcile_live_placement();
            if panel.is_pinned() {
                self.geometry_dirty_at = Some(Instant::now());
            }
        }
    }

    fn open_tui(&self) {
        let proxy = self.proxy.clone();
        self.submit_lifecycle_command(move |_| async move {
            match tokio::task::spawn_blocking(launch::open_tui).await {
                Ok(Ok(_)) => {
                    let _ = proxy.send_event(UserEvent::Refresh);
                }
                Ok(Err(error)) => report_command_error(error),
                Err(error) => {
                    report_command_error(format_args!("terminal launch worker failed: {error}"));
                }
            }
        });
    }

    fn open_tui_for_panel(&self, request: Option<(u64, u64)>) {
        let proxy = self.proxy.clone();
        self.submit_panel_lifecycle_command(request, move |_| async move {
            let error = match tokio::task::spawn_blocking(launch::open_tui).await {
                Ok(Ok(_)) => {
                    let _ = proxy.send_event(UserEvent::Refresh);
                    None
                }
                Ok(Err(error)) => Some(error.to_string()),
                Err(error) => Some(format!("terminal launch worker failed: {error}")),
            };
            if let Some((generation, id)) = request {
                let error =
                    error.map(|message| DesktopCommandError::new("launch_failed", message, true));
                let _ = proxy.send_event(UserEvent::PanelResult {
                    generation,
                    id,
                    error,
                });
            }
        });
    }

    fn submit_command<F, Fut>(&self, make_future: F)
    where
        F: FnOnce(u64) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + 'static,
    {
        let result = self
            .command_executor
            .as_ref()
            .ok_or(SubmitError::Closed)
            .and_then(|executor| {
                executor.submit_with_generation(Arc::clone(&self.poll_generation), make_future)
            });
        if let Err(error) = result {
            report_command_error(format_args!("could not queue desktop command: {error}"));
        }
    }

    fn submit_panel_command<F, Fut>(&self, request: Option<(u64, u64)>, make_future: F)
    where
        F: FnOnce(u64) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + 'static,
    {
        let result = self
            .command_executor
            .as_ref()
            .ok_or(SubmitError::Closed)
            .and_then(|executor| {
                executor.submit_with_generation(Arc::clone(&self.poll_generation), make_future)
            });
        if let Err(error) = result {
            report_error(format_args!("could not queue panel command: {error}"));
            self.complete_panel_request(request, Some(&submission_error(error)));
        }
    }

    fn submit_lifecycle_command<F, Fut>(&self, make_future: F)
    where
        F: FnOnce(u64) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + 'static,
    {
        let result = self
            .command_executor
            .as_ref()
            .ok_or(SubmitError::Closed)
            .and_then(|executor| {
                executor.submit_lifecycle_with_generation(
                    Arc::clone(&self.poll_generation),
                    make_future,
                )
            });
        if let Err(error) = result {
            report_command_error(format_args!("could not queue lifecycle command: {error}"));
        }
    }

    fn submit_panel_lifecycle_command<F, Fut>(&self, request: Option<(u64, u64)>, make_future: F)
    where
        F: FnOnce(u64) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + 'static,
    {
        let result = self
            .command_executor
            .as_ref()
            .ok_or(SubmitError::Closed)
            .and_then(|executor| {
                executor.submit_lifecycle_with_generation(
                    Arc::clone(&self.poll_generation),
                    make_future,
                )
            });
        if let Err(error) = result {
            report_error(format_args!(
                "could not queue panel lifecycle command: {error}"
            ));
            self.complete_panel_request(request, Some(&submission_error(error)));
        }
    }

    fn shutdown(&mut self) {
        self.persist_geometry();
        self.stop_polling();
        self.gateway.take();
        self.command_executor.take();
        drop(self.log_guard.take());
    }
}

fn submission_error(error: SubmitError) -> DesktopCommandError {
    DesktopCommandError::new(
        match error {
            SubmitError::Full => "backpressure",
            SubmitError::Closed => "closed",
        },
        error.to_string(),
        matches!(error, SubmitError::Full),
    )
}

fn init_file_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let dir = crate::desktop::persistence::cache_dir()?;
    if let Err(e) = crate::util::safe_fs::ensure_private_dir(&dir) {
        report_error(format_args!(
            "could not create log directory {}: {e}",
            dir.display()
        ));
        return None;
    }

    let guard = crate::logging::init_named(&dir, "yututray.log");
    if guard.is_some() {
        tracing::info!(
            target: "ytt_tray",
            path = %dir.join("yututray.log").display(),
            "yututray logging initialized"
        );
    }
    guard
}

fn install_tray_panic_hook() {
    // panic = "abort" kills the process before tracing-appender's worker thread can
    // flush, so mirror every panic synchronously into a plain file next to the log.
    let panic_log =
        crate::desktop::persistence::cache_dir().map(|dir| dir.join("yututray-panic.jsonl"));
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        tracing::error!(target: "ytt_tray", panic = %info, "yututray panic");
        if let Some(path) = &panic_log {
            let unix_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or_default();
            let record = serde_json::json!({
                "unix": unix_secs,
                "process": "yututray",
                "panic": info.to_string(),
            });
            let _ = crate::util::safe_fs::append_private_jsonl(path, &record.to_string());
        }
        previous(info);
    }));
}

fn report_error(message: impl std::fmt::Display) {
    let message = message.to_string();
    tracing::error!(target: "ytt_tray", "yututray: {message}");
    #[cfg(debug_assertions)]
    eprintln!("yututray: {message}");
}

fn report_command_error(message: impl std::fmt::Display) {
    let message = message.to_string();
    report_error(&message);
    crate::desktop::native_error::notify("YuTuTui! command failed", &message);
}

fn template_icon() -> Result<Icon, tray_icon::BadIcon> {
    const SIZE: u32 = 18;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let center = (SIZE as f32 - 1.0) / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            let idx = ((y * SIZE + x) * 4) as usize;
            let ring = (6.1..=8.0).contains(&dist);
            let stem = (8..=10).contains(&x) && (3..=14).contains(&y);
            let dot = dist <= 2.1;
            if ring || stem || dot {
                rgba[idx] = 0;
                rgba[idx + 1] = 0;
                rgba[idx + 2] = 0;
                rgba[idx + 3] = 255;
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_icon_is_valid_rgba() {
        assert!(template_icon().is_ok());
    }
}
