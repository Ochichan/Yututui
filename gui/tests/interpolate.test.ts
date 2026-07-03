// The ported mini-player interpolation constants and math (docs/gui/05 §5.1) — frozen
// as table cases so a refactor can't quietly re-derive them.

import { describe, expect, it } from 'vitest';
import {
  SEEK_HOLD_MS,
  VOLUME_ECHO_MS,
  VOLUME_SEND_DEBOUNCE_MS,
  interpolate,
} from '../src/lib/time';

describe('ported constants', () => {
  it('match panel.rs verbatim', () => {
    expect(SEEK_HOLD_MS).toBe(1500);
    expect(VOLUME_ECHO_MS).toBe(1800);
    expect(VOLUME_SEND_DEBOUNCE_MS).toBe(70);
  });
});

describe('interpolate', () => {
  const base = {
    elapsedMs: 10_000,
    anchorAt: 1_000,
    speedTenths: 10,
    paused: false,
    durationMs: 60_000,
  };

  it('advances with wall clock while playing', () => {
    expect(interpolate(base, 3_500)).toBe(12_500);
  });

  it('freezes while paused', () => {
    expect(interpolate({ ...base, paused: true }, 99_000)).toBe(10_000);
  });

  it('scales by speed (tenths)', () => {
    expect(interpolate({ ...base, speedTenths: 15 }, 3_000)).toBe(13_000);
  });

  it('clamps to duration', () => {
    expect(interpolate(base, 10_000_000)).toBe(60_000);
  });

  it('live stream (null duration) is unclamped', () => {
    expect(interpolate({ ...base, durationMs: null }, 101_000)).toBe(110_000);
  });

  it('null elapsed ⇒ nothing playing', () => {
    expect(interpolate({ ...base, elapsedMs: null }, 2_000)).toBeNull();
  });
});
