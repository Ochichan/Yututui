//! Session-only per-track WhyGem provenance for the native App owner.
//!
//! Producer paths normalize untrusted model metadata here, then commit it only after the
//! corresponding queue mutation is accepted. Rendering and input use the narrow accessors below
//! instead of reaching into the ledger directly.

use super::*;
use crate::remote::proto::WhyGemModel;

pub(crate) const DJ_GEM_SLOT: &str = "dj_gem";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WhyGemPick {
    pub(crate) video_id: String,
    pub(crate) model: WhyGemModel,
}

impl WhyGemPick {
    pub(crate) fn new(video_id: impl Into<String>, model: WhyGemModel) -> Self {
        Self {
            video_id: video_id.into(),
            model,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PendingWhyGemBatch {
    pub(crate) request_id: u64,
    pub(crate) seed_video_id: String,
    pub(crate) mode: StreamingMode,
    pub(crate) detailed: Vec<WhyGemPick>,
}

#[derive(Debug, Clone)]
pub(crate) enum WhyGemCommit {
    Clear,
    Forget(Vec<String>),
    Replace(Vec<WhyGemPick>),
    Upsert(Vec<WhyGemPick>),
}

pub(crate) fn streaming_origin_model(mode: StreamingMode) -> WhyGemModel {
    WhyGemModel {
        slot: match mode {
            StreamingMode::Focused => "Focused",
            StreamingMode::Balanced => "Balanced",
            StreamingMode::Discovery => "Discovery",
        }
        .to_owned(),
        reasons: Vec::new(),
        confidence: None,
    }
}

pub(crate) fn dj_gem_origin_model() -> WhyGemModel {
    WhyGemModel {
        slot: DJ_GEM_SLOT.to_owned(),
        reasons: Vec::new(),
        confidence: None,
    }
}

pub(crate) fn model_from_ai_pick(pick: &AiPick, confidence: Option<f32>) -> WhyGemModel {
    let slot = pick
        .role
        .as_deref()
        .filter(|role| is_known_role(role))
        .unwrap_or(DJ_GEM_SLOT)
        .to_owned();
    let mut reasons = Vec::with_capacity(pick.reasons.len().min(7));
    for reason in &pick.reasons {
        if is_known_reason(reason) && !reasons.contains(reason) {
            reasons.push(reason.clone());
        }
    }
    let confidence = confidence
        .filter(|value| value.is_finite())
        .and_then(|value| serde_json::Number::from_f64(f64::from(value.clamp(0.0, 1.0))));
    WhyGemModel {
        slot,
        reasons,
        confidence,
    }
}

fn is_known_role(role: &str) -> bool {
    matches!(
        role,
        "core" | "bridge" | "adjacent" | "discovery" | "stabilizer" | "recovery"
    )
}

fn is_known_reason(reason: &str) -> bool {
    matches!(reason, "co" | "tr" | "u" | "nov" | "cont" | "comp" | "m")
}

pub(crate) fn models_for_songs(
    songs: &[Song],
    detailed: &[WhyGemPick],
    default: &WhyGemModel,
) -> Vec<WhyGemPick> {
    songs
        .iter()
        .map(|song| {
            let model = detailed
                .iter()
                .rev()
                .find(|pick| pick.video_id == song.video_id)
                .map_or_else(|| default.clone(), |pick| pick.model.clone());
            WhyGemPick::new(song.video_id.clone(), model)
        })
        .collect()
}

impl App {
    pub(crate) fn why_gem_for(&self, video_id: &str) -> Option<&WhyGemModel> {
        self.why_gem.get(video_id)
    }

    pub(crate) fn why_gem_target_song(&self) -> Option<(&Song, &WhyGemModel)> {
        let video_id = self.overlays.why_gem_video_id.as_deref()?;
        if self.overlays.why_gem_queue_revision != Some(self.queue.rev()) {
            return None;
        }
        let queue_index = self.overlays.why_gem_queue_index?;
        let song = self
            .queue
            .song_at_cursor(queue_index)
            .filter(|song| song.video_id == video_id)?;
        Some((song, self.why_gem.get(video_id)?))
    }

    pub(in crate::app) fn open_why_gem_at(&mut self, queue_index: usize) {
        let Some(video_id) = self
            .queue
            .song_at_cursor(queue_index)
            .map(|song| song.video_id.clone())
        else {
            self.status.kind = StatusKind::Info;
            self.status.text = crate::i18n::why_gem::no_provenance().to_owned();
            self.dirty = true;
            return;
        };
        if self.why_gem.contains(&video_id) {
            self.cancel_seekbar_scrub();
            self.interaction.drag_selection = None;
            self.interaction.drag_scrollbar = None;
            self.interaction.ai_transcript_drag = None;
            self.overlays.why_gem_video_id = Some(video_id);
            self.overlays.why_gem_queue_index = Some(queue_index);
            self.overlays.why_gem_queue_revision = Some(self.queue.rev());
        } else {
            self.status.kind = StatusKind::Info;
            self.status.text = crate::i18n::why_gem::no_provenance().to_owned();
        }
        self.dirty = true;
    }

    pub(in crate::app) fn open_selected_why_gem(&mut self) {
        let queue_index = if self.queue_popup.open {
            Some(self.queue_popup.cursor)
        } else if self.queue.current().is_some() {
            Some(self.queue.cursor_pos())
        } else {
            None
        };
        if let Some(queue_index) = queue_index {
            self.open_why_gem_at(queue_index);
        } else {
            self.status.kind = StatusKind::Info;
            self.status.text = crate::i18n::why_gem::no_provenance().to_owned();
            self.dirty = true;
        }
    }

    pub(in crate::app) fn close_why_gem(&mut self) {
        self.overlays.why_gem_queue_index = None;
        self.overlays.why_gem_queue_revision = None;
        if self.overlays.why_gem_video_id.take().is_some() {
            self.dirty = true;
        }
    }

    pub(in crate::app) fn upsert_why_gem_picks(&mut self, picks: &[WhyGemPick]) {
        if self.why_gem.upsert_many(
            picks
                .iter()
                .map(|pick| (pick.video_id.clone(), pick.model.clone())),
        ) {
            self.dirty = true;
        }
    }

    pub(in crate::app) fn forget_why_gem_ids<'a>(
        &mut self,
        video_ids: impl IntoIterator<Item = &'a str>,
    ) {
        if self.why_gem.forget_many(video_ids) {
            self.dirty = true;
        }
    }

    pub(in crate::app) fn clear_why_gem(&mut self) {
        if self.why_gem.clear() {
            self.dirty = true;
        }
        self.close_why_gem();
    }

    pub(in crate::app) fn replace_why_gem_picks(&mut self, picks: &[WhyGemPick]) {
        let live: Vec<(String, WhyGemModel)> = self
            .queue
            .ordered_iter()
            .filter_map(|song| {
                picks
                    .iter()
                    .rev()
                    .find(|pick| pick.video_id == song.video_id)
                    .map(|pick| (song.video_id.clone(), pick.model.clone()))
            })
            .collect();
        if self.why_gem.replace(live) {
            self.dirty = true;
        }
        self.close_why_gem();
    }

    pub(in crate::app) fn reconcile_why_gem(&mut self) {
        let changed = self.why_gem.retain_video_ids(
            self.queue.rev(),
            self.queue.ordered_iter().map(|song| song.video_id.as_str()),
        );
        let target_is_live = self
            .overlays
            .why_gem_video_id
            .as_deref()
            .is_none_or(|video_id| {
                self.overlays.why_gem_queue_revision == Some(self.queue.rev())
                    && self.overlays.why_gem_queue_index.is_some_and(|index| {
                        self.queue
                            .song_at_cursor(index)
                            .is_some_and(|song| song.video_id == video_id)
                    })
                    && self.why_gem.contains(video_id)
            });
        if !target_is_live {
            self.overlays.why_gem_video_id = None;
            self.overlays.why_gem_queue_index = None;
            self.overlays.why_gem_queue_revision = None;
        }
        if changed || !target_is_live {
            self.dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_normalizes_unknown_codes_and_confidence() {
        let pick = AiPick {
            cid: "cid".to_owned(),
            role: Some("raw\u{1b}".to_owned()),
            reasons: vec!["tr".to_owned(), "raw".to_owned(), "tr".to_owned()],
        };
        let model = model_from_ai_pick(&pick, Some(1.7));
        assert_eq!(model.slot, DJ_GEM_SLOT);
        assert_eq!(model.reasons, ["tr"]);
        assert_eq!(model.confidence.and_then(|value| value.as_f64()), Some(1.0));
        assert!(
            model_from_ai_pick(&pick, Some(f32::NAN))
                .confidence
                .is_none()
        );
        assert_eq!(
            model_from_ai_pick(&pick, Some(-0.5))
                .confidence
                .and_then(|value| value.as_f64()),
            Some(0.0)
        );
    }

    #[test]
    fn final_song_models_preserve_detail_and_fill_origin() {
        let songs = vec![
            Song::remote("a", "A", "Artist", "3:00"),
            Song::remote("b", "B", "Artist", "3:00"),
        ];
        let detail = WhyGemPick::new(
            "a",
            WhyGemModel {
                slot: "bridge".to_owned(),
                reasons: vec!["tr".to_owned()],
                confidence: None,
            },
        );
        let origin = streaming_origin_model(StreamingMode::Balanced);
        let models = models_for_songs(&songs, std::slice::from_ref(&detail), &origin);
        assert_eq!(models[0], detail);
        assert_eq!(models[1].model, origin);
    }

    #[test]
    fn streaming_origin_slots_do_not_overlap_lowercase_model_roles() {
        for (mode, slot) in [
            (StreamingMode::Focused, "Focused"),
            (StreamingMode::Balanced, "Balanced"),
            (StreamingMode::Discovery, "Discovery"),
        ] {
            let model = streaming_origin_model(mode);
            assert_eq!(model.slot, slot);
            assert!(!is_known_role(&model.slot));
        }
    }

    #[test]
    fn queue_affordance_opens_the_exact_row_without_closing_queue() {
        let mut app = App::new(50);
        app.queue.set(
            vec![
                Song::remote("a", "A", "Artist", "3:00"),
                Song::remote("b", "B", "Artist", "3:00"),
            ],
            0,
        );
        app.why_gem.upsert("b".to_owned(), dj_gem_origin_model());
        app.open_queue_popup();

        app.on_mouse_target(MouseTarget::QueueWhyGem(1));

        assert_eq!(app.overlays.why_gem_video_id.as_deref(), Some("b"));
        assert_eq!(app.overlays.why_gem_queue_index, Some(1));
        assert_eq!(app.overlays.why_gem_queue_revision, Some(app.queue.rev()));
        assert!(app.queue_popup.open);
    }

    #[test]
    fn why_gem_mouse_modal_consumes_inside_and_outside_clicks() {
        let mut app = App::new(50);
        app.queue.set(
            vec![
                Song::remote("a", "A", "Artist", "3:00"),
                Song::remote("b", "B", "Artist", "3:00"),
            ],
            0,
        );
        app.why_gem.upsert("b".to_owned(), dj_gem_origin_model());
        app.open_queue_popup();
        app.overlays.why_gem_video_id = Some("b".to_owned());
        app.overlays.why_gem_queue_index = Some(1);
        app.overlays.why_gem_queue_revision = Some(app.queue.rev());
        app.register_mouse_button(Rect::new(0, 0, 1, 1), MouseTarget::QueueDel(0));
        app.register_mouse_button(Rect::new(2, 2, 4, 3), MouseTarget::WhyGemCard);

        app.on_mouse_click(3, 3, false);
        assert_eq!(app.queue.len(), 2, "inside click is inert");
        assert_eq!(app.overlays.why_gem_video_id.as_deref(), Some("b"));

        app.on_mouse_click(0, 0, false);
        assert_eq!(app.queue.len(), 2, "outside click never reaches QueueDel");
        assert!(app.overlays.why_gem_video_id.is_none());
        assert!(app.queue_popup.open, "the covered queue stays open");
    }

    #[test]
    fn queue_revision_change_closes_duplicate_card_before_pruning_shared_provenance() {
        let mut app = App::new(50);
        app.queue.set(
            vec![
                Song::remote("dup", "First", "Artist", "3:00"),
                Song::remote("dup", "Second", "Artist", "3:00"),
            ],
            0,
        );
        app.why_gem.upsert("dup".to_owned(), dj_gem_origin_model());
        app.overlays.why_gem_video_id = Some("dup".to_owned());
        app.overlays.why_gem_queue_index = Some(1);
        app.overlays.why_gem_queue_revision = Some(app.queue.rev());

        app.queue.remove_at(0);
        app.reconcile_why_gem();
        assert!(app.why_gem.contains("dup"));
        assert!(app.overlays.why_gem_video_id.is_none());

        app.queue.remove_at(0);
        app.reconcile_why_gem();
        assert!(!app.why_gem.contains("dup"));
        assert!(app.overlays.why_gem_video_id.is_none());
    }

    #[test]
    fn manual_enqueue_forgets_stale_origin_while_dj_enqueue_records_one() {
        let mut app = App::new(50);
        app.queue
            .set(vec![Song::remote("seed", "Seed", "Artist", "3:00")], 0);
        app.prefetch.loaded_video_id = Some("seed".to_owned());
        app.why_gem.upsert("dup".to_owned(), dj_gem_origin_model());

        app.enqueue(Song::remote("dup", "Manual", "Artist", "3:00"));
        assert!(!app.why_gem.contains("dup"));

        app.extend_queue_from_dj_gem(vec![Song::remote(
            "recommended",
            "Recommended",
            "Artist",
            "3:00",
        )]);
        assert_eq!(
            app.why_gem_for("recommended")
                .map(|model| model.slot.as_str()),
            Some(DJ_GEM_SLOT)
        );
    }
}
