// The GUI's provisional shortcut set — a static fallback list.
//
// The dispatcher and Settings→Hotkeys now consume the live keymap read model
// (lib/stores/keymap.svelte.ts + lib/keyboard/); this table is the Help overlay's last
// remaining consumer.
// TODO(wire:M5/help.keymap): delete this module when HelpOverlay reads the keymap model too.

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
