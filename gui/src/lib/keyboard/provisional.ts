// The GUI's provisional shortcut set — hardcoded until the real keymap dispatcher lands.
//
// TODO(wire:M3/settings.hotkeys): delete this module when lib/keyboard/{chord,dispatcher}
// consume the pushed keymap read model. Until then this table is the single source for
// App.svelte's handler, the Help overlay, and Settings→Hotkeys, so the three can't drift.

export interface ProvisionalShortcut {
  chord: string;
  label: string;
}

export const PROVISIONAL_SHORTCUTS: ProvisionalShortcut[] = [
  { chord: 'Space', label: 'Play / pause' },
  { chord: '←  →', label: 'Seek −5 s / +5 s' },
  { chord: '↑  ↓', label: 'Volume +5 / −5' },
  { chord: '1 … 5', label: 'Switch view (Now / Search / Library / DJ Gem / Settings)' },
  { chord: 'q', label: 'Toggle queue panel' },
  { chord: '?', label: 'Help overlay' },
  { chord: 'Esc', label: 'Close overlay / dialog' },
];
