// Chord normalization (docs/gui/05 §8.3–§8.4). Mirrors `Chord::new` + `chord_to_config`
// (src/keymap.rs) so a GUI keypress produces the SAME canonical config chord string the core
// persists and pushes (`wire_bindings`) — dispatch is then a plain string match and rebinds
// round-trip byte-stable. The Rust chord-fixture cross-test (§8.5) is deferred until the
// export exists.
//
// Format (canonical config form): modifier tokens `ctrl+`, `alt+`, `shift+` (in that order)
// prefix a base key. A plain shifted letter is the uppercase char with NO shift token
// (`Shift+a → A`); Ctrl/Alt + letter lowercases the base but keeps modifiers
// (`Ctrl+Shift+A → ctrl+shift+a`). `Shift+Tab` is `backtab`. Named keys: space, enter, esc,
// tab, backtab, backspace, delete, left, right, up, down, home, end, pageup, pagedown,
// insert, f1–f12. `displayChord` prettifies for the UI (`ctrl+u → Ctrl+u`, `space → Space`).

import { KOREAN2SET } from './korean2set';

const NAMED_FROM_KEY: Record<string, string> = {
  ' ': 'space',
  Spacebar: 'space',
  Escape: 'esc',
  Esc: 'esc',
  Enter: 'enter',
  Tab: 'tab',
  Backspace: 'backspace',
  Delete: 'delete',
  Del: 'delete',
  ArrowLeft: 'left',
  ArrowRight: 'right',
  ArrowUp: 'up',
  ArrowDown: 'down',
  Home: 'home',
  End: 'end',
  PageUp: 'pageup',
  PageDown: 'pagedown',
  Insert: 'insert',
};

const NAMED_FROM_CODE: Record<string, string> = {
  Space: 'space',
  Escape: 'esc',
  Enter: 'enter',
  NumpadEnter: 'enter',
  Tab: 'tab',
  Backspace: 'backspace',
  Delete: 'delete',
  ArrowLeft: 'left',
  ArrowRight: 'right',
  ArrowUp: 'up',
  ArrowDown: 'down',
  Home: 'home',
  End: 'end',
  PageUp: 'pageup',
  PageDown: 'pagedown',
  Insert: 'insert',
};

const FKEY = /^[Ff]([1-9]|1[0-2])$/;

function withMods(e: KeyboardEvent, base: string, dropShift: boolean): string {
  const mods: string[] = [];
  if (e.ctrlKey) mods.push('ctrl');
  if (e.altKey) mods.push('alt');
  if (!dropShift && e.shiftKey) mods.push('shift');
  return mods.length ? `${mods.join('+')}+${base}` : base;
}

/** Branch 1 (key-first): normalize a chord from `e.key`, remapping plain jamo via 2-set. */
export function chordFromKey(e: KeyboardEvent): string | null {
  let key = e.key;
  if (KOREAN2SET[key]) key = KOREAN2SET[key]; // plain jamo delivered outside an IME composition

  if (key === 'Tab' && e.shiftKey && !e.ctrlKey && !e.altKey) return 'backtab';
  if (FKEY.test(key)) return withMods(e, key.toLowerCase(), false);
  const named = NAMED_FROM_KEY[key];
  if (named) return withMods(e, named, /* dropShift for Tab handled above */ false);

  if (key.length === 1) {
    if (/[a-zA-Z]/.test(key)) {
      // Letters: Ctrl/Alt lowercases the base and keeps modifiers; plain uses e.key's case
      // (the browser already gives 'A' for Shift+a), with no shift token.
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
    base = code.toLowerCase();
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
 * The 3-branch rule (docs/gui/05 §8.4): key-first, with a code fallback while an IME is
 * mid-composition (isComposing / Process / Dead) OR any Alt-modified chord, so GUI chord
 * semantics match the TUI on every layout including Korean. Alt (Option on macOS) composes
 * layout-dependent glyphs into `e.key` (Option+Shift+R → '®'), which would drop the base
 * key and Shift; the physical code recovers the intended `alt+shift+r`.
 */
export function chordFromEvent(e: KeyboardEvent): string | null {
  if (e.metaKey) return null; // leave Cmd/Meta shortcuts to the OS/browser
  if (e.altKey || e.key === 'Process' || e.key === 'Dead' || e.isComposing) {
    return chordFromCode(e);
  }
  return chordFromKey(e);
}

/** Capture follows dispatch normalization, including its IME/Alt physical-code fallback. */
export function chordFromCapture(e: KeyboardEvent): string | null {
  return chordFromEvent(e);
}

/** Pretty token for display: modifiers / named keys TitleCased, letter case preserved. */
const DISPLAY_TOKEN: Record<string, string> = {
  ctrl: 'Ctrl',
  alt: 'Alt',
  shift: 'Shift',
  space: 'Space',
  enter: 'Enter',
  esc: 'Esc',
  tab: 'Tab',
  backtab: 'BackTab',
  backspace: 'Backspace',
  delete: 'Delete',
  insert: 'Insert',
  left: 'Left',
  right: 'Right',
  up: 'Up',
  down: 'Down',
  home: 'Home',
  end: 'End',
  pageup: 'PageUp',
  pagedown: 'PageDown',
};

/** Render a canonical config chord for humans (`ctrl+u → Ctrl+u`, `shift+f10 → Shift+F10`). */
export function displayChord(chord: string): string {
  return chord
    .split('+')
    .map((tok) => DISPLAY_TOKEN[tok] ?? (FKEY.test(tok) ? tok.toUpperCase() : tok))
    .join('+');
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
