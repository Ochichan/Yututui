// Playlists (docs/gui/07 §5): the local-playlist list, a drill-down detail, and the three
// CRUD dialogs (Create / Delete / Add-to-playlist). Like library pages, the list arrives on
// the `playlists` topic (a snapshot push) while a playlist's tracks are pulled with a
// correlated `fetch_playlist_detail` request; a membership change re-pushes the snapshot,
// which re-pulls an open detail. This store is the feature's hub — it also owns the modal
// UI state so the LibraryView header and a track row's "add to playlist" affordance can both
// drive it without prop-threading. The desktop bridge forwards these once the core advertises
// `library-v8`; until then the in-page demo core answers (see gui/WIRING.md).

import type { TrackModel } from '../../generated/protocol/TrackModel';
import type { Client } from '../ipc/client';

// PROVISIONAL wire shapes — only the demo core speaks them. Reconcile with the M2 core wire +
// ts-rs types when they land (mirrors the search/library/downloads/lyrics provisional note).
export interface PlaylistSummary {
  id: string;
  name: string;
  count: number;
  description: string | null;
}
export interface PlaylistsSnapshot {
  kind: 'playlists_snapshot';
  items: PlaylistSummary[];
}
export interface PlaylistDetail {
  id: string;
  name: string;
  description: string | null;
  tracks: TrackModel[];
}

export class PlaylistsStore {
  list = $state<PlaylistSummary[]>([]);

  // Drill-down: the currently open playlist's tracks (a fetch_playlist_detail response).
  detail = $state<PlaylistDetail | null>(null);
  detailLoading = $state(false);

  // Modal state — store-owned so any surface can open a dialog.
  createOpen = $state(false);
  deleteTarget = $state<PlaylistSummary | null>(null);
  addTarget = $state<TrackModel | null>(null);
  creating = $state(false);
  deleting = $state(false);
  adding = $state(false);

  #seq = 0;
  readonly #client: Client;

  constructor(client: Client) {
    this.#client = client;
    this.#client.on('playlists', (payload) => this.#onPush(payload as PlaylistsSnapshot));
  }

  // ── drill-down ─────────────────────────────────────────────────────────────────────

  async open(id: string): Promise<void> {
    const seq = ++this.#seq;
    this.detailLoading = true;
    try {
      const d = await this.#client.req<PlaylistDetail | null>('fetch_playlist_detail', {
        playlist_id: id,
      });
      if (seq !== this.#seq) return; // a newer open/close superseded this fetch
      this.detail = d ?? null;
    } catch {
      if (seq === this.#seq) this.detail = null;
    } finally {
      if (seq === this.#seq) this.detailLoading = false;
    }
  }

  closeDetail(): void {
    this.#seq++; // invalidate any in-flight fetch
    this.detail = null;
    this.detailLoading = false;
  }

  // ── CRUD ───────────────────────────────────────────────────────────────────────────

  beginCreate(): void {
    this.createOpen = true;
  }
  cancelCreate(): void {
    this.createOpen = false;
  }
  async submitCreate(name: string): Promise<boolean> {
    const n = name.trim();
    if (!n || this.creating) return false;
    this.creating = true;
    const result = await this.#client.cmd('playlist_create', { name: n });
    this.creating = false;
    if (!result.ok) return false;
    this.createOpen = false;
    return true;
  }

  beginDelete(p: PlaylistSummary): void {
    this.deleteTarget = p;
  }
  cancelDelete(): void {
    this.deleteTarget = null;
  }
  async confirmDelete(): Promise<void> {
    const p = this.deleteTarget;
    if (!p || this.deleting) return;
    this.deleting = true;
    const result = await this.#client.cmd('playlist_delete', { playlist_id: p.id });
    this.deleting = false;
    if (!result.ok || this.deleteTarget?.id !== p.id) return;
    if (this.detail?.id === p.id) this.closeDetail();
    this.deleteTarget = null;
  }

  beginAdd(track: TrackModel): void {
    this.addTarget = track;
  }
  cancelAdd(): void {
    this.addTarget = null;
  }
  /** Add the pending track to a playlist and dismiss the picker. */
  async addTo(playlistId: string): Promise<void> {
    const t = this.addTarget;
    if (!t || this.adding) return;
    this.adding = true;
    const result = await this.#client.cmd('playlist_add_tracks', {
      playlist_id: playlistId,
      video_ids: [t.video_id],
    });
    this.adding = false;
    if (!result.ok || this.addTarget?.video_id !== t.video_id) return;
    this.addTarget = null;
  }

  removeTrack(playlistId: string, videoId: string): void {
    this.#client.cmd('playlist_remove_track', { playlist_id: playlistId, video_id: videoId });
  }

  play(playlistId: string): void {
    this.#client.cmd('playlist_play', { playlist_id: playlistId });
  }

  // ── topic ──────────────────────────────────────────────────────────────────────────

  #onPush(s: PlaylistsSnapshot): void {
    if (s.kind !== 'playlists_snapshot') return;
    this.list = s.items;
    const openId = this.detail?.id;
    if (openId == null) return;
    if (!s.items.some((p) => p.id === openId)) {
      // The open playlist was deleted (possibly by another window) — drop the drill-down.
      this.closeDetail();
    } else {
      void this.open(openId); // reflect an add/remove-track membership change
    }
  }
}
