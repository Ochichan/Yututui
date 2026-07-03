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
}
