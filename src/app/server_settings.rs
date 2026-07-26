//! Music-server settings state and reducer.
//!
//! This domain deliberately lives outside [`crate::settings::SettingsDraft`]. Connection
//! material is move-only, zeroized on drop, and reaches durable storage only after the
//! connection test has produced a prepared core transaction.

mod playlist_create_recovery;
mod state;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use zeroize::{Zeroize, Zeroizing};

use super::{App, Cmd, MouseTarget, PersistCmd, StatusKind};
use crate::keymap::Action;
use crate::open_subsonic::{PlaylistCreateAttention, PreparedSetup};
use crate::search_source::SearchSource;
use crate::util::text_edit::TextCursor;

pub use state::{
    MusicServerHealth, MusicServerHistoryHealth, MusicServerSettingsState, MusicServerSummary,
};

const MAX_SETUP_DISPLAY_NAME_BYTES: usize = 1_024;
const MAX_SETUP_ORIGIN_BYTES: usize = 4_096;
const MAX_SETUP_USERNAME_BYTES: usize = 1_024;
const MAX_SETUP_PASSWORD_BYTES: usize = 64 * 1_024;
const MAX_SETUP_API_KEY_BYTES: usize = 2_048;
const MAX_SETUP_CA_PATH_BYTES: usize = 16 * 1_024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncArea {
    #[default]
    Status,
    PersonalState,
    MusicServer,
    DevicesRecovery,
}

impl SyncArea {
    pub const ALL: [Self; 4] = [
        Self::Status,
        Self::PersonalState,
        Self::MusicServer,
        Self::DevicesRecovery,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Status => crate::t!("Status", "상태", "状態"),
            Self::PersonalState => {
                crate::t!("Personal state", "개인 상태", "個人データ")
            }
            Self::MusicServer => {
                crate::t!("Music server", "음악 서버", "音楽サーバー")
            }
            Self::DevicesRecovery => {
                crate::t!("Devices & recovery", "기기 및 복구", "デバイスと復旧")
            }
        }
    }

    fn stepped(self, forward: bool) -> Self {
        let index = Self::ALL.iter().position(|area| *area == self).unwrap_or(0);
        let next = if forward {
            (index + 1) % Self::ALL.len()
        } else {
            (index + Self::ALL.len() - 1) % Self::ALL.len()
        };
        Self::ALL[next]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MusicServerFailure {
    Authentication,
    Certificate,
    Connection,
    InvalidInput,
    Storage,
    Unavailable,
}

impl MusicServerFailure {
    pub fn label(self) -> &'static str {
        match self {
            Self::Authentication => crate::t!(
                "The sign-in details were rejected.",
                "로그인 정보가 거부되었어요.",
                "ログイン情報が拒否されました。"
            ),
            Self::Certificate => crate::t!(
                "The server certificate could not be verified.",
                "서버 인증서를 확인할 수 없어요.",
                "サーバー証明書を確認できません。"
            ),
            Self::Connection => crate::t!(
                "The music server could not be reached.",
                "음악 서버에 연결할 수 없어요.",
                "音楽サーバーに接続できません。"
            ),
            Self::InvalidInput => crate::t!(
                "Check the connection details.",
                "연결 정보를 확인해 주세요.",
                "接続情報を確認してください。"
            ),
            Self::Storage => crate::t!(
                "Music server settings could not be saved.",
                "음악 서버 설정을 저장할 수 없어요.",
                "音楽サーバー設定を保存できません。"
            ),
            Self::Unavailable => crate::t!(
                "Music server support is not available right now.",
                "지금은 음악 서버 기능을 사용할 수 없어요.",
                "現在、音楽サーバー機能を利用できません。"
            ),
        }
    }

    pub fn recovery_label(self) -> &'static str {
        match self {
            Self::Authentication => {
                crate::t!("Update password", "비밀번호 업데이트", "パスワードを更新")
            }
            Self::Certificate => {
                crate::t!("Choose CA file", "CA 파일 선택", "CAファイルを選択")
            }
            Self::Connection => crate::t!("Try again", "다시 시도", "再試行"),
            Self::InvalidInput => {
                crate::t!("Edit connection", "연결 정보 수정", "接続情報を編集")
            }
            Self::Storage => crate::t!("Try again", "다시 시도", "再試行"),
            Self::Unavailable => crate::t!("Close", "닫기", "閉じる"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MusicServerCredentialMode {
    #[default]
    Password,
    ApiKey,
}

impl MusicServerCredentialMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Password => crate::t!("Password", "비밀번호", "パスワード"),
            Self::ApiKey => crate::t!("API key", "API 키", "APIキー"),
        }
    }

    fn toggled(self) -> Self {
        match self {
            Self::Password => Self::ApiKey,
            Self::ApiKey => Self::Password,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MusicServerIdentityIntent {
    Create,
    UpdateSameServerAndAccount,
    ReplaceServerOrAccount,
}

/// Move-only, secret-bearing request sent from the reducer to the runtime worker.
///
/// It intentionally implements neither `Debug` nor `Clone`; every text field is zeroized even
/// though only the final credential is cryptographic secret material. This keeps URLs and account
/// names out of retained crash/debug values too.
pub struct MusicServerSetupInput {
    pub(crate) display_name: Zeroizing<String>,
    pub(crate) origin: Zeroizing<String>,
    pub(crate) username: Zeroizing<String>,
    pub(crate) secret: Zeroizing<String>,
    pub(crate) credential_mode: MusicServerCredentialMode,
    pub(crate) custom_ca_path: Zeroizing<String>,
    pub(crate) allow_lan_http: bool,
    pub(crate) identity_intent: MusicServerIdentityIntent,
}

pub enum MusicServerCommand {
    Refresh {
        generation: u64,
    },
    TestAndPrepare {
        generation: u64,
        input: MusicServerSetupInput,
    },
    Commit {
        generation: u64,
        prepared: Box<PreparedSetup>,
    },
    DisableHistory {
        generation: u64,
    },
    Remove {
        generation: u64,
    },
    AbandonPlaylistCreate {
        generation: u64,
        local_playlist_id: crate::personal_state::PlaylistId,
    },
}

pub enum MusicServerEvent {
    Refreshed {
        generation: u64,
        result: Result<MusicServerRefreshOutcome, MusicServerFailure>,
    },
    Prepared {
        generation: u64,
        result: Result<Box<PreparedSetup>, MusicServerFailure>,
    },
    Committed {
        generation: u64,
        result: Result<MusicServerSummary, MusicServerFailure>,
    },
    HistoryDisabled {
        generation: u64,
        result: Result<MusicServerSummary, MusicServerFailure>,
    },
    Removed {
        generation: u64,
        result: Result<(), MusicServerFailure>,
    },
    PlaylistCreateAbandoned {
        generation: u64,
        result: Result<MusicServerSummary, MusicServerFailure>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MusicServerRefreshOutcome {
    pub summary: MusicServerSummary,
    pub failure: Option<MusicServerFailure>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MusicServerBusy {
    Refresh,
    Testing,
    Saving,
    History,
    Removing,
    PlaylistRecovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MusicServerSetupField {
    DisplayName,
    Origin,
    Identity,
    CredentialMode,
    Username,
    Secret,
    CustomCa,
    AllowLanHttp,
    SaveAndTest,
    Cancel,
}

impl MusicServerSetupField {
    pub const ALL: [Self; 10] = [
        Self::DisplayName,
        Self::Origin,
        Self::Identity,
        Self::CredentialMode,
        Self::Username,
        Self::Secret,
        Self::CustomCa,
        Self::AllowLanHttp,
        Self::SaveAndTest,
        Self::Cancel,
    ];
}

/// Secret-owning setup form. No `Debug`/`Clone`; dropping or cancelling it wipes all input.
pub struct MusicServerSetupForm {
    pub(crate) display_name: Zeroizing<String>,
    pub(crate) origin: Zeroizing<String>,
    pub(crate) username: Zeroizing<String>,
    pub(crate) secret: Zeroizing<String>,
    pub(crate) credential_mode: MusicServerCredentialMode,
    pub(crate) custom_ca_path: Zeroizing<String>,
    pub(crate) allow_lan_http: bool,
    pub(crate) selected: usize,
    pub(crate) cursor: TextCursor,
    pub(crate) reveal_secret: bool,
    pub(crate) identity_intent: Option<MusicServerIdentityIntent>,
}

impl Default for MusicServerSetupForm {
    fn default() -> Self {
        Self::new(false)
    }
}

impl MusicServerSetupForm {
    fn new(configured: bool) -> Self {
        Self {
            display_name: zeroizing_buffer(MAX_SETUP_DISPLAY_NAME_BYTES),
            origin: zeroizing_buffer(MAX_SETUP_ORIGIN_BYTES),
            username: zeroizing_buffer(MAX_SETUP_USERNAME_BYTES),
            secret: zeroizing_buffer(MAX_SETUP_PASSWORD_BYTES),
            credential_mode: MusicServerCredentialMode::Password,
            custom_ca_path: zeroizing_buffer(MAX_SETUP_CA_PATH_BYTES),
            allow_lan_http: false,
            selected: 0,
            cursor: TextCursor::default(),
            reveal_secret: false,
            identity_intent: (!configured).then_some(MusicServerIdentityIntent::Create),
        }
    }

    pub(crate) fn selected_field(&self) -> MusicServerSetupField {
        MusicServerSetupField::ALL[self.selected.min(MusicServerSetupField::ALL.len() - 1)]
    }

    pub(crate) fn text_value(&self, field: MusicServerSetupField) -> Option<&str> {
        match field {
            MusicServerSetupField::DisplayName => Some(self.display_name.as_str()),
            MusicServerSetupField::Origin => Some(self.origin.as_str()),
            MusicServerSetupField::Username => Some(self.username.as_str()),
            MusicServerSetupField::Secret => Some(self.secret.as_str()),
            MusicServerSetupField::CustomCa => Some(self.custom_ca_path.as_str()),
            MusicServerSetupField::CredentialMode
            | MusicServerSetupField::Identity
            | MusicServerSetupField::AllowLanHttp
            | MusicServerSetupField::SaveAndTest
            | MusicServerSetupField::Cancel => None,
        }
    }

    fn text_value_mut(&mut self, field: MusicServerSetupField) -> Option<&mut String> {
        match field {
            MusicServerSetupField::DisplayName => Some(&mut self.display_name),
            MusicServerSetupField::Origin => Some(&mut self.origin),
            MusicServerSetupField::Username => Some(&mut self.username),
            MusicServerSetupField::Secret => Some(&mut self.secret),
            MusicServerSetupField::CustomCa => Some(&mut self.custom_ca_path),
            MusicServerSetupField::CredentialMode
            | MusicServerSetupField::Identity
            | MusicServerSetupField::AllowLanHttp
            | MusicServerSetupField::SaveAndTest
            | MusicServerSetupField::Cancel => None,
        }
    }

    fn text_byte_limit(&self, field: MusicServerSetupField) -> Option<usize> {
        match field {
            MusicServerSetupField::DisplayName => Some(MAX_SETUP_DISPLAY_NAME_BYTES),
            MusicServerSetupField::Origin => Some(MAX_SETUP_ORIGIN_BYTES),
            MusicServerSetupField::Username => Some(MAX_SETUP_USERNAME_BYTES),
            MusicServerSetupField::Secret => Some(match self.credential_mode {
                MusicServerCredentialMode::Password => MAX_SETUP_PASSWORD_BYTES,
                MusicServerCredentialMode::ApiKey => MAX_SETUP_API_KEY_BYTES,
            }),
            MusicServerSetupField::CustomCa => Some(MAX_SETUP_CA_PATH_BYTES),
            MusicServerSetupField::CredentialMode
            | MusicServerSetupField::Identity
            | MusicServerSetupField::AllowLanHttp
            | MusicServerSetupField::SaveAndTest
            | MusicServerSetupField::Cancel => None,
        }
    }

    fn set_selected(&mut self, selected: usize) {
        self.selected = selected.min(MusicServerSetupField::ALL.len() - 1);
        self.cursor = self
            .text_value(self.selected_field())
            .map(TextCursor::at_end)
            .unwrap_or_default();
    }

    fn into_input(mut self) -> MusicServerSetupInput {
        MusicServerSetupInput {
            display_name: std::mem::take(&mut self.display_name),
            origin: std::mem::take(&mut self.origin),
            username: std::mem::take(&mut self.username),
            secret: std::mem::take(&mut self.secret),
            credential_mode: self.credential_mode,
            custom_ca_path: std::mem::take(&mut self.custom_ca_path),
            allow_lan_http: self.allow_lan_http,
            identity_intent: self
                .identity_intent
                .expect("identity intent is validated before setup starts"),
        }
    }
}

fn zeroizing_buffer(capacity: usize) -> Zeroizing<String> {
    Zeroizing::new(String::with_capacity(capacity))
}

impl Drop for MusicServerSetupForm {
    fn drop(&mut self) {
        self.display_name.zeroize();
        self.origin.zeroize();
        self.username.zeroize();
        self.secret.zeroize();
        self.custom_ca_path.zeroize();
    }
}

pub enum MusicServerWizard {
    Setup(MusicServerSetupForm),
    Waiting,
    RemoveConfirm,
    AbandonPlaylistCreateConfirm(PlaylistCreateAttention),
}

impl App {
    pub(in crate::app) fn on_sync_settings_action(
        &mut self,
        action: Option<Action>,
        key: KeyEvent,
    ) -> Option<Vec<Cmd>> {
        let area = self.server.settings.area;
        match action {
            Some(Action::MoveUp | Action::MoveDown) if area == SyncArea::MusicServer => {
                Some(self.on_key_music_server_settings(key))
            }
            Some(Action::MoveUp | Action::MoveDown)
                if matches!(area, SyncArea::PersonalState | SyncArea::DevicesRecovery) =>
            {
                let delta = if matches!(action, Some(Action::MoveUp)) {
                    -1
                } else {
                    1
                };
                self.personal_state.sync_ui.move_row(delta);
                self.dirty = true;
                Some(Vec::new())
            }
            Some(Action::MoveUp | Action::MoveDown) => Some(Vec::new()),
            Some(Action::ChangeDecrease) => Some(self.switch_sync_area(false)),
            Some(Action::ChangeIncrease) => Some(self.switch_sync_area(true)),
            Some(Action::Confirm) if area == SyncArea::MusicServer => {
                Some(self.on_key_music_server_settings(key))
            }
            Some(Action::Confirm)
                if matches!(area, SyncArea::PersonalState | SyncArea::DevicesRecovery) =>
            {
                Some(self.activate_sync_row())
            }
            Some(Action::Confirm) => Some(Vec::new()),
            _ => None,
        }
    }

    pub(in crate::app) fn switch_sync_area(&mut self, forward: bool) -> Vec<Cmd> {
        if self.server.settings.modal_open() {
            return Vec::new();
        }
        let next = self.server.settings.area.stepped(forward);
        self.select_sync_area(next)
    }

    pub(in crate::app) fn select_sync_area(&mut self, area: SyncArea) -> Vec<Cmd> {
        if self.server.settings.modal_open() {
            return Vec::new();
        }
        self.server.settings.area = area;
        self.bridges.settings_scroll.reset();
        self.dirty = true;
        match area {
            SyncArea::Status => {
                self.personal_state.sync_ui.page = super::sync_ui::SyncPage::Overview;
                self.personal_state.sync_ui.row = 0;
                let mut commands = self.request_sync_ui_refresh();
                commands.extend(self.request_music_server_status());
                commands
            }
            SyncArea::PersonalState => {
                self.personal_state.sync_ui.page = super::sync_ui::SyncPage::Overview;
                self.personal_state.sync_ui.row = 0;
                self.request_sync_ui_refresh()
            }
            SyncArea::MusicServer => self.request_music_server_status(),
            SyncArea::DevicesRecovery => {
                self.personal_state.sync_ui.page = super::sync_ui::SyncPage::Devices;
                self.personal_state.sync_ui.row = 0;
                self.request_sync_ui_refresh()
            }
        }
    }

    pub(in crate::app) fn request_music_server_status(&mut self) -> Vec<Cmd> {
        if self.server.settings.busy.is_some() {
            return Vec::new();
        }
        let generation = self.server.settings.next_generation();
        self.server.settings.busy = Some(MusicServerBusy::Refresh);
        self.server.settings.failure = None;
        self.dirty = true;
        vec![Cmd::MusicServer(MusicServerCommand::Refresh { generation })]
    }

    pub(in crate::app) fn cancel_music_server_session(&mut self) {
        self.server.settings.next_generation();
        self.server.settings.busy = None;
        self.server.settings.wizard = None;
        self.server.settings.failure = None;
    }

    fn set_music_server_search_enabled(&mut self, enabled: bool) -> Vec<Cmd> {
        self.config
            .search
            .set_enabled(SearchSource::OpenSubsonic, enabled);
        self.config.search = self.config.search.clone().normalized();
        self.search.source = self.config.search.normalized_source(self.search.source);
        if !enabled {
            self.dropdowns.search_source_open = false;
        }
        vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))]
    }

    pub(in crate::app) fn on_key_music_server_settings(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if self.server.settings.wizard.is_some() {
            return self.on_key_music_server_wizard(key);
        }
        let row_count = self.server.settings.row_count();
        match key.code {
            KeyCode::Up => {
                self.server.settings.selected = self.server.settings.selected.saturating_sub(1);
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Down => {
                self.server.settings.selected =
                    (self.server.settings.selected + 1).min(row_count.saturating_sub(1));
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Enter => self.activate_music_server_row(self.server.settings.selected),
            _ => Vec::new(),
        }
    }

    pub(in crate::app) fn activate_music_server_row(&mut self, row: usize) -> Vec<Cmd> {
        if self.server.settings.busy.is_some() || row >= self.server.settings.row_count() {
            return Vec::new();
        }
        self.server.settings.selected = row;
        if self.server.settings.summary.configured {
            let has_pending_create = !self
                .server
                .settings
                .summary
                .playlist_create_attention
                .is_empty();
            match row {
                0 => self.request_music_server_status(),
                1 => {
                    self.open_music_server_setup();
                    Vec::new()
                }
                2 => self.activate_music_server_history(),
                3 if has_pending_create => self.open_playlist_create_abandon_confirmation(),
                3 | 4 => {
                    self.server.settings.wizard = Some(MusicServerWizard::RemoveConfirm);
                    self.dirty = true;
                    Vec::new()
                }
                _ => Vec::new(),
            }
        } else {
            match row {
                0 => {
                    self.open_music_server_setup();
                    Vec::new()
                }
                1 => self.request_music_server_status(),
                _ => Vec::new(),
            }
        }
    }

    pub(in crate::app) fn open_music_server_setup(&mut self) {
        if self.server.settings.busy.is_some() {
            return;
        }
        self.server.settings.next_generation();
        self.server.settings.failure = None;
        self.server.settings.wizard = Some(MusicServerWizard::Setup(MusicServerSetupForm::new(
            self.server.settings.summary.configured,
        )));
        self.dirty = true;
    }

    fn on_key_music_server_wizard(&mut self, key: KeyEvent) -> Vec<Cmd> {
        let durable_change_in_flight = matches!(
            self.server.settings.busy,
            Some(
                MusicServerBusy::Saving
                    | MusicServerBusy::Removing
                    | MusicServerBusy::PlaylistRecovery
            )
        );
        if durable_change_in_flight {
            return Vec::new();
        }
        let editing_text = matches!(
            self.server.settings.wizard.as_ref(),
            Some(MusicServerWizard::Setup(form)) if form.text_value(form.selected_field()).is_some()
        );
        if key.code == KeyCode::Esc
            || (!editing_text
                && key.code == KeyCode::Char('q')
                && key.modifiers == KeyModifiers::NONE)
        {
            self.cancel_music_server_session();
            self.dirty = true;
            return Vec::new();
        }
        if self.server.settings.busy.is_some() {
            return Vec::new();
        }
        if matches!(
            self.server.settings.wizard,
            Some(MusicServerWizard::RemoveConfirm)
        ) {
            return match key.code {
                KeyCode::Enter => self.start_music_server_remove(),
                _ => Vec::new(),
            };
        }
        if matches!(
            self.server.settings.wizard,
            Some(MusicServerWizard::AbandonPlaylistCreateConfirm(_))
        ) {
            return match key.code {
                KeyCode::Enter => self.start_abandon_playlist_create(),
                _ => Vec::new(),
            };
        }

        let Some(MusicServerWizard::Setup(form)) = self.server.settings.wizard.as_mut() else {
            return Vec::new();
        };
        let field = form.selected_field();
        match key.code {
            KeyCode::Up | KeyCode::BackTab => {
                form.set_selected(form.selected.saturating_sub(1));
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Down | KeyCode::Tab => {
                form.set_selected((form.selected + 1).min(MusicServerSetupField::ALL.len() - 1));
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if field == MusicServerSetupField::CredentialMode =>
            {
                form.credential_mode = form.credential_mode.toggled();
                form.secret.clear();
                if form.credential_mode == MusicServerCredentialMode::ApiKey {
                    form.username.clear();
                }
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if field == MusicServerSetupField::Identity
                    && form.identity_intent != Some(MusicServerIdentityIntent::Create) =>
            {
                form.identity_intent = Some(match form.identity_intent {
                    Some(MusicServerIdentityIntent::UpdateSameServerAndAccount) => {
                        MusicServerIdentityIntent::ReplaceServerOrAccount
                    }
                    _ => MusicServerIdentityIntent::UpdateSameServerAndAccount,
                });
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if field == MusicServerSetupField::AllowLanHttp =>
            {
                form.allow_lan_http = !form.allow_lan_http;
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Enter if field == MusicServerSetupField::SaveAndTest => {
                self.start_music_server_setup()
            }
            KeyCode::Enter if field == MusicServerSetupField::Cancel => {
                self.cancel_music_server_session();
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Enter if field == MusicServerSetupField::Secret => {
                form.reveal_secret = !form.reveal_secret;
                self.dirty = true;
                Vec::new()
            }
            KeyCode::Backspace => {
                let mut cursor = form.cursor;
                if let Some(value) = form.text_value_mut(field) {
                    cursor.delete_previous_grapheme(value);
                    form.cursor = cursor;
                    self.dirty = true;
                }
                Vec::new()
            }
            KeyCode::Left if form.text_value(field).is_some() => {
                let mut cursor = form.cursor;
                if form
                    .text_value(field)
                    .is_some_and(|value| cursor.move_left(value))
                {
                    form.cursor = cursor;
                    self.dirty = true;
                }
                Vec::new()
            }
            KeyCode::Right if form.text_value(field).is_some() => {
                let mut cursor = form.cursor;
                if form
                    .text_value(field)
                    .is_some_and(|value| cursor.move_right(value))
                {
                    form.cursor = cursor;
                    self.dirty = true;
                }
                Vec::new()
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let mut cursor = form.cursor;
                let may_insert = form.text_byte_limit(field).is_some_and(|limit| {
                    form.text_value(field).is_some_and(|value| {
                        value.len().saturating_add(character.len_utf8()) <= limit
                    })
                });
                if may_insert && let Some(value) = form.text_value_mut(field) {
                    cursor.insert_char(value, character);
                    form.cursor = cursor;
                    self.dirty = true;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn start_music_server_setup(&mut self) -> Vec<Cmd> {
        if self.server.settings.busy.is_some() {
            return Vec::new();
        }
        if matches!(
            self.server.settings.wizard.as_ref(),
            Some(MusicServerWizard::Setup(MusicServerSetupForm {
                identity_intent: None,
                ..
            }))
        ) {
            self.server.settings.failure = Some(MusicServerFailure::InvalidInput);
            self.status.kind = StatusKind::Error;
            self.status.text = crate::t!(
                "Choose whether this is the same server and account",
                "같은 서버와 계정인지 선택해 주세요",
                "同じサーバーとアカウントか選択してください"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        }
        let Some(MusicServerWizard::Setup(form)) = self.server.settings.wizard.take() else {
            return Vec::new();
        };
        let generation = self.server.settings.generation.max(1);
        self.server.settings.busy = Some(MusicServerBusy::Testing);
        self.server.settings.failure = None;
        self.server.settings.wizard = Some(MusicServerWizard::Waiting);
        self.dirty = true;
        vec![Cmd::MusicServer(MusicServerCommand::TestAndPrepare {
            generation,
            input: form.into_input(),
        })]
    }

    fn start_music_server_remove(&mut self) -> Vec<Cmd> {
        if self.server.settings.busy.is_some() {
            return Vec::new();
        }
        let generation = self.server.settings.generation.max(1);
        self.server.settings.busy = Some(MusicServerBusy::Removing);
        self.server.settings.wizard = Some(MusicServerWizard::Waiting);
        self.dirty = true;
        vec![Cmd::MusicServer(MusicServerCommand::Remove { generation })]
    }

    pub(in crate::app) fn on_music_server_wizard_mouse_target(
        &mut self,
        target: MouseTarget,
    ) -> Vec<Cmd> {
        if !self.server.settings.modal_open() {
            return Vec::new();
        }
        match target {
            MouseTarget::MusicServerWizardSecondary
                if matches!(
                    self.server.settings.busy,
                    Some(
                        MusicServerBusy::Saving
                            | MusicServerBusy::Removing
                            | MusicServerBusy::PlaylistRecovery
                    )
                ) =>
            {
                Vec::new()
            }
            MouseTarget::MusicServerWizardSecondary => {
                self.cancel_music_server_session();
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::MusicServerWizardPrimary if self.server.settings.busy.is_some() => {
                Vec::new()
            }
            MouseTarget::MusicServerWizardPrimary => match self.server.settings.wizard.as_ref() {
                Some(MusicServerWizard::Setup(_)) => self.start_music_server_setup(),
                Some(MusicServerWizard::RemoveConfirm) => self.start_music_server_remove(),
                Some(MusicServerWizard::AbandonPlaylistCreateConfirm(_)) => {
                    self.start_abandon_playlist_create()
                }
                Some(MusicServerWizard::Waiting) | None => Vec::new(),
            },
            MouseTarget::MusicServerWizardReveal => {
                if let Some(MusicServerWizard::Setup(form)) = self.server.settings.wizard.as_mut() {
                    form.reveal_secret = !form.reveal_secret;
                    self.dirty = true;
                }
                Vec::new()
            }
            MouseTarget::MusicServerWizardField(index) => {
                let Some(field) = MusicServerSetupField::ALL.get(index).copied() else {
                    return Vec::new();
                };
                let Some(MusicServerWizard::Setup(form)) = self.server.settings.wizard.as_mut()
                else {
                    return Vec::new();
                };
                form.set_selected(index);
                self.dirty = true;
                match field {
                    MusicServerSetupField::CredentialMode
                    | MusicServerSetupField::Identity
                    | MusicServerSetupField::AllowLanHttp => self.on_key_music_server_wizard(
                        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
                    ),
                    MusicServerSetupField::SaveAndTest => self.start_music_server_setup(),
                    MusicServerSetupField::Cancel => {
                        self.cancel_music_server_session();
                        Vec::new()
                    }
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    pub(in crate::app) fn finish_music_server_event(
        &mut self,
        event: MusicServerEvent,
    ) -> Vec<Cmd> {
        let event_generation = match &event {
            MusicServerEvent::Refreshed { generation, .. }
            | MusicServerEvent::Prepared { generation, .. }
            | MusicServerEvent::Committed { generation, .. }
            | MusicServerEvent::HistoryDisabled { generation, .. }
            | MusicServerEvent::Removed { generation, .. }
            | MusicServerEvent::PlaylistCreateAbandoned { generation, .. } => *generation,
        };
        if event_generation != self.server.settings.generation {
            return Vec::new();
        }

        match event {
            MusicServerEvent::Refreshed { result, .. } => {
                self.server.settings.busy = None;
                match result {
                    Ok(outcome) => {
                        self.server.settings.summary = outcome.summary;
                        self.server.settings.failure = outcome.failure;
                    }
                    Err(failure) => {
                        self.server.settings.failure = Some(failure);
                        self.server.settings.summary.health = MusicServerHealth::NeedsAttention;
                    }
                }
                self.dirty = true;
                Vec::new()
            }
            MusicServerEvent::Prepared { result, .. } => match result {
                Ok(prepared) => {
                    self.server.settings.busy = Some(MusicServerBusy::Saving);
                    self.dirty = true;
                    vec![Cmd::MusicServer(MusicServerCommand::Commit {
                        generation: event_generation,
                        prepared,
                    })]
                }
                Err(failure) => {
                    self.server.settings.busy = None;
                    self.server.settings.failure = Some(failure);
                    self.server.settings.wizard = Some(MusicServerWizard::Setup(
                        MusicServerSetupForm::new(self.server.settings.summary.configured),
                    ));
                    self.status.kind = StatusKind::Error;
                    self.status.text = failure.label().to_owned();
                    self.dirty = true;
                    Vec::new()
                }
            },
            MusicServerEvent::Committed { result, .. } => {
                self.server.settings.busy = None;
                let commands = match result {
                    Ok(summary) => {
                        let runtime_ready = summary.health == MusicServerHealth::UpToDate;
                        self.server.settings.summary = summary;
                        self.server.settings.failure =
                            (!runtime_ready).then_some(MusicServerFailure::Connection);
                        self.server.settings.wizard = None;
                        if runtime_ready {
                            self.status.kind = StatusKind::Info;
                            self.status.text = crate::t!(
                                "Music server connected",
                                "음악 서버가 연결되었어요",
                                "音楽サーバーに接続しました"
                            )
                            .to_owned();
                        } else {
                            self.status.kind = StatusKind::Error;
                            self.status.text = crate::t!(
                                "Connection saved; the server needs attention",
                                "연결은 저장됐지만 서버 확인이 필요해요",
                                "接続は保存されましたが、サーバーの確認が必要です"
                            )
                            .to_owned();
                        }
                        let reload_library =
                            self.server.library.source == super::LibrarySource::OpenSubsonic;
                        self.server.library.invalidate_after_profile_change();
                        let mut commands = self.set_music_server_search_enabled(true);
                        if reload_library {
                            commands.extend(self.request_server_library_page(0, false));
                        }
                        commands
                    }
                    Err(failure) => {
                        self.server.settings.failure = Some(failure);
                        self.server.settings.wizard = Some(MusicServerWizard::Setup(
                            MusicServerSetupForm::new(self.server.settings.summary.configured),
                        ));
                        self.status.kind = StatusKind::Error;
                        self.status.text = failure.label().to_owned();
                        Vec::new()
                    }
                };
                self.dirty = true;
                commands
            }
            MusicServerEvent::HistoryDisabled { result, .. } => {
                self.server.settings.busy = None;
                match result {
                    Ok(summary) => {
                        let runtime_ready = summary.health == MusicServerHealth::UpToDate;
                        self.server.settings.summary = summary;
                        self.server.settings.failure =
                            (!runtime_ready).then_some(MusicServerFailure::Connection);
                        if runtime_ready {
                            self.status.kind = StatusKind::Info;
                            self.status.text = crate::t!(
                                "Detailed history is off; standard server access was kept",
                                "상세 이력을 껐어요. 일반 서버 연결은 그대로예요",
                                "詳細履歴をオフにしました。通常のサーバー接続は保持されます"
                            )
                            .to_owned();
                        } else {
                            self.status.kind = StatusKind::Error;
                            self.status.text = crate::t!(
                                "Detailed history is off; the server needs attention",
                                "상세 이력은 껐지만 서버 확인이 필요해요",
                                "詳細履歴はオフですが、サーバーの確認が必要です"
                            )
                            .to_owned();
                        }
                    }
                    Err(failure) => {
                        self.server.settings.failure = Some(failure);
                        self.server.settings.summary.health = MusicServerHealth::NeedsAttention;
                        self.status.kind = StatusKind::Error;
                        self.status.text = failure.label().to_owned();
                    }
                }
                self.dirty = true;
                Vec::new()
            }
            MusicServerEvent::Removed { result, .. } => {
                self.server.settings.busy = None;
                let commands = match result {
                    Ok(()) => {
                        self.server.settings.summary = MusicServerSummary::default();
                        self.server.settings.failure = None;
                        self.server.settings.wizard = None;
                        self.server.library.reset_after_profile_removal();
                        self.status.kind = StatusKind::Info;
                        self.status.text = crate::t!(
                            "Music server removed; local music was kept",
                            "음악 서버를 제거했어요. 로컬 음악은 그대로예요",
                            "音楽サーバーを削除しました。ローカル音楽は保持されます"
                        )
                        .to_owned();
                        self.set_music_server_search_enabled(false)
                    }
                    Err(failure) => {
                        self.server.settings.failure = Some(failure);
                        self.server.settings.summary.health = MusicServerHealth::NeedsAttention;
                        self.server.settings.wizard = None;
                        self.status.kind = StatusKind::Error;
                        self.status.text = failure.label().to_owned();
                        Vec::new()
                    }
                };
                self.dirty = true;
                commands
            }
            MusicServerEvent::PlaylistCreateAbandoned { result, .. } => {
                self.finish_playlist_create_abandoned(result)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Msg;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn cancelling_setup_zeroizes_and_invalidates_late_result() {
        let mut app = App::new(50);
        app.open_music_server_setup();
        let generation = app.server.settings.generation;
        if let Some(MusicServerWizard::Setup(form)) = app.server.settings.wizard.as_mut() {
            form.secret.push_str("very-secret");
        }
        assert!(app.update(Msg::Key(key(KeyCode::Esc))).is_empty());
        assert!(app.server.settings.wizard.is_none());
        assert_ne!(app.server.settings.generation, generation);
    }

    #[test]
    fn q_is_text_in_setup_fields_but_durable_steps_are_not_dismissed() {
        let mut app = App::new(50);
        app.open_music_server_setup();
        assert!(
            app.on_key_music_server_settings(key(KeyCode::Char('q')))
                .is_empty()
        );
        let Some(MusicServerWizard::Setup(form)) = app.server.settings.wizard.as_ref() else {
            panic!("setup form");
        };
        assert_eq!(form.display_name.as_str(), "q");

        let generation = app.server.settings.generation;
        app.server.settings.wizard = Some(MusicServerWizard::Waiting);
        app.server.settings.busy = Some(MusicServerBusy::Saving);
        assert!(
            app.on_key_music_server_settings(key(KeyCode::Esc))
                .is_empty()
        );
        assert!(
            app.on_music_server_wizard_mouse_target(MouseTarget::MusicServerWizardSecondary)
                .is_empty()
        );
        assert_eq!(app.server.settings.generation, generation);
        assert!(matches!(
            app.server.settings.wizard,
            Some(MusicServerWizard::Waiting)
        ));
        assert_eq!(app.server.settings.busy, Some(MusicServerBusy::Saving));
    }

    #[test]
    fn setup_inputs_are_preallocated_and_enforce_credential_byte_limits() {
        let mut form = MusicServerSetupForm::new(false);
        assert!(form.secret.capacity() >= MAX_SETUP_PASSWORD_BYTES);
        form.set_selected(MusicServerSetupField::Secret as usize);
        form.credential_mode = MusicServerCredentialMode::ApiKey;
        form.secret.push_str(&"x".repeat(MAX_SETUP_API_KEY_BYTES));

        let mut app = App::new(50);
        app.server.settings.wizard = Some(MusicServerWizard::Setup(form));
        assert!(
            app.on_key_music_server_settings(key(KeyCode::Char('é')))
                .is_empty()
        );
        let Some(MusicServerWizard::Setup(form)) = app.server.settings.wizard.as_ref() else {
            panic!("setup form");
        };
        assert_eq!(form.secret.len(), MAX_SETUP_API_KEY_BYTES);
        assert!(form.secret.capacity() >= MAX_SETUP_PASSWORD_BYTES);
    }

    #[test]
    fn confirmed_remove_switches_to_non_cancellable_waiting_state() {
        let mut app = App::new(50);
        app.server.settings.summary.configured = true;
        app.server.settings.summary.history = MusicServerHistoryHealth::Detailed;
        assert!(matches!(
            app.activate_music_server_row(2).as_slice(),
            [Cmd::MusicServer(MusicServerCommand::DisableHistory { .. })]
        ));
        app.server.settings.busy = None;
        assert!(app.activate_music_server_row(3).is_empty());
        let commands = app.on_key_music_server_settings(key(KeyCode::Enter));

        assert_eq!(commands.len(), 1);
        assert_eq!(app.server.settings.busy, Some(MusicServerBusy::Removing));
        assert!(matches!(
            app.server.settings.wizard,
            Some(MusicServerWizard::Waiting)
        ));
        assert!(
            app.on_key_music_server_settings(key(KeyCode::Esc))
                .is_empty()
        );
        assert!(matches!(
            app.server.settings.wizard,
            Some(MusicServerWizard::Waiting)
        ));
    }

    #[test]
    fn wizard_mouse_fields_toggle_reveal_save_and_cancel() {
        let index = |field| {
            MusicServerSetupField::ALL
                .iter()
                .position(|candidate| *candidate == field)
                .unwrap()
        };
        let mut app = App::new(50);
        app.open_music_server_setup();
        if let Some(MusicServerWizard::Setup(form)) = app.server.settings.wizard.as_mut() {
            form.secret.push_str("wipe-on-mode-change");
        }
        app.on_music_server_wizard_mouse_target(MouseTarget::MusicServerWizardReveal);
        let Some(MusicServerWizard::Setup(form)) = app.server.settings.wizard.as_ref() else {
            panic!("setup form");
        };
        assert!(form.reveal_secret);

        app.on_music_server_wizard_mouse_target(MouseTarget::MusicServerWizardField(index(
            MusicServerSetupField::CredentialMode,
        )));
        let Some(MusicServerWizard::Setup(form)) = app.server.settings.wizard.as_ref() else {
            panic!("setup form");
        };
        assert_eq!(form.credential_mode, MusicServerCredentialMode::ApiKey);
        assert!(form.secret.is_empty());

        app.on_music_server_wizard_mouse_target(MouseTarget::MusicServerWizardField(index(
            MusicServerSetupField::Cancel,
        )));
        assert!(app.server.settings.wizard.is_none());

        app.open_music_server_setup();
        let commands = app.on_music_server_wizard_mouse_target(
            MouseTarget::MusicServerWizardField(index(MusicServerSetupField::SaveAndTest)),
        );
        assert_eq!(commands.len(), 1);
        assert!(matches!(
            app.server.settings.wizard,
            Some(MusicServerWizard::Waiting)
        ));
    }

    #[test]
    fn duplicate_save_is_blocked_while_test_is_in_flight() {
        let mut app = App::new(50);
        app.open_music_server_setup();
        if let Some(MusicServerWizard::Setup(form)) = app.server.settings.wizard.as_mut() {
            form.selected = MusicServerSetupField::SaveAndTest as usize;
        }
        let first = app.on_key_music_server_settings(key(KeyCode::Enter));
        assert_eq!(first.len(), 1);
        let second = app.on_key_music_server_settings(key(KeyCode::Enter));
        assert!(second.is_empty());
        assert_eq!(app.server.settings.busy, Some(MusicServerBusy::Testing));
    }

    #[test]
    fn new_connection_identity_cannot_be_changed_from_create() {
        let mut app = App::new(50);
        app.open_music_server_setup();
        let identity_index = MusicServerSetupField::ALL
            .iter()
            .position(|field| *field == MusicServerSetupField::Identity)
            .unwrap();
        if let Some(MusicServerWizard::Setup(form)) = app.server.settings.wizard.as_mut() {
            form.set_selected(identity_index);
        }
        for code in [KeyCode::Left, KeyCode::Right, KeyCode::Char(' ')] {
            assert!(app.on_key_music_server_settings(key(code)).is_empty());
        }
        let Some(MusicServerWizard::Setup(form)) = app.server.settings.wizard.as_ref() else {
            panic!("setup form");
        };
        assert_eq!(
            form.identity_intent,
            Some(MusicServerIdentityIntent::Create)
        );
    }

    #[test]
    fn existing_connection_requires_an_explicit_identity_choice() {
        let mut app = App::new(50);
        app.server.settings.summary.configured = true;
        app.open_music_server_setup();
        if let Some(MusicServerWizard::Setup(form)) = app.server.settings.wizard.as_mut() {
            form.set_selected(MusicServerSetupField::SaveAndTest as usize);
        }
        assert!(
            app.on_key_music_server_settings(key(KeyCode::Enter))
                .is_empty()
        );
        assert_eq!(
            app.server.settings.failure,
            Some(MusicServerFailure::InvalidInput)
        );

        let identity_index = MusicServerSetupField::ALL
            .iter()
            .position(|field| *field == MusicServerSetupField::Identity)
            .unwrap();
        if let Some(MusicServerWizard::Setup(form)) = app.server.settings.wizard.as_mut() {
            form.set_selected(identity_index);
        }
        app.on_key_music_server_settings(key(KeyCode::Char(' ')));
        let Some(MusicServerWizard::Setup(form)) = app.server.settings.wizard.as_ref() else {
            panic!("setup form");
        };
        assert_eq!(
            form.identity_intent,
            Some(MusicServerIdentityIntent::UpdateSameServerAndAccount)
        );
    }

    #[test]
    fn committed_profile_enables_and_persists_integrated_search() {
        let mut app = App::new(50);
        app.server.settings.generation = 7;
        assert!(!app.config.search.open_subsonic);

        let commands = app.finish_music_server_event(MusicServerEvent::Committed {
            generation: 7,
            result: Ok(MusicServerSummary {
                health: MusicServerHealth::UpToDate,
                configured: true,
                display_name: Some("Home server".to_owned()),
                credential_kind: Some(MusicServerCredentialMode::Password),
                lan_http: false,
                custom_ca: false,
                history: MusicServerHistoryHealth::Off,
                playback_reports_needing_decision: 0,
                playlist_creates_needing_decision: 0,
                playlist_create_attention: Vec::new(),
                playlist_links_needing_decision: 0,
                playlist_projections_needing_decision: 0,
                playlist_contents_needing_decision: 0,
            }),
        });

        assert!(app.config.search.open_subsonic);
        assert!(matches!(
            commands.as_slice(),
            [Cmd::Persist(PersistCmd::Config(config))] if config.search.open_subsonic
        ));
    }

    #[test]
    fn committed_replacement_invalidates_server_library_before_reloading() {
        let mut app = App::new(50);
        app.server.settings.generation = 11;
        app.server.library.source = crate::app::LibrarySource::OpenSubsonic;
        app.server.library.generation = 4;
        app.server.library.page = Some(crate::open_subsonic::ServerLibraryPage {
            section: crate::open_subsonic::ServerLibrarySection::RecentlyPlayed,
            rows: Vec::new(),
            next_offset: None,
            warning: None,
        });

        let commands = app.finish_music_server_event(MusicServerEvent::Committed {
            generation: 11,
            result: Ok(MusicServerSummary {
                health: MusicServerHealth::UpToDate,
                configured: true,
                display_name: Some("Replacement".to_owned()),
                credential_kind: Some(MusicServerCredentialMode::ApiKey),
                lan_http: false,
                custom_ca: false,
                history: MusicServerHistoryHealth::Off,
                playback_reports_needing_decision: 0,
                playlist_creates_needing_decision: 0,
                playlist_create_attention: Vec::new(),
                playlist_links_needing_decision: 0,
                playlist_projections_needing_decision: 0,
                playlist_contents_needing_decision: 0,
            }),
        });

        assert!(app.server.library.page.is_none());
        assert!(app.server.library.generation > 4);
        assert!(commands.iter().any(|command| matches!(
            command,
            Cmd::ServerLibrary(crate::app::ServerLibraryCommand::LoadPage { .. })
        )));
    }

    #[test]
    fn durable_commit_with_runtime_failure_stays_configured_and_recoverable() {
        let mut app = App::new(50);
        app.server.settings.generation = 12;
        let commands = app.finish_music_server_event(MusicServerEvent::Committed {
            generation: 12,
            result: Ok(MusicServerSummary {
                health: MusicServerHealth::NeedsAttention,
                configured: true,
                display_name: Some("Saved server".to_owned()),
                credential_kind: Some(MusicServerCredentialMode::Password),
                lan_http: false,
                custom_ca: false,
                history: MusicServerHistoryHealth::Off,
                playback_reports_needing_decision: 0,
                playlist_creates_needing_decision: 0,
                playlist_create_attention: Vec::new(),
                playlist_links_needing_decision: 0,
                playlist_projections_needing_decision: 0,
                playlist_contents_needing_decision: 0,
            }),
        });

        assert!(app.server.settings.summary.configured);
        assert!(app.server.settings.wizard.is_none());
        assert_eq!(
            app.server.settings.failure,
            Some(MusicServerFailure::Connection)
        );
        assert!(app.config.search.open_subsonic);
        assert!(matches!(
            commands.as_slice(),
            [Cmd::Persist(PersistCmd::Config(config))] if config.search.open_subsonic
        ));
    }

    #[test]
    fn failed_refresh_keeps_a_saved_profile_editable_and_removable() {
        for failure in [
            MusicServerFailure::Connection,
            MusicServerFailure::Authentication,
            MusicServerFailure::Certificate,
        ] {
            let mut app = App::new(50);
            app.server.settings.generation = 13;
            app.server.settings.busy = Some(MusicServerBusy::Refresh);
            app.finish_music_server_event(MusicServerEvent::Refreshed {
                generation: 13,
                result: Ok(MusicServerRefreshOutcome {
                    summary: MusicServerSummary {
                        health: MusicServerHealth::NeedsAttention,
                        configured: true,
                        display_name: Some("Saved server".to_owned()),
                        credential_kind: Some(MusicServerCredentialMode::Password),
                        lan_http: false,
                        custom_ca: false,
                        history: MusicServerHistoryHealth::Off,
                        playback_reports_needing_decision: 0,
                        playlist_creates_needing_decision: 0,
                        playlist_create_attention: Vec::new(),
                        playlist_links_needing_decision: 0,
                        playlist_projections_needing_decision: 0,
                        playlist_contents_needing_decision: 0,
                    },
                    failure: Some(failure),
                }),
            });

            assert!(app.server.settings.summary.configured);
            assert_eq!(app.server.settings.row_count(), 4);
            assert_eq!(app.server.settings.failure, Some(failure));
            app.open_music_server_setup();
            let Some(MusicServerWizard::Setup(form)) = app.server.settings.wizard.as_ref() else {
                panic!("saved profile edit form");
            };
            assert_eq!(form.identity_intent, None);
        }
    }

    #[test]
    fn playback_report_attention_keeps_the_configured_action_rows_stable() {
        let mut state = MusicServerSettingsState::default();
        state.summary.configured = true;
        state.summary.health = MusicServerHealth::NeedsAttention;
        state.summary.playback_reports_needing_decision = 3;

        assert_eq!(state.row_count(), 4);
        state.selected = state.row_count() - 1;
        assert_eq!(state.selected, 3);
    }

    #[test]
    fn removed_profile_disables_and_normalizes_selected_search_source() {
        let mut app = App::new(50);
        app.server.settings.generation = 9;
        app.config
            .search
            .set_enabled(SearchSource::OpenSubsonic, true);
        app.config.search.source = SearchSource::OpenSubsonic;
        app.search.source = SearchSource::OpenSubsonic;
        app.dropdowns.search_source_open = true;

        let commands = app.finish_music_server_event(MusicServerEvent::Removed {
            generation: 9,
            result: Ok(()),
        });

        assert!(!app.config.search.open_subsonic);
        assert_ne!(app.config.search.source, SearchSource::OpenSubsonic);
        assert_ne!(app.search.source, SearchSource::OpenSubsonic);
        assert!(!app.dropdowns.search_source_open);
        assert!(matches!(
            commands.as_slice(),
            [Cmd::Persist(PersistCmd::Config(config))]
                if !config.search.open_subsonic
                    && config.search.source != SearchSource::OpenSubsonic
        ));

        app.server.settings.summary.configured = true;
        app.server.settings.wizard = Some(MusicServerWizard::Waiting);
        app.finish_music_server_event(MusicServerEvent::Removed {
            generation: 9,
            result: Err(MusicServerFailure::Storage),
        });
        assert!(app.server.settings.wizard.is_none());
        assert_eq!(
            app.server.settings.summary.health,
            MusicServerHealth::NeedsAttention
        );
    }

    #[test]
    fn sync_area_labels_exist_in_all_languages() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        for language in [
            crate::i18n::Language::English,
            crate::i18n::Language::Korean,
            crate::i18n::Language::Japanese,
        ] {
            crate::i18n::set_language(language);
            assert!(SyncArea::ALL.iter().all(|area| !area.label().is_empty()));
        }
        crate::i18n::set_language(original);
    }
}
