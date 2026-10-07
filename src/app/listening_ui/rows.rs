use super::*;

pub(super) fn project_rows(
    tab: ListeningTab,
    projection: ListeningProjection,
) -> Vec<ListeningRow> {
    let mut rows: Vec<ListeningRow> = match tab {
        ListeningTab::Bookmarks => projection
            .bookmarks
            .into_values()
            .flatten()
            .map(ListeningRow::Bookmark)
            .chain(
                projection
                    .resumes
                    .into_values()
                    .filter(|resume| {
                        resume.candidates.len() > 1
                            || matches!(
                                resume.candidates.first(),
                                Some(ResumeCandidate::Position(_))
                            )
                    })
                    .flat_map(|resume| resume.candidates)
                    .map(ListeningRow::Resume),
            )
            .collect(),
        ListeningTab::Presets => projection
            .dj_presets
            .into_values()
            .flatten()
            .map(ListeningRow::Preset)
            .collect(),
        ListeningTab::Passport => projection
            .passport_visits
            .into_values()
            .flat_map(|visit| {
                let notes = projection
                    .passport_notes
                    .get(&visit.station_uuid)
                    .cloned()
                    .unwrap_or_default();
                if notes.is_empty() {
                    vec![ListeningRow::Visit { visit, note: None }]
                } else {
                    notes
                        .into_iter()
                        .map(|note| ListeningRow::Visit {
                            visit: visit.clone(),
                            note: Some(note),
                        })
                        .collect()
                }
            })
            .collect(),
    };
    rows.sort_by_key(ListeningRow::label);
    rows
}

impl ListeningRow {
    pub fn same_record(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Bookmark(left), Self::Bookmark(right)) => left.bookmark_id == right.bookmark_id,
            (Self::Resume(left), Self::Resume(right)) => left.track().key == right.track().key,
            (Self::Preset(left), Self::Preset(right)) => left.preset_id == right.preset_id,
            (Self::Visit { visit: left, .. }, Self::Visit { visit: right, .. }) => {
                left.station_uuid == right.station_uuid
            }
            _ => false,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Bookmark(bookmark) => format!(
                "{}  {} · {}",
                timestamp(bookmark.position_ms),
                bookmark.label,
                bookmark.track.title
            ),
            Self::Resume(ResumeCandidate::Position(point)) => format!(
                "{}  {} · {}",
                timestamp(point.position_ms),
                t!("Resume", "이어듣기", "再開"),
                point.track.title
            ),
            Self::Resume(ResumeCandidate::Clear(clear)) => format!(
                "{} · {}",
                t!("Start from beginning", "처음부터", "最初から"),
                clear.track.title
            ),
            Self::Preset(preset) => preset.name.clone(),
            Self::Visit { visit, .. } => format!(
                "{}  {}",
                visit.country_code.as_deref().unwrap_or("--"),
                visit.station_name
            ),
        }
    }

    pub fn detail(&self, app: &App) -> String {
        let device_name = |id: &crate::personal_state::DeviceId| {
            app.personal_state
                .ledger
                .device_registry
                .get(id)
                .map(|device| device.name.clone())
                .unwrap_or_else(|| t!("Another device", "다른 기기", "別の端末").to_owned())
        };
        match self {
            Self::Bookmark(bookmark) => bookmark.track.artist.clone(),
            Self::Resume(ResumeCandidate::Position(point)) => format!(
                "{} · {}",
                point.track.artist,
                device_name(&point.provenance.device_id)
            ),
            Self::Resume(ResumeCandidate::Clear(clear)) => device_name(&clear.provenance.device_id),
            Self::Preset(preset) => format!(
                "{} {} · {} {} · {} {}",
                t!("Seeds", "추천어", "シード"),
                preset.snapshot.seeds.len(),
                t!("Excluded tracks", "제외 곡", "除外曲"),
                preset.snapshot.banned_tracks.len(),
                t!("Excluded artists", "제외 가수", "除外歌手"),
                preset.snapshot.banned_artists.len()
            ),
            Self::Visit { visit, note } => format!(
                "{}\n{} {} · {} {} UTC",
                note.as_ref().map(|note| note.note.as_str()).unwrap_or(t!(
                    "Edit to add a station note",
                    "수정으로 방송국 메모 추가",
                    "編集して放送局メモを追加"
                )),
                t!("First", "첫 청취", "初回"),
                utc_date(visit.first_listened_at_unix),
                t!("Last", "최근", "最終"),
                utc_date(visit.last_listened_at_unix),
            ),
        }
    }

    pub fn full_detail(&self, app: &App) -> String {
        let mut lines = vec![self.label(), self.detail(app)];
        if let Self::Preset(preset) = self {
            for seed in &preset.snapshot.seeds {
                lines.push(format!(
                    "{} {}",
                    match seed.polarity {
                        crate::streaming::SeedPolarity::MoreLike => "+",
                        crate::streaming::SeedPolarity::Exclude => "−",
                    },
                    seed.term.as_str()
                ));
            }
            for track in &preset.snapshot.banned_tracks {
                lines.push(format!(
                    "{}: {} [{}]",
                    t!("Excluded track", "제외 곡", "除外曲"),
                    track.label,
                    track.id.as_str()
                ));
            }
            for artist in &preset.snapshot.banned_artists {
                lines.push(format!(
                    "{}: {}",
                    t!("Excluded artist", "제외 가수", "除外歌手"),
                    artist.display
                ));
            }
        }
        lines.join("\n")
    }
}

fn utc_date(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month + 2) / 5 + 1;
    let month = month + if month < 10 { 3 } else { -9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

pub fn timestamp(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn visit_dates_use_utc_calendar_boundaries() {
        assert_eq!(super::utc_date(0), "1970-01-01");
        assert_eq!(super::utc_date(1_709_164_800), "2024-02-29");
        assert_eq!(super::utc_date(1_709_251_200), "2024-03-01");
    }
}
