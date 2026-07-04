// Chord normalization (docs/gui/05 §8.3–§8.4). Mirrors `Chord::new` (src/keymap.rs) closely
// enough that a GUI keypress produces the SAME persisted chord string the TUI stores, so the
// user's remap is honored identically. The format is self-consistent with the demo bindings
// (lib/stores/keymap.svelte.ts); the Rust chord-fixture cross-test (§8.5) is deferred until
// the export exists.
//
// Format: modifier tokens `Ctrl+`, `Alt+`, `Shift+` (in that order) prefix a base key. A
// plain shifted letter is the uppercase char with NO Shift token (`Shift+a → A`); Ctrl/Alt +
// letter lowercases the base but keeps modifiers (`Ctrl+Shift+A → Ctrl+Shift+a`). `Shift+Tab`
// is `BackTab`. Named keys: Space, Enter, Esc, Tab, BackTab, Backspace, Delete, Left, Right,
// Up, Down, Home, End, PageUp, PageDown, Insert, F1–F12.

import { KOREAN2SET } from './korean2set';

const NAMED_FROM_KEY: Record<string, string> = {
  ' ': 'Space',
  Spacebar: 'Space',
  Escape: 'Esc',
  Esc: 'Esc',
  Enter: 'Enter',
  Tab: 'Tab',
  Backspace: 'Backspace',
  Delete: 'Delete',
  Del: 'Delete',
  ArrowLeft: 'Left',
  ArrowRight: 'Right',
  ArrowUp: 'Up',
  ArrowDown: 'Down',
  Home: 'Home',
  End: 'End',
  PageUp: 'PageUp',
  PageDown: 'PageDown',
  Insert: 'Insert',
};

const NAMED_FROM_CODE: Record<string, string> = {
  Space: 'Space',
  Escape: 'Esc',
  Enter: 'Enter',
  NumpadEnter: 'Enter',
  Tab: 'Tab',
  Backspace: 'Backspace',
  Delete: 'Delete',
  ArrowLeft: 'Left',
  ArrowRight: 'Right',
  ArrowUp: 'Up',
  ArrowDown: 'Down',
  Home: 'Home',
  End: 'End',
  PageUp: 'PageUp',
  PageDown: 'PageDown',
  Insert: 'Insert',
};

const FKEY = /^F([1-9]|1[0-2])$/;

function withMods(e: KeyboardEvent, base: string, dropShift: boolean): string {
  const mods: string[] = [];
  if (e.ctrlKey) mods.push('Ctrl');
  if (e.altKey) mods.push('Alt');
  if (!dropShift && e.shiftKey) mods.push('Shift');
  return mods.length ? `${mods.join('+')}+${base}` : base;
}

/** Branch 1 (key-first): normalize a chord from `e.key`, remapping plain jamo via 2-set. */
export function chordFromKey(e: KeyboardEvent): string | null {
  let key = e.key;
  if (KOREAN2SET[key]) key = KOREAN2SET[key]; // plain jamo delivered outside an IME composition

  if (key === 'Tab' && e.shiftKey && !e.ctrlKey && !e.altKey) return 'BackTab';
  if (FKEY.test(key)) return withMods(e, key, false);
  const named = NAMED_FROM_KEY[key];
  if (named) return withMods(e, named, /* dropShift for Tab handled above */ false);

  if (key.length === 1) {
    if (/[a-zA-Z]/.test(key)) {
      // Letters: Ctrl/Alt lowercases the base and keeps modifiers; plain uses e.key's case
      // (the browser already gives 'A' for Shift+a), with no Shift token.
      if (e.ctrlKey || e.altKey) return withMods(e, key.toLowerCase(), false);
      return key;
    }
    // Digits / symbols: the produced character already bakes in Shift ('?', '!', …).
    return withMods(e, key, true);
  }
  return null; // a key we don't map (lone modifier, Dead, etc.)
}

/** Branch 2 & 3 (code-first): normalize from the physical `e.code` — composition + capture. */
export function chordFromCode(e: KeyboardEvent): string | null {
  const code = e.code;
  let base: string | null = null;
  let isLetter = false;

  const letter = /^Key([A-Z])$/.exec(code);
  const digit = /^Digit([0-9])$/.exec(code);
  const numpad = /^Numpad([0-9])$/.exec(code);
  if (letter) {
    base = letter[1].toLowerCase();
    isLetter = true;
  } else if (digit) {
    base = digit[1];
  } else if (numpad) {
    base = numpad[1];
  } else if (FKEY.test(code)) {
    base = code;
  } else if (NAMED_FROM_CODE[code]) {
    base = NAMED_FROM_CODE[code];
  }
  if (base == null) return null;

  if (isLetter) {
    if (e.ctrlKey || e.altKey) return withMods(e, base, false);
    return e.shiftKey ? base.toUpperCase() : base;
  }
  return withMods(e, base, false);
}

/**
 * The 3-branch rule (docs/gui/05 §8.4): key-first, with a code fallback only while an IME is
 * mid-composition (isComposing / Process / Dead), so GUI chord semantics match the TUI on
 * every layout including Korean.
 */
export function chordFromEvent(e: KeyboardEvent): string | null {
  if (e.metaKey) return null; // leave Cmd/Meta shortcuts to the OS/browser
  if (e.key === 'Process' || e.key === 'Dead' || e.isComposing) return chordFromCode(e);
  return chordFromKey(e);
}

/** Capture is always code-based (works while an IME is active — §8.4 branch 3). */
export function chordFromCapture(e: KeyboardEvent): string | null {
  if (e.metaKey) return null;
  return chordFromCode(e);
}

/** A DOM target that accepts text entry — the is_typeable guard's focus check. */
export function isTypeableTarget(target: EventTarget | null): boolean {
  return (
    target instanceof HTMLElement &&
    target.closest('input, textarea, select, [contenteditable]') != null
  );
}

/** A plain, character-producing key with no Ctrl/Alt/Meta — never stolen from a text field. */
export function isPlainTypeable(e: KeyboardEvent): boolean {
  return !e.ctrlKey && !e.altKey && !e.metaKey && e.key.length === 1;
}

/** A lone modifier keydown (Shift/Control/Alt/Meta) — ignored by capture. */
export function isModifierKey(key: string): boolean {
  return key === 'Shift' || key === 'Control' || key === 'Alt' || key === 'Meta';
}
