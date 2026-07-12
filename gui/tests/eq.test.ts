import { describe, expect, it } from 'vitest';
import { EQ_MAX, EQ_MIN, clampGain, gainAtPointer } from '../src/lib/eq';

const rect = { top: 100, height: 96 };

describe('gainAtPointer', () => {
  it('maps the track top to +12 and the bottom to -12', () => {
    expect(gainAtPointer(rect, 100)).toBe(EQ_MAX);
    expect(gainAtPointer(rect, 196)).toBe(EQ_MIN);
  });

  it('maps the middle to 0', () => {
    expect(gainAtPointer(rect, 148)).toBe(0);
  });

  it('clamps pointer positions outside the track', () => {
    expect(gainAtPointer(rect, -500)).toBe(EQ_MAX);
    expect(gainAtPointer(rect, 900)).toBe(EQ_MIN);
  });

  it('degrades to 0 on a zero-height rect instead of NaN', () => {
    expect(gainAtPointer({ top: 0, height: 0 }, 50)).toBe(0);
  });
});

describe('clampGain', () => {
  it('rounds and clamps to the ±12 whole-dB range', () => {
    expect(clampGain(3.6)).toBe(4);
    expect(clampGain(99)).toBe(EQ_MAX);
    expect(clampGain(-99)).toBe(EQ_MIN);
    expect(clampGain(0)).toBe(0);
  });
});
