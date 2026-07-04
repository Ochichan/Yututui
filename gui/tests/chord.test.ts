// Chord normalization (docs/gui/05 §8.3–§8.4). The GUI must produce the same persisted chord
// string the TUI stores so a remap is honored identically — including the Korean 3-branch
// rule. These cases mirror the keymap.rs normalization the demo bindings assume.

import { describe, expect, it } from 'vitest';
import {
  chordFromEvent,
  chordFromCapture,
  isPlainTypeable,
} from '../src/lib/keyboard/chord';

function ev(init: KeyboardEventInit): KeyboardEvent {
  return new KeyboardEvent('keydown', init);
}

describe('chordFromEvent (key-first)', () => {
  const cases: Array<[string, KeyboardEventInit, string | null]> = [
    ['plain letter', { key: 'a' }, 'a'],
    ['shifted letter → uppercase, no Shift token', { key: 'A', shiftKey: true }, 'A'],
    ['Ctrl+letter lowercases the base', { key: 'u', ctrlKey: true }, 'Ctrl+u'],
    ['Ctrl+Shift+letter keeps mods, base lowercased', { key: 'U', ctrlKey: true, shiftKey: true }, 'Ctrl+Shift+u'],
    ['Shift+Tab → BackTab', { key: 'Tab', shiftKey: true }, 'BackTab'],
    ['plain Tab', { key: 'Tab' }, 'Tab'],
    ['arrow', { key: 'ArrowLeft' }, 'Left'],
    ['Shift+arrow keeps Shift', { key: 'ArrowRight', shiftKey: true }, 'Shift+Right'],
    ['space', { key: ' ' }, 'Space'],
    ['question mark (shift baked into the char)', { key: '?', shiftKey: true }, '?'],
    ['digit', { key: '1' }, '1'],
    ['escape', { key: 'Escape' }, 'Esc'],
    ['function key', { key: 'F2' }, 'F2'],
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
});

describe('chordFromCapture (code-first, IME-safe)', () => {
  it('reads the physical key regardless of produced character', () => {
    expect(chordFromCapture(ev({ key: 'ㄱ', code: 'KeyR' }))).toBe('r');
    expect(chordFromCapture(ev({ code: 'KeyA', shiftKey: true }))).toBe('A');
    expect(chordFromCapture(ev({ code: 'Space' }))).toBe('Space');
    expect(chordFromCapture(ev({ code: 'Digit1' }))).toBe('1');
    expect(chordFromCapture(ev({ code: 'KeyK', ctrlKey: true }))).toBe('Ctrl+k');
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
