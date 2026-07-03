// Library pages (docs/gui/07 §4): windowed, filtered lists per scope. Unlike search, pages
// are pulled with a correlated `fetch_library_page` request (not a topic push); the `library`
// topic only pushes invalidations that trigger a re-fetch of the current view. A per-fetch
// sequence guards against a slow earlier page landing over a newer scope/filter. The desktop
// bridge forwards these once the core advertises `library-v8`; until then the in-page demo
// core answers (see gui/WIRING.md).

import type { TrackModel } from '../../generated/protocol/TrackModel';
import type { Client } from '../ipc/client';

/** Fetchable library scopes — the `downloads`/`playlists` tabs are their own features. */
export type LibraryScope = 'all' | 'favorites' | 'history' | 'radio_likes' | 'radio_history';

// PROVISIONAL wire shape — only the demo core speaks it. Reconcile with the M2 core wire +
// ts-rs types when they land (mirrors the search/lyrics provisional note).
export interface LibraryPage {
  scope: LibraryScope;
  filter: string;
  offset: number;
  total: number;
  tracks: TrackModel[];
}

const PAGE = 50;

export class LibraryStore {
  scope = $state<LibraryScope>('all');
  filter = $state('');
  tracks = $state<TrackModel[]>([]);
  total = $state(0);
  loading = $state(false);
  /** True once a page has been requested, so the empty state reads "nothing here" not "loading". */
  ran = $state(false);
  #seq = 0;
  readonly #client: Client;

  constructor(client: Client) {
    this.#client = client;
    // A membership change core-side invalidates the current view — re-pull it.
    this.#client.on('library', () => void this.reload());
  }

  /** Point the view at a scope + filter and pull the first page. */
  load(scope: LibraryScope, filter: string): Promise<void> {
    this.scope = scope;
    this.filter = filter;
    return this.#fetch(0, true);
  }

  reload(): Promise<void> {
    return this.#fetch(0, true);
  }

  /** Pull and append the next window. */
  more(): Promise<void> {
    if (!this.hasMore) return Promise.resolve();
    return this.#fetch(this.tracks.length, false);
  }

  get hasMore(): boolean {
    return this.tracks.length < this.total;
  }

  get empty(): boolean {
    return this.ran && !this.loading && this.tracks.length === 0;
  }

  playAll(): void {
    this.#client.cmd('library_play', { scope: this.scope, filter: this.filter });
  }
  enqueueAll(): void {
    this.#client.cmd('library_enqueue', { scope: this.scope, filter: this.filter });
  }
  play(track: TrackModel): void {
    this.#client.cmd('play_tracks', { video_ids: [track.video_id] });
  }
  enqueue(track: TrackModel): void {
    this.#client.cmd('enqueue_tracks', { video_ids: [track.video_id] });
  }
  /** Drop a row from a removable scope (un-favorite / forget history) — core reconciles. */
  remove(track: TrackModel): void {
    this.#client.cmd('library_remove', { scope: this.scope, video_id: track.video_id });
  }

  async #fetch(offset: number, replace: boolean): Promise<void> {
    const seq = ++this.#seq;
    const { scope, filter } = this;
    this.loading = true;
    this.ran = true;
    try {
      const page = await this.#client.req<LibraryPage>('fetch_library_page', {
        scope,
        filter,
        offset,
        limit: PAGE,
      });
      if (seq !== this.#seq) return; // a newer load/reload superseded this page
      this.total = page.total;
      this.tracks = replace ? page.tracks : [...this.tracks, ...page.tracks];
    } catch {
      if (seq === this.#seq && replace) {
        this.tracks = [];
        this.total = 0;
      }
    } finally {
      if (seq === this.#seq) this.loading = false;
    }
  }
}
