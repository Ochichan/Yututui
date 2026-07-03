// Downloads (docs/gui/07 §15): trigger a download, watch per-track progress, delete. The
// `downloads` topic pushes a snapshot of every tracked download (Running %/Done/Failed); the
// store just mirrors it. The desktop bridge forwards download/delete_download once the core
// advertises `downloads-v8`; until then the in-page demo core answers (see gui/WIRING.md).

import type { TrackModel } from '../../generated/protocol/TrackModel';
import type { Client } from '../ipc/client';

export type DownloadState = 'running' | 'done' | 'failed';

// PROVISIONAL wire shape — only the demo core speaks it. Reconcile with the M2 core wire +
// ts-rs types when they land.
export interface DownloadStatus {
  video_id: string;
  title: string;
  state: DownloadState;
  /** 0..100 while running, 100 when done. */
  pct: number;
  /** Set on `failed`; null otherwise. */
  error: string | null;
}
export interface DownloadsSnapshot {
  kind: 'downloads_snapshot';
  items: DownloadStatus[];
}

export class DownloadsStore {
  items = $state<DownloadStatus[]>([]);
  readonly #client: Client;

  constructor(client: Client) {
    this.#client = client;
    this.#client.on('downloads', (payload) => this.#onPush(payload as DownloadsSnapshot));
  }

  /** Count of in-flight downloads — drives the transport-bar chip. */
  get active(): number {
    return this.items.filter((d) => d.state === 'running').length;
  }

  download(track: TrackModel): void {
    this.#client.cmd('download', {
      video_id: track.video_id,
      title: track.display_title ?? track.title,
    });
  }

  retry(item: DownloadStatus): void {
    this.#client.cmd('download', { video_id: item.video_id, title: item.title });
  }

  remove(item: DownloadStatus, deleteFile: boolean): void {
    this.#client.cmd('delete_download', { video_id: item.video_id, delete_file: deleteFile });
  }

  #onPush(s: DownloadsSnapshot): void {
    if (s.kind !== 'downloads_snapshot') return;
    this.items = s.items;
  }
}
