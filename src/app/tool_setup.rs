use crossterm::event::{KeyCode, KeyEvent};

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSetupContext {
    Startup,
    Downloads,
}

#[derive(Debug, Clone)]
pub struct ToolSetupPrompt {
    pub context: ToolSetupContext,
    pub missing: Vec<&'static str>,
    pub command: Option<String>,
    needs_player_restart: bool,
    ytdlp_was_on_path: bool,
}

impl ToolSetupPrompt {
    fn new(context: ToolSetupContext, missing: Vec<&'static str>) -> Self {
        let command = crate::deps::install_command(&missing);
        let needs_player_restart = missing.contains(&"mpv");
        let ytdlp_was_on_path = crate::deps::on_path("yt-dlp");
        Self {
            context,
            missing,
            command,
            needs_player_restart,
            ytdlp_was_on_path,
        }
    }
}

impl App {
    /// Runtime bootstrapping enables host tool probes after selection is initialized. Keeping
    /// construction pure makes reducer/render tests independent of the developer machine's PATH.
    pub fn enable_runtime_tool_checks(&mut self) {
        self.runtime_tool_checks = true;
    }

    pub fn show_tool_setup(&mut self, context: ToolSetupContext, missing: Vec<&'static str>) {
        if missing.is_empty() {
            return;
        }
        self.tool_setup = Some(ToolSetupPrompt::new(context, missing));
        self.dirty = true;
    }

    fn tool_setup_missing(context: ToolSetupContext) -> Vec<&'static str> {
        match context {
            ToolSetupContext::Startup => crate::deps::missing(),
            ToolSetupContext::Downloads => crate::deps::missing_for_downloads(),
        }
    }

    pub(in crate::app) fn on_key_tool_setup(&mut self, key: KeyEvent) -> Vec<Cmd> {
        match key.code {
            KeyCode::Char('c' | 'C') => self.activate_tool_setup(MouseTarget::ToolSetupCopy),
            KeyCode::Char('g' | 'G') => self.activate_tool_setup(MouseTarget::ToolSetupGuide),
            KeyCode::Char('r' | 'R') | KeyCode::Enter => {
                self.activate_tool_setup(MouseTarget::ToolSetupRetry)
            }
            KeyCode::Esc => self.activate_tool_setup(MouseTarget::ToolSetupLater),
            _ => Vec::new(),
        }
    }

    pub(in crate::app) fn activate_tool_setup(&mut self, target: MouseTarget) -> Vec<Cmd> {
        let Some(prompt) = self.tool_setup.clone() else {
            return Vec::new();
        };
        match target {
            MouseTarget::ToolSetupCopy => {
                let copied = prompt.command.as_deref().is_some_and(copy_to_clipboard);
                self.set_status_info(if copied {
                    t!("Install command copied", "설치 명령을 복사했습니다")
                } else {
                    t!("Clipboard unavailable", "클립보드를 사용할 수 없습니다")
                });
            }
            MouseTarget::ToolSetupGuide => {
                open_in_browser(crate::deps::setup_guide_url());
                self.set_status_info(t!("Setup guide opened", "설치 안내를 열었습니다"));
            }
            MouseTarget::ToolSetupRetry => {
                let mut missing = Self::tool_setup_missing(prompt.context);
                // Tool selection is intentionally cached, but a package installed after this
                // card opened can now be executed by its bare PATH name. Only relax a stale
                // selection error when yt-dlp genuinely appeared since the prompt was created.
                if !prompt.ytdlp_was_on_path && crate::deps::on_path("yt-dlp") {
                    missing.retain(|tool| *tool != "yt-dlp");
                }
                if missing.is_empty() {
                    self.tool_setup = None;
                    self.set_status_info(t!(
                        "Playback tools are ready",
                        "재생 도구가 준비되었습니다"
                    ));
                    self.arm_beginner_onboarding();
                    if prompt.context == ToolSetupContext::Downloads {
                        return self.pump_downloads();
                    }
                    if prompt.needs_player_restart {
                        return vec![Cmd::PlayerControl(PlayerControl::Restart {
                            restore: Vec::new(),
                        })];
                    }
                } else {
                    let mut next = ToolSetupPrompt::new(prompt.context, missing);
                    next.needs_player_restart |= prompt.needs_player_restart;
                    next.ytdlp_was_on_path |= prompt.ytdlp_was_on_path;
                    self.tool_setup = Some(next);
                    self.set_status_error(t!(
                        "Tools are still missing — install them, then check again",
                        "아직 도구가 없습니다 — 설치한 뒤 다시 확인하세요"
                    ));
                }
            }
            MouseTarget::ToolSetupLater => {
                if prompt.context == ToolSetupContext::Downloads {
                    self.downloads.pending.clear();
                }
                self.tool_setup = None;
                self.arm_beginner_onboarding();
                self.dirty = true;
            }
            _ => return Vec::new(),
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn setup_card_is_keyboard_modal_and_can_be_deferred() {
        let mut app = App::new(50);
        app.show_tool_setup(ToolSetupContext::Startup, vec!["mpv"]);
        let cmds = app.on_key_tool_setup(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(cmds.is_empty());
        assert!(app.tool_setup.is_none());
    }

    #[test]
    fn setup_card_renders_buttons_and_survives_tiny_frames() {
        let mut app = App::new(50);
        app.show_tool_setup(ToolSetupContext::Startup, vec!["mpv", "yt-dlp"]);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &app))
            .unwrap();
        assert!(
            app.hits
                .regions()
                .iter()
                .any(|region| region.target == MouseTarget::ToolSetupRetry)
        );
        let backend = TestBackend::new(20, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &app))
            .unwrap();
    }
}
