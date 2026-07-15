// Shared GUI text-editing primitives. Word boundaries mirror src/util/text_edit.rs:
// whitespace is skipped first, then one run of either word characters (letters, numbers,
// combining marks, underscore) or symbols is removed.

type TextControl = HTMLInputElement | HTMLTextAreaElement;

const TEXT_INPUT_TYPES = new Set(['', 'text', 'search', 'password', 'email', 'url', 'tel']);
const WORD_CHAR = /^[\p{L}\p{N}\p{M}_]$/u;
const WHITESPACE = /^\s$/u;
const pendingChange = new WeakSet<TextControl>();

function asTextControl(target: EventTarget | null): TextControl | null {
  if (target instanceof HTMLTextAreaElement) return target;
  if (target instanceof HTMLInputElement && TEXT_INPUT_TYPES.has(target.type.toLowerCase())) {
    return target;
  }
  return null;
}

function characterClass(char: string): 'word' | 'symbol' | 'space' {
  if (WHITESPACE.test(char)) return 'space';
  return WORD_CHAR.test(char) ? 'word' : 'symbol';
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

/** Delete the selection, or the previous editor-style word, from an input/textarea. */
export function deleteWordBackward(target: EventTarget | null): boolean {
  const control = asTextControl(target);
  if (!control) return false;

  const selectionStart = control.selectionStart;
  const selectionEnd = control.selectionEnd;
  if (selectionStart == null || selectionEnd == null) return false;

  let deleteFrom = selectionStart;
  if (selectionStart === selectionEnd && selectionStart > 0) {
    const beforeCaret = Array.from(control.value.slice(0, selectionStart));
    while (beforeCaret.length && characterClass(beforeCaret.at(-1)!) === 'space') {
      beforeCaret.pop();
    }
    if (beforeCaret.length) {
      const segmentClass = characterClass(beforeCaret.at(-1)!);
      while (beforeCaret.length && characterClass(beforeCaret.at(-1)!) === segmentClass) {
        beforeCaret.pop();
      }
    }
    deleteFrom = beforeCaret.join('').length;
  }

  if (deleteFrom !== selectionEnd) {
    control.setRangeText('', deleteFrom, selectionEnd, 'end');
    control.setSelectionRange(deleteFrom, deleteFrom);
    control.dispatchEvent(
      new InputEvent('input', {
        bubbles: true,
        composed: true,
        inputType: 'deleteWordBackward',
      }),
    );
    markPendingChange(control);
  }

  // A supported text control owns the chord even when the caret is already at the start.
  return true;
}
