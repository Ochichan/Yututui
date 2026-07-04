// Boot: read the injected payload, pick a transport (real shell vs the in-page demo
// core), apply the theme, assemble the store bundle, and mount App (docs/gui/05 §2).

import { mount } from 'svelte';
import App from './App.svelte';
import './app.css';
import { readBoot } from './lib/ipc/boot';
import { WryTransport, type Transport } from './lib/ipc/transport';
import { Client } from './lib/ipc/client';
import { DemoCoreTransport } from './lib/dev/democore';
import type { AppCtx } from './lib/ctx';
import { ConnectionStore } from './lib/stores/connection.svelte';
import { UiStore } from './lib/stores/ui.svelte';
import { ThemeStore } from './lib/stores/theme.svelte';
import { PlaybackStore } from './lib/stores/playback.svelte';
import { QueueStore } from './lib/stores/queue.svelte';
import { SearchStore } from './lib/stores/search.svelte';
import { LibraryStore } from './lib/stores/library.svelte';
import { AiStore } from './lib/stores/ai.svelte';
import { DownloadsStore } from './lib/stores/downloads.svelte';
import { PlaylistsStore } from './lib/stores/playlists.svelte';
import { TransferStore } from './lib/stores/transfer.svelte';
import { AccountsStore } from './lib/stores/accounts.svelte';
import { SettingsStore } from './lib/stores/settings.svelte';
import { AnimStore } from './lib/stores/anim.svelte';
import { KeymapStore } from './lib/stores/keymap.svelte';
import { LyricsStore } from './lib/stores/lyrics.svelte';
import { WhyGemStore } from './lib/stores/whygem.svelte';
import { ToastStore } from './lib/stores/toasts.svelte';
import { WipStore } from './lib/wiring/wip.svelte';

const boot = readBoot();

// Real shell when wry injected window.ipc; otherwise the demo core keeps the whole UI
// alive and interactive in a plain browser (docs/gui/05 §4.3).
const transport: Transport = window.ipc ? new WryTransport() : new DemoCoreTransport();
const client = new Client(transport);

// Apply the theme before mount so the first paint is themed: core-provided boot theme
// first, then the user's persisted local skin (lib/theme/local.ts) on top. The store also
// subscribes to the `settings` theme push for the live editor (settings.theme-editor).
const theme = new ThemeStore(client);
theme.boot(boot.theme);

const connection = new ConnectionStore(client);
const toasts = new ToastStore();
toasts.attach(client);

const ctx: AppCtx = {
  boot,
  client,
  demo: !transport.live,
  connection,
  theme,
  ui: new UiStore(),
  playback: new PlaybackStore(client),
  queue: new QueueStore(client),
  search: new SearchStore(client),
  library: new LibraryStore(client),
  ai: new AiStore(client),
  downloads: new DownloadsStore(client),
  playlists: new PlaylistsStore(client),
  transfer: new TransferStore(client),
  accounts: new AccountsStore(client),
  settings: new SettingsStore(client),
  anim: new AnimStore(client),
  keymap: new KeymapStore(client),
  lyrics: new LyricsStore(client),
  whygem: new WhyGemStore(client),
  toasts,
  wip: new WipStore(connection),
};

// One subscription for the whole window; the gateway aggregates across windows. Topics
// without a live core wire yet simply never push (see gui/WIRING.md).
client.sub([
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
]);

const app = mount(App, {
  target: document.getElementById('app')!,
  props: { ctx },
});

export default app;
