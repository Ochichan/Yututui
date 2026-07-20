use super::*;
use std::borrow::Cow;

use crate::api::Song;

fn has_target(
    parts: &[(Option<MouseTarget>, Cow<'static, str>)],
    pred: impl Fn(&MouseTarget) -> bool,
) -> bool {
    parts
        .iter()
        .any(|(target, _)| target.as_ref().is_some_and(&pred))
}

#[test]
fn status_line_shows_why_only_for_provenance_and_yields_it_when_narrow() {
    let mut app = App::new(100);
    app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
    assert!(!has_target(
        &status_line_parts(&app, " ", false, false),
        |target| { matches!(target, MouseTarget::Global(Action::WhyAi)) }
    ));

    app.why_gem.upsert(
        "a".to_owned(),
        crate::remote::proto::WhyGemModel {
            slot: "Balanced".to_owned(),
            reasons: Vec::new(),
            confidence: None,
        },
    );
    let (_, roomy) = fitted_status_line_parts(&app, 100, false);
    assert!(has_target(&roomy, |target| {
        matches!(target, MouseTarget::Global(Action::WhyAi))
    }));
    let (_, tiny) = fitted_status_line_parts(&app, 1, false);
    assert!(!has_target(&tiny, |target| {
        matches!(target, MouseTarget::Global(Action::WhyAi))
    }));
}
