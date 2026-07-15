import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  applyTextEditAction,
  deleteCharBackward,
  deleteWordBackward,
  isTextInputTarget,
} from '../src/lib/keyboard/text-edit';

function input(value: string, type = 'text'): HTMLInputElement {
  const el = document.createElement('input');
  el.type = type;
  el.value = value;
  document.body.appendChild(el);
  if (type !== 'number') el.setSelectionRange(value.length, value.length);
  return el;
}

afterEach(() => {
  document.body.replaceChildren();
});

describe('deleteWordBackward', () => {
  it.each([
    ['lofi hip hop', 'lofi hip '],
    ['foo/bar', 'foo/'],
    ['foo/', 'foo'],
    ['snake_case', ''],
    ['안녕 세계', '안녕 '],
    ['日本語 テスト', '日本語 '],
    ['cafe\u0301', ''],
    ['hello 👋', 'hello '],
    ['snake_a\u200d', ''],
    ['go ☕️👩‍💻', 'go '],
  ])('deletes one editor-style segment from %s', (before, after) => {
    const el = input(before);
    expect(deleteWordBackward(el)).toBe(true);
    expect(el.value).toBe(after);
    expect(el.selectionStart).toBe(after.length);
  });

  it('deletes trailing whitespace together with the previous segment', () => {
    const el = input('one two   ');
    deleteWordBackward(el);
    expect(el.value).toBe('one ');
  });

  it('deletes a selection and preserves text after the caret', () => {
    const selected = input('one two three');
    selected.setSelectionRange(4, 7);
    deleteWordBackward(selected);
    expect(selected.value).toBe('one  three');
    expect(selected.selectionStart).toBe(4);

    const middle = input('one two three');
    middle.setSelectionRange(7, 7);
    deleteWordBackward(middle);
    expect(middle.value).toBe('one  three');
  });

  it('supports password fields but leaves numeric controls to the browser', () => {
    const password = input('secret token', 'password');
    expect(isTextInputTarget(password)).toBe(true);
    expect(deleteWordBackward(password)).toBe(true);
    expect(password.value).toBe('secret ');

    const number = input('8888', 'number');
    expect(isTextInputTarget(number)).toBe(false);
    expect(deleteWordBackward(number)).toBe(false);
    expect(number.value).toBe('8888');
  });

  it('dispatches input immediately and change only when editing is committed', async () => {
    const el = input('alpha beta');
    const onInput = vi.fn();
    const onChange = vi.fn();
    el.addEventListener('input', onInput);
    el.addEventListener('change', onChange);

    deleteWordBackward(el);
    expect(onInput).toHaveBeenCalledTimes(1);
    expect(onChange).not.toHaveBeenCalled();

    el.dispatchEvent(new Event('blur'));
    await Promise.resolve();
    expect(onChange).toHaveBeenCalledTimes(1);
  });

  it('does not duplicate a native change event at blur', async () => {
    const el = input('alpha beta');
    const onChange = vi.fn();
    el.addEventListener('change', onChange);

    deleteWordBackward(el);
    el.dispatchEvent(new Event('change', { bubbles: true }));
    el.dispatchEvent(new Event('blur'));
    await Promise.resolve();
    expect(onChange).toHaveBeenCalledTimes(1);
  });
});

describe('grapheme-aware character editing', () => {
  it.each([
    ['cafe\u0301', 'caf'],
    ['wave 👋🏽', 'wave '],
    ['family 👨‍👩‍👧‍👦', 'family '],
    ['flag 🇰🇷', 'flag '],
  ])('deletes one complete grapheme from %s', (before, after) => {
    const el = input(before);
    expect(deleteCharBackward(el)).toBe(true);
    expect(el.value).toBe(after);
    expect(el.selectionStart).toBe(after.length);
  });

  it('moves by grapheme using DOM UTF-16 offsets', () => {
    const el = input('a👨‍👩‍👧‍👦e\u0301🇰🇷');

    applyTextEditAction(el, 'move_cursor_left');
    expect(el.selectionStart).toBe('a👨‍👩‍👧‍👦e\u0301'.length);
    applyTextEditAction(el, 'move_cursor_left');
    expect(el.selectionStart).toBe('a👨‍👩‍👧‍👦'.length);
    applyTextEditAction(el, 'move_cursor_left');
    expect(el.selectionStart).toBe(1);
    applyTextEditAction(el, 'move_cursor_right');
    expect(el.selectionStart).toBe('a👨‍👩‍👧‍👦'.length);
  });

  it('collapses forward and backward selections toward the requested direction', () => {
    const el = input('zero one two');
    el.setSelectionRange(5, 8, 'backward');
    applyTextEditAction(el, 'move_cursor_left');
    expect(el.selectionStart).toBe(5);
    expect(el.selectionEnd).toBe(5);

    el.setSelectionRange(5, 8, 'forward');
    applyTextEditAction(el, 'move_cursor_right');
    expect(el.selectionStart).toBe(8);
    expect(el.selectionEnd).toBe(8);
  });

  it('keeps extended graphemes intact when Intl.Segmenter is unavailable', () => {
    const segmenter = Intl.Segmenter;
    Object.defineProperty(Intl, 'Segmenter', { configurable: true, value: undefined });
    try {
      const clusters = ['a', 'e\u0301', '☕️', '👋🏽', '👩‍💻', '🇰🇷', '한'];
      const el = input(clusters.join(''));

      for (let i = clusters.length - 1; i >= 0; i -= 1) {
        applyTextEditAction(el, 'move_cursor_left');
        expect(el.selectionStart).toBe(clusters.slice(0, i).join('').length);
      }

      el.setSelectionRange(el.value.length, el.value.length);
      for (let i = clusters.length - 1; i >= 0; i -= 1) {
        deleteCharBackward(el);
        expect(el.value).toBe(clusters.slice(0, i).join(''));
      }
    } finally {
      Object.defineProperty(Intl, 'Segmenter', { configurable: true, value: segmenter });
    }
  });
});

describe('cursor word movement', () => {
  it('moves to adjacent word and symbol run starts in both directions', () => {
    const el = input('one  foo/bar  안녕 세계');
    el.setSelectionRange(0, 0);

    const rightStops = [5, 8, 9, 14, 17, el.value.length];
    for (const stop of rightStops) {
      applyTextEditAction(el, 'move_cursor_word_right');
      expect(el.selectionStart).toBe(stop);
    }

    const leftStops = [17, 14, 9, 8, 5, 0];
    for (const stop of leftStops) {
      applyTextEditAction(el, 'move_cursor_word_left');
      expect(el.selectionStart).toBe(stop);
    }
  });

  it('collapses a selection without jumping an additional word', () => {
    const el = input('alpha beta gamma');
    el.setSelectionRange(6, 10);
    applyTextEditAction(el, 'move_cursor_word_left');
    expect(el.selectionStart).toBe(6);

    el.setSelectionRange(6, 10);
    applyTextEditAction(el, 'move_cursor_word_right');
    expect(el.selectionStart).toBe(10);
  });
});

describe('text-edit events', () => {
  it('does not emit input or change events for movement', () => {
    const el = input('alpha beta');
    const onInput = vi.fn();
    const onChange = vi.fn();
    el.addEventListener('input', onInput);
    el.addEventListener('change', onChange);

    applyTextEditAction(el, 'move_cursor_left');
    applyTextEditAction(el, 'move_cursor_word_left');
    applyTextEditAction(el, 'move_cursor_right');
    applyTextEditAction(el, 'move_cursor_word_right');

    expect(onInput).not.toHaveBeenCalled();
    expect(onChange).not.toHaveBeenCalled();
    expect(el.value).toBe('alpha beta');
  });

  it('emits the matching inputType for character deletion', () => {
    const el = input('alpha😀');
    const inputTypes: string[] = [];
    el.addEventListener('input', (event) => inputTypes.push((event as InputEvent).inputType));

    applyTextEditAction(el, 'delete_char');
    expect(inputTypes).toEqual(['deleteContentBackward']);
    expect(el.value).toBe('alpha');
  });
});
