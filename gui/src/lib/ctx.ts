// The store bundle main.ts assembles and every view receives — explicit props, no
// implicit context magic, so component tests can hand in fakes wholesale.

import type { BootPayload } from './ipc/boot';
import type { Client } from './ipc/client';
import type { ConnectionStore } from './stores/connection.svelte';
import type { ThemeStore } from './stores/theme.svelte';
import type { UiStore } from './stores/ui.svelte';
import type { PlaybackStore } from './stores/playback.svelte';
import type { QueueStore } from './stores/queue.svelte';
import type { SearchStore } from './stores/search.svelte';
import type { LibraryStore } from './stores/library.svelte';
import type { AiStore } from './stores/ai.svelte';
import type { DownloadsStore } from './stores/downloads.svelte';
import type { PlaylistsStore } from './stores/playlists.svelte';
import type { TransferStore } from './stores/transfer.svelte';
import type { AccountsStore } from './stores/accounts.svelte';
import type { SettingsStore } from './stores/settings.svelte';
import type { AnimStore } from './stores/anim.svelte';
import type { KeymapStore } from './stores/keymap.svelte';
import type { LyricsStore } from './stores/lyrics.svelte';
import type { WhyGemStore } from './stores/whygem.svelte';
import type { ToastStore } from './stores/toasts.svelte';

export interface AppCtx {
  boot: BootPayload;
  client: Client;
  /** True when running against the in-page demo core instead of the Rust shell. */
  demo: boolean;
  connection: ConnectionStore;
  theme: ThemeStore;
  ui: UiStore;
  playback: PlaybackStore;
  queue: QueueStore;
  search: SearchStore;
  library: LibraryStore;
  ai: AiStore;
  downloads: DownloadsStore;
  /** Local playlists: list + drill-down detail + the Create/Delete/Add-to-playlist dialogs. */
  playlists: PlaylistsStore;
  /** Spotify import wizard: the `transfer` topic job lifecycle + list/start/cancel. */
  transfer: TransferStore;
  /** Last.fm / ListenBrainz / Spotify connection state + browser-approval connect flows. */
  accounts: AccountsStore;
  settings: SettingsStore;
  /** Animation runtime: the shared fps-gated rAF ticker + the master/reduced-motion contract. */
  anim: AnimStore;
  /** The remappable keymap read model + the in-webview dispatcher's source of truth. */
  keymap: KeymapStore;
  lyrics: LyricsStore;
  /** Why-DJ-Gem provenance + the anchored popover fetch (docs/gui/07 §13). */
  whygem: WhyGemStore;
  toasts: ToastStore;
}
