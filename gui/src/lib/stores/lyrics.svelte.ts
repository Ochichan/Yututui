// Synced lyrics for the Now Playing pane.
//
// TODO(wire:B1/lyrics.live): the `lyrics` topic does not exist in the core yet (it is a
// B1 deliverable). The { kind: 'lyrics_snapshot', lines } shape below is PROVISIONAL —
// today only the browser demo core emits it. When B1 lands, align this with the real
// PushEvent variant and the regenerated ts-rs types. See gui/WIRING.md.

import type { Client } from '../ipc/client';

export interface LyricLine {
  /** Timestamp; null for unsynced lines. */
  ms: number | null;
  text: string;
}

interface ProvisionalLyricsPush {
  kind: string;
  video_id?: string;
  lines?: LyricLine[];
}

export class LyricsStore {
  lines = $state<LyricLine[]>([]);
  videoId = $state<string | null>(null);

  constructor(client: Client) {
    client.on('lyrics', (payload) => {
      const ev = payload as ProvisionalLyricsPush;
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
