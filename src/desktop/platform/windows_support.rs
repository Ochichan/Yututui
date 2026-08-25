use tray_icon::menu::{IsMenuItem, Submenu};

use crate::desktop::menu_model::MenuSubmenuId;

// Status carries the poll snapshot inline; events are dispatched one at a time
// (never queued in bulk), so boxing would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum UserEvent {
    Status {
        generation: u64,
        update: PollUpdate,
        restart_fallback: bool,
    },
    ShowMenu,
    /// Show the mini player, optionally anchored near a tray click (physical px).
    ShowMiniPlayer(Option<(f64, f64)>),
    Panel {
        generation: u64,
        request: PanelRequest,
    },
    PanelResult {
        generation: u64,
        id: u64,
        error: Option<DesktopCommandError>,
    },
    Menu(MenuAction),
    /// The first daemon menu slot; whether it means Start or Stop depends on the
    /// state at click time, so the decision lives in the app, not in the menu id.
    DaemonPrimary,
    Refresh,
    StartupChanged {
        error: Option<String>,
    },
    /// Live connection state from the persistent v8 gateway thread.
    Gateway(gateway::GatewayEvent),
    Activation(ActivationIntent),
    Quit,
}

impl WindowsTrayApp {
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

    fn send_remote(&self, command: RemoteCommand) {
        if let Some(gateway) = &self.gateway
            && gateway.is_online()
        {
            if let Err(error) = gateway.send_remote(command) {
                report_command_error(format_args!(
                    "could not queue desktop command: {}",
                    error.code()
                ));
            }
            return;
        }
        if !matches!(
            self.last_conn,
            gateway::ConnState::Offline { ref reason } if reason == "no_v8"
        ) {
            report_command_error("player is offline");
            return;
        }
        let proxy = self.proxy.clone();
        self.submit_command(move |generation| async move {
            if let Err(e) = control::send_remote(command).await {
                report_command_error(e);
            }
            let update = status::poll_once_exclusive().await;
            let _ = proxy.send_event(UserEvent::Status {
                generation,
                update,
                restart_fallback: true,
            });
        });
    }

    fn send_panel_remote(&mut self, command: RemoteCommand, request: Option<(u64, u64)>) {
        if let Some(gateway) = &self.gateway
            && gateway.is_online()
        {
            match gateway.send_remote(command) {
                Ok(gateway_id) => {
                    if let Some(request) = request {
                        self.panel_gateway_requests.insert(gateway_id, request);
                    }
                }
                Err(error) => {
                    let error = DesktopCommandError::new(
                        error.code(),
                        format!("Could not queue command: {}", error.code()),
                        true,
                    );
                    self.complete_panel_request(request, Some(&error));
                }
            }
            return;
        }
        if !matches!(
            self.last_conn,
            gateway::ConnState::Offline { ref reason } if reason == "no_v8"
        ) {
            let error = DesktopCommandError::new("offline", "Player is offline", true);
            self.complete_panel_request(request, Some(&error));
            return;
        }
        let proxy = self.proxy.clone();
        self.submit_panel_command(request, move |generation| async move {
            let result = control::send_remote(command).await;
            if let Some((page_generation, id)) = request {
                let error = result.as_ref().err().map(panel::command_error_from_control);
                let _ = proxy.send_event(UserEvent::PanelResult {
                    generation: page_generation,
                    id,
                    error,
                });
            }
            let update = status::poll_once_exclusive().await;
            let _ = proxy.send_event(UserEvent::Status {
                generation,
                update,
                restart_fallback: true,
            });
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
            let command_error = DesktopCommandError::new(
                match error {
                    SubmitError::Full => "backpressure",
                    SubmitError::Closed => "closed",
                },
                error.to_string(),
                matches!(error, SubmitError::Full),
            );
            self.complete_panel_request(request, Some(&command_error));
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
            let command_error = DesktopCommandError::new(
                match error {
                    SubmitError::Full => "backpressure",
                    SubmitError::Closed => "closed",
                },
                error.to_string(),
                matches!(error, SubmitError::Full),
            );
            self.complete_panel_request(request, Some(&command_error));
        }
    }

    fn shutdown(&mut self) {
        self.persist_geometry();
        self.stop_polling();
        // Tear down the gateway session cleanly (drops the handle → signals shutdown).
        self.gateway.take();
        // Closing the bounded lane drains already accepted work in order, then
        // joins its single runtime thread before native teardown completes.
        self.command_executor.take();
        // Flush the log before tao exits the process. (muda/tray-icon handlers live
        // in set-once cells — unsetting them here was always a no-op, so we don't.)
        drop(self.log_guard.take());
    }
}

fn tray_click_coalesce() -> Duration {
    // Match the user's Control Panel setting so Click + DoubleClick notifications collapse
    // into exactly one panel toggle without guessing a fixed interval.
    Duration::from_millis(u64::from(system_double_click_time()))
}

struct WindowsMenu {
    // The tray root must be a `Menu`. A `Submenu` root registers muda's window
    // subclass with a `MenuChild` payload, and muda's WM_NCACTIVATE/WM_NCPAINT arm
    // blindly casts that payload back to `Menu` — type confusion that access-violates
    // the process the moment the menu opens (SetForegroundWindow) or is dismissed.
    // See tray-icon#115 / tauri#11363.
    root: Menu,
    track: MenuItem,
    state: MenuItem,
    startup: CheckMenuItem,
    // Single handle for the first daemon slot. Its action flips between StartDaemon
    // and StopDaemon with the state, so an action-keyed handle map would strand it:
    // whichever variant was absent at build time could never appear afterwards.
    daemon_primary: MenuItem,
    actions: HashMap<MenuAction, MenuItem>,
    submenus: HashMap<MenuSubmenuId, Submenu>,
}

impl WindowsMenu {
    fn new(state: &TrayState) -> Result<Self, Box<dyn Error>> {
        let model = menu_model::build_menu(state);
        let root = Menu::new();
        let mut handles = NativeMenuHandles::default();
        append_native_entries(NativeMenuParent::Root(&root), &model.entries, &mut handles)?;

        Ok(Self {
            root,
            track: handles.track.ok_or("missing track menu item")?,
            state: handles.state.ok_or("missing state menu item")?,
            startup: handles.startup.ok_or("missing startup menu item")?,
            daemon_primary: handles.daemon_primary.ok_or("missing daemon menu item")?,
            actions: handles.actions,
            submenus: handles.submenus,
        })
    }

    fn apply_state(&self, state: &TrayState) {
        let model = menu_model::build_menu(state);
        for (id, handle) in &self.submenus {
            if let Some(submenu) = model.submenu(*id) {
                handle.set_text(&submenu.label);
                handle.set_enabled(submenu.enabled);
            }
        }
        let mut disabled_index = 0usize;
        model.visit_items(|item| match item.action {
            Some(MenuAction::ToggleStartup) => {
                // Label only. Checked/enabled refresh on StartupChanged events —
                // reading the registry on every status poll was wasted IO.
                self.startup.set_text(&item.label);
            }
            Some(action) if is_daemon_primary(Some(action)) => {
                self.daemon_primary.set_text(&item.label);
                self.daemon_primary.set_enabled(item.enabled);
            }
            Some(action) => {
                if let Some(handle) = self.actions.get(&action) {
                    handle.set_text(&item.label);
                    handle.set_enabled(item.enabled);
                }
            }
            None => {
                match disabled_index {
                    0 => {
                        self.track.set_text(&item.label);
                        self.track.set_enabled(item.enabled);
                    }
                    1 => {
                        self.state.set_text(&item.label);
                        self.state.set_enabled(item.enabled);
                    }
                    _ => {}
                }
                disabled_index += 1;
            }
        });
    }

    fn apply_startup_status(&self) {
        let (checked, enabled) = startup_menu_state();
        self.startup.set_checked(checked);
        self.startup.set_enabled(enabled);
    }

    fn set_startup_pending(&self, pending: bool) {
        if pending {
            self.startup.set_enabled(false);
        } else {
            self.apply_startup_status();
        }
    }
}

#[derive(Default)]
struct NativeMenuHandles {
    track: Option<MenuItem>,
    state: Option<MenuItem>,
    startup: Option<CheckMenuItem>,
    daemon_primary: Option<MenuItem>,
    actions: HashMap<MenuAction, MenuItem>,
    submenus: HashMap<MenuSubmenuId, Submenu>,
    disabled_index: usize,
}

#[derive(Clone, Copy)]
enum NativeMenuParent<'a> {
    Root(&'a Menu),
    Submenu(&'a Submenu),
}

impl NativeMenuParent<'_> {
    fn append(self, item: &dyn IsMenuItem) -> tray_icon::menu::Result<()> {
        match self {
            Self::Root(menu) => menu.append(item),
            Self::Submenu(menu) => menu.append(item),
        }
    }
}

fn append_native_entries(
    parent: NativeMenuParent<'_>,
    entries: &[MenuEntry],
    handles: &mut NativeMenuHandles,
) -> tray_icon::menu::Result<()> {
    for entry in entries {
        match entry {
            MenuEntry::Separator => parent.append(&PredefinedMenuItem::separator())?,
            MenuEntry::Submenu(model) => {
                let menu = Submenu::with_id(submenu_menu_id(model.id), &model.label, model.enabled);
                append_native_entries(NativeMenuParent::Submenu(&menu), &model.entries, handles)?;
                parent.append(&menu)?;
                handles.submenus.insert(model.id, menu);
            }
            MenuEntry::Item(item) => append_native_item(parent, item, handles)?,
        }
    }
    Ok(())
}

fn append_native_item(
    parent: NativeMenuParent<'_>,
    item: &ModelItem,
    handles: &mut NativeMenuHandles,
) -> tray_icon::menu::Result<()> {
    if item.action == Some(MenuAction::ToggleStartup) {
        let menu_item = make_startup_menu_item(item);
        parent.append(&menu_item)?;
        handles.startup = Some(menu_item);
        return Ok(());
    }
    if is_daemon_primary(item.action) {
        let menu_item = MenuItem::with_id(
            MenuId::new(DAEMON_PRIMARY_ID),
            &item.label,
            item.enabled,
            None,
        );
        parent.append(&menu_item)?;
        handles.daemon_primary = Some(menu_item);
        return Ok(());
    }

    let menu_item = make_menu_item(item, handles.disabled_index);
    if let Some(action) = item.action {
        handles.actions.insert(action, menu_item.clone());
    } else {
        match handles.disabled_index {
            0 => handles.track = Some(menu_item.clone()),
            1 => handles.state = Some(menu_item.clone()),
            _ => {}
        }
        handles.disabled_index += 1;
    }
    parent.append(&menu_item)
}

fn make_menu_item(item: &ModelItem, disabled_index: usize) -> MenuItem {
    if let Some(action) = item.action {
        MenuItem::with_id(action_menu_id(action), &item.label, item.enabled, None)
    } else {
        MenuItem::with_id(
            MenuId::new(format!("yututray:disabled:{disabled_index}")),
            &item.label,
            item.enabled,
            None,
        )
    }
}

fn make_startup_menu_item(item: &ModelItem) -> CheckMenuItem {
    let (checked, enabled) = startup_menu_state();
    CheckMenuItem::with_id(
        action_menu_id(MenuAction::ToggleStartup),
        &item.label,
        item.enabled && enabled,
        checked,
        None,
    )
}

const DAEMON_PRIMARY_ID: &str = "yututray:daemon_primary";

fn submenu_menu_id(id: MenuSubmenuId) -> MenuId {
    MenuId::new(format!("yututray:submenu:{}", id.slug()))
}

fn is_daemon_primary(action: Option<MenuAction>) -> bool {
    matches!(
        action,
        Some(MenuAction::StartDaemon) | Some(MenuAction::StopDaemon)
    )
}

fn user_event_from_menu_id(id: &MenuId) -> Option<UserEvent> {
    if id.as_ref() == DAEMON_PRIMARY_ID {
        return Some(UserEvent::DaemonPrimary);
    }
    let action = action_from_menu_id(id)?;
    Some(match action {
        MenuAction::ShowMiniPlayer => UserEvent::ShowMiniPlayer(None),
        MenuAction::Refresh => UserEvent::Refresh,
        MenuAction::QuitTray => UserEvent::Quit,
        other => UserEvent::Menu(other),
    })
}

fn action_menu_id(action: MenuAction) -> MenuId {
    MenuId::new(format!("yututray:{}", action_slug(action)))
}

fn action_from_menu_id(id: &MenuId) -> Option<MenuAction> {
    let slug = id.as_ref().strip_prefix("yututray:")?;
    match slug {
        "play_pause" => Some(MenuAction::PlayPause),
        "next" => Some(MenuAction::Next),
        "previous" => Some(MenuAction::Previous),
        "seek_back" => Some(MenuAction::SeekBack),
        "seek_forward" => Some(MenuAction::SeekForward),
        "volume_up" => Some(MenuAction::VolumeUp),
        "volume_down" => Some(MenuAction::VolumeDown),
        "toggle_streaming" => Some(MenuAction::ToggleStreaming),
        "start_daemon" => Some(MenuAction::StartDaemon),
        "resume_daemon" => Some(MenuAction::ResumeDaemon),
        "stop_daemon" => Some(MenuAction::StopDaemon),
        "show_mini_player" => Some(MenuAction::ShowMiniPlayer),
        "open_tui" => Some(MenuAction::OpenTui),
        "refresh" => Some(MenuAction::Refresh),
        "toggle_startup" => Some(MenuAction::ToggleStartup),
        "quit_player" => Some(MenuAction::QuitPlayer),
        "quit_tray" => Some(MenuAction::QuitTray),
        _ => None,
    }
}

fn action_slug(action: MenuAction) -> &'static str {
    match action {
        MenuAction::PlayPause => "play_pause",
        MenuAction::Next => "next",
        MenuAction::Previous => "previous",
        MenuAction::SeekBack => "seek_back",
        MenuAction::SeekForward => "seek_forward",
        MenuAction::VolumeUp => "volume_up",
        MenuAction::VolumeDown => "volume_down",
        MenuAction::ToggleStreaming => "toggle_streaming",
        MenuAction::StartDaemon => "start_daemon",
        MenuAction::ResumeDaemon => "resume_daemon",
        MenuAction::StopDaemon => "stop_daemon",
        MenuAction::ShowMiniPlayer => "show_mini_player",
        MenuAction::OpenTui => "open_tui",
        MenuAction::Refresh => "refresh",
        MenuAction::ToggleStartup => "toggle_startup",
        MenuAction::QuitPlayer => "quit_player",
        MenuAction::QuitTray => "quit_tray",
    }
}

fn toggle_startup_entry() -> Result<(), startup::StartupError> {
    match startup::status()? {
        StartupStatus::Enabled { .. } => startup::uninstall(),
        StartupStatus::Disabled => startup::install().map(|_| ()),
        StartupStatus::Unsupported => Err(startup::StartupError::Unsupported),
    }
}

fn startup_menu_state() -> (bool, bool) {
    match startup::status() {
        Ok(StartupStatus::Enabled { .. }) => (true, true),
        Ok(StartupStatus::Disabled) => (false, true),
        Ok(StartupStatus::Unsupported) => (false, false),
        Err(e) => {
            report_error(e);
            (false, true)
        }
    }
}

fn tooltip_for_state(state: &TrayState) -> String {
    let text = match state {
        TrayState::Disconnected { .. } => "YuTuTui! is not running".to_string(),
        TrayState::Connected(status) => {
            if status.owner_mode == InstanceMode::Daemon
                && status.total == 0
                && status.title.as_deref().unwrap_or_default().is_empty()
            {
                return "YuTuTui! daemon idle".to_string();
            }
            let prefix = if status.paused { "Paused" } else { "Playing" };
            match (status.artist.as_deref(), status.title.as_deref()) {
                (Some(artist), Some(title)) if !artist.is_empty() && !title.is_empty() => {
                    format!("{prefix}: {artist} - {title}")
                }
                (_, Some(title)) if !title.is_empty() => format!("{prefix}: {title}"),
                _ => "YuTuTui!: nothing playing".to_string(),
            }
        }
    };
    truncate_tooltip(text)
}

fn truncate_tooltip(text: String) -> String {
    // The shell's szTip buffer holds 128 UTF-16 units including the NUL, so budget
    // in UTF-16 units, not chars — a title full of non-BMP codepoints (emoji, rare
    // CJK) would otherwise overflow into a garbled tooltip.
    const MAX_UTF16: usize = 124;
    if text.encode_utf16().count() <= MAX_UTF16 {
        return text;
    }
    let mut used = 0usize;
    let mut short = String::new();
    for ch in text.chars() {
        let units = ch.len_utf16();
        if used + units > MAX_UTF16 - 3 {
            break;
        }
        used += units;
        short.push(ch);
    }
    short.push_str("...");
    short
}

fn set_app_user_model_id() {
    let app_id = APP_ID
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let hr = set_process_app_id(&app_id);
    if hr < 0 {
        report_error(format_args!(
            "could not set AppUserModelID (HRESULT {hr:#x})"
        ));
    }
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

fn app_icon() -> Result<Icon, Box<dyn Error>> {
    let entry = find_ico_entry(ICO_BYTES, 32)
        .or_else(|| find_ico_entry(ICO_BYTES, 48))
        .or_else(|| find_ico_entry(ICO_BYTES, 256))
        .ok_or("yututui.ico has no usable tray icon image")?;
    let image = image::load_from_memory(&ICO_BYTES[entry.offset..entry.end()])?.to_rgba8();
    let (width, height) = image.dimensions();
    Ok(Icon::from_rgba(image.into_raw(), width, height)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IcoEntry {
    width: u32,
    height: u32,
    bytes: usize,
    offset: usize,
}

impl IcoEntry {
    fn end(self) -> usize {
        self.offset + self.bytes
    }
}

fn ico_entries(bytes: &[u8]) -> Result<Vec<IcoEntry>, &'static str> {
    if bytes.len() < 6 {
        return Err("ICO header is too short");
    }
    let reserved = u16::from_le_bytes([bytes[0], bytes[1]]);
    let kind = u16::from_le_bytes([bytes[2], bytes[3]]);
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    if reserved != 0 || kind != 1 || count == 0 {
        return Err("invalid ICO header");
    }
    let dir_len = 6usize
        .checked_add(count.checked_mul(16).ok_or("ICO entry count overflows")?)
        .ok_or("ICO directory overflows")?;
    if bytes.len() < dir_len {
        return Err("ICO directory is truncated");
    }

    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let base = 6 + index * 16;
        let width = if bytes[base] == 0 {
            256
        } else {
            bytes[base] as u32
        };
        let height = if bytes[base + 1] == 0 {
            256
        } else {
            bytes[base + 1] as u32
        };
        let bytes_len = u32::from_le_bytes([
            bytes[base + 8],
            bytes[base + 9],
            bytes[base + 10],
            bytes[base + 11],
        ]) as usize;
        let offset = u32::from_le_bytes([
            bytes[base + 12],
            bytes[base + 13],
            bytes[base + 14],
            bytes[base + 15],
        ]) as usize;
        let end = offset
            .checked_add(bytes_len)
            .ok_or("ICO image range overflows")?;
        if end > bytes.len() {
            return Err("ICO image range is outside the file");
        }
        entries.push(IcoEntry {
            width,
            height,
            bytes: bytes_len,
            offset,
        });
    }
    Ok(entries)
}

fn find_ico_entry(bytes: &[u8], size: u32) -> Option<IcoEntry> {
    ico_entries(bytes)
        .ok()?
        .into_iter()
        .find(|entry| entry.width == size && entry.height == size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::proto::{InstanceMode, StatusSnapshot};

    #[test]
    fn menu_ids_round_trip_actions() {
        for action in [
            MenuAction::PlayPause,
            MenuAction::Next,
            MenuAction::Previous,
            MenuAction::SeekBack,
            MenuAction::SeekForward,
            MenuAction::VolumeUp,
            MenuAction::VolumeDown,
            MenuAction::ToggleStreaming,
            MenuAction::StartDaemon,
            MenuAction::ResumeDaemon,
            MenuAction::StopDaemon,
            MenuAction::ShowMiniPlayer,
            MenuAction::OpenTui,
            MenuAction::Refresh,
            MenuAction::ToggleStartup,
            MenuAction::QuitPlayer,
            MenuAction::QuitTray,
        ] {
            assert_eq!(action_from_menu_id(&action_menu_id(action)), Some(action));
        }
    }

    #[test]
    fn tooltip_tracks_playback_state_and_shell_limit() {
        let state = TrayState::Connected(StatusSnapshot {
            title: Some("Song".to_string()),
            artist: Some("Artist".to_string()),
            paused: true,
            volume: 60,
            position: 1,
            total: 2,
            streaming: false,
            owner_mode: InstanceMode::StandaloneTui,
            settings: Default::default(),
            queue: Vec::new(),
            shuffle: false,
            repeat: Default::default(),
            elapsed_ms: None,
            duration_ms: None,
            is_live: false,
            queue_rev: None,
            track_id: None,
            position_epoch: 0,
            artwork: None,
            personal_sync: None,
            sleep_remaining_secs: None,
        });
        assert_eq!(tooltip_for_state(&state), "Paused: Artist - Song");
        let idle_daemon = TrayState::Connected(StatusSnapshot {
            title: None,
            artist: None,
            paused: true,
            volume: 80,
            position: 0,
            total: 0,
            streaming: false,
            owner_mode: InstanceMode::Daemon,
            settings: Default::default(),
            queue: Vec::new(),
            shuffle: false,
            repeat: Default::default(),
            elapsed_ms: None,
            duration_ms: None,
            is_live: false,
            queue_rev: None,
            track_id: None,
            position_epoch: 0,
            artwork: None,
            personal_sync: None,
            sleep_remaining_secs: None,
        });
        assert_eq!(tooltip_for_state(&idle_daemon), "YuTuTui! daemon idle");
        assert_eq!(
            tooltip_for_state(&TrayState::disconnected(false)),
            "YuTuTui! is not running"
        );

        let long = tooltip_for_state(&TrayState::Connected(StatusSnapshot {
            title: Some("T".repeat(200)),
            artist: Some("Artist".to_string()),
            paused: false,
            volume: 60,
            position: 1,
            total: 2,
            streaming: false,
            owner_mode: InstanceMode::StandaloneTui,
            settings: Default::default(),
            queue: Vec::new(),
            shuffle: false,
            repeat: Default::default(),
            elapsed_ms: None,
            duration_ms: None,
            is_live: false,
            queue_rev: None,
            track_id: None,
            position_epoch: 0,
            artwork: None,
            personal_sync: None,
            sleep_remaining_secs: None,
        }));
        assert_eq!(long.chars().count(), 124);
        assert!(long.ends_with("..."));
    }

    #[test]
    fn daemon_primary_menu_id_dispatches_state_dependent_event() {
        assert!(matches!(
            user_event_from_menu_id(&MenuId::new(DAEMON_PRIMARY_ID)),
            Some(UserEvent::DaemonPrimary)
        ));
    }

    #[test]
    fn submenu_ids_are_stable_and_semantic() {
        assert_eq!(
            submenu_menu_id(MenuSubmenuId::Session).as_ref(),
            "yututray:submenu:session"
        );
        assert_eq!(
            submenu_menu_id(MenuSubmenuId::Playback).as_ref(),
            "yututray:submenu:playback"
        );
    }

    #[test]
    fn tooltip_truncates_by_utf16_units() {
        let long = truncate_tooltip("\u{1F3B5}".repeat(100));
        assert!(long.encode_utf16().count() <= 124);
        assert!(long.ends_with("..."));
    }

    #[test]
    fn ico_resource_has_tray_and_shortcut_sizes() {
        let sizes = ico_entries(ICO_BYTES)
            .unwrap()
            .into_iter()
            .map(|entry| entry.width)
            .collect::<Vec<_>>();
        for expected in [16, 20, 24, 32, 40, 48, 64, 128, 256] {
            assert!(
                sizes.contains(&expected),
                "missing {expected}x{expected} icon in yututui.ico; found {sizes:?}"
            );
        }
    }

    #[test]
    fn app_icon_decodes_from_ico() {
        assert!(app_icon().is_ok());
    }
}
