use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{
    App, BeginnerStep, LibraryTab, Mode, MouseTarget, OnboardingAction, ToolSetupContext,
};
use crate::keymap::{Action, KeyContext};
use crate::settings::{Field, SettingsTab};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons::{self, Seg};

struct CoachCopy {
    heading: String,
    body: String,
    primary: String,
}

pub fn render_beginner_coach(frame: &mut Frame, app: &App, area: Rect) {
    if !app.beginner_coach_visible() || area.is_empty() {
        return;
    }
    let mini = crate::ui::layout::tier(area) == crate::ui::layout::UiTier::Mini;
    let confirming = app.onboarding.skip_confirmation();
    if area.width < 8 || area.height < 4 {
        app.register_mouse_button(area, MouseTarget::Onboarding(OnboardingAction::Noop));
        frame.render_widget(
            Paragraph::new(t!(
                "Beginner Mode · resize",
                "비기너 모드 · 창을 키워 주세요"
            ))
            .style(app.theme.style(R::Accent).add_modifier(Modifier::BOLD)),
            area,
        );
        return;
    }

    let copy = coach_copy(app, mini, confirming);
    let max_width = area.width.saturating_sub(2).clamp(6, 68);
    let body_width = max_width.saturating_sub(4).max(1) as usize;
    let mut body = crate::ui::text::wrap_to_width(&copy.body, body_width);
    let max_height = area.height.saturating_sub(2).max(3);
    let height = u16::try_from(body.len())
        .unwrap_or(u16::MAX)
        .saturating_add(7)
        .min(max_height);
    let body_rows = usize::from(height.saturating_sub(7));
    let body_truncated = body.len() > body_rows.max(1);
    body.truncate(body_rows.max(1));
    if body_truncated && let Some(last) = body.last_mut() {
        *last = with_ellipsis(last, body_width);
    }
    let anchor = (!mini && !confirming)
        .then(|| coach_anchor(app))
        .flatten()
        .and_then(|target| app.hits.rect_of_target(target));
    let popup = place_coach(area, anchor, max_width, height);

    // Skip confirmation is the one modal coach state: seal the whole frame. Otherwise only the
    // card body blocks clicks and the instructed surface remains interactive around it.
    app.register_mouse_button(
        if confirming { area } else { popup },
        MouseTarget::Onboarding(OnboardingAction::Noop),
    );
    crate::ui::render_popup_background(frame, app, popup);
    let title = if confirming {
        t!(" Skip Beginner Mode? ", " 비기너 모드를 건너뛸까요? ").to_owned()
    } else {
        format!(
            " {} · {} {}/{} ",
            t!("Beginner Mode", "비기너 모드"),
            t!("Step", "단계"),
            app.onboarding.step().number(),
            BeginnerStep::COUNT
        )
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.is_empty() {
        return;
    }

    let heading = Rect {
        height: 1.min(inner.height),
        ..inner
    };
    frame.render_widget(
        Paragraph::new(copy.heading).style(app.theme.style(R::Accent).add_modifier(Modifier::BOLD)),
        heading,
    );
    let body_area = Rect {
        y: inner.y.saturating_add(2),
        height: u16::try_from(body.len())
            .unwrap_or(u16::MAX)
            .min(inner.height.saturating_sub(4)),
        ..inner
    };
    let lines: Vec<Line<'_>> = body.into_iter().map(Line::from).collect();
    frame.render_widget(Paragraph::new(lines), body_area);

    let focused = app.onboarding.guide_focused_for(mini);
    let hint = if confirming {
        t!("Esc cancel · ←/→ · Enter", "Esc 취소 · ←/→ · Enter")
    } else if mini && focused {
        t!("Esc: app · Enter: Skip", "Esc: 앱 · Enter: 건너뛰기")
    } else if mini {
        t!(
            "F6: Skip · resize to resume",
            "F6: 건너뛰기 · 창을 키우면 계속"
        )
    } else if focused {
        t!(
            "F6/Esc: app · ←/→ choose · Enter",
            "F6/Esc: 앱 · ←/→ 선택 · Enter"
        )
    } else {
        t!(
            "Use the app normally · F6 guide controls",
            "앱을 직접 사용해 보세요 · F6 안내 버튼"
        )
    };
    let hint_area = Rect {
        y: inner.bottom().saturating_sub(2),
        height: 1.min(inner.height),
        ..inner
    };
    frame.render_widget(
        Paragraph::new(with_ellipsis(hint, hint_area.width as usize))
            .alignment(Alignment::Center)
            .style(app.theme.style(R::TextMuted)),
        hint_area,
    );
    render_coach_buttons(frame, app, inner, &copy.primary, mini, confirming);
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

fn coach_copy(app: &App, mini: bool, confirming: bool) -> CoachCopy {
    if confirming {
        return CoachCopy {
            heading: t!(
                "You can restart it from Settings.",
                "설정에서 언제든 다시 시작할 수 있어요."
            )
            .to_owned(),
            body: t!(
                "Skipping turns Beginner Mode off and resets this tour to Welcome.",
                "건너뛰면 비기너 모드가 꺼지고 이 투어는 처음 단계로 초기화돼요."
            )
            .to_owned(),
            primary: String::new(),
        };
    }
    if mini {
        return CoachCopy {
            heading: t!(
                "Make the terminal a little bigger",
                "터미널 창을 조금 더 키워 주세요"
            )
            .to_owned(),
            body: format!(
                "{} · {} 32×14 · {} {}/{}",
                t!("The tour is paused", "튜토리얼이 잠시 멈췄어요"),
                t!("minimum", "최소 크기"),
                t!("Step", "단계"),
                app.onboarding.step().number(),
                BeginnerStep::COUNT
            ),
            primary: String::new(),
        };
    }

    let key = |context, action| {
        app.keymap
            .label_for_display(context, action, app.retro_mode())
    };
    let reached = app.onboarding.target_reached();
    match app.onboarding.step() {
        BeginnerStep::Welcome => CoachCopy {
            heading: t!("Welcome to YuTuTui!", "YuTuTui!에 오신 걸 환영해요").to_owned(),
            body: t!(
                "Take a short, safe tour of every screen. Your place is saved, and the tour never requires playback, downloads, sign-in, or a network search.",
                "모든 화면을 짧고 안전하게 둘러봐요. 진행 위치는 저장되며 재생, 다운로드, 로그인, 실제 검색은 요구하지 않아요."
            )
            .to_owned(),
            primary: t!("Start tour", "투어 시작").to_owned(),
        },
        BeginnerStep::NavigationHelp => CoachCopy {
            heading: t!("Navigation and Help", "화면 이동과 도움말").to_owned(),
            body: format!(
                "{} {}. {}",
                t!(
                    "The top row switches between Player, Search, Library, Settings, and DJ Gem. Open the complete live key guide with",
                    "위쪽 메뉴에서 플레이어, 검색, 라이브러리, 설정, DJ Gem으로 이동할 수 있어요. 현재 단축키 도움말은"
                ),
                key(KeyContext::Global, Action::ToggleHelp),
                t!("Mouse controls work too.", "키로 열 수 있고 마우스도 사용할 수 있어요.")
            ),
            primary: t!("Open Help", "도움말 열기").to_owned(),
        },
        BeginnerStep::Search => CoachCopy {
            heading: t!("Find music safely", "안전하게 음악 찾기").to_owned(),
            body: if reached {
                t!(
                    "Choose a source, type in the query box, and inspect results or filters. Enter submits; double-click plays; right-click opens safe row actions. You do not need to search now.",
                    "검색 소스를 고르고 입력 칸과 결과 필터를 살펴보세요. Enter는 검색, 더블클릭은 재생, 우클릭은 안전한 행 메뉴예요. 지금 실제 검색할 필요는 없어요."
                )
                .to_owned()
            } else {
                format!(
                    "{} {} {}",
                    t!("Open Search with", "검색 화면은"),
                    key(KeyContext::Player, Action::OpenSearch),
                    t!("or the top navigation.", "키 또는 위쪽 메뉴로 열 수 있어요.")
                )
            },
            primary: if reached {
                t!("Continue", "계속").to_owned()
            } else {
                t!("Open Search", "검색 열기").to_owned()
            },
        },
        BeginnerStep::Player => CoachCopy {
            heading: t!("Player controls", "플레이어 조작").to_owned(),
            body: if reached {
                t!(
                    "Transport, seek, Volume, Queue, Rating, Shuffle, Repeat, and Equalizer live here. Beginner Mode spells each control and state out. Autoplay and Repeat cannot be enabled together.",
                    "재생, 탐색, 볼륨, 대기열, 평가, 셔플, 반복, 이퀄라이저를 여기서 조작해요. 비기너 모드는 이름과 상태를 풀어서 보여 줘요. 자동재생과 반복은 함께 켤 수 없어요."
                )
                .to_owned()
            } else {
                format!(
                    "{} {}.",
                    t!("Return to Player/Home with", "플레이어 홈으로 돌아가는 키는"),
                    key(KeyContext::Global, Action::Home)
                )
            },
            primary: if reached {
                t!("Continue", "계속").to_owned()
            } else {
                t!("Open Player", "플레이어 열기").to_owned()
            },
        },
        BeginnerStep::Library => CoachCopy {
            heading: t!("Your Library", "내 라이브러리").to_owned(),
            body: if reached {
                t!(
                    "Browse All, Favorites, History, Downloads, and Playlists. Single-click selects; double-click activates; right-click opens the row menu. Nothing needs to be downloaded now.",
                    "전체, 즐겨찾기, 기록, 다운로드, 플레이리스트를 둘러보세요. 한 번 클릭은 선택, 더블클릭은 실행, 우클릭은 행 메뉴예요. 지금 다운로드할 필요는 없어요."
                )
                .to_owned()
            } else {
                format!(
                    "{} {} {}",
                    t!("Open Library with", "라이브러리는"),
                    key(KeyContext::Player, Action::OpenLibrary),
                    t!("or the top navigation.", "키 또는 위쪽 메뉴로 열 수 있어요.")
                )
            },
            primary: if reached {
                t!("Continue", "계속").to_owned()
            } else {
                t!("Open Library", "라이브러리 열기").to_owned()
            },
        },
        BeginnerStep::DjGem => CoachCopy {
            heading: t!("Meet DJ Gem", "DJ Gem 만나기").to_owned(),
            body: if reached {
                t!(
                    "DJ Gem can chat about music and help curate autoplay when an API key is configured. It is optional; Radio and Local Deck are optional advanced modes too.",
                    "DJ Gem은 API 키가 있을 때 음악 대화와 자동재생 큐레이팅을 도와줘요. 선택 기능이며 라디오와 Local Deck도 선택형 고급 모드예요."
                )
                .to_owned()
            } else {
                let context = match app.mode {
                    Mode::Player => Some(KeyContext::Player),
                    Mode::Library
                        if app.effective_library_tab() == LibraryTab::Playlists =>
                    {
                        Some(KeyContext::Playlists)
                    }
                    Mode::Library => Some(KeyContext::Library),
                    _ => None,
                };
                context.map_or_else(
                    || {
                        t!(
                            "Use the top navigation or the guide action below.",
                            "위쪽 메뉴나 아래 안내 버튼을 사용하세요."
                        )
                        .to_owned()
                    },
                    |context| {
                        format!(
                            "{} {} {}",
                            t!("Open DJ Gem with", "DJ Gem은"),
                            key(context, Action::OpenAi),
                            t!(
                                "or the top navigation.",
                                "키 또는 위쪽 메뉴로 열 수 있어요."
                            )
                        )
                    },
                )
            },
            primary: if reached {
                t!("Continue", "계속").to_owned()
            } else {
                t!("Open DJ Gem", "DJ Gem 열기").to_owned()
            },
        },
        BeginnerStep::Settings => settings_copy(app, &key),
        BeginnerStep::Finish => finish_copy(app, &key),
    }
}

fn settings_copy(app: &App, key: &impl Fn(KeyContext, Action) -> String) -> CoachCopy {
    let tab = app.settings.as_ref().map(|settings| settings.tab);
    let visited = app.onboarding.settings_tab_visited();
    let (body, primary) = match tab {
        None => (
            t!(
                "Choose Settings in the top navigation, or use the guide action below.",
                "위쪽 메뉴에서 설정을 선택하거나 아래 안내 버튼을 사용하세요."
            )
            .to_owned(),
            t!("Open Settings", "설정 열기").to_owned(),
        ),
        Some(SettingsTab::General) if !visited => (
            format!(
                "{} {}.",
                t!(
                    "Explore General, Playback, Hotkeys, Graphics, DJ Gem, and Accounts. Switch to any other tab with",
                    "일반, 재생, 핫키, 그래픽, DJ Gem, 계정 탭을 둘러봐요. 다른 탭으로 이동하는 키는"
                ),
                key(KeyContext::Settings, Action::FocusNext)
            ),
            t!("Try another tab", "다른 탭 보기").to_owned(),
        ),
        Some(SettingsTab::General) => (
            format!(
                "{} {}.",
                t!(
                    "Great. Arrows change values and Enter activates. Save and close Settings with",
                    "좋아요. 화살표로 값을 바꾸고 Enter로 실행해요. 설정을 저장하고 닫는 키는"
                ),
                key(KeyContext::Settings, Action::SettingsCancel)
            ),
            t!("Continue", "계속").to_owned(),
        ),
        Some(_) if visited => (
            t!(
                "Each tab groups related options. Return to General to finish the tour.",
                "각 탭에는 관련 설정이 모여 있어요. 투어를 마치려면 일반 탭으로 돌아가세요."
            )
            .to_owned(),
            t!("Back to General", "일반으로 돌아가기").to_owned(),
        ),
        Some(_) => (
            t!(
                "You switched a settings tab. Return to General when you are ready.",
                "설정 탭을 직접 바꿨어요. 준비되면 일반 탭으로 돌아가세요."
            )
            .to_owned(),
            t!("Back to General", "일반으로 돌아가기").to_owned(),
        ),
    };
    CoachCopy {
        heading: t!("Settings tour", "설정 둘러보기").to_owned(),
        body,
        primary,
    }
}

fn finish_copy(app: &App, key: &impl Fn(KeyContext, Action) -> String) -> CoachCopy {
    let in_settings = app.mode == Mode::Settings;
    let enabled = app.beginner_mode();
    let (body, primary) = if !in_settings {
        (
            t!(
                "Open Settings one last time. The Beginner Mode row is at the top of General.",
                "마지막으로 설정을 열어 주세요. 비기너 모드는 일반 탭 맨 위에 있어요."
            )
            .to_owned(),
            t!("Open Beginner Mode", "비기너 모드 열기").to_owned(),
        )
    } else if enabled {
        (
            t!(
                "Turn Beginner Mode Off yourself. This keeps completion intentional; you can enable it again whenever you want.",
                "비기너 모드를 직접 꺼 주세요. 그래야 온보딩 완료가 분명해지며, 원하면 언제든 다시 켤 수 있어요."
            )
            .to_owned(),
            t!("Turn off Beginner Mode", "비기너 모드 끄기").to_owned(),
        )
    } else {
        (
            format!(
                "{} {}.",
                t!(
                    "All set. Save and close Settings with",
                    "모두 끝났어요. 설정을 저장하고 닫는 키는"
                ),
                key(KeyContext::Settings, Action::SettingsCancel)
            ),
            t!("Save and finish", "저장하고 완료").to_owned(),
        )
    };
    CoachCopy {
        heading: t!("Finish Beginner Mode", "비기너 모드 마치기").to_owned(),
        body,
        primary,
    }
}

fn coach_anchor(app: &App) -> Option<MouseTarget> {
    let reached = app.onboarding.target_reached();
    match app.onboarding.step() {
        BeginnerStep::Welcome => None,
        BeginnerStep::NavigationHelp => Some(MouseTarget::Global(Action::ToggleHelp)),
        BeginnerStep::Search => Some(if reached {
            MouseTarget::SearchInput
        } else {
            MouseTarget::Nav(Mode::Search)
        }),
        BeginnerStep::Player => Some(if reached {
            MouseTarget::VolumeArea
        } else {
            MouseTarget::Nav(Mode::Player)
        }),
        BeginnerStep::Library => Some(if reached {
            MouseTarget::LibraryTab(app.effective_library_tab())
        } else {
            MouseTarget::Nav(Mode::Library)
        }),
        BeginnerStep::DjGem => Some(if reached {
            MouseTarget::AiInput
        } else {
            MouseTarget::Nav(Mode::Ai)
        }),
        BeginnerStep::Settings => match app.settings.as_ref().map(|settings| settings.tab) {
            None => Some(MouseTarget::Nav(Mode::Settings)),
            Some(SettingsTab::General) if !app.onboarding.settings_tab_visited() => {
                Some(MouseTarget::SettingsTab(SettingsTab::Playback.index()))
            }
            _ => Some(MouseTarget::SettingsTab(SettingsTab::General.index())),
        },
        BeginnerStep::Finish => {
            let row = app.settings.as_ref().and_then(|settings| {
                settings
                    .fields()
                    .iter()
                    .position(|field| *field == Field::BeginnerMode)
            });
            row.map_or(Some(MouseTarget::Nav(Mode::Settings)), |row| {
                Some(MouseTarget::SettingsChange { row, delta: 1 })
            })
        }
    }
}

fn render_coach_buttons(
    frame: &mut Frame,
    app: &App,
    inner: Rect,
    primary: &str,
    mini: bool,
    confirming: bool,
) {
    let selected = app.onboarding.selected_action();
    let focused = app.onboarding.guide_focused_for(mini);
    let mut owned: Vec<(OnboardingAction, String)> = if confirming {
        vec![
            (
                OnboardingAction::CancelSkip,
                t!("Cancel", "취소").to_owned(),
            ),
            (
                OnboardingAction::ConfirmSkip,
                t!("Skip tour", "투어 건너뛰기").to_owned(),
            ),
        ]
    } else if mini {
        vec![(OnboardingAction::Skip, t!("Skip", "건너뛰기").to_owned())]
    } else {
        vec![
            (OnboardingAction::Back, t!("Back", "이전").to_owned()),
            (OnboardingAction::Primary, primary.to_owned()),
            (OnboardingAction::Skip, t!("Skip", "건너뛰기").to_owned()),
        ]
    };
    let compact = inner.width < 44;
    if compact
        && let Some((_, label)) = owned
            .iter_mut()
            .find(|(action, _)| *action == OnboardingAction::Primary)
    {
        *label = compact_primary_label(app).to_owned();
    }
    for (action, label) in &mut owned {
        let marker = if app.retro_mode() {
            ('>', '<')
        } else {
            ('›', '‹')
        };
        let action_selected = focused && (selected == *action || (mini && !confirming));
        if action_selected {
            *label = if compact {
                format!("{}{label}{}", marker.0, marker.1)
            } else {
                format!("{} {label} {}", marker.0, marker.1)
            };
        } else if !compact {
            *label = format!("  {label}  ");
        }
    }
    let mut segments = Vec::with_capacity(owned.len().saturating_mul(2));
    for (index, (action, label)) in owned.iter().enumerate() {
        if index > 0 {
            segments.push(Seg::label(" "));
        }
        segments.push(Seg::button(MouseTarget::Onboarding(*action), label));
    }
    let row = Rect {
        y: inner.bottom().saturating_sub(1),
        height: 1.min(inner.height),
        ..inner
    };
    buttons::render_segments(
        frame,
        app,
        row,
        &segments,
        crate::ui::confirm_button_style(app),
        crate::ui::confirm_gap_style(app),
        Alignment::Center,
    );
}

fn compact_primary_label(app: &App) -> &'static str {
    let next = t!("Next", "다음");
    match app.onboarding.step() {
        BeginnerStep::Welcome => t!("Start", "시작"),
        BeginnerStep::NavigationHelp => t!("Help", "도움말"),
        BeginnerStep::Search => {
            if app.onboarding.target_reached() {
                next
            } else {
                t!("Search", "검색")
            }
        }
        BeginnerStep::Player => {
            if app.onboarding.target_reached() {
                next
            } else {
                t!("Player", "플레이어")
            }
        }
        BeginnerStep::Library => {
            if app.onboarding.target_reached() {
                next
            } else {
                t!("Library", "라이브러리")
            }
        }
        BeginnerStep::DjGem => {
            if app.onboarding.target_reached() {
                next
            } else {
                "DJ Gem"
            }
        }
        BeginnerStep::Settings => {
            let tab = app.settings.as_ref().map(|settings| settings.tab);
            if app.mode != Mode::Settings {
                t!("Settings", "설정")
            } else if !app.onboarding.settings_tab_visited() {
                t!("Try tab", "탭 보기")
            } else if tab != Some(SettingsTab::General) {
                t!("General", "일반")
            } else {
                next
            }
        }
        BeginnerStep::Finish => {
            if app.mode != Mode::Settings {
                t!("Settings", "설정")
            } else if app.beginner_mode() {
                t!("Turn off", "끄기")
            } else {
                t!("Save", "저장")
            }
        }
    }
}

fn with_ellipsis(text: &str, width: usize) -> String {
    if buttons::text_width(text) <= u16::try_from(width).unwrap_or(u16::MAX) {
        return text.to_owned();
    }
    let mut truncated = crate::ui::text::truncate_to_width(text, width.saturating_sub(1));
    truncated.push('…');
    truncated
}

fn place_coach(area: Rect, anchor: Option<Rect>, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let max_x = area.right().saturating_sub(width);
    let max_y = area.bottom().saturating_sub(height);
    let clamp_x = |x: u16| x.clamp(area.x, max_x.max(area.x));
    let clamp_y = |y: u16| y.clamp(area.y, max_y.max(area.y));
    if let Some(anchor) = anchor {
        let centered_x = clamp_x(
            anchor
                .x
                .saturating_add(anchor.width / 2)
                .saturating_sub(width / 2),
        );
        let centered_y = clamp_y(
            anchor
                .y
                .saturating_add(anchor.height / 2)
                .saturating_sub(height / 2),
        );
        let below = anchor.bottom().saturating_add(1);
        if below.saturating_add(height) <= area.bottom() {
            return Rect::new(centered_x, below, width, height);
        }
        if anchor.y >= area.y.saturating_add(height).saturating_add(1) {
            return Rect::new(centered_x, anchor.y - height - 1, width, height);
        }
        let right = anchor.right().saturating_add(1);
        if right.saturating_add(width) <= area.right() {
            return Rect::new(right, centered_y, width, height);
        }
        if anchor.x >= area.x.saturating_add(width).saturating_add(1) {
            return Rect::new(anchor.x - width - 1, centered_y, width, height);
        }
    }
    Rect::new(
        clamp_x(area.x + area.width.saturating_sub(width) / 2),
        clamp_y(area.bottom().saturating_sub(height).saturating_sub(1)),
        width,
        height,
    )
}

pub fn render_tool_setup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(prompt) = app.tool_setup.as_ref() else {
        return;
    };
    if area.is_empty() {
        return;
    }
    if area.width < 32 || area.height < 7 {
        frame.render_widget(
            Paragraph::new(t!(
                "Playback tools required · R check · G guide · Esc later",
                "재생 도구 설치 필요 · R 확인 · G 안내 · Esc 나중에"
            ))
            .style(app.theme.style(R::Accent).add_modifier(Modifier::BOLD))
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let width = area.width.saturating_sub(4).min(76);
    let height = area.height.saturating_sub(2).min(12);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    crate::ui::render_popup_background(frame, app, popup);
    let title = match prompt.context {
        ToolSetupContext::Startup => {
            t!(" Playback tools required ", " 재생 도구 설치가 필요합니다 ")
        }
        ToolSetupContext::Downloads => {
            t!(" Download tool required ", " 다운로드 도구가 필요합니다 ")
        }
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.is_empty() {
        return;
    }

    let missing = prompt.missing.join(", ");
    let reason = match prompt.context {
        ToolSetupContext::Startup => t!(
            "YuTuTui! needs these tools to search and play music.",
            "YuTuTui!가 음악을 검색하고 재생하려면 이 도구가 필요합니다."
        ),
        ToolSetupContext::Downloads => t!(
            "Install these tools to finish the queued download.",
            "대기 중인 다운로드를 마치려면 이 도구를 설치하세요."
        ),
    };
    let command = prompt.command.as_deref().unwrap_or(t!(
        "Use the setup guide for this system.",
        "이 시스템에서는 설치 안내를 이용하세요."
    ));
    let body = vec![
        Line::from(reason),
        Line::from(vec![
            Span::styled(
                format!("{}: ", t!("Missing", "없는 도구")),
                app.theme.style(R::TextMuted),
            ),
            Span::styled(missing, app.theme.style(R::Accent)),
        ]),
        Line::from(""),
        Line::styled(command, app.theme.style(R::TextPrimary)),
    ];
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: true }),
        Rect {
            height: inner.height.saturating_sub(2),
            ..inner
        },
    );

    let copy = t!(" Copy command (C) ", " 명령 복사 (C) ");
    let guide = t!(" Setup guide (G) ", " 설치 안내 (G) ");
    let retry = t!(" Check again (R) ", " 다시 확인 (R) ");
    let later = if prompt.context == ToolSetupContext::Downloads {
        t!(" Cancel (Esc) ", " 취소 (Esc) ")
    } else {
        t!(" Later (Esc) ", " 나중에 (Esc) ")
    };
    let mut segments = Vec::with_capacity(7);
    if prompt.command.is_some() {
        segments.push(Seg::button(MouseTarget::ToolSetupCopy, copy));
        segments.push(Seg::label(" "));
    }
    segments.push(Seg::button(MouseTarget::ToolSetupGuide, guide));
    segments.push(Seg::label(" "));
    segments.push(Seg::button(MouseTarget::ToolSetupRetry, retry));
    segments.push(Seg::label(" "));
    segments.push(Seg::button(MouseTarget::ToolSetupLater, later));
    let button_row = Rect {
        y: inner.bottom().saturating_sub(2),
        height: 2.min(inner.height),
        ..inner
    };
    buttons::render_segments_with_hit_height(
        frame,
        app,
        button_row,
        &segments,
        (
            crate::ui::confirm_button_style(app),
            crate::ui::confirm_gap_style(app),
        ),
        Alignment::Center,
        2,
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coach_prefers_below_its_anchor() {
        let area = Rect::new(4, 2, 100, 40);
        let anchor = Rect::new(40, 5, 12, 1);
        assert_eq!(
            place_coach(area, Some(anchor), 30, 10),
            Rect::new(31, 7, 30, 10)
        );
    }

    #[test]
    fn coach_uses_space_above_a_low_anchor() {
        let area = Rect::new(0, 0, 80, 30);
        let anchor = Rect::new(35, 26, 10, 1);
        assert_eq!(
            place_coach(area, Some(anchor), 30, 10),
            Rect::new(25, 15, 30, 10)
        );
    }

    #[test]
    fn coach_fallback_is_clamped_inside_offset_area() {
        let area = Rect::new(7, 11, 20, 8);
        let popup = place_coach(area, None, 60, 30);
        assert_eq!(popup, Rect::new(7, 11, 20, 8));
        assert!(popup.left() >= area.left() && popup.right() <= area.right());
        assert!(popup.top() >= area.top() && popup.bottom() <= area.bottom());
    }

    #[test]
    fn coach_copy_uses_the_live_remapped_key_label() {
        let _guard = crate::i18n::lock_for_test();
        crate::i18n::set_language(crate::i18n::Language::English);
        let mut app = App::new(50);
        app.config.beginner_mode = true;
        app.config.beginner_tutorial.next_step = BeginnerStep::Search.id().to_owned();
        app.keymap
            .rebind(
                KeyContext::Player,
                Action::OpenSearch,
                crate::keymap::parse_chord("f8").unwrap(),
            )
            .unwrap();
        app.prepare_beginner_onboarding(true);

        let copy = coach_copy(&app, false, false);
        assert!(copy.body.contains("F8"), "got {:?}", copy.body);
    }

    #[test]
    fn resumed_dj_step_uses_the_current_player_binding() {
        let _guard = crate::i18n::lock_for_test();
        crate::i18n::set_language(crate::i18n::Language::English);
        let mut app = App::new(50);
        app.config.beginner_mode = true;
        app.config.beginner_tutorial.next_step = BeginnerStep::DjGem.id().to_owned();
        app.keymap
            .rebind(
                KeyContext::Player,
                Action::OpenAi,
                crate::keymap::parse_chord("f8").unwrap(),
            )
            .unwrap();
        app.keymap
            .rebind(
                KeyContext::Library,
                Action::OpenAi,
                crate::keymap::parse_chord("f9").unwrap(),
            )
            .unwrap();
        app.prepare_beginner_onboarding(true);

        let copy = coach_copy(&app, false, false);
        assert!(copy.body.contains("F8"), "got {:?}", copy.body);
        assert!(!copy.body.contains("F9"), "got {:?}", copy.body);
    }

    #[test]
    fn clipped_coach_copy_marks_the_omission() {
        let clipped = with_ellipsis("resize the terminal to continue", 12);
        assert!(clipped.ends_with('…'));
        assert!(buttons::text_width(&clipped) <= 12);
    }
}
