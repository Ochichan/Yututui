//! Chapter-list parsing: mpv's `chapter-list` property becomes bounded, ordered
//! [`Chapter`]s. Garbage never reaches the UI.

use crate::player::{Chapter, chapter_policy};

/// Parse mpv's `chapter-list` array into bounded, ordered [`Chapter`]s. Garbage never
/// reaches the UI: non-finite/negative times, non-object entries, and boundary noise
/// shorter than [`chapter_policy::MIN_CHAPTER_SECS`] are dropped, and the list is capped
/// at [`chapter_policy::MAX_CHAPTERS`].
pub(super) fn parse_chapter_list(value: &serde_json::Value) -> Vec<Chapter> {
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };
    let mut out: Vec<Chapter> = entries
        .iter()
        .filter_map(|entry| {
            let title = entry.get("title")?.as_str().unwrap_or_default();
            let start = entry.get("time")?.as_f64()?;
            (start.is_finite() && start >= 0.0).then_some(Chapter {
                title: title.to_owned(),
                start_secs: start,
            })
        })
        .collect();
    // Sort by start time (mpv lists them in order, but a hostile/mangled file may not) and
    // drop boundaries that sit closer than the noise floor to the previous one.
    out.sort_by(|a, b| a.start_secs.total_cmp(&b.start_secs));
    out.dedup_by(|a, b| (a.start_secs - b.start_secs).abs() < chapter_policy::MIN_CHAPTER_SECS);
    out.truncate(chapter_policy::MAX_CHAPTERS);
    out
}

#[cfg(test)]
mod chapter_tests {
    use super::*;
    use serde_json::json;

    fn parse(json: serde_json::Value) -> Vec<Chapter> {
        parse_chapter_list(&json)
    }

    #[test]
    fn parses_ordered_chapters_and_drops_garbage() {
        let chapters = parse(json!([
            { "title": "Intro", "time": 0.0 },
            { "title": "Drop", "time": 120.5 },
            { "title": 42, "time": "not-a-number" },
            "not-an-object",
            { "title": "Outro", "time": f64::NAN },
            { "title": "Negative", "time": -3.0 },
            { "title": "End", "time": 600.0 },
        ]));
        assert_eq!(
            chapters,
            vec![
                Chapter {
                    title: "Intro".into(),
                    start_secs: 0.0
                },
                Chapter {
                    title: "Drop".into(),
                    start_secs: 120.5
                },
                Chapter {
                    title: "End".into(),
                    start_secs: 600.0
                },
            ]
        );
    }

    #[test]
    fn caps_and_dedups_noise_boundaries() {
        // Two boundaries 0.4 s apart collapse to one; a hostile 10k-entry list truncates.
        let mut entries = vec![
            json!({ "title": "a", "time": 10.0 }),
            json!({ "title": "b", "time": 10.4 }),
        ];
        for i in 0..10_000 {
            entries.push(json!({ "title": format!("c{i}"), "time": (i as f64 + 1.0) * 1000.0 }));
        }
        let chapters = parse(json!(entries));
        // 10.0/10.4 dedup to one entry, then the synthetic list caps at MAX_CHAPTERS.
        assert_eq!(chapters.len(), chapter_policy::MAX_CHAPTERS);
        assert_eq!(chapters[0].start_secs, 10.0);
        assert!(
            chapters
                .windows(2)
                .all(|w| w[0].start_secs < w[1].start_secs)
        );
    }

    #[test]
    fn non_array_property_is_empty() {
        assert!(parse(json!({ "title": "no list" })).is_empty());
        assert!(parse(json!("not a list")).is_empty());
    }
}
