//! Admission-atomic ownership transitions for the external video overlay.
//!
//! Opening a video while audio is playing first admits an absolute audio pause. Closing an
//! overlay which owns that pause first admits the matching absolute unpause. Process lifetime,
//! projected playback state, ownership, and user-visible status therefore move together only
//! after the player lane accepts the command.

use super::*;

#[derive(Clone, Copy)]
enum VideoSpawner {
    Real,
    #[cfg(test)]
    Fake,
    #[cfg(test)]
    Fail,
}

/// Spawn a quiet child tree which remains alive until its owner closes or drops it.
/// Test overlays need only a real process handle; keeping the wait primitive in the platform
/// shell avoids launching mpv or depending on a Unix-only `sleep` executable on Windows.
#[cfg(test)]
pub(in crate::app) fn spawn_fake_video_overlay_process()
-> std::io::Result<crate::util::process_tree::OwnedProcessTree> {
    #[cfg(any(unix, windows))]
    use std::process::Stdio;

    #[cfg(unix)]
    let mut command = {
        let mut command =
            crate::util::process::std_command("sh", crate::util::process::ProcessProfile::Media);
        command.args(["-c", "read _"]);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = crate::util::process::std_command(
            "cmd.exe",
            crate::util::process::ProcessProfile::Media,
        );
        command.args(["/D", "/Q", "/C", "set /p _="]);
        command
    };
    #[cfg(any(unix, windows))]
    {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|child| {
                crate::util::process_tree::OwnedProcessTree::new(
                    child,
                    crate::util::process::ProcessProfile::Media,
                )
            })
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "fake video overlay processes are supported only on Unix and Windows",
        ))
    }
}

/// Overlay-open state committed after the audio pause command is admitted.
#[derive(Clone)]
pub struct VideoOpenPlan {
    id: String,
    layout: crate::config::VideoOverlay,
    spawner: VideoSpawner,
    expected_generation: u64,
    expected_process_id: Option<u32>,
    expected_paused: bool,
    expected_pause_owned: bool,
}

/// Overlay-finish state committed after the audio unpause command is admitted.
#[derive(Clone)]
pub struct VideoFinishPlan {
    status: String,
    kind: StatusKind,
    expected_generation: u64,
    expected_process_id: Option<u32>,
    expected_paused: bool,
    expected_pause_owned: bool,
}

impl App {
    #[cfg(test)]
    pub(crate) fn set_video_pause_ownership_for_test(&mut self, owned: bool) {
        self.video.paused_audio = owned;
    }

    #[cfg(test)]
    pub(crate) fn video_pause_owned_for_test(&self) -> bool {
        self.video.paused_audio
    }

    /// `v`: toggle the external mpv video overlay. A playing audio track is paused through a
    /// typed player intent before the process is spawned. Audio which was already paused remains
    /// user-owned, so opening and closing the overlay needs no player command.
    pub(in crate::app) fn toggle_video_overlay(&mut self) -> Vec<Cmd> {
        self.toggle_video_overlay_with(VideoSpawner::Real)
    }

    #[cfg(test)]
    pub(crate) fn toggle_video_overlay_with_fake_spawn(&mut self, succeeds: bool) -> Vec<Cmd> {
        self.toggle_video_overlay_with(if succeeds {
            VideoSpawner::Fake
        } else {
            VideoSpawner::Fail
        })
    }

    fn toggle_video_overlay_with(&mut self, spawner: VideoSpawner) -> Vec<Cmd> {
        if self.video_open() || self.video.paused_audio {
            return self.finish_video_overlay(
                t!("Video closed", "영상 닫음", "動画を閉じました"),
                StatusKind::Info,
            );
        }
        let Some(song) = self.queue.current().cloned() else {
            self.status.text = t!(
                "No track playing",
                "재생 중인 곡이 없습니다",
                "再生中の曲がありません"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        };
        let Some(id) = self.recover_youtube_id(&song) else {
            self.status.text = t!(
                "This track is local-only — no video",
                "로컬 전용 트랙이라 영상이 없어요",
                "ローカル専用の曲のため動画がありません"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        };
        let layout = self.config.video_layout;
        if !self.playback.paused {
            return self.player_intent(
                "video_pause_audio",
                PlayerCmd::SetProperty {
                    name: "pause".to_owned(),
                    value: serde_json::Value::Bool(true),
                },
                PlayerCommit::VideoOpen(Box::new(VideoOpenPlan {
                    id,
                    layout,
                    spawner,
                    expected_generation: self.video.generation,
                    expected_process_id: self.video.proc.as_ref().map(|child| child.id()),
                    expected_paused: self.playback.paused,
                    expected_pause_owned: self.video.paused_audio,
                })),
            );
        }

        let mut effects = Vec::new();
        if self.spawn_video_overlay_now(&id, layout, spawner, &mut effects) {
            self.set_video_transition_status(
                StatusKind::Info,
                t!(
                    "Opening video in mpv…",
                    "mpv에서 영상을 여는 중…",
                    "mpvで動画を開いています…"
                ),
            );
        } else {
            self.set_video_transition_status(
                StatusKind::Error,
                t!(
                    "Failed to launch mpv",
                    "mpv 실행에 실패했습니다",
                    "mpv の起動に失敗しました"
                ),
            );
        }
        effects
    }

    pub(in crate::app) fn video_open_is_current(&self, plan: &VideoOpenPlan) -> bool {
        self.video.generation == plan.expected_generation
            && self.video.proc.as_ref().map(|child| child.id()) == plan.expected_process_id
            && self.playback.paused == plan.expected_paused
            && self.video.paused_audio == plan.expected_pause_owned
            && self.config.video_layout == plan.layout
            && self
                .queue
                .current()
                .and_then(|song| self.recover_youtube_id(song))
                .as_deref()
                == Some(plan.id.as_str())
    }

    pub(in crate::app) fn commit_video_open(&mut self, plan: VideoOpenPlan) -> Vec<Cmd> {
        assert!(
            self.video_open_is_current(&plan),
            "video-open state changed before player admission commit"
        );
        // Admission is authoritative: the absolute pause is now queued for mpv, so projected
        // playback and ownership may move before the overlay process is launched.
        self.playback.paused = true;
        self.video.paused_audio = true;
        self.dirty = true;

        let mut effects = Vec::new();
        if self.spawn_video_overlay_now(&plan.id, plan.layout, plan.spawner, &mut effects) {
            self.set_video_transition_status(
                StatusKind::Info,
                t!(
                    "Opening video in mpv…",
                    "mpv에서 영상을 여는 중…",
                    "mpvで動画を開いています…"
                ),
            );
            return effects;
        }

        // The pause was already admitted. Compensate with another absolute, typed intent; if
        // that lane is temporarily busy, ownership stays explicit and `v` can retry the finish.
        effects.extend(self.video_unpause_intent(
            t!(
                "Failed to launch mpv",
                "mpv 실행에 실패했습니다",
                "mpv の起動に失敗しました"
            ),
            StatusKind::Error,
        ));
        effects
    }

    /// Finish immediately when the overlay never owned audio. Otherwise defer every ownership
    /// and process-state mutation until the absolute unpause is admitted.
    pub(in crate::app) fn finish_video_overlay(
        &mut self,
        status: &str,
        kind: StatusKind,
    ) -> Vec<Cmd> {
        if self.video.paused_audio {
            return self.video_unpause_intent(status, kind);
        }
        self.close_video();
        self.set_video_transition_status(kind, status);
        Vec::new()
    }

    fn video_unpause_intent(&self, status: &str, kind: StatusKind) -> Vec<Cmd> {
        self.player_intent(
            "video_unpause_audio",
            PlayerCmd::SetProperty {
                name: "pause".to_owned(),
                value: serde_json::Value::Bool(false),
            },
            PlayerCommit::VideoFinish(Box::new(VideoFinishPlan {
                status: status.to_owned(),
                kind,
                expected_generation: self.video.generation,
                expected_process_id: self.video.proc.as_ref().map(|child| child.id()),
                expected_paused: self.playback.paused,
                expected_pause_owned: self.video.paused_audio,
            })),
        )
    }

    pub(in crate::app) fn video_finish_is_current(&self, plan: &VideoFinishPlan) -> bool {
        self.video.generation == plan.expected_generation
            && self.video.proc.as_ref().map(|child| child.id()) == plan.expected_process_id
            && self.playback.paused == plan.expected_paused
            && self.video.paused_audio == plan.expected_pause_owned
    }

    pub(in crate::app) fn commit_video_finish(&mut self, plan: VideoFinishPlan) -> Vec<Cmd> {
        assert!(
            self.video_finish_is_current(&plan),
            "video-finish ownership changed before player admission commit"
        );
        self.close_video();
        self.video.paused_audio = false;
        self.playback.paused = false;
        self.set_video_transition_status(plan.kind, &plan.status);
        Vec::new()
    }

    /// A committed track transition could not reach the overlay command lane. The old overlay
    /// is no longer truthful, so close it now, then compensate its owned audio pause through the
    /// same typed finish admission path.
    pub(crate) fn compensate_video_load_rejection(&mut self) -> Vec<Cmd> {
        self.close_video();
        self.dirty = true;
        let status = t!(
            "Video unavailable — continuing with audio",
            "영상을 사용할 수 없어 소리로 이어서 재생해요",
            "動画を利用できないため音声で続けて再生します"
        );
        if self.video.paused_audio {
            self.video_unpause_intent(status, StatusKind::Error)
        } else {
            self.set_video_transition_status(StatusKind::Error, status);
            Vec::new()
        }
    }

    /// `Shift+V`: persist the next layout and respawn a live overlay. A failed respawn uses the
    /// same admission-atomic finish path, so owned audio is never optimistically resumed.
    pub(in crate::app) fn toggle_video_layout(&mut self) -> Vec<Cmd> {
        self.toggle_video_layout_with(VideoSpawner::Real)
    }

    #[cfg(test)]
    pub(crate) fn toggle_video_layout_with_fake_spawn(&mut self, succeeds: bool) -> Vec<Cmd> {
        self.toggle_video_layout_with(if succeeds {
            VideoSpawner::Fake
        } else {
            VideoSpawner::Fail
        })
    }

    fn toggle_video_layout_with(&mut self, spawner: VideoSpawner) -> Vec<Cmd> {
        self.config.video_layout = self.config.video_layout.toggled();
        let layout = self.config.video_layout;
        let mut effects = vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))];

        if self.video_open() {
            let id = self
                .queue
                .current()
                .cloned()
                .and_then(|song| self.recover_youtube_id(&song));
            self.close_video();
            match id {
                Some(id) if self.spawn_video_overlay_now(&id, layout, spawner, &mut effects) => {}
                Some(_) => {
                    effects.extend(self.finish_video_overlay(
                        t!(
                            "Failed to launch mpv",
                            "mpv 실행에 실패했습니다",
                            "mpv の起動に失敗しました"
                        ),
                        StatusKind::Error,
                    ));
                    return effects;
                }
                None => {
                    effects.extend(self.finish_video_overlay(
                        t!(
                            "This track is local-only — no video",
                            "로컬 전용 트랙이라 영상이 없어요",
                            "ローカル専用の曲のため動画がありません"
                        ),
                        StatusKind::Info,
                    ));
                    return effects;
                }
            }
        } else if self.video.paused_audio {
            effects.extend(self.finish_video_overlay(
                t!("Video closed", "영상 닫음", "動画を閉じました"),
                StatusKind::Info,
            ));
            return effects;
        }

        self.set_video_transition_status(
            StatusKind::Info,
            &format!("{}: {}", t!("Video", "영상", "動画"), layout.label()),
        );
        effects
    }

    fn spawn_video_overlay_now(
        &mut self,
        id: &str,
        layout: crate::config::VideoOverlay,
        spawner: VideoSpawner,
        effects: &mut Vec<Cmd>,
    ) -> bool {
        let url = format!("https://www.youtube.com/watch?v={id}");
        let data_dir = crate::paths::data_dir();
        let (cookies, cookies_warning) = self
            .config
            .cookies_file_for_external_tools_with_warning(data_dir.as_deref());
        if let Some(warning) = cookies_warning {
            tracing::warn!(warning, "video overlay cookies configuration warning");
        }
        self.video.generation = self.video.generation.wrapping_add(1);
        let generation = self.video.generation;
        let ipc_path = crate::player::mpv::video_ipc_path(generation)
            .inspect_err(|error| {
                tracing::warn!(%error, "video overlay IPC path unavailable");
            })
            .ok();
        let child = match spawner {
            VideoSpawner::Real => {
                spawn_video_overlay(&url, cookies.as_deref(), layout, ipc_path.as_deref())
            }
            #[cfg(test)]
            VideoSpawner::Fake => spawn_fake_video_overlay_process()
                .map_err(|error| tracing::warn!(%error, "fake video overlay failed to spawn"))
                .ok(),
            #[cfg(test)]
            VideoSpawner::Fail => None,
        };
        let Some(child) = child else {
            return false;
        };
        self.video.proc = Some(child);
        self.video.ipc_path = ipc_path.clone();
        if let Some(ipc_path) = ipc_path {
            effects.push(Cmd::VideoConnect {
                ipc_path,
                generation,
                bindings: self.video_overlay_bindings(),
            });
        }
        true
    }

    fn set_video_transition_status(&mut self, kind: StatusKind, text: &str) {
        self.status.kind = kind;
        self.status.text = text.to_owned();
        self.dirty = true;
    }
}
