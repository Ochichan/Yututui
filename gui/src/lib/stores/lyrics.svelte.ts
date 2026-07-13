// Synced lyrics for the Now Playing pane.
//
// LIVE-WIRED (B1): mirrors the `lyrics` topic — the core pushes a `lyrics_snapshot`
// as the initial subscribe snapshot, a clearing push on track change, and the resolved
// lines when its fetch completes. Shapes come from the generated ts-rs types.

import type { LyricLineModel } from '../../generated/protocol/LyricLineModel';
import type { PushEvent } from '../../generated/protocol/PushEvent';
import type { Client } from '../ipc/client';

export type LyricLine = LyricLineModel;

export class LyricsStore {
  lines = $state<LyricLine[]>([]);
  videoId = $state<string | null>(null);

  constructor(client: Client) {
    client.on('lyrics', (payload) => {
      const ev = payload as PushEvent;
      if (ev.kind !== 'lyrics_snapshot') return;
      this.lines = ev.lines ?? [];
      this.videoId = ev.video_id ?? null;
    });
  }

  /** Index of the active line for a playback position; -1 before the first line. */
  activeIndex(positionMs: number | null): number {
    if (positionMs == null) return -1;
    let active = -1;
    for (let i = 0; i < this.lines.length; i++) {
      const ms = this.lines[i].ms;
      if (ms != null && ms <= positionMs) active = i;
    }
    return active;
  }
}
