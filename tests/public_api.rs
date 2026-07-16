//! Compile-time compatibility checks for public facade paths retained across refactors.

use yututui::{ai, app};

#[test]
fn legacy_app_ai_dto_paths_keep_the_neutral_type_identity() {
    let _: Option<app::AiContext> = None::<ai::AiContext>;
    let _: Option<app::AiPick> = None::<ai::AiPick>;
    let _: Option<app::PlaylistInfo> = None::<ai::PlaylistInfo>;
}
