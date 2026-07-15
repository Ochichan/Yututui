// Shared GUI text-editing primitives. DOM selection offsets are UTF-16 code-unit offsets,
// while every visible-character operation below follows extended grapheme boundaries. Word
// boundaries mirror src/util/text_edit.rs: whitespace is skipped first, then one run of either
// word characters (letters, numbers, combining marks, underscore) or symbols is traversed.

type TextControl = HTMLInputElement | HTMLTextAreaElement;

export const TEXT_EDIT_ACTIONS = [
  'delete_char',
  'delete_word',
  'move_cursor_left',
  'move_cursor_right',
  'move_cursor_word_left',
  'move_cursor_word_right',
] as const;
export type TextEditAction = (typeof TEXT_EDIT_ACTIONS)[number];

const TEXT_INPUT_TYPES = new Set(['', 'text', 'search', 'password', 'email', 'url', 'tel']);
const WORD_SCALAR = /^[\p{L}\p{N}\p{M}_]$/u;
const WHITESPACE_SCALAR = /^\s$/u;
const MARK = /^\p{M}$/u;
const pendingChange = new WeakSet<TextControl>();

interface Grapheme {
  text: string;
  start: number;
  end: number;
}

function asTextControl(target: EventTarget | null): TextControl | null {
  if (target instanceof HTMLTextAreaElement) return target;
  if (target instanceof HTMLInputElement && TEXT_INPUT_TYPES.has(target.type.toLowerCase())) {
    return target;
  }
  return null;
}

function characterClass(grapheme: string): 'word' | 'symbol' | 'space' {
  const first = Array.from(grapheme)[0] ?? '';
  if (WHITESPACE_SCALAR.test(first)) return 'space';
  return WORD_SCALAR.test(first) ? 'word' : 'symbol';
}

interface Scalar {
  text: string;
  codePoint: number;
  start: number;
  end: number;
}

function isRegionalIndicator(codePoint: number): boolean {
  return codePoint >= 0x1f1e6 && codePoint <= 0x1f1ff;
}

function isGraphemeExtension(scalar: Scalar): boolean {
  return (
    MARK.test(scalar.text) ||
    (scalar.codePoint >= 0x1f3fb && scalar.codePoint <= 0x1f3ff) ||
    (scalar.codePoint >= 0xe0020 && scalar.codePoint <= 0xe007f)
  );
}

function hangulClass(codePoint: number): 'l' | 'v' | 't' | 'lv' | 'lvt' | null {
  if (
    (codePoint >= 0x1100 && codePoint <= 0x115f) ||
    (codePoint >= 0xa960 && codePoint <= 0xa97c)
  ) {
    return 'l';
  }
  if (
    (codePoint >= 0x1160 && codePoint <= 0x11a7) ||
    (codePoint >= 0xd7b0 && codePoint <= 0xd7c6)
  ) {
    return 'v';
  }
  if (
    (codePoint >= 0x11a8 && codePoint <= 0x11ff) ||
    (codePoint >= 0xd7cb && codePoint <= 0xd7fb)
  ) {
    return 't';
  }
  if (codePoint >= 0xac00 && codePoint <= 0xd7a3) {
    return (codePoint - 0xac00) % 28 === 0 ? 'lv' : 'lvt';
  }
  return null;
}

function joinsHangul(left: Scalar, right: Scalar): boolean {
  const a = hangulClass(left.codePoint);
  const b = hangulClass(right.codePoint);
  return (
    (a === 'l' && (b === 'l' || b === 'v' || b === 'lv' || b === 'lvt')) ||
    ((a === 'lv' || a === 'v') && (b === 'v' || b === 't')) ||
    ((a === 'lvt' || a === 't') && b === 't')
  );
}

/** Dependency-free fallback for WebViews without Intl.Segmenter. */
function fallbackGraphemes(value: string): Grapheme[] {
  const scalars: Scalar[] = [];
  let offset = 0;
  for (const text of Array.from(value)) {
    const end = offset + text.length;
    scalars.push({ text, codePoint: text.codePointAt(0) ?? 0, start: offset, end });
    offset = end;
  }

  const out: Grapheme[] = [];
  let i = 0;
  while (i < scalars.length) {
    const start = i;
    i += 1;

    if (scalars[start].codePoint === 0x0d && scalars[i]?.codePoint === 0x0a) {
      i += 1;
    } else if (isRegionalIndicator(scalars[start].codePoint)) {
      if (scalars[i] && isRegionalIndicator(scalars[i].codePoint)) i += 1;
    } else {
      while (scalars[i] && joinsHangul(scalars[i - 1], scalars[i])) i += 1;
    }

    while (scalars[i] && isGraphemeExtension(scalars[i])) i += 1;
    while (scalars[i]?.codePoint === 0x200d) {
      i += 1;
      if (!scalars[i]) break;
      i += 1;
      while (scalars[i] && isGraphemeExtension(scalars[i])) i += 1;
    }

    const startOffset = scalars[start].start;
    const endOffset = scalars[i - 1].end;
    out.push({ text: value.slice(startOffset, endOffset), start: startOffset, end: endOffset });
  }
  return out;
}

/** Split a string without ever confusing DOM UTF-16 offsets with character counts. */
function graphemes(value: string): Grapheme[] {
  try {
    if (typeof Intl.Segmenter === 'function') {
      const out: Grapheme[] = [];
      const segments = new Intl.Segmenter(undefined, { granularity: 'grapheme' }).segment(value);
      for (const part of segments) {
        out.push({ text: part.segment, start: part.index, end: part.index + part.segment.length });
      }
      return out;
    }
  } catch {
    // An incomplete WebView Intl implementation uses the local extended-cluster fallback.
  }
  return fallbackGraphemes(value);
}

function clampOffset(value: string, offset: number): number {
  return Math.max(0, Math.min(value.length, offset));
}

function boundaryAtOrBefore(parts: Grapheme[], valueLength: number, offset: number): number {
  const wanted = Math.max(0, Math.min(valueLength, offset));
  let boundary = 0;
  for (const part of parts) {
    if (part.start > wanted) break;
    boundary = part.start;
    if (part.end <= wanted) boundary = part.end;
  }
  return boundary;
}

function boundaryAtOrAfter(parts: Grapheme[], valueLength: number, offset: number): number {
  const wanted = Math.max(0, Math.min(valueLength, offset));
  if (wanted === 0) return 0;
  for (const part of parts) {
    if (part.start >= wanted) return part.start;
    if (part.end >= wanted) return part.end;
  }
  return valueLength;
}

function previousGraphemeBoundary(parts: Grapheme[], valueLength: number, offset: number): number {
  const wanted = Math.max(0, Math.min(valueLength, offset));
  let previous = 0;
  for (const part of parts) {
    if (part.end >= wanted) return part.start;
    previous = part.end;
  }
  return previous;
}

function nextGraphemeBoundary(parts: Grapheme[], valueLength: number, offset: number): number {
  const wanted = Math.max(0, Math.min(valueLength, offset));
  for (const part of parts) {
    if (part.start >= wanted && part.start !== wanted) return part.start;
    if (part.end > wanted) return part.end;
  }
  return valueLength;
}

function wordBoundaryLeft(parts: Grapheme[], offset: number): number {
  let i = 0;
  while (i < parts.length && parts[i].end <= offset) i += 1;

  while (i > 0 && characterClass(parts[i - 1].text) === 'space') i -= 1;
  if (i === 0) return 0;

  const wanted = characterClass(parts[i - 1].text);
  while (i > 0 && characterClass(parts[i - 1].text) === wanted) i -= 1;
  return parts[i]?.start ?? 0;
}

function wordBoundaryRight(parts: Grapheme[], valueLength: number, offset: number): number {
  let i = parts.findIndex((part) => part.end > offset);
  if (i < 0) return valueLength;

  const current = characterClass(parts[i].text);
  if (current !== 'space') {
    while (i < parts.length && characterClass(parts[i].text) === current) i += 1;
  }
  while (i < parts.length && characterClass(parts[i].text) === 'space') i += 1;
  return parts[i]?.start ?? valueLength;
}

function markPendingChange(control: TextControl): void {
  if (pendingChange.has(control)) return;
  pendingChange.add(control);

  const onChange = () => {
    pendingChange.delete(control);
    control.removeEventListener('blur', onBlur);
  };
  const onBlur = () => {
    queueMicrotask(() => {
      control.removeEventListener('change', onChange);
      if (pendingChange.delete(control)) {
        control.dispatchEvent(new Event('change', { bubbles: true }));
      }
    });
  };

  control.addEventListener('change', onChange, { once: true });
  control.addEventListener('blur', onBlur, { once: true });
}

/** Whether the event target is one of the app's editable string controls. */
export function isTextInputTarget(target: EventTarget | null): boolean {
  return asTextControl(target) != null;
}

function selection(control: TextControl): { start: number; end: number } | null {
  const start = control.selectionStart;
  const end = control.selectionEnd;
  if (start == null || end == null) return null;
  return { start, end };
}

function moveCursor(
  target: EventTarget | null,
  direction: 'left' | 'right',
  byWord: boolean,
): boolean {
  const control = asTextControl(target);
  if (!control) return false;

  const selected = selection(control);
  if (!selected) return false;

  if (selected.start !== selected.end) {
    const parts = graphemes(control.value);
    const collapseTo =
      direction === 'left'
        ? boundaryAtOrBefore(parts, control.value.length, selected.start)
        : boundaryAtOrAfter(parts, control.value.length, selected.end);
    control.setSelectionRange(collapseTo, collapseTo, 'none');
    return true;
  }

  const parts = graphemes(control.value);
  const caret = clampOffset(control.value, selected.start);
  let next: number;
  if (direction === 'left') {
    next = byWord
      ? wordBoundaryLeft(parts, boundaryAtOrAfter(parts, control.value.length, caret))
      : previousGraphemeBoundary(parts, control.value.length, caret);
  } else {
    next = byWord
      ? wordBoundaryRight(parts, control.value.length, caret)
      : nextGraphemeBoundary(parts, control.value.length, caret);
  }
  control.setSelectionRange(next, next, 'none');
  return true;
}

function dispatchDeletion(
  control: TextControl,
  deleteFrom: number,
  deleteTo: number,
  inputType: 'deleteContentBackward' | 'deleteWordBackward',
): void {
  if (deleteFrom === deleteTo) return;
  control.setRangeText('', deleteFrom, deleteTo, 'end');
  control.setSelectionRange(deleteFrom, deleteFrom, 'none');
  control.dispatchEvent(
    new InputEvent('input', {
      bubbles: true,
      composed: true,
      inputType,
    }),
  );
  markPendingChange(control);
}

/** Delete the selection, or the previous grapheme, from an input/textarea. */
export function deleteCharBackward(target: EventTarget | null): boolean {
  const control = asTextControl(target);
  if (!control) return false;

  const selected = selection(control);
  if (!selected) return false;
  const parts = graphemes(control.value);

  let deleteFrom: number;
  let deleteTo: number;
  if (selected.start !== selected.end) {
    deleteFrom = boundaryAtOrBefore(parts, control.value.length, selected.start);
    deleteTo = boundaryAtOrAfter(parts, control.value.length, selected.end);
  } else {
    const caret = clampOffset(control.value, selected.start);
    const atBoundary = boundaryAtOrBefore(parts, control.value.length, caret) === caret;
    deleteTo = atBoundary ? caret : boundaryAtOrAfter(parts, control.value.length, caret);
    deleteFrom = previousGraphemeBoundary(parts, control.value.length, deleteTo);
  }

  dispatchDeletion(control, deleteFrom, deleteTo, 'deleteContentBackward');
  // A supported text control owns the chord even when the caret is already at the start.
  return true;
}

/** Delete the selection, or the previous editor-style word, from an input/textarea. */
export function deleteWordBackward(target: EventTarget | null): boolean {
  const control = asTextControl(target);
  if (!control) return false;

  const selected = selection(control);
  if (!selected) return false;
  const parts = graphemes(control.value);

  let deleteFrom: number;
  let deleteTo: number;
  if (selected.start !== selected.end) {
    deleteFrom = boundaryAtOrBefore(parts, control.value.length, selected.start);
    deleteTo = boundaryAtOrAfter(parts, control.value.length, selected.end);
  } else {
    deleteTo = boundaryAtOrAfter(parts, control.value.length, selected.start);
    deleteFrom = wordBoundaryLeft(parts, deleteTo);
  }

  dispatchDeletion(control, deleteFrom, deleteTo, 'deleteWordBackward');

  // A supported text control owns the chord even when the caret is already at the start.
  return true;
}

/** Apply one remappable Common text-editor action to a native text control. */
export function applyTextEditAction(target: EventTarget | null, action: TextEditAction): boolean {
  switch (action) {
    case 'delete_char':
      return deleteCharBackward(target);
    case 'delete_word':
      return deleteWordBackward(target);
    case 'move_cursor_left':
      return moveCursor(target, 'left', false);
    case 'move_cursor_right':
      return moveCursor(target, 'right', false);
    case 'move_cursor_word_left':
      return moveCursor(target, 'left', true);
    case 'move_cursor_word_right':
      return moveCursor(target, 'right', true);
  }
}
