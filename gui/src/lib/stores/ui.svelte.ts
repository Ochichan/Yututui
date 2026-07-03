// Client-only view/overlay state — the web analog of the TUI's top-level Mode (docs/gui/05 §3).

export type View = 'now' | 'search' | 'library' | 'settings' | 'ai';

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
  queueOpen = $state(false);

  setView(v: View): void {
    this.view = v;
  }
}
