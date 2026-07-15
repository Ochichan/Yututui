import { afterEach, describe, expect, it, vi } from 'vitest';
import { deleteWordBackward, isTextInputTarget } from '../src/lib/keyboard/text-edit';

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
