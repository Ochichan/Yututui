// Client-only view/overlay state — the web analog of the TUI's top-level Mode (docs/gui/05 §3).

export type View = 'now' | 'search' | 'library' | 'settings' | 'ai';

export type SettingsTab = 'general' | 'playback' | 'hotkeys' | 'graphics' | 'djgem' | 'accounts';

export type LibraryTab =
  'all' | 'favorites' | 'history' | 'downloads' | 'playlists' | 'radio_likes' | 'radio_history';

export interface NavItem {
  id: View;
  label: string;
  glyph: string;
}

export interface UiSnapshot {
  view: View;
  queueOpen: boolean;
  settingsTab: SettingsTab;
  libraryTab: LibraryTab;
  scrollY: number;
  activeControl?: string;
  scrollPositions?: Record<string, number>;
  drafts?: Record<string, string>;
}

export const NAV_ITEMS: NavItem[] = [
  { id: 'now', label: 'Now Playing', glyph: '♪' },
  { id: 'search', label: 'Search', glyph: '⌕' },
  { id: 'library', label: 'Library', glyph: '☰' },
  { id: 'ai', label: 'DJ Gem', glyph: '✦' },
  { id: 'settings', label: 'Settings', glyph: '⚙' },
];

export class UiStore {
  view = $state<View>('now');
  helpOpen = $state(false);
  aboutOpen = $state(false);
  /** The right queue dock (keymap OpenQueue). */
  queueOpen = $state(true);
  settingsTab = $state<SettingsTab>('general');
  libraryTab = $state<LibraryTab>('all');

  constructor(snapshot?: unknown) {
    const restored = parseUiSnapshot(snapshot);
    if (!restored) return;
    this.view = restored.view;
    this.queueOpen = restored.queueOpen;
    this.settingsTab = restored.settingsTab;
    this.libraryTab = restored.libraryTab;
  }

  setView(v: View): void {
    this.view = v;
  }

  toggleQueue(): void {
    this.queueOpen = !this.queueOpen;
  }

  /** Close the topmost overlay; true if one was open (Esc routing until the M3 dispatcher). */
  closeTopOverlay(): boolean {
    if (this.helpOpen) {
      this.helpOpen = false;
      return true;
    }
    if (this.aboutOpen) {
      this.aboutOpen = false;
      return true;
    }
    return false;
  }

  snapshot(): UiSnapshot {
    const active = document.activeElement instanceof HTMLElement ? document.activeElement.id : '';
    const scrollPositions: Record<string, number> = {};
    for (const element of document.querySelectorAll<HTMLElement>('[data-ui-scroll-key]')) {
      const key = element.dataset.uiScrollKey;
      if (!key || Object.keys(scrollPositions).length >= MAX_SCROLL_POSITIONS) continue;
      scrollPositions[key] = boundedScroll(element.scrollTop);
    }
    const drafts: Record<string, string> = {};
    for (const element of document.querySelectorAll<HTMLInputElement | HTMLTextAreaElement>(
      '[data-ui-draft-key]',
    )) {
      const key = element.dataset.uiDraftKey;
      if (!key || Object.keys(drafts).length >= MAX_DRAFTS || isSensitiveInput(element)) continue;
      drafts[key] = truncateDraft(element.value);
    }
    return {
      view: this.view,
      queueOpen: this.queueOpen,
      settingsTab: this.settingsTab,
      libraryTab: this.libraryTab,
      scrollY: boundedScroll(window.scrollY),
      ...(active && active.length <= 128 ? { activeControl: active } : {}),
      ...(Object.keys(scrollPositions).length > 0 ? { scrollPositions } : {}),
      ...(Object.keys(drafts).length > 0 ? { drafts } : {}),
    };
  }

  restoreDocument(snapshot: unknown): void {
    const restored = parseUiSnapshot(snapshot);
    if (!restored) return;
    window.scrollTo({ top: restored.scrollY, behavior: 'instant' });
    for (const element of document.querySelectorAll<HTMLElement>('[data-ui-scroll-key]')) {
      const key = element.dataset.uiScrollKey;
      if (!key || restored.scrollPositions?.[key] === undefined) continue;
      element.scrollTop = restored.scrollPositions[key];
      element.dispatchEvent(new Event('scroll'));
    }
    for (const element of document.querySelectorAll<HTMLInputElement | HTMLTextAreaElement>(
      '[data-ui-draft-key]',
    )) {
      const key = element.dataset.uiDraftKey;
      if (!key || restored.drafts?.[key] === undefined || isSensitiveInput(element)) continue;
      element.value = restored.drafts[key];
      // Svelte bindings update through the input event, so restore the component's state as
      // well as the DOM value before focus is returned.
      element.dispatchEvent(new Event('input', { bubbles: true }));
    }
    if (restored.activeControl) document.getElementById(restored.activeControl)?.focus();
  }
}

const MAX_SCROLL = 10_000_000;
const MAX_SCROLL_POSITIONS = 16;
const MAX_DRAFTS = 8;
const MAX_KEY_LENGTH = 64;
const MAX_DRAFT_LENGTH = 4 * 1024;

function boundedScroll(value: number): number {
  return Math.max(0, Math.min(MAX_SCROLL, Math.round(value)));
}

function isSensitiveInput(element: HTMLInputElement | HTMLTextAreaElement): boolean {
  return (
    element instanceof HTMLInputElement && ['password', 'file', 'hidden'].includes(element.type)
  );
}

function truncateDraft(value: string): string {
  if (new TextEncoder().encode(value).byteLength <= MAX_DRAFT_LENGTH) return value;
  let result = '';
  let bytes = 0;
  for (const character of value) {
    const next = new TextEncoder().encode(character).byteLength;
    if (bytes + next > MAX_DRAFT_LENGTH) break;
    result += character;
    bytes += next;
  }
  return result;
}

const VIEWS: readonly View[] = ['now', 'search', 'library', 'settings', 'ai'];
const SETTINGS_TABS: readonly SettingsTab[] = [
  'general',
  'playback',
  'hotkeys',
  'graphics',
  'djgem',
  'accounts',
];
const LIBRARY_TABS: readonly LibraryTab[] = [
  'all',
  'favorites',
  'history',
  'downloads',
  'playlists',
  'radio_likes',
  'radio_history',
];

function parseUiSnapshot(value: unknown): UiSnapshot | null {
  if (!value || typeof value !== 'object') return null;
  const candidate = value as Partial<UiSnapshot>;
  if (
    !VIEWS.includes(candidate.view as View) ||
    typeof candidate.queueOpen !== 'boolean' ||
    !SETTINGS_TABS.includes(candidate.settingsTab as SettingsTab) ||
    !LIBRARY_TABS.includes(candidate.libraryTab as LibraryTab) ||
    !Number.isSafeInteger(candidate.scrollY) ||
    candidate.scrollY! < 0 ||
    candidate.scrollY! > MAX_SCROLL ||
    (candidate.activeControl !== undefined &&
      (typeof candidate.activeControl !== 'string' || candidate.activeControl.length > 128)) ||
    !isScrollMap(candidate.scrollPositions) ||
    !isDraftMap(candidate.drafts)
  ) {
    return null;
  }
  return {
    view: candidate.view as View,
    queueOpen: candidate.queueOpen,
    settingsTab: candidate.settingsTab as SettingsTab,
    libraryTab: candidate.libraryTab as LibraryTab,
    scrollY: candidate.scrollY!,
    ...(candidate.activeControl ? { activeControl: candidate.activeControl } : {}),
    ...(candidate.scrollPositions ? { scrollPositions: candidate.scrollPositions } : {}),
    ...(candidate.drafts ? { drafts: candidate.drafts } : {}),
  };
}

function isScrollMap(value: unknown): value is Record<string, number> | undefined {
  if (value === undefined) return true;
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
  const entries = Object.entries(value);
  return (
    entries.length <= MAX_SCROLL_POSITIONS &&
    entries.every(
      ([key, offset]) =>
        key.length > 0 &&
        key.length <= MAX_KEY_LENGTH &&
        Number.isSafeInteger(offset) &&
        (offset as number) >= 0 &&
        (offset as number) <= MAX_SCROLL,
    )
  );
}

function isDraftMap(value: unknown): value is Record<string, string> | undefined {
  if (value === undefined) return true;
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
  const entries = Object.entries(value);
  return (
    entries.length <= MAX_DRAFTS &&
    entries.every(
      ([key, draft]) =>
        key.length > 0 &&
        key.length <= MAX_KEY_LENGTH &&
        typeof draft === 'string' &&
        new TextEncoder().encode(draft).byteLength <= MAX_DRAFT_LENGTH,
    )
  );
}
