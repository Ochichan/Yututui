//! Latin-script display names for tracks whose original metadata is hard to read in Retro mode.
//!
//! The original [`crate::api::Song`] title/artist are never mutated. Search, streaming, downloads, and
//! lyrics keep using source metadata; this module only supplies a cached display overlay.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::api::Song;
use crate::util::safe_fs;

const CACHE_FILE: &str = "romanized_titles.json";
/// Cap the on-disk read so a bloated/corrupt/synced cache can't be slurped whole at startup;
/// an oversize file is moved to `*.too-large.bak` and the cache falls back to empty.
const CACHE_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Cap on cached romanizations (one per distinct non-Latin song). Generous — a very large
/// library stays well under it — but bounds the cache on an unusually long-lived install; the
/// cache is cheaply rebuildable, so evicting the oldest-by-key entry past the cap is harmless.
const CACHE_ENTRIES_MAX: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RomanizeQuality {
    #[default]
    Local,
    Gemini,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RomanizedEntry {
    pub title: String,
    pub artist: String,
    pub quality: RomanizeQuality,
    /// True once Gemini has supplied an upgraded result for this exact key. Local entries with
    /// `false` can be retried later when an API key becomes available or is corrected.
    pub gemini_checked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

impl Default for RomanizedEntry {
    fn default() -> Self {
        Self {
            title: String::new(),
            artist: String::new(),
            quality: RomanizeQuality::Local,
            gemini_checked: false,
            confidence: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RomanizeItem {
    pub key: String,
    pub title: String,
    pub artist: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RomanizedResult {
    pub key: String,
    pub title: String,
    pub artist: String,
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RomanizeCache {
    entries: BTreeMap<String, RomanizedEntry>,
    /// Mutation counter so display-dependent caches (the library row cache filters on
    /// romanized titles) can key on cache content without hashing it.
    #[serde(skip)]
    rev: u64,
    /// Reusable key buffer for [`Self::entry_for`] — it runs twice per list row per
    /// frame when romanized titles are on, and the joined key was a fresh `String` each
    /// time. `RefCell` because lookups come through `&self` from the render path.
    #[serde(skip)]
    key_scratch: std::cell::RefCell<String>,
}

impl RomanizeCache {
    pub fn load() -> Self {
        let Some(path) = cache_path() else {
            return Self::default();
        };
        // Schema-drift tolerant: keeps cached romanizations across incompatible changes.
        let cache = safe_fs::load_json_or_default_limited::<RomanizeCache>(&path, CACHE_MAX_BYTES);
        crate::persist::replay_journaled_snapshot(
            crate::persist::StoreKind::RomanizedTitles,
            &path,
            cache,
            CACHE_MAX_BYTES,
        )
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = cache_path() else {
            return Ok(());
        };
        safe_fs::write_private_atomic_json(&path, self)
    }

    pub fn delete_saved() -> std::io::Result<()> {
        let Some(path) = cache_path() else {
            return Ok(());
        };
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn clear(&mut self) {
        self.rev = self.rev.wrapping_add(1);
        self.entries.clear();
    }

    /// Content revision — changes whenever an overlay entry is added/replaced/cleared.
    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn display_title<'a>(&'a self, song: &'a Song) -> Cow<'a, str> {
        self.entry_for(song)
            .and_then(|entry| usable_overlay(&entry.title, &song.title))
            .map(Cow::Borrowed)
            .unwrap_or_else(|| Cow::Borrowed(song.title.as_str()))
    }

    pub fn display_artist<'a>(&'a self, song: &'a Song) -> Cow<'a, str> {
        self.entry_for(song)
            .and_then(|entry| usable_overlay(&entry.artist, &song.artist))
            .map(Cow::Borrowed)
            .unwrap_or_else(|| Cow::Borrowed(song.artist.as_str()))
    }

    pub fn entry_for(&self, song: &Song) -> Option<&RomanizedEntry> {
        let mut buf = self.key_scratch.borrow_mut();
        buf.clear();
        write_key(&mut buf, song);
        // The returned reference borrows `entries`, not the scratch buffer, so the
        // RefCell borrow ends here.
        self.entries.get(buf.as_str())
    }

    pub fn ensure_local(&mut self, song: &Song) -> bool {
        if !needs_latinization(&song.title, &song.artist) {
            return false;
        }
        let key = key_for_song(song);
        if self.entries.contains_key(&key) {
            return false;
        }
        let title = clean_display(&romanize_text(&song.title));
        let artist = clean_display(&romanize_text(&song.artist));
        self.rev = self.rev.wrapping_add(1);
        // Bound the cache; the key is new here (checked above), so evict oldest-by-key first.
        while self.entries.len() >= CACHE_ENTRIES_MAX {
            self.entries.pop_first();
        }
        self.entries.insert(
            key,
            RomanizedEntry {
                title,
                artist,
                quality: RomanizeQuality::Local,
                gemini_checked: false,
                confidence: None,
            },
        );
        true
    }

    pub fn gemini_candidate(&self, song: &Song) -> Option<RomanizeItem> {
        if !needs_latinization(&song.title, &song.artist) {
            return None;
        }
        let key = key_for_song(song);
        if self
            .entries
            .get(&key)
            .is_some_and(|entry| entry.quality == RomanizeQuality::Gemini || entry.gemini_checked)
        {
            return None;
        }
        Some(RomanizeItem {
            key,
            title: song.title.clone(),
            artist: song.artist.clone(),
        })
    }

    pub fn apply_gemini_results(&mut self, results: &[RomanizedResult]) -> bool {
        let mut changed = false;
        for result in results {
            let title = clean_display(&result.title);
            let artist = clean_display(&result.artist);
            if title.is_empty() && artist.is_empty() {
                continue;
            }
            let next = RomanizedEntry {
                title,
                artist,
                quality: RomanizeQuality::Gemini,
                gemini_checked: true,
                confidence: result.confidence,
            };
            if self.entries.get(&result.key) != Some(&next) {
                self.entries.insert(result.key.clone(), next);
                changed = true;
            }
        }
        if changed {
            self.rev = self.rev.wrapping_add(1);
        }
        changed
    }
}

pub(crate) fn cache_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join(CACHE_FILE))
}

pub fn key_for_song(song: &Song) -> String {
    let mut key = String::new();
    write_key(&mut key, song);
    key
}

/// The persisted cache-key format (`\u{1f}`-joined fields) written into a caller-owned
/// buffer — [`RomanizeCache::entry_for`] reuses one across lookups. Must stay in sync
/// with what older versions produced via `join`, since keys live in the cache file.
fn write_key(buf: &mut String, song: &Song) {
    let stable_id = song.youtube_id().unwrap_or(song.video_id.as_str());
    buf.push_str(song.source.code());
    buf.push('\u{1f}');
    buf.push_str(stable_id);
    buf.push('\u{1f}');
    buf.push_str(song.title.trim());
    buf.push('\u{1f}');
    buf.push_str(song.artist.trim());
}

fn usable_overlay<'a>(overlay: &'a str, original: &str) -> Option<&'a str> {
    let overlay = overlay.trim();
    if overlay.is_empty() || overlay == original.trim() {
        None
    } else {
        Some(overlay)
    }
}

pub fn needs_latinization(title: &str, artist: &str) -> bool {
    title.chars().chain(artist.chars()).any(is_target_script)
}

fn is_target_script(c: char) -> bool {
    matches!(
        c as u32,
        0x1100..=0x11ff // Hangul Jamo
            | 0x3130..=0x318f // Hangul Compatibility Jamo
            | 0xac00..=0xd7af // Hangul syllables
            | 0x3040..=0x309f // Hiragana
            | 0x30a0..=0x30ff // Katakana
            | 0x3400..=0x4dbf // CJK Extension A
            | 0x4e00..=0x9fff // CJK Unified Ideographs
    )
}

pub fn romanize_text(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if let Some(s) = romanize_hangul_syllable_at(&chars, i) {
            out.push_str(s.as_str());
            i += 1;
            continue;
        }
        if is_hangul_jamo(c) {
            out.push(c);
            i += 1;
            continue;
        }
        if let Some((s, used)) = romanize_kana_at(&chars, i, &out) {
            out.push_str(&s);
            i += used;
            continue;
        }
        out.push(normalize_fullwidth_ascii(c));
        i += 1;
    }
    out
}

const HANGUL_INITIAL: [&str; 19] = [
    "g", "kk", "n", "d", "tt", "r", "m", "b", "pp", "s", "ss", "", "j", "jj", "ch", "k", "t", "p",
    "h",
];
const HANGUL_MEDIAL: [&str; 21] = [
    "a", "ae", "ya", "yae", "eo", "e", "yeo", "ye", "o", "wa", "wae", "oe", "yo", "u", "wo", "we",
    "wi", "yu", "eu", "ui", "i",
];
const HANGUL_FINAL: [&str; 28] = [
    "", "k", "k", "ks", "n", "nj", "nh", "t", "l", "lk", "lm", "lb", "ls", "lt", "lp", "lh", "m",
    "p", "ps", "t", "t", "ng", "t", "t", "k", "t", "p", "t",
];
const HANGUL_FINAL_BEFORE_VOWEL: [&str; 28] = [
    "", "g", "kk", "gs", "n", "nj", "nh", "d", "r", "lg", "lm", "lb", "ls", "lt", "lp", "lh", "m",
    "b", "ps", "s", "ss", "ng", "j", "ch", "k", "t", "p", "h",
];

fn romanize_hangul_syllable_at(chars: &[char], idx: usize) -> Option<String> {
    let (initial, medial, final_idx) = hangul_indices(chars[idx])?;
    let final_s = if chars
        .get(idx + 1)
        .is_some_and(|&next| starts_with_silent_ieung(next))
    {
        HANGUL_FINAL_BEFORE_VOWEL[final_idx]
    } else {
        HANGUL_FINAL[final_idx]
    };
    Some(format!(
        "{}{}{}",
        HANGUL_INITIAL[initial], HANGUL_MEDIAL[medial], final_s
    ))
}

fn hangul_indices(c: char) -> Option<(usize, usize, usize)> {
    let code = c as u32;
    if !(0xac00..=0xd7a3).contains(&code) {
        return None;
    }
    let offset = code - 0xac00;
    let initial = (offset / 588) as usize;
    let medial = ((offset % 588) / 28) as usize;
    let final_idx = (offset % 28) as usize;
    Some((initial, medial, final_idx))
}

fn starts_with_silent_ieung(c: char) -> bool {
    hangul_indices(c).is_some_and(|(initial, _, _)| initial == 11)
}

fn is_hangul_jamo(c: char) -> bool {
    matches!(c as u32, 0x1100..=0x11ff | 0x3130..=0x318f)
}

fn romanize_kana_at(chars: &[char], idx: usize, out: &str) -> Option<(String, usize)> {
    let c = hira(chars[idx])?;
    if c == 'っ' {
        if let Some((next, _)) = chars
            .get(idx + 1)
            .and_then(|_| romanize_kana_base(chars, idx + 1))
        {
            let doubled = first_consonant(&next).unwrap_or_default();
            return Some((doubled.to_owned(), 1));
        }
        return Some(("".to_owned(), 1));
    }
    if c == 'ー' {
        return Some((last_vowel(out).unwrap_or('-').to_string(), 1));
    }
    if let Some(next) = chars.get(idx + 1).and_then(|c| hira(*c))
        && let Some(foreign) = foreign_vowel_combo(c, next)
    {
        return Some((foreign.to_owned(), 2));
    }
    if let Some(next) = chars.get(idx + 1).and_then(|c| hira(*c))
        && matches!(next, 'ゃ' | 'ゅ' | 'ょ')
        && let Some(base) = yoon_base(c)
    {
        let vowel = match next {
            'ゃ' => "a",
            'ゅ' => "u",
            'ょ' => "o",
            _ => "",
        };
        return Some((format!("{base}{vowel}"), 2));
    }
    romanize_kana_base(chars, idx)
}

fn foreign_vowel_combo(c: char, next: char) -> Option<&'static str> {
    match (c, next) {
        ('ふ', 'ぁ') => Some("fa"),
        ('ふ', 'ぃ') => Some("fi"),
        ('ふ', 'ぇ') => Some("fe"),
        ('ふ', 'ぉ') => Some("fo"),
        ('ゔ', 'ぁ') => Some("va"),
        ('ゔ', 'ぃ') => Some("vi"),
        ('ゔ', 'ぇ') => Some("ve"),
        ('ゔ', 'ぉ') => Some("vo"),
        ('て', 'ぃ') => Some("ti"),
        ('で', 'ぃ') => Some("di"),
        ('と', 'ぅ') => Some("tu"),
        ('ど', 'ぅ') => Some("du"),
        ('し', 'ぇ') => Some("she"),
        ('じ', 'ぇ') => Some("je"),
        ('ち', 'ぇ') => Some("che"),
        ('つ', 'ぁ') => Some("tsa"),
        ('つ', 'ぃ') => Some("tsi"),
        ('つ', 'ぇ') => Some("tse"),
        ('つ', 'ぉ') => Some("tso"),
        _ => None,
    }
}

fn romanize_kana_base(chars: &[char], idx: usize) -> Option<(String, usize)> {
    let c = hira(chars[idx])?;
    let s = match c {
        'あ' => "a",
        'い' | 'ぃ' => "i",
        'う' | 'ぅ' => "u",
        'え' | 'ぇ' => "e",
        'お' | 'ぉ' => "o",
        'ぁ' => "a",
        'か' => "ka",
        'き' => "ki",
        'く' => "ku",
        'け' => "ke",
        'こ' => "ko",
        'さ' => "sa",
        'し' => "shi",
        'す' => "su",
        'せ' => "se",
        'そ' => "so",
        'た' => "ta",
        'ち' => "chi",
        'つ' => "tsu",
        'て' => "te",
        'と' => "to",
        'な' => "na",
        'に' => "ni",
        'ぬ' => "nu",
        'ね' => "ne",
        'の' => "no",
        'は' => "ha",
        'ひ' => "hi",
        'ふ' => "fu",
        'へ' => "he",
        'ほ' => "ho",
        'ま' => "ma",
        'み' => "mi",
        'む' => "mu",
        'め' => "me",
        'も' => "mo",
        'や' | 'ゃ' => "ya",
        'ゆ' | 'ゅ' => "yu",
        'よ' | 'ょ' => "yo",
        'ら' => "ra",
        'り' => "ri",
        'る' => "ru",
        'れ' => "re",
        'ろ' => "ro",
        'わ' => "wa",
        'を' => "wo",
        'ん' => "n",
        'が' => "ga",
        'ぎ' => "gi",
        'ぐ' => "gu",
        'げ' => "ge",
        'ご' => "go",
        'ざ' => "za",
        'じ' => "ji",
        'ず' => "zu",
        'ぜ' => "ze",
        'ぞ' => "zo",
        'だ' => "da",
        'ぢ' => "ji",
        'づ' => "zu",
        'で' => "de",
        'ど' => "do",
        'ば' => "ba",
        'び' => "bi",
        'ぶ' => "bu",
        'べ' => "be",
        'ぼ' => "bo",
        'ぱ' => "pa",
        'ぴ' => "pi",
        'ぷ' => "pu",
        'ぺ' => "pe",
        'ぽ' => "po",
        'ゔ' => "vu",
        'ゎ' => "wa",
        _ => return None,
    };
    Some((s.to_owned(), 1))
}

fn hira(c: char) -> Option<char> {
    if c == 'ー' {
        return Some(c);
    }
    let code = c as u32;
    match code {
        0x3040..=0x309f => Some(c),
        0x30a0..=0x30ff => char::from_u32(code - 0x60),
        _ => None,
    }
}

fn yoon_base(c: char) -> Option<&'static str> {
    match c {
        'き' => Some("ky"),
        'し' => Some("sh"),
        'ち' => Some("ch"),
        'に' => Some("ny"),
        'ひ' => Some("hy"),
        'み' => Some("my"),
        'り' => Some("ry"),
        'ぎ' => Some("gy"),
        'じ' => Some("j"),
        'ぢ' => Some("j"),
        'び' => Some("by"),
        'ぴ' => Some("py"),
        _ => None,
    }
}

fn first_consonant(s: &str) -> Option<&str> {
    if s.starts_with("ch") {
        Some("c")
    } else if s.starts_with("sh") {
        Some("s")
    } else {
        let first = s.chars().next()?;
        (!matches!(first, 'a' | 'e' | 'i' | 'o' | 'u')).then_some(&s[..first.len_utf8()])
    }
}

fn last_vowel(s: &str) -> Option<char> {
    s.chars()
        .rev()
        .find(|c| matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u'))
}

fn normalize_fullwidth_ascii(c: char) -> char {
    let code = c as u32;
    if (0xff01..=0xff5e).contains(&code) {
        char::from_u32(code - 0xfee0).unwrap_or(c)
    } else {
        c
    }
}

fn clean_display(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.trim().chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_romanizes_hangul_and_kana_without_touching_ascii() {
        assert_eq!(
            clean_display(&romanize_text("아이유 - 좋은 날")),
            "aiyu - joheun nal"
        );
        assert_eq!(clean_display(&romanize_text("きらり")), "kirari");
        assert_eq!(
            clean_display(&romanize_text("ファースト Love")),
            "faasuto Love"
        );
    }

    #[test]
    fn romanization_handles_assimilation_small_tsu_yoon_and_fullwidth_ascii() {
        assert_eq!(romanize_text("막아"), "maga");
        assert_eq!(romanize_text("먹다"), "meokda");
        assert_eq!(romanize_text("きゃりー"), "kyarii");
        assert_eq!(romanize_text("がっこう"), "gakkou");
        assert_eq!(romanize_text("Ｈｅｌｌｏ！"), "Hello!");
    }

    #[test]
    fn latinization_detection_is_limited_to_target_scripts() {
        assert!(needs_latinization("좋은 날", "아이유"));
        assert!(needs_latinization("夜に駆ける", "YOASOBI"));
        assert!(!needs_latinization("Cafe del Mar", "Energy 52"));
        assert!(!needs_latinization("Беги", "Кино"));
    }

    #[test]
    fn cache_uses_overlay_only_when_it_differs() {
        let song = Song::remote("vid", "아이유", "좋은 날", "3:00");
        let mut cache = RomanizeCache::default();
        assert!(cache.ensure_local(&song));
        assert_eq!(cache.display_title(&song), "aiyu");
        assert_eq!(cache.display_artist(&song), "joheun nal");
        assert!(!cache.ensure_local(&song));
    }

    #[test]
    fn gemini_result_replaces_local_entry() {
        let song = Song::remote("vid", "아이유", "좋은 날", "3:00");
        let mut cache = RomanizeCache::default();
        cache.ensure_local(&song);
        let key = key_for_song(&song);
        assert!(cache.apply_gemini_results(&[RomanizedResult {
            key,
            title: "IU".to_owned(),
            artist: "Joeun Nal".to_owned(),
            confidence: Some(0.9),
        }]));
        assert_eq!(cache.display_title(&song), "IU");
        assert_eq!(cache.display_artist(&song), "Joeun Nal");
    }

    #[test]
    fn cache_skips_ascii_entries_and_tracks_revision_changes() {
        let ascii = Song::remote("vid", "Plain Title", "Plain Artist", "3:00");
        let mut cache = RomanizeCache::default();
        assert_eq!(cache.rev(), 0);
        assert!(!cache.ensure_local(&ascii));
        assert_eq!(cache.display_title(&ascii), "Plain Title");
        assert_eq!(cache.display_artist(&ascii), "Plain Artist");
        assert_eq!(cache.rev(), 0);

        let cjk = Song::remote("vid2", "밤편지", "아이유", "4:13");
        assert!(cache.ensure_local(&cjk));
        let after_insert = cache.rev();
        assert!(after_insert > 0);
        cache.clear();
        assert!(cache.rev() > after_insert);
        assert_eq!(cache.display_title(&cjk), "밤편지");
    }

    #[test]
    fn gemini_candidate_respects_checked_and_quality_state() {
        let song = Song::remote("vid", "밤편지", "아이유", "4:13");
        let mut cache = RomanizeCache::default();

        let candidate = cache.gemini_candidate(&song).expect("needs Gemini");
        assert_eq!(candidate.title, "밤편지");
        assert_eq!(candidate.artist, "아이유");

        cache.ensure_local(&song);
        assert!(cache.gemini_candidate(&song).is_some());
        let key = key_for_song(&song);
        assert!(cache.apply_gemini_results(&[RomanizedResult {
            key: key.clone(),
            title: "Bam Pyeonji".to_owned(),
            artist: "IU".to_owned(),
            confidence: Some(0.88),
        }]));
        assert!(cache.gemini_candidate(&song).is_none());

        let unchanged = cache.apply_gemini_results(&[RomanizedResult {
            key,
            title: "Bam Pyeonji".to_owned(),
            artist: "IU".to_owned(),
            confidence: Some(0.88),
        }]);
        assert!(!unchanged, "same Gemini result must not bump the cache");
    }

    #[test]
    fn empty_gemini_result_is_ignored_without_marking_checked() {
        let song = Song::remote("vid", "아이유", "좋은 날", "3:00");
        let mut cache = RomanizeCache::default();
        let key = key_for_song(&song);

        assert!(!cache.apply_gemini_results(&[RomanizedResult {
            key,
            title: "   ".to_owned(),
            artist: "\n\t".to_owned(),
            confidence: Some(0.1),
        }]));
        assert!(cache.gemini_candidate(&song).is_some());
    }
}
