// The composition root shared by the embedded shell and browser-backed tests. Keep store
// construction and subscription ownership here so every frontend host boots the same graph.

import type { Topic } from '../generated/protocol/Topic';
import type { AppCtx } from './ctx';
import type { BootPayload } from './ipc/boot';
import { Client } from './ipc/client';
import type { Transport } from './ipc/transport';
import { AccountsStore } from './stores/accounts.svelte';
import { AiStore } from './stores/ai.svelte';
import { AnimStore } from './stores/anim.svelte';
import { ConnectionStore } from './stores/connection.svelte';
import { DownloadsStore } from './stores/downloads.svelte';
import { KeymapStore } from './stores/keymap.svelte';
import { LibraryStore } from './stores/library.svelte';
import { LyricsStore } from './stores/lyrics.svelte';
import { PlaybackStore } from './stores/playback.svelte';
import { PlaylistsStore } from './stores/playlists.svelte';
import { QueueStore } from './stores/queue.svelte';
import { SearchStore } from './stores/search.svelte';
import { SettingsStore } from './stores/settings.svelte';
import { ThemeStore } from './stores/theme.svelte';
import { ToastStore } from './stores/toasts.svelte';
import { TransferStore } from './stores/transfer.svelte';
import { UiStore } from './stores/ui.svelte';
import { WhyGemStore } from './stores/whygem.svelte';

/** Topics needed by the application frame. The client deduplicates reconnect subscriptions. */
export const APP_TOPICS: Topic[] = [
  'player',
  'queue',
  'lyrics',
  'search',
  'library',
  'playlists',
  'ai',
  'downloads',
  'transfer',
  'accounts',
  'settings',
  'system',
];

/** Build and boot the complete frontend store graph for one window. */
export function createAppCtx(boot: BootPayload, transport: Transport): AppCtx {
  const client = new Client(transport);
  const theme = new ThemeStore(client);
  const connection = new ConnectionStore(client);
  const toasts = new ToastStore();

  theme.boot(boot.theme);
  toasts.attach(client);

  const ctx: AppCtx = {
    boot,
    client,
    demo: !transport.live,
    connection,
    theme,
    ui: new UiStore(boot.uiState),
    playback: new PlaybackStore(client),
    queue: new QueueStore(client, (message) => toasts.show('error', message)),
    search: new SearchStore(client),
    library: new LibraryStore(client),
    ai: new AiStore(client),
    downloads: new DownloadsStore(client),
    playlists: new PlaylistsStore(client),
    transfer: new TransferStore(client),
    accounts: new AccountsStore(client),
    settings: new SettingsStore(client, (message) => toasts.show('error', message)),
    anim: new AnimStore(client),
    keymap: new KeymapStore(client),
    lyrics: new LyricsStore(client),
    whygem: new WhyGemStore(client),
    toasts,
  };

  // One subscription for the whole window; the gateway aggregates across windows. Topics
  // without a live core wire yet simply never push (see gui/WIRING.md).
  client.sub(APP_TOPICS);

  // This handshake must run after WryTransport installed its receiver and all stores registered
  // handlers. The host responds with its latest connection and compact protocol snapshots.
  if (transport.live) client.win('frontendReady');

  return ctx;
}
