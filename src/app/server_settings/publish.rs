//! Publishing one downloaded track into the music server's folder, from the Library.
//!
//! Deliberately without a confirmation step. Publication never overwrites and never deletes — an
//! occupied name is reported, not replaced — so there is nothing here for a user to undo, and
//! `ytt server publish` omits the prompt for the same reason. Adding one on this surface only
//! would make the two disagree about how dangerous the same operation is.

use super::*;
use crate::api::Song;

impl App {
    pub(in crate::app) fn start_publish_track_to_server(&mut self, song: &Song) -> Vec<Cmd> {
        if self.server.settings.busy.is_some() {
            return Vec::new();
        }
        if song.local_path.is_none() {
            self.status.kind = StatusKind::Error;
            self.status.text = crate::t!(
                "Download this track before copying it to the server",
                "서버로 복사하려면 먼저 이 트랙을 다운로드하세요",
                "サーバーへコピーする前にこのトラックをダウンロードしてください"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        }
        let generation = self.server.settings.next_generation();
        self.server.settings.busy = Some(MusicServerBusy::PlaylistRecovery);
        self.status.kind = StatusKind::Info;
        self.status.text = crate::t!(
            "Copying into the music server's folder…",
            "음악 서버 폴더로 복사하는 중…",
            "音楽サーバーのフォルダーへコピー中…"
        )
        .to_owned();
        self.dirty = true;
        vec![Cmd::MusicServer(MusicServerCommand::PublishTrack {
            generation,
            video_id: song.video_id.clone(),
        })]
    }

    pub(super) fn finish_track_published(
        &mut self,
        result: Result<TrackPublishOutcome, MusicServerFailure>,
    ) -> Vec<Cmd> {
        self.server.settings.busy = None;
        match result {
            Ok(outcome) => {
                self.status.kind = match outcome.report {
                    TrackPublishReport::Conflict => StatusKind::Error,
                    _ => StatusKind::Info,
                };
                self.status.text = publish_status_text(&outcome);
            }
            Err(failure) => {
                self.status.kind = StatusKind::Error;
                self.status.text = failure.label().to_owned();
            }
        }
        self.dirty = true;
        Vec::new()
    }
}

/// Say what was established and nothing more.
///
/// A copy proves only that bytes reached the folder the user named. Whether the server can see
/// them is a separate claim, and the configured path cannot be checked against the server at all —
/// `getMusicFolders` returns ids and names, never paths.
fn publish_status_text(outcome: &TrackPublishOutcome) -> String {
    use crate::open_subsonic::LibraryScanRequest as Scan;

    let copied = match outcome.report {
        TrackPublishReport::Published => crate::t!(
            "Copied to the music server",
            "음악 서버로 복사했어요",
            "音楽サーバーへコピーしました"
        ),
        TrackPublishReport::AlreadyPublished => crate::t!(
            "Already on the music server, unchanged",
            "이미 음악 서버에 있어요. 그대로 뒀어요",
            "すでに音楽サーバーにあります。変更していません"
        ),
        TrackPublishReport::Conflict => {
            return crate::t!(
                "A different file already has that name on the server; nothing was replaced",
                "서버에 같은 이름의 다른 파일이 있어요. 아무것도 바꾸지 않았어요",
                "サーバーに同名の別ファイルがあります。何も置き換えていません"
            )
            .to_owned();
        }
    };

    let scan = match outcome.scan {
        Scan::Started => crate::t!(
            "your server is rescanning",
            "서버가 다시 검색하는 중이에요",
            "サーバーが再スキャン中です"
        ),
        Scan::NoServer => crate::t!(
            "no server is connected to rescan",
            "다시 검색할 서버가 연결되어 있지 않아요",
            "再スキャンするサーバーが接続されていません"
        ),
        Scan::Unsupported => crate::t!(
            "it appears at your server's next scan",
            "서버가 다음에 검색할 때 나타나요",
            "サーバーの次のスキャンで表示されます"
        ),
        Scan::NotPermitted => crate::t!(
            "this account cannot start a scan, so it appears at the next scheduled one",
            "이 계정은 검색을 시작할 수 없어요. 다음 예약 검색 때 나타나요",
            "このアカウントはスキャンを開始できません。次の定期スキャンで表示されます"
        ),
        Scan::Unavailable => crate::t!(
            "the server could not be reached to rescan",
            "다시 검색을 요청하려 했지만 서버에 연결하지 못했어요",
            "再スキャンを要求できませんでした"
        ),
    };
    format!("{copied} — {scan}")
}

#[cfg(test)]
mod tests;
