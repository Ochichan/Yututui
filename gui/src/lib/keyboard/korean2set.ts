// 두벌식 (2-set) Hangul jamo → QWERTY base-key table (docs/gui/05 §8.4, branch 1 & 3).
//
// PROVISIONAL: hand-authored from the standard 2-set layout. When the Rust chord-fixture
// export lands (gui/src/generated/chord-fixtures.json, §8.5), cross-check this table against
// it in a vitest so a drift fails CI. Until then chord.ts + the demo bindings are the
// self-consistent contract.
//
// Both the plain jamo and its shifted (double / compound) form map to the SAME base latin
// key — the Shift state is carried by the KeyboardEvent, so e.g. ㅃ (Shift+q) resolves to
// base 'q' and the shift rule uppercases it to 'Q', exactly like a QWERTY Shift+q.

export const KOREAN2SET: Record<string, string> = {
  // consonants (top + home rows)
  ㅂ: 'q',
  ㅃ: 'q',
  ㅈ: 'w',
  ㅉ: 'w',
  ㄷ: 'e',
  ㄸ: 'e',
  ㄱ: 'r',
  ㄲ: 'r',
  ㅅ: 't',
  ㅆ: 't',
  ㅁ: 'a',
  ㄴ: 's',
  ㅇ: 'd',
  ㄹ: 'f',
  ㅎ: 'g',
  ㅋ: 'z',
  ㅌ: 'x',
  ㅊ: 'c',
  ㅍ: 'v',
  // vowels
  ㅛ: 'y',
  ㅕ: 'u',
  ㅑ: 'i',
  ㅐ: 'o',
  ㅒ: 'o',
  ㅔ: 'p',
  ㅖ: 'p',
  ㅗ: 'h',
  ㅓ: 'j',
  ㅏ: 'k',
  ㅣ: 'l',
  ㅠ: 'b',
  ㅜ: 'n',
  ㅡ: 'm',
};

/** True if `ch` is a single Hangul compatibility jamo we can remap to a physical key. */
export function isJamo(ch: string): boolean {
  return ch.length === 1 && ch in KOREAN2SET;
}
