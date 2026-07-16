// Frontend-owned i18n (docs/gui/05 §9, i18n.catalog). A flat keyed catalog per language;
// `t(key, params?)` is reactive on the active language, so switching it live-swaps every
// label with no reload. The language rides the `settings` model (`ui.language`); App syncs
// it into here (see App.svelte's $effect). This is the wired path — the demo core already
// seeds + mutates `ui.language`, and the real core's settings push drives it identically.
//
// Romanized titles stay CORE-side (TrackModel.display_*): never romanize in the GUI. This
// catalog is chrome only — labels, hints, buttons, empty states — not user content.

import en from '../i18n/en.json';
import ko from '../i18n/ko.json';
import ja from '../i18n/ja.json';

type Catalog = Record<string, string>;

const CATALOGS: Record<string, Catalog> = { en: en as Catalog, ko: ko as Catalog, ja: ja as Catalog };
const FALLBACK = 'en';

class I18n {
  /** The active language code; kept in sync with `settings.ui.language` by App. */
  lang = $state<string>(FALLBACK);

  /** Adopt a language, ignoring anything we have no catalog for (stays put on garbage). */
  set(lang: string | null | undefined): void {
    if (lang && CATALOGS[lang]) this.lang = lang;
  }

  /**
   * Translate `key`, interpolating `{name}` placeholders from `params`. Falls back to the
   * English string, then to the raw key, so a missing translation degrades visibly-but-safely
   * rather than blanking the UI. Bound as a field so it can be destructured/passed freely.
   */
  t = (key: string, params?: Record<string, string | number>): string => {
    const table = CATALOGS[this.lang] ?? CATALOGS[FALLBACK];
    let s = table[key] ?? CATALOGS[FALLBACK][key] ?? key;
    if (params) {
      for (const [k, v] of Object.entries(params)) s = s.replaceAll(`{${k}}`, String(v));
    }
    return s;
  };
}

/** The process-wide i18n singleton. Import `{ t }` in components, `{ i18n }` to drive it. */
export const i18n = new I18n();
export const t = i18n.t;

/** The languages the catalog ships, for a settings picker. */
export const LANGUAGES: Array<{ code: string; label: string }> = [
  { code: 'en', label: 'English' },
  { code: 'ko', label: '한국어' },
  { code: 'ja', label: '日本語' },
];
