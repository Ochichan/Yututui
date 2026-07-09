use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, ScrollSurface};

const MARQUEE_FRAME_DIV: u64 = 6;
const MARQUEE_START_HOLD: u64 = 4;

#[derive(Default)]
pub struct MarqueeCache {
    key: Option<(ScrollSurface, usize)>,
    text: String,
    avail: usize,
    looped_twice: String,
    period: usize,
}

impl MarqueeCache {
    fn refresh(
        &mut self,
        surface: ScrollSurface,
        index: usize,
        text: &str,
        avail: usize,
        width: usize,
    ) {
        let key = Some((surface, index));
        if self.key == key && self.avail == avail && self.text == text {
            return;
        }
        self.key = key;
        self.avail = avail;
        self.text.clear();
        self.text.push_str(text);
        self.looped_twice.clear();
        self.looped_twice.reserve(text.len().saturating_mul(2) + 6);
        self.looped_twice.push_str(text);
        self.looped_twice.push_str("   ");
        self.looped_twice.push_str(text);
        self.looped_twice.push_str("   ");
        self.period = width.saturating_add(3).max(1);
    }
}

pub(crate) fn col_window(s: &str, start_col: usize, width: usize) -> String {
    let mut out = String::with_capacity(width.min(s.len()));
    let mut col = 0usize;
    let mut taken = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if col < start_col {
            col += w;
            continue;
        }
        if taken + w > width {
            break;
        }
        out.push(ch);
        taken += w;
    }
    out
}

pub fn selected_marquee(
    app: &App,
    surface: ScrollSurface,
    index: usize,
    text: &str,
    avail: usize,
) -> String {
    let total = UnicodeWidthStr::width(text);
    if avail <= 4 || total <= avail {
        return text.to_owned();
    }
    let key = Some((surface, index));
    if app.bridges.marquee_key.get() != key {
        app.bridges.marquee_key.set(key);
        app.bridges.marquee_origin.set(app.anim_frame());
    }
    app.bridges.marquee_ran.set(true);
    let elapsed = app
        .anim_frame()
        .wrapping_sub(app.bridges.marquee_origin.get());
    let mut cache = app.bridges.marquee_cache.borrow_mut();
    cache.refresh(surface, index, text, avail, total);
    let start =
        ((elapsed / MARQUEE_FRAME_DIV).saturating_sub(MARQUEE_START_HOLD)) as usize % cache.period;
    col_window(&cache.looped_twice, start, avail)
}
