import { cleanup, fireEvent, render } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import SeekBar from '../src/lib/components/SeekBar.svelte';

afterEach(cleanup);

function installPointerGeometry(track: HTMLElement): void {
  vi.spyOn(track, 'getBoundingClientRect').mockReturnValue({
    x: 0,
    y: 0,
    left: 0,
    top: 0,
    right: 200,
    bottom: 14,
    width: 200,
    height: 14,
    toJSON: () => ({}),
  });

  let captured: number | null = null;
  track.setPointerCapture = (pointerId: number) => {
    captured = pointerId;
  };
  track.hasPointerCapture = (pointerId: number) => captured === pointerId;
  track.releasePointerCapture = (pointerId: number) => {
    if (captured === pointerId) captured = null;
  };
}

describe('SeekBar drag ownership', () => {
  it('commits against the scale captured on pointer down', async () => {
    const onseek = vi.fn();
    const { getByRole } = render(SeekBar, {
      props: {
        mediaKey: 'video-a:10',
        positionMs: 10_000,
        durationMs: 100_000,
        onseek,
      },
    });
    const track = getByRole('slider');
    installPointerGeometry(track);

    await fireEvent.pointerDown(track, { pointerId: 7, clientX: 50 });
    await fireEvent.pointerUp(track, { pointerId: 7, clientX: 50 });

    expect(onseek).toHaveBeenCalledOnce();
    expect(onseek).toHaveBeenCalledWith(25_000);
  });

  it('cancels a stale drag when same-video source recovery changes the epoch', async () => {
    const onseek = vi.fn();
    const props = {
      mediaKey: 'video-a:10',
      positionMs: 10_000,
      durationMs: 100_000,
      onseek,
    };
    const { getByRole, rerender } = render(SeekBar, { props });
    const track = getByRole('slider');
    installPointerGeometry(track);

    await fireEvent.pointerDown(track, { pointerId: 8, clientX: 50 });
    await rerender({ ...props, mediaKey: 'video-a:11', durationMs: 200_000 });
    await fireEvent.pointerUp(track, { pointerId: 8, clientX: 50 });

    expect(onseek).not.toHaveBeenCalled();
  });
});
