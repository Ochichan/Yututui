// Chord normalization (docs/gui/05 §8.3–§8.4). The GUI must produce the same canonical
// config chord string the TUI persists (`chord_to_config`, src/keymap.rs) so a remap is
// honored identically — including the Korean 3-branch rule. These cases mirror the
// keymap.rs normalization + config token set.

import { describe, expect, it } from 'vitest';
import {
  chordFromEvent,
  chordFromCapture,
  displayChord,
  isPlainTypeable,
} from '../src/lib/keyboard/chord';

function ev(init: KeyboardEventInit): KeyboardEvent {
  return new KeyboardEvent('keydown', init);
}

describe('chordFromEvent (key-first)', () => {
  const cases: Array<[string, KeyboardEventInit, string | null]> = [
    ['plain letter', { key: 'a' }, 'a'],
    ['shifted letter → uppercase, no shift token', { key: 'A', shiftKey: true }, 'A'],
    ['Ctrl+letter lowercases the base', { key: 'u', ctrlKey: true }, 'ctrl+u'],
    [
      'Ctrl+Shift+letter keeps mods, base lowercased',
      { key: 'U', ctrlKey: true, shiftKey: true },
      'ctrl+shift+u',
    ],
    ['Shift+Tab → backtab', { key: 'Tab', shiftKey: true }, 'backtab'],
    ['plain Tab', { key: 'Tab' }, 'tab'],
    ['arrow', { key: 'ArrowLeft' }, 'left'],
    ['Shift+arrow keeps shift', { key: 'ArrowRight', shiftKey: true }, 'shift+right'],
    ['space', { key: ' ' }, 'space'],
    ['question mark (shift baked into the char)', { key: '?', shiftKey: true }, '?'],
    ['digit', { key: '1' }, '1'],
    ['escape', { key: 'Escape' }, 'esc'],
    ['function key', { key: 'F2' }, 'f2'],
    ['Shift+function key', { key: 'F10', shiftKey: true }, 'shift+f10'],
    ['Ctrl+= (text zoom)', { key: '=', ctrlKey: true }, 'ctrl+='],
    ['Meta is left to the OS', { key: 'c', metaKey: true }, null],
  ];
  for (const [name, init, want] of cases) {
    it(name, () => expect(chordFromEvent(ev(init))).toBe(want));
  }

  it('branch 1: a plain jamo in e.key maps through the 2-set table', () => {
    // ㄱ sits on the physical R key in 2-set.
    expect(chordFromEvent(ev({ key: 'ㄱ' }))).toBe('r');
  });

  it('branch 2: mid-composition (key="Process") falls back to the physical code', () => {
    expect(chordFromEvent(ev({ key: 'Process', code: 'KeyR' }))).toBe('r');
    expect(chordFromEvent(ev({ key: 'Process', code: 'KeyR', shiftKey: true }))).toBe('R');
  });

  it('Alt-modified keys read the physical code (macOS Option composes glyphs into e.key)', () => {
    // Option+Shift+R on macOS delivers e.key='®'; the code path recovers alt+shift+r.
    expect(chordFromEvent(ev({ key: '®', code: 'KeyR', altKey: true, shiftKey: true }))).toBe(
      'alt+shift+r',
    );
    expect(chordFromEvent(ev({ key: '¬', code: 'KeyL', altKey: true }))).toBe('alt+l');
  });
});

describe('chordFromCapture (code-first, IME-safe)', () => {
  it('reads the physical key regardless of produced character', () => {
    expect(chordFromCapture(ev({ key: 'ㄱ', code: 'KeyR' }))).toBe('r');
    expect(chordFromCapture(ev({ code: 'KeyA', shiftKey: true }))).toBe('A');
    expect(chordFromCapture(ev({ code: 'Space' }))).toBe('space');
    expect(chordFromCapture(ev({ code: 'Digit1' }))).toBe('1');
    expect(chordFromCapture(ev({ code: 'KeyK', ctrlKey: true }))).toBe('ctrl+k');
  });
});

describe('displayChord', () => {
  it('prettifies config tokens without touching letter case', () => {
    expect(displayChord('ctrl+u')).toBe('Ctrl+u');
    expect(displayChord('alt+shift+r')).toBe('Alt+Shift+r');
    expect(displayChord('space')).toBe('Space');
    expect(displayChord('backtab')).toBe('BackTab');
    expect(displayChord('shift+f10')).toBe('Shift+F10');
    expect(displayChord('A')).toBe('A');
    expect(displayChord('?')).toBe('?');
  });
});

describe('isPlainTypeable (the is_typeable guard)', () => {
  it('true for a lone character key, false with modifiers or for control keys', () => {
    expect(isPlainTypeable(ev({ key: 'a' }))).toBe(true);
    expect(isPlainTypeable(ev({ key: ' ' }))).toBe(true);
    expect(isPlainTypeable(ev({ key: 'a', ctrlKey: true }))).toBe(false);
    expect(isPlainTypeable(ev({ key: 'ArrowLeft' }))).toBe(false);
    expect(isPlainTypeable(ev({ key: 'Enter' }))).toBe(false);
  });
});
